//! 有效工具集（`EffectiveTools`）与系统提示词组装。
//!
//! 每轮只算一次有效工具集，schema 注册、提示词组装、执行分发三处共用
//! （`crate::turn`）；server 不再持有静态提示词。提示词内部稳定段在前、
//! 工具段在后（换工具是显式动作，前缀缓存失效属预期）。

/// 一轮实际生效的工具集。`bash`/`spawn_turn`/`inspect` 三个布尔即全部
/// 工具面；新增工具时在这里加字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveTools {
    pub bash: bool,
    pub spawn_turn: bool,
    pub inspect: bool,
}

impl EffectiveTools {
    /// 计算一轮的有效工具集。
    ///
    /// - `tools_override`: None = 全量；Some(list) = 只保留列表内的名字。
    /// - spawn 工具（spawn_turn/inspect）额外要求有 spawner 且
    ///   `depth < MAX_SPAWN_DEPTH`。
    pub fn compute(tools_override: Option<&[String]>, has_spawner: bool, depth: usize) -> Self {
        let allowed = |name: &str| match tools_override {
            None => true,
            Some(list) => list.iter().any(|t| t == name),
        };
        let spawn_ok = has_spawner && depth < crate::spawn::MAX_SPAWN_DEPTH;
        Self {
            bash: allowed("bash"),
            spawn_turn: spawn_ok && allowed("spawn_turn"),
            inspect: spawn_ok && allowed("inspect"),
        }
    }

    pub fn allowed(&self, name: &str) -> bool {
        match name {
            "bash" => self.bash,
            "spawn_turn" => self.spawn_turn,
            "inspect" => self.inspect,
            _ => false,
        }
    }

    pub fn any(&self) -> bool {
        self.bash || self.spawn_turn || self.inspect
    }

    /// 规范序名字列表，用于 Turn 节点记录与 spawn 继承传递。
    pub fn names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.bash {
            names.push("bash".to_string());
        }
        if self.spawn_turn {
            names.push("spawn_turn".to_string());
        }
        if self.inspect {
            names.push("inspect".to_string());
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
        if tools.spawn_turn {
            out.push_str(
                "\n- `spawn_turn`: fork a new session from a committed turn node (`pointer`) or a \
                 fresh root, with `content` as its task. Returns the new turn's node id \
                 immediately — the turn runs in the background.",
            );
        }
        if tools.inspect {
            out.push_str(
                "\n- `inspect`: wait for a node (usually a spawned turn) to commit and read its \
                 result.",
            );
        }
    } else {
        out.push_str("\nAll tools are disabled for this turn — answer directly in text.");
    }
    // 搜索引导是策略 guideline（条件 = 工具集谓词），不是 bash 的固有描述：
    // 以后若有专用 grep 工具，此条变为 "prefer the grep tool"（grep 在场时
    // 替换本行，类似 delegation 段的合取条件）。
    if tools.bash {
        out.push_str(
            "\nWhen searching files, prefer `rg` over `grep -r`/`find` — it respects \
             `.gitignore` (e.g. skips `target/`).",
        );
    }
    // Delegation 需要 spawn + inspect 合取；只开一个时不成立，整段丢弃。
    if tools.spawn_turn && tools.inspect {
        out.push_str(
            "\n\nDelegation policy: when a task decomposes into INDEPENDENT subtasks (e.g. \
             investigating several packages, auditing several files, trying several approaches), \
             do NOT do them all yourself — `spawn_turn` one branch per subtask first, then \
             `inspect` each to collect results and synthesize. Spawned sessions inherit your \
             current tool set, and you may pass a narrower `tools` subset (spawn depth is \
             limited). Do the work yourself only when the subtasks are trivially small or \
             strictly sequential.",
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
    use crate::spawn::MAX_SPAWN_DEPTH;

    fn compute_all() -> EffectiveTools {
        EffectiveTools::compute(None, true, 0)
    }

    #[test]
    fn compute_full_and_override() {
        let all = compute_all();
        assert!(all.bash && all.spawn_turn && all.inspect);
        assert_eq!(all.names(), vec!["bash", "spawn_turn", "inspect"]);

        // override = 只保留列表内；规范序输出与列表顺序无关。
        let only_bash = EffectiveTools::compute(Some(&["bash".to_string()]), true, 0);
        assert!(only_bash.bash && !only_bash.spawn_turn && !only_bash.inspect);
        assert_eq!(only_bash.names(), vec!["bash"]);

        // 显式空数组 = 无工具。
        let none = EffectiveTools::compute(Some(&[]), true, 0);
        assert!(!none.any());
        assert!(none.names().is_empty());

        // spawn 工具受 spawner / depth 门控，override 再开也没用。
        let no_spawner = EffectiveTools::compute(None, false, 0);
        assert!(no_spawner.bash && !no_spawner.spawn_turn && !no_spawner.inspect);
        let at_max = EffectiveTools::compute(None, true, MAX_SPAWN_DEPTH);
        assert!(at_max.bash && !at_max.spawn_turn && !at_max.inspect);
    }

    #[test]
    fn prompt_full_tool_set() {
        let p = build_system_prompt(&compute_all());
        assert!(p.starts_with("You are rua, a coding agent living on a conversation graph."));
        assert!(p.contains("You have these tools:"));
        assert!(p.contains("`bash`"));
        assert!(p.contains("`spawn_turn`"));
        assert!(p.contains("`inspect`"));
        assert!(p.contains("Delegation policy:"));
        assert!(p.ends_with(
            "Keep answers concise, and prefer examining the relevant files before changing them."
        ));
        // 提示词不提模型。
        assert!(!p.to_lowercase().contains("deepseek"));
        assert!(!p.contains("model"));
    }

    #[test]
    fn prompt_without_spawn_tools_has_no_delegation() {
        // 关掉 spawn+inspect：无 spawn bullet、无 delegation 段。
        let p = build_system_prompt(&EffectiveTools::compute(
            Some(&["bash".to_string()]),
            true,
            0,
        ));
        assert!(p.contains("`bash`"));
        assert!(!p.contains("`spawn_turn`"));
        assert!(!p.contains("`inspect`"));
        assert!(!p.contains("Delegation policy:"));

        // 只开 spawn_turn 不开 inspect：delegation 不成立，整段丢弃。
        let p = build_system_prompt(&EffectiveTools {
            bash: false,
            spawn_turn: true,
            inspect: false,
        });
        assert!(p.contains("`spawn_turn`"));
        assert!(!p.contains("Delegation policy:"));

        // depth 到顶：无 spawn 工具。
        let p = build_system_prompt(&EffectiveTools::compute(None, true, MAX_SPAWN_DEPTH));
        assert!(p.contains("`bash`"));
        assert!(!p.contains("`spawn_turn`"));
        assert!(!p.contains("Delegation policy:"));
    }

    #[test]
    fn prompt_bash_only_and_empty_set() {
        let p = build_system_prompt(&EffectiveTools {
            bash: true,
            spawn_turn: false,
            inspect: false,
        });
        assert!(p.contains("You have these tools:"));
        assert!(p.contains("`bash`"));
        assert!(!p.contains("disabled"));
        // bash 在场 → 有 rg 搜索引导。
        assert!(p.contains("prefer `rg`"));

        let p = build_system_prompt(&EffectiveTools {
            bash: false,
            spawn_turn: false,
            inspect: false,
        });
        assert!(p.contains("All tools are disabled for this turn — answer directly in text."));
        assert!(!p.contains("You have these tools:"));
        assert!(!p.contains("Delegation policy:"));
        // bash 不在场 → 无 rg 引导。
        assert!(!p.contains("prefer `rg`"));
        // 结尾恒定段仍在。
        assert!(p.ends_with(
            "Keep answers concise, and prefer examining the relevant files before changing them."
        ));
    }
}
