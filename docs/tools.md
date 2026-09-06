# 工具

模型可调用的工具共 2 个：`bash` 与 `script`。对图的**查询与生长不设专门
工具**，统一经 `script` 的绑定面以编程方式完成（PTC 风格：解析、过滤、
聚合发生在脚本里，只有 `console.log` 输出进上下文）。

工具定义、每轮有效集（`EffectiveTools`，`prompt.rs`）与执行分发三处共用
同一份布尔；工具输出全部是文本（错误也以 `error: ...` 前缀返回，不抛出），
逐次落盘为 Turn 正文 `tool_exec` 行（call_id/name/args/output/duration_ms），
并向 WS 广播 `ToolExecStarted` 等事件。`distill` 不是模型工具：
`POST /api/summarize` 是端点动作（UI 触发），产 Context 节点。

## 有效集与分发规则

- **每轮计算一次**：`EffectiveTools::compute(tools_override, has_host, depth)`。
  `tools_override` 为 `None` = 全量；`Some(list)` = 白名单（严格匹配）。
- **script 门控**：额外要求 script host 在位且 `depth < MAX_SPAWN_DEPTH`
  （= 4）；override 再开也没用。能力沿 spawn 边**单调衰减**，永不放大。
- **软拒绝**：模型调用未启用的工具不抛错，返回
  `error: tool not enabled: <name>`，事件与 step 照记，turn 继续。
- **记录**：该轮有效集以规范序（`bash`, `script`）记入 Turn meta 的
  `tools`，spawn 继承按它传递。

---

## bash

执行一条 bash 命令，工作目录为项目根。行为对齐 pi 的 bash 工具
（`coding-agent/src/core/tools/bash.ts`）。

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `command` | string | ✓ | 要执行的命令 |
| `timeout` | number | | 秒；不设 = 无超时（等命令自己结束或被取消） |

行为：

- **实现**：`sh -c "exec 2>&1\n<command>"`，`stdin` 关闭。`exec 2>&1` 在
  shell 内把 stderr 并进 stdout——单管道保证输出严格按产生顺序交错；
  模型自己写的 `2>file` 等重定向在命令文本里，仍然生效。
- **尾部截断**：最后 2000 行（`MAX_LINES`）或 50KB（`MAX_BYTES`），先到
  先截。截断时完整输出落盘到临时文件
  （`$TMPDIR/rua-bash-<pid>-<nanos>.log`），通知行带路径与保留区间。
  最后一行单独超限 → 只留该行尾部字节。
- **空输出**：`(no output)`。
- **超时与取消**：无默认超时；超时或会话取消都会 `SIGKILL` 整个**进程组**
  （shell 经 `process_group(0)` 自成组长，子孙一并带走）。shell 退出后
  管道只再排干 2s，避免被 detached 子孙握着写端挂住。
- **结果标注**（追加在输出尾部）：正常退出且 code=0 无标注；否则
  `Command exited with code N` / `Command terminated by signal` /
  `Command timed out after N seconds` / `Command aborted`。

---

## script

运行一段 **JavaScript** 程序，经 `graph` 对象以编程方式查询与生长会话图
（boa 承载，纯 Rust 嵌入）。

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `code` | string | ✓ | 要运行的 JS 程序 |

### 绑定面（`graph` 对象）

| 绑定 | 签名 | 说明 |
|---|---|---|
| `me` | `graph.me() -> string` | 调用方 turn 的节点 id |
| `list` | `graph.list({kind?, actor?, outcome?, limit?}) -> header[]` | 扫描 meta 索引（created_at 升序，O(1)，不触正文）；kind ∈ `input`/`turn`/`context`；过滤为精确匹配；返回 header 行（id/kind/preview/actor/outcome/...） |
| `view` | `graph.view(id) -> row` | 单节点：header + `text`（turn 另有 `steps_count`；input/context 为正文原文） |
| `wait` | `graph.wait(id, timeout_secs?) -> row` | 同步阻塞到该节点 commit（每轮必闭账，故必然终止）；返回 header + `status:"committed"` + `outcome`/`text`/`usage`（turn）；超时返回 `{status:"running"}`；**不设 timeout = 一直等**（turn 被取消时可中断） |
| `spawn` | `graph.spawn({pointer?, content}) -> {cursor_id, input_node_id, turn_node_id}` | fork 新会话：`pointer` 为已提交 turn（省略 = 新根），`content` 为子会话首条消息。**立即返回**，子轮后台真实运行；走与 `/api/inputs` 相同的原子路径，事件进 WS |

语义要点：

- **输出契约**：只有 `console.log(...)` 的内容作为工具输出返回（尾部截断，
  同 bash 纪律）；无输出 = `(no output)`。
- **解析在脚本侧**：过滤、聚合、扇出-收敛都在脚本里完成，中间产物不进
  上下文。委托首选形态即一段脚本：逐子任务 `graph.spawn`，逐个
  `graph.wait`，只打印蒸馏后的结果。
- **计算预算**：解释器有指令预算（CPU 工作量上限；绑定内阻塞等待不消耗），
  死循环脚本以 `NoInstructionsRemainError` 终止。turn 取消会在 wait/spawn
  处即时生效（返回 `error: script aborted (turn cancelled)`）。
- **错误即文本**：语法/运行时错误带 JS 栈与行列号（`Error: ... at ...`），
  模型可读、可自行修正重试。
- **继承与溯源**：`graph.spawn` 的子代继承调用方有效工具集（v1 不支持
  显式子集），depth+1，到顶后子代不再获得 `script` 工具；子 input 的
  `created_by` 由绑定面盖章为调用方 turn，actor 记为
  `agent:<调用方 id 前 8 位>`——模型无需（也无法）自行传入。
- **门控**：与 script 工具一致——host 在位且 depth 未到顶。未注入 host
  （如纯 engine 环境）时 script 工具不注册。
