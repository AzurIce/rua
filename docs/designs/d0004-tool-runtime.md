# D0004：Tool Runtime 与执行语义

## 背景

工具是 agent turn 中唯一会产生外部副作用的环节。旧实现把工具注册、schema、参数解析、进程启动和 UI 通知集中在 `ToolRegistry`，并在执行失败时把错误压成普通字符串。这样无法区分已知失败与执行结果未知，也无法保证 tool call 与 canonical conversation 中的 call ID 一一对应。

## 目标

- 用 provider-neutral 的 `ToolDefinition` 描述可发现的工具。
- 通过稳定的 `ToolCallId` 关联 assistant tool call、执行事件和 tool result。
- 将参数校验、未知工具、取消和执行结果分类为可诊断状态。
- 已知失败形成 `ToolResultMessage { is_error: true }`；执行结果未知时不伪造成功结果。
- 首版串行执行，保证顺序和副作用边界清晰。
- 为 TUI 和未来客户端发布结构化 tool runtime events。

## 非目标

首版不定义 sandbox、approval、并行工具执行、持久化 journal 或工具输出的实时分片协议。输出先采用有上限的文本快照，后续可以在不改变 call/result 关系的前提下扩展。

## 边界与契约

`ToolExecutor` 只负责执行一个已经由 runtime 选中的 call；它不拥有 conversation，也不决定是否继续请求模型。`ToolRegistry` 负责定义发现和按名称分派。runtime 负责把 `ToolOutcome` 转为 canonical `ToolResultMessage`，并在 `OutcomeUnknown` 时停在可恢复状态。

```rust
pub trait ToolExecutor: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn execute(&self, call: ToolCall, cancel: CancellationToken) -> ToolFuture<'_>;
}

pub enum ToolOutcome {
    Completed { content: String },
    FailedKnown { message: String },
    OutcomeUnknown { message: String },
}
```

参数必须是合法 JSON object，并由工具自行检查 required fields。未知工具、参数错误和非零退出码都是 `FailedKnown`。取消、进程被强制终止或无法确定子进程是否产生副作用属于 `OutcomeUnknown`，runtime 不提交结果并允许后续 resume 决定如何处理。

## Bash 首版策略

`bash` 工具在 Windows 使用 `powershell -NoProfile -Command`，其他平台使用 `sh -c`。stdout/stderr 合并为有界文本；退出码非零仍是已知失败。工具执行接受 `CancellationToken`，取消会尝试终止子进程并返回 `OutcomeUnknown`。

## Runtime 事件

runtime 发布 `ToolStarted`、`ToolCompleted`、`ToolFailed`，事件携带 turn、step、attempt 和 call ID。客户端只能把这些事件投影为展示状态，不能据此修改 canonical conversation。

## 迁移

在 `src/agent/tools.rs` 引入 registry 和 Bash 实现，再由 `AgentRuntime` 串行调用。旧 `src/tools.rs` 与 `src/session.rs` 已随主路径切换删除；canonical conversation 不再从 UI history 重建。

## 当前状态

本文与 D0001、D0002 一起描述目标架构。实现采用首版串行、进程内 registry 和 bounded text output；execution journal 与持久化恢复由 [D0005：执行 Journal 与 Session 恢复](d0005-execution-journal-and-session-recovery.md) 定义，尚待实现。
