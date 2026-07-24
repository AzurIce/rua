# D0004：Tool Runtime 与执行语义

工具调用不是一次普通的函数调用。它同时是模型已经写入 conversation 的事实、runtime 准备施加于外部世界的动作，以及 UI 可以观察但不能驱动的生命周期。真正困难的问题不是“怎样启动一个命令”，而是：外部动作如何被识别、执行，并在结果不确定时诚实地写回 conversation？

## 从定义到结果

一次工具调用经过四层对象：

```text
ToolDefinition
    -> ToolCall
    -> ToolExecution
    -> ToolResult
```

`ToolDefinition` 是 provider-neutral 的公开契约，描述名称、用途和参数 schema。Provider 只负责把它编码成自己的请求格式，不拥有工具实现，也不决定执行结果。

`ToolCall` 是 assistant message 中已经提交的请求，包含稳定的 `ToolCallId`、工具名和参数。这个 ID 必须贯穿 runtime event、execution journal 与最终 `ToolResultMessage`；不能在 provider adapter、registry 或 UI 中重新生成。它使“模型请求的动作”和“后来报告的结果”保持一一对应。

`ToolExecution` 是 Rua 对这次 call 的一次具体执行尝试。恢复或人工确认后再次执行同一个 call 时，call identity 不变，但 execution identity 必须不同，以免把两次外部动作混成一次。

`ToolResult` 是能够提交给模型的有界结果。它不是完整 stdout/stderr 的同义词，也不是 UI 日志；它只陈述 runtime 能够证明的 outcome。

## 谁负责校验和执行

职责沿边界逐层收窄：

- `AgentRuntime` 决定何时执行、按什么顺序执行，以及何时把 result 追加到 canonical conversation。
- `ToolRuntime` / registry 按名称寻找工具，执行通用的 schema 与大小限制，并建立可观察的 execution。
- 具体 `Tool` 解释自己的参数约束并实施动作。例如 Bash tool 负责确认参数是 object、`command` 是字符串，再启动相应进程。
- Provider 只转换 definition、call 和 result；UI 只投影事件。二者都不能执行工具，也不能补写 conversation。

这一区分很重要：registry 能证明“这个名字对应哪个工具”，但只有工具本身知道某个参数组合是否具有业务意义。文档中的 schema 是边界契约，不应暗示所有约束都已经由 `AgentRuntime` 预先验证。

## 成功、失败与结果未知

工具 outcome 有两条彼此独立的轴：动作是否成功，以及 Rua 是否知道发生了什么。不能把它们压成一个 `Result<String, String>`。

| Outcome | Rua 知道什么 | Conversation 动作 |
| --- | --- | --- |
| `Completed` | 执行完成并取得可陈述结果 | 提交普通 tool result |
| `FailedKnown` | 工具未找到、参数非法，或执行明确失败 | 提交 `is_error: true` 的 tool result |
| `OutcomeUnknown` | 动作可能已经开始，但最终效果无法证明 | 不伪造 result，停止自动推进并进入协调 |

输出被截断不等于执行失败。result 可以是成功或已知失败，同时显式携带“内容已截断”的标记。相反，进程已启动后连接丢失、取消未确认、或 outcome 无法可靠持久化，都可能导致 `OutcomeUnknown`。

在允许调用工具之前，runtime 必须先持久化 execution intent 与 started 边界：

```text
intended -> started -> outcome recorded -> result committed
```

如果 durable `started` 写入失败，动作不得开始。如果动作已经开始、但 outcome 记录失败，Rua 不能倒推“它一定没发生”；恢复规则由 [D0005](D0005-execution-journal-and-session-recovery.md) 定义。

## 重放需要契约，也需要证据

工具定义还需要说明重放能力。这个能力不是从命令文本猜出来的；`bash` 即使收到 `ls`，首版也仍按 `Unknown` 处理。一个耐久的最小分类是：

- `ReadOnly`：不会改变外部状态；
- `Idempotent`：重复执行具有定义好的幂等语义，并说明 key 的来源；
- `Effectful`：会改变外部状态，重复执行通常不安全；
- `Unknown`：Rua 没有足够契约判断。

这组 metadata 是规范方向，不要求它已经存在于当前 `ToolDefinition` 字段中。自动重放同时依赖工具契约和 journal 证据：声明为安全但没有可靠 execution 边界不够，有完整记录但工具语义未知也不够。任何一侧不足，都进入 reconciliation。

## 输出、观察与敏感数据

同一次 execution 会产生三种不同投影：

- 给模型的 bounded result，必须稳定、可序列化，并明确标记截断；
- 给 runtime 的 execution observation，用于诊断、journal 与恢复；
- 给用户的 UI projection，可以流式显示活动和有限日志。

三者不能互相冒充。UI 展示过的文本不自动成为 conversation；无界 stdout 也不能直接进入模型上下文。

工具参数与结果都可能含有敏感数据。session 文件按用户权限持久化，diagnostic 应脱敏；认证 header、环境秘密和无界原始输出不得为了“方便调试”被额外复制。具体工具若把凭据作为其业务结果返回，则由工具契约和持久化策略共同处理，本文不作“凭据绝不出现”的虚假绝对承诺。

## 顺序、取消与事件

首版串行执行同一 assistant message 中的多个 tool calls。这样 result 顺序与 call 顺序一致，也为副作用提供清楚的因果基线。未来若引入并发，必须由工具契约证明可并行，并定义稳定的 result commit 顺序。

取消是请求，不是对外部世界的时间倒流。工具尚未启动时可以得到已知取消；进程已启动后，即使本地 future 被丢弃，也不能据此断言没有副作用。无法确认的取消进入 `OutcomeUnknown`。

Runtime event 描述观察事实，例如 execution started、output available、execution finished。事件携带稳定 call/execution identity，供 TUI 和其他客户端投影；它们不是命令回路，事件消费者不能通过回写 event 改变 canonical state。

## Bash 只是一个工具

Bash tool 接收 `{"command": "..."}`，在 Windows 使用 PowerShell，在 Unix 使用 `sh`。进程启动失败、明确的非零退出与非法参数属于 `FailedKnown`；启动后的取消或通信中断可能属于 `OutcomeUnknown`。stdout/stderr 必须有界并注明截断。

Shell 选择、sandbox、approval UX、具体进程树清理和 streaming transport 都可以演进，不改变上面的 identity、outcome 与 commit 语义。因此它们不是 Tool Runtime 的中心模型。
