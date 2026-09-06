//! 有效工具集（`EffectiveTools`）与系统提示词组装。
//!
//! 每轮只算一次有效工具集，schema 注册、提示词组装、执行分发三处共用
//! （`crate::turn`）；server 不再持有静态提示词。提示词内部稳定段在前、
//! 工具段在后（换工具是显式动作，前缀缓存失效属预期）。

/// 一轮实际生效的工具集。`bash`/`script` 两个布尔即全部工具面；新增工具
/// 时在这里加字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveTools {
    pub bash: bool,
    pub script: bool,
}

impl EffectiveTools {
    /// 计算一轮的有效工具集。
    ///
    /// - `tools_override`: None = 全量；Some(list) = 只保留列表内的名字。
    /// - `script`（图查询与生长）额外要求有 script host 且
    ///   `depth < MAX_SPAWN_DEPTH`。
    pub fn compute(tools_override: Option<&[String]>, has_host: bool, depth: usize) -> Self {
        let allowed = |name: &str| match tools_override {
            None => true,
            Some(list) => list.iter().any(|t| t == name),
        };
        let script_ok = has_host && depth < crate::script::MAX_SPAWN_DEPTH;
        Self {
            bash: allowed("bash"),
            script: script_ok && allowed("script"),
        }
    }

    pub fn allowed(&self, name: &str) -> bool {
        match name {
            "bash" => self.bash,
            "script" => self.script,
            _ => false,
        }
    }

    pub fn any(&self) -> bool {
        self.bash || self.script
    }

    /// 规范序名字列表，用于 Turn 节点记录与 spawn 继承传递。
    pub fn names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.bash {
            names.push("bash".to_string());
        }
        if self.script {
            names.push("script".to_string());
        }
        names
    }
}

/// 按有效工具集组装系统提示词：稳定段在前，工具 bullet 各自独立可丢，
/// 文本只引用使条件为真的工具；完全不提模型。
pub fn build_system_prompt(tools: &EffectiveTools) -> String {
    let mut out = String::from("You are rua, a coding agent living on a conversation graph.");
    if tools.any() {
        out.push_str("\nYou have these tools:");
        if tools.bash {
            out.push_str(
                "\n- `bash`: run a shell command with the project root as its working directory.",
            );
        }
        if tools.script {
            out.push_str(
                "\n- `script`: run a JavaScript program against the conversation graph — \
                 `graph.list(filter)` to scan nodes, `graph.view(id)` to read one, \
                 `graph.spawn({pointer?, content})` to fork a session as a subtask, \
                 `graph.wait(id)` to block for its result, `graph.me()` for your id. Only \
                 `console.log` output is returned.",
            );
        }
    } else {
        out.push_str("\nAll tools are disabled for this turn — answer directly in text.");
    }
    // 搜索引导是策略 guideline（条件 = 工具集谓词），不是 bash 的固有描述：
    // 以后若有专用 grep 工具，此条变为 "prefer the grep tool"（grep 在场时
    // 替换本行，类似 operating loop 段的合取条件）。
    if tools.bash {
        out.push_str(
            "\nWhen searching files, prefer `rg` over `grep -r`/`find` — it respects \
             `.gitignore` (e.g. skips `target/`).",
        );
    }
    // Operating loop：script 在场时的默认操作循环 —— supervisor 默认（开局
    // reasoning 显式分类任务，独立分支 = 第一动作是一段 spawn/wait 脚本），
    // 亲自动手降级为需要给出理由的例外。回数/上下文的经济账写明，让模型
    // 在决策点看到收益差。
    if tools.script {
        out.push_str(
            "\n\nOperating loop: in your FIRST reasoning pass, classify the task — list its \
             independent branches, or state why it is one strictly sequential thread. When it \
             has two or more independent branches (e.g. investigating several packages, \
             comparing several versions, auditing several files), do NOT work through them \
             one by one: write ONE `script` that `graph.spawn`s one session per branch and \
             `graph.wait`s each, printing only the distilled results — one round-trip \
             replaces dozens of tool rounds, and only the distilled output enters your \
             context. Ask each spawned session to end with a short distilled summary. Work \
             through the task yourself only when it is trivially small or strictly \
             sequential — if so, say why in your first reasoning.",
        );
    }
    out.push_str(
        "\nKeep answers concise, and prefer examining the relevant files before changing them.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::MAX_SPAWN_DEPTH;

    fn compute_all() -> EffectiveTools {
        EffectiveTools::compute(None, true, 0)
    }

    #[test]
    fn compute_full_and_override() {
        let all = compute_all();
        assert!(all.bash && all.script);
        assert_eq!(all.names(), vec!["bash", "script"]);

        // override = 只保留列表内；规范序输出与列表顺序无关。
        let only_bash = EffectiveTools::compute(Some(&["bash".to_string()]), true, 0);
        assert!(only_bash.bash && !only_bash.script);
        assert_eq!(only_bash.names(), vec!["bash"]);

        // 显式空数组 = 无工具。
        let none = EffectiveTools::compute(Some(&[]), true, 0);
        assert!(!none.any());
        assert!(none.names().is_empty());

        // script 受 host / depth 门控，override 再开也没用。
        let no_host = EffectiveTools::compute(None, false, 0);
        assert!(no_host.bash && !no_host.script);
        let at_max = EffectiveTools::compute(None, true, MAX_SPAWN_DEPTH);
        assert!(at_max.bash && !at_max.script);
    }

    #[test]
    fn prompt_full_tool_set() {
        let p = build_system_prompt(&compute_all());
        assert!(p.starts_with("You are rua, a coding agent living on a conversation graph."));
        assert!(p.contains("You have these tools:"));
        assert!(p.contains("`bash`"));
        assert!(p.contains("`script`"));
        assert!(p.contains("Operating loop:"));
        assert!(p.ends_with(
            "Keep answers concise, and prefer examining the relevant files before changing them."
        ));
        // 提示词不提模型。
        assert!(!p.to_lowercase().contains("deepseek"));
        assert!(!p.contains("model"));
    }

    #[test]
    fn prompt_without_script_has_no_operating_loop() {
        // 关掉 script：无 script bullet、无 operating loop 段。
        let p = build_system_prompt(&EffectiveTools::compute(
            Some(&["bash".to_string()]),
            true,
            0,
        ));
        assert!(p.contains("`bash`"));
        assert!(!p.contains("`script`"));
        assert!(!p.contains("Operating loop:"));

        // depth 到顶：无 script 工具。
        let p = build_system_prompt(&EffectiveTools::compute(None, true, MAX_SPAWN_DEPTH));
        assert!(p.contains("`bash`"));
        assert!(!p.contains("`script`"));
        assert!(!p.contains("Operating loop:"));
    }

    #[test]
    fn prompt_script_only_and_empty_set() {
        let p = build_system_prompt(&EffectiveTools {
            bash: false,
            script: true,
        });
        assert!(p.contains("You have these tools:"));
        assert!(p.contains("`script`"));
        assert!(p.contains("Operating loop:"));
        assert!(!p.contains("disabled"));
        // bash 不在场 → 无 rg 引导。
        assert!(!p.contains("prefer `rg`"));

        let p = build_system_prompt(&EffectiveTools {
            bash: false,
            script: false,
        });
        assert!(p.contains("All tools are disabled for this turn — answer directly in text."));
        assert!(!p.contains("You have these tools:"));
        assert!(!p.contains("Operating loop:"));
        // 结尾恒定段仍在。
        assert!(p.ends_with(
            "Keep answers concise, and prefer examining the relevant files before changing them."
        ));
    }
}
