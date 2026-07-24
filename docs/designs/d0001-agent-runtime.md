# D0001：Agent Runtime 与会话所有权

## 背景

Rua 当前从 TUI 发起流式模型请求，在 `Session` 中执行模型请求的工具，然后继续请求模型。这已经验证了基本的 agent loop，但一个 turn 的职责分散在 UI 状态、DeepSeek 协议类型和 session 任务之间。

TUI 的 `AppState` 当前持有用于展示的历史。每个用户 turn 开始时，这份历史会被重新转换为 provider 消息。`ChatEntry` 无法表示的协议信息——包括 assistant tool calls 及其与 tool results 的对应关系——无法在转换后保留下来。`StreamDone` UI 事件也同时表示单次模型流结束和整个用户 turn 结束，然而一个 turn 可能包含多个模型与工具步骤。

在加入更多 provider、工具、持久化能力或用户界面之前，Rua 需要一个稳定的 agent core。这个核心必须无损地保存会话，并为每个参与组件建立明确的边界。

这里首先要分开三种经常被混为“会话状态”的东西：

```text
canonical conversation   模型已经被告知了哪些事实
execution journal        runtime 已经或准备进行哪些外部动作
display projection       用户此刻看见、选择和编辑什么
```

它们会相互引用，却不能合并。把 projection 当作 conversation 会让折叠、流式草稿和局部 UI 状态污染下一次请求；只保存 conversation 又无法判断崩溃前的工具是否已经产生副作用。后文的 ownership 与提交点都从这一区分展开。

Provider 与 canonical conversation 的具体数据契约由 [D0002：Provider 边界与标准消息模型](d0002-provider-model.md) 定义。

## 目标

- 让 agent runtime 成为 canonical conversation 的唯一所有者。
- 跨用户 turn 完整保留 assistant tool calls 和 tool results。
- 将用户 turn 定义为一个可包含多个模型与工具步骤的生命周期。
- 将 provider 协议处理、工具执行和展示从 agent 编排中分离。
- 暴露结构化 runtime events，供 TUI、测试和未来的非交互客户端使用。
- 明确定义取消、失败和循环限制的语义。
- 允许同一个 turn 从稳定提交点重新进入，并重试可恢复的 model step。

## 非目标

本设计不决定：

- 持久化 session 的文件格式或会话分支模型；
- 上下文压缩算法；
- 多 provider 的认证或配置方式；
- sandbox、approval 或工具级安全策略；
- 并行工具执行；
- extension、skill 或 plugin 系统；
- TUI 的布局、样式或编辑器行为。

这些能力可以建立在本文定义的边界之上，但不属于首版 runtime 设计。

## 谁拥有会话

`AgentRuntime` 持有 canonical conversation，并且是唯一可以向其中追加具有协议意义消息的组件。Conversation 必须保留未来发起 provider 请求所需的全部信息，无需读取 UI 状态。

Canonical conversation 至少表达：

- system instructions 和 user content；
- assistant content 和 reasoning；
- assistant tool calls，包括稳定的 call ID 和参数；
- 与 call ID 对应的 tool results；
- provider 无关的 stop reason，以及 provider 可提供时的 usage。

Canonical model 不是某个 provider 的 wire model。Provider adapter 负责在 canonical model 与 provider 特有的请求及流式协议之间转换。无法标准化但必须跨请求保留的信息，可以作为明确的 provider metadata 保存，而不能放入 UI model。

TUI 只维护用于渲染和输入的投影视图。它消费 runtime events，不得根据展示条目重建 canonical messages。

## 一个 Turn 怎样推进

Runtime 接受用户输入时，一个 turn 开始；它以 completed、cancelled 或 failed 三种结果之一结束，并且只能结束一次。同一个 runtime 同时只能存在一个活动 turn。

一个 turn 可以包含多个 model steps：

```text
用户输入
  -> 模型流
  -> assistant tool calls
  -> 工具执行并提交结果
  -> 模型继续生成
  -> 最终 assistant response
```

模型流结束只是内部 step 的边界，不一定意味着 turn 结束。Runtime 在执行 tool calls 前提交完整的 assistant message，在继续请求模型前提交每一个 tool result，并且只发出一次终止 turn event。

Runtime 对 model steps 和 tool calls 执行可配置的数量限制。超过限制时以独立错误结束 turn，而不是无限继续。

一个逻辑 step 可以有多次执行 attempt。瞬时失败不会创建新的 turn，也不会把失败的 attempt 伪装成新的 conversation message。Runtime 保持原有 turn ID 和 step ID，并为每次尝试分配新的 attempt ID。

## 为什么需要稳定提交点

Runtime 只能从稳定提交点恢复执行。稳定提交点是 canonical conversation 中最后一个完整、协议有效的状态，包括：

- 用户消息已提交，但下一条 assistant message 尚未提交；
- 包含 tool calls 的完整 assistant message 已提交；
- 一个或多个 tool results 已提交；
- 最终 assistant message 已提交。

模型正在生成的 text、reasoning 和尚未完成的 tool-call arguments 属于 provisional draft，不是 canonical conversation 的一部分。Provider stream 在完整 assistant message 提交前失败时，runtime 丢弃该 attempt 的 draft，从相同 committed history 重试同一个 model step。再次生成的内容不要求与失败 attempt 相同。

Runtime 的执行接口在语义上是可重入的：它可以根据 turn execution state 和 canonical conversation 继续推进一个尚未终止的 turn。每次推进持续到 turn 完成、等待重试、需要人工协调、被取消或发生不可恢复的失败。实现可以在首版提供连续运行的便利接口，但不能把正确性建立在一次函数调用必然运行到 turn 结束的假设上。

同一个 turn 同时只能有一个执行者。Runtime 通过锁、lease 或 revision check 防止两个任务并发推进同一 turn，并在追加 committed state 时验证预期的 conversation head。

## Conversation 不能代替 execution journal

Canonical conversation 记录模型已经知道的内容，但不足以表示 runtime 已经执行过的外部动作。Runtime 因此还维护最小的 turn execution journal，用于记录：

- 当前 turn phase；
- 当前 model 或 tool step 及其 attempt；
- 尚未执行、正在执行和已经完成的 tool calls；
- 重试安排与已消耗的限制；
- 结果未知、需要协调的工具执行。

Turn phase 至少能够表达 awaiting model、executing tools、waiting to retry、needs reconciliation 和 terminal outcomes。Execution journal 是控制状态，不会作为普通消息发送给模型。

跨进程恢复、journal 的 write-ahead 顺序和文件格式由 [D0005：执行 Journal 与 Session 恢复](d0005-execution-journal-and-session-recovery.md) 定义。

## 组件怎样协作

```text
                         Provider
                            ^
                            |
                            v
用户界面 <--------- AgentRuntime ---------> ToolRuntime
   ^                     |
   |                     v
   +------ events --- Conversation
```

各边界的职责如下：

- `AgentRuntime` 编排 turn、持有 conversation、应用限制、传播取消信号并发布事件。
- `Provider` 接受 provider 无关的 conversation 和工具定义，产生标准化的 assistant deltas、tool calls、usage、stop reasons 和 errors 流。
- `ToolRuntime` 查找已注册工具、校验输入、执行工具并返回标准化结果。首版设计串行执行工具。
- 用户界面提交输入并观察 runtime events，不能直接修改 canonical conversation。

Provider adapter 不执行工具，也不发布 UI events。工具不向 conversation 追加消息。用户界面不决定一个 model step 是否应当继续。

## Events 只用于观察

Events 描述有意义的生命周期变化，而不是 provider wire chunks。事件类型至少必须区分：

- turn 开始和 turn 的最终结果；
- assistant text 和 reasoning deltas；
- 已提交的 assistant messages；
- tool call 开始、可选的 output deltas 和执行完成；
- 已提交的 tool results；
- 取消和分类后的失败。

每个与活动任务有关的事件都携带稳定的 turn ID，并在适用时携带 tool-call ID。消费者可以使用 deltas 实现即时展示，但已提交的 conversation state 始终是权威状态。

与重试有关的事件还应携带 step ID 和 attempt ID，使消费者能够区分同一逻辑步骤的多次尝试。Attempt failure 和 retry scheduled 是非终止事件；只有 runtime 确认 turn 无法继续时才发布 terminal failure。

事件投递仅用于观察。缓慢或失败的展示端不得改变 conversation 语义。实现阶段可以调整背压和投递机制，但必须保留这项原则。

## 取消与错误

取消从 runtime 边界发起，并传播给活动的 provider stream 和工具执行。取消只产生一次最终 turn outcome，不能被静默地视为成功完成。

错误分类至少能够区分 provider、protocol、tool、limit、cancellation 和 internal failure，并标记错误是否可重试。可恢复的 provider 或 transport failure 在同一个 model step 内重试；达到 attempt limit 后才终止 turn。工具正常返回的业务错误应成为一个已提交的 error tool result，让模型有机会响应，而不是自动终止 turn。

工具执行还必须区分“明确失败”和“结果未知”。如果工具可能已经产生外部副作用，但 runtime 未能提交结果，则只凭 conversation history 无法判断是否可以再次执行。只读、明确幂等或支持 idempotency key 的工具可以按策略自动重试；其他工具进入 needs reconciliation 状态，不能被盲目重放。

部分流式 assistant output 可以通过事件展示，但不能自动视为完整的 canonical assistant message。如需保留部分输出，runtime 必须显式记录 incomplete 或 failed 状态。

## 重要决策与权衡

### 标准化 conversation，而不是保留 DeepSeek messages

继续使用 DeepSeek wire messages 可以减少首次重构的工作，但会让单一 provider 协议成为整个架构的核心数据模型。现在增加 adapter 虽然有成本，却可以避免未来每项 runtime 能力都与 DeepSeek 耦合。

标准化过程必须避免信息丢失。Reasoning signature、provider metadata 或后续请求必需的其他值，不能只因为别的 provider 不使用就被丢弃。

### 只允许一个活动 turn

串行的 turn 所有权使状态修改、取消和事件顺序保持确定。未来可以在此基础上增加 steering 和排队的 follow-up messages，而无需允许多个 loop 并发修改同一个 conversation。

### 首版串行执行工具

Provider 可能一次返回多个 tool calls，但首版采用串行执行，使顺序和取消行为更清晰。Tool runtime API 不应把串行执行固化为不可改变的约束；后续设计可以允许相互独立的调用并发执行。

### 分离 committed state 与 streaming presentation

Streaming deltas 用于提升响应性，committed messages 用于确保协议正确性。区分二者会增加生命周期状态，但可以防止 UI 中的部分文本意外成为有效的 conversation history。

### 使用 history 重试模型，使用 journal 约束副作用

对于未提交的 model step，committed conversation 通常足以构造一次新的 provider attempt。模型调用不是确定性重放，因此 runtime 只保证协议与因果状态一致，不保证新 attempt 产生相同内容。

工具调用可能改变外部世界。仅凭缺少 tool result 的 history，无法区分“尚未执行”和“已经执行但结果未提交”。增加 execution journal 会提高 runtime 的状态复杂度，但这是安全重入和未来持久化恢复所必需的边界。
