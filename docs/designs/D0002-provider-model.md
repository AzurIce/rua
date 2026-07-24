# D0002：Provider 边界与标准消息模型

## 背景

Rua 当前使用 `deepseek.rs` 中的 `Message`、`ToolCall` 和 `StreamEvent` 作为 agent loop 的输入与输出。它们既承担 DeepSeek wire format，又承担 session 内的 conversation model。`ChatEntry` 再把其中一部分信息转换为 TUI history。这个结构无法无损保存 tool calls 和 reasoning，也让重试、切换 provider、持久化及测试都依赖 DeepSeek 的协议细节。

[D0001：Agent Runtime 与会话所有权](D0001-agent-runtime.md) 确立了 `AgentRuntime` 对 canonical conversation 和 turn lifecycle 的所有权。本文进一步定义 runtime 与 provider 之间共享的标准模型：conversation 保存什么、provider 接收什么、流式响应如何完成，以及 provider 特有状态如何在不污染核心模型的情况下保留下来。

Provider 是 adapter，不是 Rua 的 domain model。这不等于取所有供应商能力的最小交集：canonical model 保留 agent loop 真正依赖的语义，无法安全标准化的能力则通过有作用域的 opaque state 往返。这样既能跨 provider 延续 conversation，也不会让某一家 SDK 的 message enum 变成 runtime 的事实来源。

## 目标

- 定义 provider 无关且可无损扩展的 canonical conversation。
- 让同一 conversation 可以被不同 provider adapter 编译为各自的 wire request。
- 将 provider stream 与 runtime events 分离，避免 wire chunks 直接进入 UI。
- 明确 provisional response、完整 assistant message 和失败 attempt 的边界。
- 保留后续请求必需的 reasoning signature、response ID 等 provider 特有状态。
- 为 model-step retry 提供稳定、可分类的错误和终止语义。
- 允许使用 deterministic fake provider 测试 agent runtime。

## 非目标

本文不决定：

- provider、model 和凭据的发现或配置来源；
- 具体 HTTP client、SSE parser 或 SDK 的选择；
- runtime 的重试次数、退避算法和用户提示；
- tool runtime 的执行、幂等或 sandbox 策略；
- conversation 的磁盘格式、分支或 compaction；
- system prompt、项目规则和 skills 的组装方式；
- 图片、音频等非文本模态的首版实现。

## Canonical conversation

Conversation 只包含已经提交、下一次请求必须再次成立的事实。stream 中尚未完成的文本、reasoning delta 和半截 tool arguments 属于 provisional response；它们可以被 UI 看见，却不能提前成为 committed message。只有 response 完整结束并通过结构校验后，runtime 才一次性追加 assistant message。

Canonical conversation 由 instruction set、按提交顺序排列的 messages，以及单调递增的 revision 构成。

```rust
pub struct Conversation {
    pub instructions: InstructionSet,
    pub messages: Vec<Message>,
    pub revision: ConversationRevision,
}

pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}
```

这些类型示意稳定的数据关系，不要求实现逐字采用字段名称。公开边界必须保留相同语义。

Conversation 是 append-only 的 committed log。修改 system instructions、切换分支或执行 compaction 时，应产生可观察的 revision 变化，而不能在活动 model step 背后静默改变请求上下文。

### Instructions

System instructions 与普通消息分开保存，因为 provider 对 system/developer messages 的表示不同。`InstructionSet` 保存 runtime 已经组装完成、准备发送给模型的有效指令及其 revision。指令来源和优先级由后续的资源与配置设计决定。

Provider adapter 可以把 instructions 编译为 system message、developer message 或 provider 的专用字段，但不能擅自加入改变 agent 行为的内容。因协议兼容而必须添加的技术性提示应当可诊断。

### User message

User message 具有 Rua 分配的稳定 message ID，并包含有序 content parts。首版只要求文本 content，但模型不应把 `String` 固化为永久边界。

```rust
pub struct UserMessage {
    pub id: MessageId,
    pub content: Vec<UserContent>,
}

pub enum UserContent {
    Text { text: String },
    // 后续设计可以增加 image 等模态。
}
```

空 content 不构成有效的 committed user message。UI 展示所需的选中状态、折叠状态等信息不属于 canonical message。

### Assistant message

Assistant message 是一个 model step 成功完成后提交的完整响应。

```rust
pub struct AssistantMessage {
    pub id: MessageId,
    pub parts: Vec<AssistantPart>,
    pub stop_reason: StopReason,
    pub usage: Option<Usage>,
    pub provenance: ResponseProvenance,
    pub provider_state: Option<OpaqueProviderState>,
}

pub enum AssistantPart {
    Text(TextPart),
    Reasoning(ReasoningPart),
    ToolCall(ToolCall),
}
```

Parts 保持 provider 返回的逻辑顺序。Text、reasoning 和 tool calls 不能被压扁成一个字符串，因为它们有不同的回放、展示和执行语义。

`ResponseProvenance` 至少记录请求使用的 provider、API family 和 model，以及 provider 可返回时的实际 model 与 response ID。这些信息用于诊断、计费、兼容判断和后续请求，不作为用户可编辑内容。

### Reasoning

Reasoning part 将可展示文本与 provider 恢复状态分开：

```rust
pub struct ReasoningPart {
    pub text: Option<String>,
    pub provider_state: Option<OpaqueProviderState>,
}
```

Provider 未公开 reasoning 文本但要求回传 opaque signature 时，`text` 可以为空。Provider adapter 只能读取属于自身 provider/API family 的 opaque state。切换 provider 时，普通 assistant text 和 tool history 仍可移植，而不兼容的 reasoning state 不发送给新 provider。

Runtime 保留 reasoning 不代表 TUI 必须展示它。展示和折叠策略属于 TUI 设计。

### Tool call 与 tool result

Committed tool call 包含 provider 返回或 Rua 规范化后的稳定 call ID、工具名和结构化参数：

```rust
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: serde_json::Value,
    pub provider_state: Option<OpaqueProviderState>,
}

pub struct ToolResultMessage {
    pub id: MessageId,
    pub tool_call_id: ToolCallId,
    pub name: String,
    pub content: Vec<ToolResultContent>,
    pub is_error: bool,
}
```

Tool-call arguments 在 provider stream 中先以 provisional fragments 累积。只有完整 JSON 成功解析后，assistant message 才能提交。参数无效属于 provider protocol failure，不能静默替换为 `{}`。

Tool result 必须引用已提交且尚需结果的 tool call。Runtime 不得创建孤立 result，也不能为同一个 call ID 提交多个相互冲突的最终结果。供 UI 使用但不发送给模型的结构化工具详情属于 ToolRuntime 或 execution journal，不放入 provider conversation。

## Opaque provider state

部分 provider 要求在后续请求中回传无法标准化的状态，例如 reasoning signature、encrypted content、response item ID 或 thought signature。核心模型以有作用域的 opaque state 保存这类值：

```rust
pub struct OpaqueProviderState {
    pub provider: ProviderId,
    pub api_family: ApiFamily,
    pub schema_version: u32,
    pub value: serde_json::Value,
}
```

Opaque state 必须可序列化、可限制大小，并且不能包含认证凭据。只有匹配 provider 和 API family 的 adapter 可以解释其内容；其他 adapter 必须忽略它，而不是猜测转换。

优先把跨 provider 共有的稳定语义提升为 canonical 字段。Opaque state 是防止信息丢失的逃生口，不应成为复制完整 wire response 的方式。

## Model request

`AgentRuntime` 为每个 model attempt 构造不可变的请求快照：

```rust
pub struct ModelRequest {
    pub turn_id: TurnId,
    pub step_id: StepId,
    pub attempt_id: AttemptId,
    pub conversation_revision: ConversationRevision,
    pub instructions: InstructionSet,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub model: ModelRef,
    pub options: GenerationOptions,
}
```

请求创建后，即使配置或 UI 状态发生变化，本次 attempt 的内容也不变。Retry 同一个 model step 时使用相同 committed conversation revision；attempt ID 和允许变化的 transport metadata 可以不同。

`ToolDefinition` 包含稳定工具名、给模型看的描述和标准 JSON Schema。Provider adapter 可以进行协议要求的字段转换，但不能改变工具的业务含义。

`ModelRef` 标识 provider、API family 和 model。Model capabilities 用于在发请求前验证 tool use、reasoning 或输入模态是否受支持；adapter 不应等到远端返回模糊错误才发现明显不兼容。

## Provider 边界

Provider 是无状态或显式持有连接状态的 adapter。它不拥有 conversation、turn retry policy 或工具执行权。

```rust
pub trait Provider: Send + Sync {
    fn capabilities(&self, model: &ModelRef) -> ModelCapabilities;

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ProviderStream, ProviderError>;
}
```

具体 Rust 实现可以使用 boxed future 或其他 object-safe 形式，但必须保持调用语义：

- 在 stream 建立前发生的认证、连接或请求构造失败，通过外层 `Result` 返回；
- stream 建立后发生的所有完成、失败和取消，通过 stream 的唯一 terminal event 表达；
- provider 不直接发送 `UiEvent` 或 `AgentEvent`；
- provider 不自行执行跨 attempt 的 runtime retry。

底层 HTTP SDK 可以执行不会越过一次 attempt 边界的 transport retry，但必须在 diagnostics 中可观察，并服从 runtime 的取消与时间限制。

## 标准化 stream events

Provider stream 描述一条 provisional assistant response 的构造过程。事件集合至少具有以下语义：

```rust
pub enum ProviderEvent {
    ResponseStarted(ResponseInfo),
    TextDelta { part: PartIndex, delta: String },
    ReasoningDelta { part: PartIndex, delta: String },
    ToolCallStarted { part: PartIndex, id: ToolCallId, name: String },
    ToolArgumentsDelta { part: PartIndex, delta: String },
    PartState {
        part: PartIndex,
        kind: PartKind,
        state: OpaqueProviderState,
    },
    UsageUpdated(Usage),
    Completed(ProviderCompletion),
    Failed(ProviderError),
}
```

一次已建立的 stream 必须且只能产生一个 terminal event：`Completed` 或 `Failed`。Terminal event 之后的事件无效。Runtime 把非 terminal events 累积到 response draft，并在 `Completed` 到达时校验以下条件：

- 所有已开始的 tool calls 均完整结束且参数为合法 JSON；
- stop reason 与 parts 组合相容；
- provider 要求的 call ID、signature 或 response state 已存在；
- 没有重复或相互冲突的 part identity。

校验通过后，runtime 才创建并提交 `AssistantMessage`。`Failed`、stream EOF without terminal event 或校验失败都不会把 draft 提交进 canonical conversation。

Provider events 与 runtime events 是两个不同层次。Runtime 可以把 text delta 映射为带 turn/step/attempt ID 的展示事件，但 provider adapter 不知道哪些消费者正在观察。

## Stop reason 与 usage

Canonical stop reason 至少区分：

- 正常结束；
- 请求工具；
- 达到输出长度限制；
- 内容策略或拒绝；
- provider 无法标准化的其他结束原因。

Provider failure 和 cancellation 不是成功的 assistant stop reason；它们通过 failed attempt 表达。这样失败的 provisional response 不会伪装成 committed assistant message。

Usage 使用 provider 可提供的 token categories，例如 input、output、reasoning、cache read 和 cache write。缺失值保持未知，不能用字符串长度估算后冒充 provider usage。成本是 model metadata 与 usage 的派生结果，不属于 wire adapter 必须提供的事实。

## 错误与重试提示

`ProviderError` 至少包含稳定错误类别、可安全展示的信息、可选 provider diagnostics，以及 retry hint。错误类别覆盖：

- authentication 和 authorization；
- invalid request 或不支持的能力；
- context length；
- rate limit；
- timeout 和 transport；
- provider server failure；
- malformed provider response 或 protocol violation；
- cancellation；
- 未分类错误。

Provider 给出 retry hint，runtime 决定是否实际重试。Hint 可以表达 never、retryable、建议等待时间或 unknown。认证失败、确定的 invalid request 和 capability mismatch 默认不可重试；rate limit、timeout、transport 与部分 server failure 通常可重试；protocol failure 只允许受限重试。

错误文本进入日志或 UI 前必须避免泄露 API key、认证 header 和未清理的原始响应。

## 跨 provider conversation

切换 provider 不会改写已提交的 conversation。新 adapter 按以下原则编译历史：

- user text、assistant text、tool calls 和 tool results 使用 canonical 语义转换；
- 只回传作用域匹配且协议仍需要的 opaque provider state；
- 不兼容的 reasoning state 可以省略，但不得伪造；
- 如果目标 provider 无法表达一段必要历史，发送请求前返回 capability 或 conversion error；
- adapter 的有损转换必须产生 diagnostics，不能静默改变工具对应关系。

是否允许用户在活动 turn 中切换 model 或 provider 由 AgentRuntime 与配置设计决定。本设计只保证已完成历史具有明确的转换规则。
