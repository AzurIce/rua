# D0005：执行 Journal 与 Session 恢复

## 背景

Canonical conversation 是 active session branch 上的 committed model context，但它不能说明 runtime 已经对外部世界做了什么。尤其是工具调用存在一个不可消除的崩溃窗口：工具可能已经启动或完成，而对应的 `ToolResultMessage` 尚未来得及写入 conversation branch。

进程内 `AgentRuntime::resume_turn` 已能从 model failure 的 stable conversation head 重新进入；进程退出后，这份 active turn 状态会丢失。若仅持久化 conversation，恢复时看到“assistant 请求了工具，但没有 result”，无法判断该工具是否应当再次执行。对 `git commit`、写文件、发送消息等副作用操作，盲目重放是不安全的。

本设计定义一个与 canonical conversation 并列、可持久化的 execution journal。它记录事实性执行状态和恢复决策，而不作为模型消息发送。

因此，session persistence 的目标不是“尽量恢复一些 UI”，而是在重启后不对外部世界撒谎。只要 Rua 不能证明一个动作尚未开始、已经完成或明确失败，就不能为了让 agent loop 看起来连续而猜测一个 tool result。

## 目标

- 让 committed conversation、turn 控制状态和工具执行事实在进程重启后可恢复。
- 明确每个外部副作用前必须持久化的记录点。
- 自动恢复未提交的 model step；绝不自动重放结果未知的 effectful tool。
- 已知 tool outcome 在崩溃后可以恰好一次地提交为 canonical tool result，而无需再次执行工具。
- 支持 schema version、校验、损坏检测和后续 migration。
- 保持 provider、TUI 和具体 tool 实现与存储格式解耦。

## 非目标

- 云同步、多设备并发编辑、协作 session；
- conversation branch、merge、compaction 的最终交互；session tree 与 branch-local working directory 由 [D0007](D0007-session-tree-and-working-directory.md) 定义；
- 加密、密钥管理或 sandbox 策略；
- 为任意外部系统提供 exactly-once 副作用保证；
- 持久化 provider stream 的逐 token draft 或 TUI scroll/fold state；
- 并行 tool execution。

Journal 只能保证 Rua 自己的记录和恢复决策可靠。对于不支持 idempotency key 的外部系统，无法把“进程在请求发出后崩溃”变成真正的 exactly-once。

## Session 由什么组成

```text
               committed session entries
                           |
                           v
SessionStore <---- AgentRuntime ----> Provider / ToolExecutor
     |                    |
     |                    v
 snapshot + WAL      active branch -> Provider
                          |
                          v
                    RuntimeEvent -> TUI
```

`AgentRuntime` 仍是唯一能追加 canonical message 的组件。`SessionStore` 是持久化端口：runtime 在改变 committed state 或越过外部副作用边界前后，调用它追加 journal record。TUI 只观察恢复和运行事件，不能直接修改 journal。

### 三类状态

| 状态 | 作用 | 能否发送给模型 |
| --- | --- | --- |
| Session tree | immutable message/context entries、durable head 与 branch-local directory | 只有 active path 的模型消息和环境上下文 |
| Conversation | 从 active root-to-head path 派生的 user、assistant、tool result messages | 是 |
| Journal | turn phase、step/attempt、tool intent/outcome、recovery decision | 否 |

Head revision 是协议状态；journal sequence 是存储状态。二者分别单调递增，journal record 引用它观察到的 stable head、directory revision 与 session entry，恢复时必须校验这些引用一致。Session tree、durable head 和 change-directory 的完整语义由 [D0007](D0007-session-tree-and-working-directory.md) 定义。

### Durable session state

恢复后的内存状态由以下内容构成：

```rust
struct RecoveredSession {
    session_id: SessionId,
    locator: SessionLocator,
    tree: SessionTree,
    head: ConversationHead,
    branch: MaterializedBranch,
    active_turn: Option<DurableTurn>,
    last_sequence: JournalSequence,
}

struct DurableTurn {
    turn_id: TurnId,
    phase: TurnPhase,
    stable_head: StableHead,
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

## Journal 记录状态转换，不是调试日志

每条 record 包含：

- `schema_version`、`session_id`、单调 `sequence`；
- record 类型和 payload；
- 关联的 `turn_id`、`step_id`、`attempt_id` 或 `tool_call_id`（适用时）；
- 写入前/后的 expected/resulting head revision，以及适用时的 directory revision；
- 可校验的 framing checksum。

首版 journal 采用一行一个 JSON record 的 append-only WAL；每条记录在承诺完成前写入并 `sync_data`。不允许修改已有行。为了防止半行写入，record 使用长度 framing 与 checksum；恢复只接受完整、校验通过且 sequence 连续的前缀，尾部不完整记录视为崩溃残留并截断到最后完整 record。

建议的事件集合：

```text
SessionCreated { initial_working_directory }
SessionRelocationPrepared { from, to, locator_generation }
SessionRelocationCommitted { to, locator_generation }
SessionEntryAppended { entry, expected_head, resulting_head_revision }
ConversationHeadMoved { expected_head, target_entry_id, resulting_head_revision }
TurnOpened { turn_id, stable_head }
ModelStepPrepared { turn_id, step_id, stable_head, turn_context_snapshot }
AttemptStarted { step_id, attempt_id }
AttemptFailed { step_id, attempt_id, error, retryable }
AssistantCommitted { step_id, entry_id, resulting_head_revision }
ToolExecutionIntended { tool_call_id, name, arguments, replay_class }
ToolExecutionStarted { tool_call_id, execution_id }
ToolOutcomeRecorded { tool_call_id, execution_id, outcome }
ToolResultCommitted { tool_call_id, entry_id, resulting_head_revision }
TurnCompleted | TurnFailed | TurnCancelled
ReconciliationResolved { tool_call_id, decision }
SnapshotCreated { last_sequence }
```

`SessionRelocationPrepared` 与 `SessionRelocationCommitted` 记录 session-global locator transaction，不进入 branch tree；跨文件系统 relocation 的 staging、owner generation 与 source redirect 由 [D0007](D0007-session-tree-and-working-directory.md) 约束。`SessionEntryAppended` 是 canonical message 与 directory/context entry 的 durable source；`ConversationHeadMoved` 使不追加 entry 的 checkout 也能恢复。`AssistantCommitted` 和 `ToolResultCommitted` 是便于恢复索引和状态机校验的语义 marker，必须与对应 entry/head revision 一致，不能独立制造消息。

## 哪些边界必须先落盘

### 普通 committed conversation

把 user、assistant 或 known tool result 加入 active branch 前，runtime 先把带完整 message、parent entry 和 expected head 的 `SessionEntryAppended` 写入 WAL 并同步；随后更新内存 tree/head、重建 branch conversation 并发布事件。重启 replay 后能得到同一条 entry 和同一 active path。

snapshot 是 replay 加速，不是额外真相：先原子写入临时 snapshot 文件、flush、rename，再追加 `SnapshotCreated`。启动时加载最新合法 snapshot，然后重放之后的 WAL records。

### Model step

```text
Session head stable at H/R
  -> durable ModelStepPrepared(H/R)
  -> durable AttemptStarted(A)
  -> provider stream
  -> Assistant message complete
  -> durable SessionEntryAppended(assistant, H -> H', R+1)
  -> durable AssistantCommitted
```

`turn_context_snapshot` 至少冻结该请求实际使用的 cwd、directory revision 与 context revision。它不是 branch state 的第二真相，而是用于证明“这次请求确实在什么上下文中发出”的审计记录；恢复时必须与 stable head 派生结果一致。

provisional deltas、未完成 tool arguments 和 stream draft 不持久化。若在 assistant commit 前崩溃，恢复到 `ModelStepPrepared` 的 stable head 与已验证 context snapshot，创建新的 attempt 并重新请求模型；输出不要求相同。

### Tool step

工具执行必须遵守 write-ahead 顺序：

```text
assistant tool call 已 committed
  -> durable ToolExecutionIntended
  -> durable ToolExecutionStarted + sync
  -> invoke ToolExecutor
  -> durable ToolOutcomeRecorded + sync
  -> durable SessionEntryAppended(tool result) + sync
  -> durable ToolResultCommitted
```

`ToolExecutionStarted` 成功同步是允许调用外部工具的前置条件。这样任何“工具可能已经开始”的崩溃都会留下证据；恢复逻辑不会把缺失 result 误判为未执行。

`ToolOutcomeRecorded` 对 `Completed` 和 `FailedKnown` 必须携带完整、已截断的 result content 与 `is_error`，以便崩溃后无需重跑工具就能补写同一个 `ToolResultMessage`。若 capability 产生由 runtime 提交的 typed session effect，known outcome 还必须携带重建该 effect 所需的完整 payload，并在 tool result 前提交对应 session entry。若 outcome 记录同步失败，runtime 必须把可能已经影响外部世界的执行升级为 `OutcomeUnknown`，不能声称已知成功。

## 何时可以重放

Tool definition 在现有 schema 之外增加 replay metadata：

```rust
enum ReplayClass {
    ReadOnly,
    Idempotent { key_source: String },
    Effectful,
    Unknown,
}
```

首版 `bash` 一律是 `Unknown`：即使模型输入看似 `ls`，runtime 不解析 shell 文本来猜测副作用。未来单独定义的 `read_file` 才能标为 `ReadOnly`；具有稳定远端 idempotency key 的写操作可标为 `Idempotent`，并说明 key 从哪个参数或 runtime identity 派生。自动重放仍需同时满足工具契约和 journal 证据，不能只看分类。

| 恢复时观察到的 durable 状态 | 自动动作 |
| --- | --- |
| assistant 未提交 | 回到上一个 stable head，重新 model step |
| `ToolExecutionIntended`，尚无 `Started` | 尚未调用工具，可按策略开始执行 |
| `ToolExecutionStarted`，无 outcome | 只有契约证明尚无外部 effect 的 read-only/runtime capability 可重试；其他情况标记 `NeedsReconciliation` |
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

## 恢复不是“尽力继续”

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

## 旧记录只用于兼容读取

已经写入磁盘的 journal frame 不能因为交互策略简化而突然不可读。Rua 不再提供内建工具审批，但 decoder 仍识别旧版本的 approval request、resolution 和 awaiting phase：已明确拒绝的旧记录保持拒绝结果，尚未决定的旧记录在恢复时归一化为普通 `ExecutingTools`。新 runtime 不再生成这些记录，兼容分支也不重新暴露审批命令或状态。

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
