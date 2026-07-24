# D0005：执行 Journal 与 Session 恢复

## 背景

`Conversation` 是模型上下文的 committed log，但它不能说明 runtime 已经对外部世界做了什么。尤其是工具调用存在一个不可消除的崩溃窗口：工具可能已经启动或完成，而对应的 `ToolResultMessage` 尚未来得及写入 conversation。

进程内 `AgentRuntime::resume_turn` 已能从 model failure 的稳定 conversation revision 重新进入；进程退出后，这份 active turn 状态会丢失。若仅持久化 conversation，恢复时看到“assistant 请求了工具，但没有 result”，无法判断该工具是否应当再次执行。对 `git commit`、写文件、发送消息等副作用操作，盲目重放是不安全的。

本设计定义一个与 canonical conversation 并列、可持久化的 execution journal。它记录事实性执行状态和恢复决策，而不作为模型消息发送。

## 目标

- 让 committed conversation、turn 控制状态和工具执行事实在进程重启后可恢复。
- 明确每个外部副作用前必须持久化的记录点。
- 自动恢复未提交的 model step；绝不自动重放结果未知的 effectful tool。
- 已知 tool outcome 在崩溃后可以恰好一次地提交为 canonical tool result，而无需再次执行工具。
- 支持 schema version、校验、损坏检测和后续 migration。
- 保持 provider、TUI 和具体 tool 实现与存储格式解耦。

## 非目标

- 云同步、多设备并发编辑、协作 session；
- conversation branch、merge、compaction 的最终交互；
- 加密、密钥管理或 sandbox/approval 策略；
- 为任意外部系统提供 exactly-once 副作用保证；
- 持久化 provider stream 的逐 token draft 或 TUI scroll/fold state；
- 并行 tool execution。

Journal 只能保证 Rua 自己的记录和恢复决策可靠。对于不支持 idempotency key 的外部系统，无法把“进程在请求发出后崩溃”变成真正的 exactly-once。

## 核心模型

```text
                 committed conversation
                           |
                           v
SessionStore <---- AgentRuntime ----> Provider / ToolExecutor
     |                    |
     |                    v
 snapshot + WAL      RuntimeEvent -> TUI
```

`AgentRuntime` 仍是唯一能追加 canonical message 的组件。`SessionStore` 是持久化端口：runtime 在改变 committed state 或越过外部副作用边界前后，调用它追加 journal record。TUI 只观察恢复和运行事件，不能直接修改 journal。

### 两类状态

| 状态 | 作用 | 能否发送给模型 |
| --- | --- | --- |
| Conversation | user、assistant、tool result 的有序 committed messages | 是 |
| Journal | turn phase、step/attempt、tool intent/outcome、recovery decision | 否 |

conversation 的 revision 是协议状态；journal 的 sequence 是存储状态。二者分别单调递增，journal record 引用它观察到的 conversation revision，恢复时必须校验两者一致。

### Durable session state

恢复后的内存状态由以下内容构成：

```rust
struct RecoveredSession {
    session_id: SessionId,
    conversation: Conversation,
    active_turn: Option<DurableTurn>,
    last_sequence: JournalSequence,
}

struct DurableTurn {
    turn_id: TurnId,
    phase: TurnPhase,
    stable_revision: ConversationRevision,
    next_step: u32,
    attempts: Vec<AttemptRecord>,
    pending_tools: Vec<DurableToolCall>,
}

enum TurnPhase {
    AwaitingModel,
    ExecutingTools,
    NeedsReconciliation,
    Completed,
    Failed { recoverable: bool },
    Cancelled,
}
```

这是逻辑形状，不要求一开始把它作为单个 JSON 文件直接序列化。持久化真相是 snapshot 加 append-only journal；上述状态由 replay 构建。

## Journal record

每条 record 包含：

- `schema_version`、`session_id`、单调 `sequence`；
- record 类型和 payload；
- 关联的 `turn_id`、`step_id`、`attempt_id` 或 `tool_call_id`（适用时）；
- 写入前/后的 expected conversation revision；
- 可校验的 framing checksum。

首版 journal 采用一行一个 JSON record 的 append-only WAL；每条记录在承诺完成前写入并 `sync_data`。不允许修改已有行。为了防止半行写入，record 使用长度 framing 与 checksum；恢复只接受完整、校验通过且 sequence 连续的前缀，尾部不完整记录视为崩溃残留并截断到最后完整 record。

建议的事件集合：

```text
SessionCreated
ConversationAppended { message, resulting_revision }
TurnOpened { turn_id, stable_revision }
ModelStepPrepared { turn_id, step_id, revision }
AttemptStarted { step_id, attempt_id }
AttemptFailed { step_id, attempt_id, error, retryable }
AssistantCommitted { step_id, message_id, resulting_revision }
ToolExecutionIntended { tool_call_id, name, arguments, replay_class }
ToolExecutionStarted { tool_call_id, execution_id }
ToolOutcomeRecorded { tool_call_id, execution_id, outcome }
ToolResultCommitted { tool_call_id, message_id, resulting_revision }
TurnCompleted | TurnFailed | TurnCancelled
ReconciliationResolved { tool_call_id, decision }
SnapshotCreated { last_sequence }
```

`ConversationAppended` 是 canonical message 的 durable source；`AssistantCommitted` 和 `ToolResultCommitted` 是便于恢复索引和状态机校验的语义 marker，必须与对应 message/revision 一致，不能独立制造消息。

## 原子性与写入顺序

### 普通 committed conversation

把 user、assistant 或 known tool result 加入 conversation 前，runtime 先把带完整 message 的 `ConversationAppended` 写入 WAL 并同步；随后更新内存 conversation 并发布事件。重启 replay 后能得到同一条 message。

snapshot 是 replay 加速，不是额外真相：先原子写入临时 snapshot 文件、flush、rename，再追加 `SnapshotCreated`。启动时加载最新合法 snapshot，然后重放之后的 WAL records。

### Model step

```text
Conversation stable at R
  -> durable ModelStepPrepared(R)
  -> durable AttemptStarted(A)
  -> provider stream
  -> Assistant message complete
  -> durable ConversationAppended(assistant, R+1)
  -> durable AssistantCommitted
```

provisional deltas、未完成 tool arguments 和 stream draft 不持久化。若在 assistant commit 前崩溃，恢复到 `ModelStepPrepared` 的 revision，创建新的 attempt 并重新请求模型；输出不要求相同。

### Tool step

工具执行必须遵守 write-ahead 顺序：

```text
assistant tool call 已 committed
  -> durable ToolExecutionIntended
  -> durable ToolExecutionStarted + sync
  -> invoke ToolExecutor
  -> durable ToolOutcomeRecorded + sync
  -> durable ConversationAppended(tool result) + sync
  -> durable ToolResultCommitted
```

`ToolExecutionStarted` 成功同步是允许调用外部工具的前置条件。这样任何“工具可能已经开始”的崩溃都会留下证据；恢复逻辑不会把缺失 result 误判为未执行。

`ToolOutcomeRecorded` 对 `Completed` 和 `FailedKnown` 必须携带完整、已截断的 result content 与 `is_error`，以便崩溃后无需重跑工具就能补写同一个 `ToolResultMessage`。若 outcome 记录同步失败，runtime 必须把该执行升级为 `OutcomeUnknown`，不能声称已知成功。

## 工具重放类别与恢复决策

Tool definition 在现有 schema 之外增加 replay metadata：

```rust
enum ReplayClass {
    ReadOnly,
    Idempotent,
    Effectful,
    IdempotencyKey { field: String },
    Unknown,
}
```

首版 `bash` 一律是 `Unknown`：即使模型输入看似 `ls`，runtime 不解析 shell 文本来猜测副作用。未来单独定义的 `read_file` 才能标为 `ReadOnly`，带稳定远端 idempotency key 的写操作可标为 `IdempotencyKey`。

| 恢复时观察到的 durable 状态 | 自动动作 |
| --- | --- |
| assistant 未提交 | 回到上一个 stable revision，重新 model step |
| `ToolExecutionIntended`，尚无 `Started` | 尚未调用工具，可按策略开始执行 |
| `ToolExecutionStarted`，无 outcome | 标记 `NeedsReconciliation`，不重放 |
| 已有 `ToolOutcomeRecorded`，无 tool result | 从 recorded outcome 补写相同 tool result |
| tool result 已提交 | 继续下一个 model step |
| 最终 assistant 已提交 | 标记 turn completed |

`NeedsReconciliation` 必须有明确的用户操作，而不是隐式 retry：

- `Inspect`：显示 call、参数、记录时间和已知证据；
- `MarkSucceeded(result)` / `MarkFailed(result)`：人工提供可发送给模型的 tool result；
- `RetryAnyway`：只在用户明确确认后创建新的 execution id；
- `AbandonTurn`：以可审计 terminal outcome 结束 turn。

每个决议写入 `ReconciliationResolved`，使二次重启不会再次询问同一问题。

## 文件布局与锁

首版将 session 放在项目内可发现但可被 `.gitignore` 忽略的位置：

```text
.rua/
  sessions/
    <session-id>/
      manifest.json
      snapshot.json
      journal.log
      lock
```

- `manifest.json` 保存 session format version、创建信息和 snapshot pointer，不包含 API key。
- `lock` 是单 writer lease。一个 session 同时只允许一个 runtime 持有写锁；异常退出后通过操作系统锁释放，而不是依赖过期时间猜测。
- `journal.log` 与 snapshot 包含 prompts、tool arguments 和 outputs，默认以用户权限创建；不将它们写进仓库或日志。
- 修复或迁移产生新文件后用 atomic rename 替换，不就地覆写已知良好 snapshot。

全局 session 浏览、项目外 session、导入/导出和用户可配置存放位置属于配置设计，不在本设计中决定。

## 恢复流程

```text
open session + acquire writer lock
  -> validate manifest / snapshot / WAL prefix
  -> replay to RecoveredSession
  -> validate conversation and journal cross-references
  -> derive ActiveTurn phase
      -> AwaitingModel: resume model step
      -> ExecutingTools with known outcomes: commit missing results, then continue
      -> NeedsReconciliation: publish RecoveryRequired and wait
      -> terminal: open read/write completed session
```

无法校验的中间 record 不是“尽力继续”的理由。Rua 应以只读 recovery report 打开 session，保留原始文件，并要求显式 repair/export；不得静默丢弃中间 committed conversation 或假设工具未执行。

## Runtime 与 UI 边界

新增端口而不是让 `AgentRuntime` 直接使用 `std::fs`：

```rust
trait SessionStore: Send + Sync {
    fn create(&self, initial: SessionBootstrap) -> StoreFuture<'_>;
    fn append(&self, record: JournalRecord) -> StoreFuture<'_>;
    fn load(&self, session_id: &SessionId) -> StoreFuture<'_, RecoveredSession>;
    fn checkpoint(&self, state: DurableSessionState) -> StoreFuture<'_>;
}
```

store append failure是 runtime failure，不可继续执行可能产生副作用的 tool。UI 接收 `SessionRecovered`、`RecoveryRequired`、`PersistenceFailed` 等 runtime events，投影为状态和明确 command；UI 不解析 journal 文件，也不自行推断 retry。

## Compatibility、迁移与版本

- `schema_version` 只允许向前读取；未知主版本拒绝写入，提供只读导出。
- migration 是 `old snapshot + old WAL -> new temporary session files -> validate -> atomic switch`，原文件保留到验证成功。
- canonical message/opaque provider state 的 version 与 journal format version 分开管理。
- session恢复时验证 tool name 与 schema/version metadata；工具已卸载时不执行，进入 reconciliation，而不是用同名新工具猜测替代。

## 测试策略

使用内存 store 和 fault-injecting file store，在每一个 durable record 后模拟崩溃：

- user、assistant、tool result commit 前后的 replay 等价性；
- 同一 model step 的 retry 保持 revision/step identity，attempt 新建；
- `Started` 后崩溃绝不自动执行 effectful/unknown tool；
- recorded known outcome 恢复后只提交一次 result；
- 半行、checksum 错误、sequence gap、snapshot/WAL 不一致；
- writer lock 冲突和 unlock 后恢复；
- Windows rename/锁行为的集成测试；
- secret 不出现在 manifest、diagnostic 或 panic message。

必须以 property/state-machine tests 覆盖“任意 journal 前缀恢复后，不会产生重复 committed message 或未经确认的 effectful tool replay”。

## 实施切片

1. 定义 `SessionStore`、journal types、in-memory store，并让 runtime 的状态转换经过该端口。
2. 为 tool intent/start/outcome/result 建立 write-ahead 顺序，用 deterministic fake tool 覆盖 crash points。
3. 实现本地 snapshot + WAL、file lock、校验与 recovery report。
4. 将 `resume_turn` 扩展为从 `RecoveredSession` 恢复，并新增 reconciliation commands/events。
5. 最后添加 session 列表、CLI 选择、checkpoint/compaction 和配置入口。

## 当前状态

本文件是 active design，尚未实现。当前 `AgentRuntime` 只保留进程内 active turn；model failure 可以在进程存活时重试或 resume，但进程退出后不会恢复 conversation、attempt 或 tool execution state。
