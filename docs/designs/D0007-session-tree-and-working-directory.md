# D0007：Session Tree 与可变工作目录

## 背景

Rua 当前把 session 放在启动项目的 `.rua/sessions` 下，并把 canonical conversation 表示为一条线性 committed log。这个组合适合恢复单一路径，却把三个不同问题绑定在了一起：session 文件属于哪里、工具下一次从哪个目录执行、模型正在沿哪一段历史继续。

只要工作目录永远不变，这三个概念看起来可以共享同一个 `project_root`。一旦 Rua 支持持续的 change-directory，绑定就会产生歧义：

```text
在 project-a 启动 Rua
  -> conversation 讨论 project-a
  -> cd ../project-b
  -> conversation 继续讨论 project-b
  -> 回到 cd 之前的消息并尝试另一条路径
```

这里 session 不应仅仅因为 `cd` 被移动到 project-b；否则一个 session 的存储位置会随着历史路径隐式改变，旧进程、恢复命令和 session selector 都无法再稳定定位它。显式的 session `move` 或 `rename` 可以改变它所在的 `.rua` 与 entry name，但那是一次 session-global 文件管理操作，不是 branch-local `cd` 的副作用。另一方面，从 `cd` 之前的消息分支时，也不能继续沿用 project-b 作为工作目录；那会让同一条历史在重放后指向不同的文件系统对象。

因此，session tree 与 change-directory 必须一起设计。目录不是一个进程全局变量，而是 active branch 上可恢复的 session state；conversation 也不再是一条可截断数组，而是不可变 entry 组成的树。执行 journal 仍保持线性，因为它记录的是事实发生顺序，而不是用户当前选择查看哪条历史。

本设计延续 [D0005](D0005-execution-journal-and-session-recovery.md) 的项目本地 store、write-ahead journal 和严格恢复模型，并在其上定义 session entry tree、durable head 与可变工作目录。

## 目标

- 保持 session locator 与 change-directory 解耦：`/cd` 不迁移或复制 session，显式 session move/rename 才改变其 `.rua` project root 或 entry name。
- 允许在同一个 session 中回到历史节点并继续，保留原分支而不修改或删除既有 entry。
- 使当前工作目录成为 active branch 的持久状态；切换分支时恢复该分支对应的目录。
- 让用户命令与模型都能请求 change-directory，同时由 runtime 统一验证、提交和发布结果。
- 保留 D0005 的 tool write-ahead、outcome-unknown reconciliation、checksum WAL 和单 writer 语义。
- 为未来的 compaction、branch summary、session fork 和目录相关资源发现提供稳定身份，而不把这些能力混入首个 tree 模型。

## 非目标

- 合并两个已经分叉的 conversation branch；
- 多进程或多人并发编辑同一 session tree；
- 自动决定进入新目录后应信任哪些项目规则、skills、plugins 或 MCP resources；
- 把目录边界当作 sandbox 或权限系统；
- 在目录移动、重命名或卸载后猜测一个替代路径；
- 全局 session 云同步或跨设备 parent-session 解析。
- `ls`、`mv`、`rename` 等完整 session 文件管理命令的最终语法与 selector 交互；本设计只定义 locator 和 relocation 必须遵守的语义边界。

## 三个身份不能再共用一个 root

Rua 为一个活动 session 明确区分三个身份：

```text
SessionLocator      session entry 当前的存储位置，只由显式 session 操作改变
WorkingDirectory    相对路径与子进程执行的当前基准，可随 branch 改变
ConversationHead   当前选中的 session entry，可在 tree 中移动
```

代表性的内存形状是：

```rust
struct SessionState {
    session_id: SessionId,
    locator: SessionLocator,
    tree: SessionTree,
    head: ConversationHead,
    directory: DirectoryState,
    active_turn: Option<DurableTurn>,
}

struct SessionLocator {
    project_root: PathBuf,
    entry_name: SessionEntryName,
}

struct ConversationHead {
    entry_id: Option<EntryId>,
    revision: HeadRevision,
}

struct DirectoryState {
    canonical_path: PathBuf,
    revision: DirectoryRevision,
}
```

`SessionEntryName` 是经过验证的单个路径组件，不能包含父目录跳转或路径分隔符。`SessionLocator` 回答“这个 session entry 当前位于哪个项目的 `.rua` 下、叫什么名字”。`WorkingDirectory` 回答“相对路径和下一次工具调用从哪里开始”。`ConversationHead` 回答“哪一条历史路径构成当前模型上下文”。三者可以在同一时刻指向不同位置，但每次变化都必须有明确所有者和 durable record。

## Session locator 与工作目录独立变化

新 session 的 initial locator 按以下顺序发现：

1. 显式指定的 project/store root；
2. 从启动目录向上找到最近的 `.rua` project marker；
3. 若没有 marker，以启动目录作为 project root，并在首次持久化时创建 `.rua`。

加载已有 session 时，locator 由打开它的 `SessionStore` entry 决定，而不是由 snapshot 中可能过期的当前工作目录决定。普通目录变化之后，布局仍然是：

```text
project-a/
  .rua/
    sessions/
      <entry-name>/
        manifest.json
        snapshot.json
        journal.log
        lock
```

即使 active branch 的工作目录已经是 `project-b`，上述文件也不会自动搬到 `project-b/.rua`。这项稳定性使 session ID、锁、repair/export 和 crash recovery 不依赖最后一次 `cd` 是否成功。

用户可以像管理文件一样显式移动或重命名 session entry。`rename` 只改变 `entry_name`；跨项目 `move` 同时改变 `project_root` 和 entry location，但保持 manifest 中的稳定 `SessionId`、entry tree、head 与历史 cwd 不变。Relocation 必须在没有 active turn 或 recovery reconciliation 时执行。它需要目标冲突检查、destination staging、flush 后的 atomic publish，以及 source redirect/tombstone；跨文件系统时不能假设一次 rename 原子完成。恢复协议必须借助 stable session ID 和 locator generation 选出唯一可写 owner，失败不能留下两个都可写的同一 session。

Locator change 是 session-global metadata，不是 session tree entry。Tree checkout 不会把 artifact 搬回旧 `.rua`，也不会撤销 rename。相反，`CwdChanged` 是 branch-local state；session relocation 不会重写任何历史 cwd，也不会隐式改变 active branch 的 cwd。需要同时改变二者的交互可以在未来提供组合命令，但其语义仍是两个显式操作组成的事务，而不是把 cwd 与 locator 重新合并。

`SessionCreated` 同时记录可证明的 initial working directory。新 session 总是写入 canonical directory；迁移旧 session 时若无法证明，则显式记录为 `None`。Branch path 在没有 `CwdChanged` entry 时从这个可选初始值开始 fold；不能把进程恢复时的 cwd 当作隐含默认值。

创建一个全新 session、移动当前 session entry 与改变当前 session 的目录是三种不同动作。`/cd` 只改变当前 session 的 branch state；`/session move` 或 rename 只改变 locator；未来的 `/session new` 可以以当时目录重新执行 project-root discovery，从而创建一个位于其他项目的新 session。

## Session entry 构成不可变树

Session header 不属于 tree。其余能够影响 branch 语义的 committed state 都是带稳定身份的 entry：

```rust
struct SessionEntry {
    id: EntryId,
    parent_id: Option<EntryId>,
    timestamp: Timestamp,
    payload: SessionEntryPayload,
}

enum SessionEntryPayload {
    Message(Message),
    CwdChanged {
        input: String,
        from: Option<DirectoryState>,
        to: PathBuf,
        source: DirectoryChangeSource,
    },
    // Future: Compaction, BranchSummary, ContextSnapshot, Label
}
```

`CwdChanged` 是一等 tree node，但不是普通 user message。`input` 保存 `/cd ../runtime` 这样的原始意图用于展示与审计；通常 `from = Some(...)`，与 `to` 一起保存验证后的绝对路径语义；恢复时使用已提交的 `to`，不得重新解析或执行旧命令文本。只有成功的目录转换才追加节点，解析失败、目标不存在或 context revision 过期都只产生诊断，不改变 tree。

`from = None` 只表示 branch 在该节点之前没有可证明的 working directory。此时只接受绝对 replacement，`to` 从 directory revision 0 建立第一份可执行目录状态。这个 establishment 仍然是 branch node：checkout 到它保留 replacement，checkout 到 parent 恢复 `DirectoryUnavailable { recorded_path: None }`，不能把 replacement 回填成 session-global initial cwd。模型工具无法在未知 cwd 下启动，因此只有显式用户 `/cd` 能产生这种节点。

状态变更在节点本身处生效。因此 checkout 到 `CwdChanged` 节点会保留该次变化，checkout 到它的 parent 则恢复变化前的 cwd：

```text
A: assistant response       cwd = project-a
|
B: [cd] . -> ../project-b   cwd = project-b
|
C: user message             cwd = project-b
```

选择 B materialize `project-b`，选择 A materialize `project-a`。Tree UI 可以把 B 渲染为结构化命令节点，但不能因此把 `/cd` 文本加入 LLM conversation。

entry 一旦提交就不能修改 parent 或 payload。普通追加把新 entry 的 `parent_id` 设为当前 head，然后把 head 前移：

```text
A -> B -> C  <- head
```

checkout 到 A 后追加 D，不截断 B 和 C：

```text
      B -> C
     /
A --+
     \
      D -> E  <- head
```

Tree 可以有多个 root，用于从“第一条消息之前”重新提交新的初始 prompt；它们在展示时位于一个虚拟 root 下。正常 branch path 仍必须是无环 parent chain，parent 必须引用已经 committed 的 entry。

### Head 是显式的 durable state

不能像简单 JSONL 实现那样把“文件最后一条 entry”默认为 active leaf。用户可能 checkout 一个旧节点后暂时退出，也可能只移动 head 而不立即追加消息。Rua 通过独立 record 持久化 head：

```text
SessionEntryAppended {
    entry,
    expected_head,
    resulting_head_revision,
}

ConversationHeadMoved {
    expected_head,
    target_entry_id,
    resulting_head_revision,
}
```

`HeadRevision` 是对 head 变更的单调版本，不等同于 branch 深度。每次 append 或 checkout 都递增，用于拒绝陈旧 controller command 和并发推进。entry identity 表达内容位置，revision 表达状态变化次数；两者不能互相替代。

### Active turn 期间不能移动 head

一个 turn 从稳定 head 开始，期间产生的 assistant、tool result 和 directory effect 必须继续追加在同一路径上。只要存在 active turn、pending retry 或 outcome-unknown reconciliation，`checkout`、`fork` 和重新编辑旧 prompt 都不可用。用户必须先让 turn 到达 terminal outcome 或显式取消/abandon。

这个限制避免产生“assistant tool call 在一个 branch，tool result 被提交到另一个 branch”的无效 conversation。

## Branch state 由 root 到 head 的路径派生

模型请求不是从所有 entry 构造，而是从当前 head 沿 parent chain 回到 root，再按正序解释：

```text
head
  -> parent
  -> ...
  -> root
  -> reverse
  -> fold entries into MaterializedBranch
```

代表性的派生结果是：

```rust
struct MaterializedBranch {
    messages: Vec<Message>,
    working_directory: Option<DirectoryState>,
    model: ModelSelection,
    reasoning: ReasoningLevel,
    effective_context: EffectiveContext,
}
```

`Message` entry 进入 canonical model context；`CwdChanged` 更新环境状态，并触发 cwd 相关 instruction、resource 与 environment context 的重新派生，但不伪装成用户说过的话。Model selection、reasoning level、project resources 和 compaction 也采用同一条路径派生原则。Branch materialization 不能只重建 messages；否则 checkout 后可能出现“历史已经回去，model、reasoning 或 cwd 仍停在另一条 branch”的分裂状态。

路径派生并不要求把永远不变的 session 配置复制到每个 entry。若某项配置在一个 session 内没有修改入口，它可以作为 materialization 的固定 seed，与 initial working directory 和 instructions 一样参与每次重建；一旦 `/model`、`/thinking` 或其他能力允许它沿历史改变，该变化就必须成为 typed branch entry，不能继续藏在 runtime 的可变字段里。这样首个 tree 只需让实际可变的 messages 与 cwd 进入节点，同时仍为后续 context state 保留同一条一致性边界。

目录因此天然是 branch-local state：

```text
root: cwd = project-a
  |
  +-- cd project-b
  |     `-- messages in project-b
  |
  `-- checkout before cd
        `-- messages still use project-a
```

checkout 完成时，runtime 从目标 path 重建 `MaterializedBranch`。若目标 cwd 为 `Some`，先验证目录与所需 runtime resources；若为 `None`，则原子发布显式 `DirectoryUnavailable` 分支状态，但仍禁止执行。随后一次性替换 head、conversation projection、working directory、model、reasoning 与 effective context。任何一项无法应用时，head move 不得部分发布。TUI 不得自己根据展示条目推导目录或只替换消息列表。

## Change-directory 不修改进程全局 cwd

Rua 不调用 `std::env::set_current_dir` 来实现持久目录变化。进程全局 cwd 会让并发任务、session maintenance、diagnostics 和未来多个 runtime 相互污染，也无法证明某个 tool execution 实际使用了哪个目录。

Runtime 持有显式 `DirectoryState`。每个 model step 和 tool execution 在准备时捕获不可变 snapshot：

```rust
struct DirectorySnapshot {
    canonical_path: PathBuf,
    revision: DirectoryRevision,
}
```

Provider request 可以读取该 snapshot 生成环境说明；Bash 使用 `Command::current_dir(snapshot.path)`；read/write/edit/glob/grep 等 coding tools 也相对同一 snapshot 解析路径。一个已经开始的 attempt 不会因为稍后的 `cd` 改变执行目录。

真正发起 LLM step 时，journal 还要冻结这次请求实际使用的环境：

```rust
struct TurnContextSnapshot {
    cwd: PathBuf,
    directory_revision: DirectoryRevision,
    context_revision: ContextRevision,
}
```

`CwdChanged` 是恢复 branch state 的权威事件，`TurnContextSnapshot` 则是一次请求已经使用什么上下文的审计事实。正常情况下，从该 turn 所在 root-to-entry path fold 出的 cwd 必须与 snapshot 一致；不一致表示 journal 或 runtime 状态损坏，不能用当前进程 cwd 猜测修复。

Shell 命令中的 `cd` 只影响该 shell 子进程，不改变 Rua 的 durable working directory。需要影响后续步骤时，必须使用 Rua 的 change-directory capability。

### 目录解析

change-directory target 按当前 branch 的 cwd 解析。提交前必须：

- 展开 Rua 明确定义支持的路径语法，而不是依赖 shell expansion；
- 解析为绝对路径并执行平台适当的 canonicalization；
- 确认目标存在且是目录；
- 保存 canonical path，同时可保留用户输入作为 display hint；
- 递增 `DirectoryRevision`。

目标可以位于 locator project root 之外。Session locator 是存储归属，不是文件系统授权边界；真正的权限来自 Rua 进程、容器或 OS sandbox。Coding-tool 的路径约束可以用于减少误操作，但不能被描述成安全隔离，因为 Bash 在相同进程权限下仍可能访问其他位置。

## 用户与模型共享同一个目录转换协议

用户通过 `/cd <path>` 请求目录变化，`/pwd` 查看 active branch 的目录。命令解析、路径补全和 availability 遵循 [D0006](D0006-tui-command-system.md)，但命令本身不能直接重建 tool registry 或修改某个共享 `PathBuf`；它向 runtime 发送结构化请求。

模型通过内建 `change_directory` capability 请求同一转换。它不能直接持有 runtime mutable state。工具返回一个经过验证的 typed effect，由 runtime 以 durable 顺序提交：

```text
assistant change_directory call committed
  -> ToolExecutionIntended
  -> ToolExecutionStarted
  -> validate target and produce DirectoryChange effect
  -> ToolOutcomeRecorded(effect)
  -> SessionEntryAppended(CwdChanged)
  -> update in-memory MaterializedBranch
  -> SessionEntryAppended(tool result)
  -> ToolResultCommitted
```

因此，工具执行与 session state mutation 仍有单一所有者：ToolRuntime 描述结果和请求的 effect，AgentRuntime 决定 effect 是否能在 expected head/directory revision 上提交。未来其他需要改变 session context 的 runtime capability 也应复用 typed-effect 边界，而不是让普通工具获得 conversation 或 session store 的写引用。

`change_directory` 在 effect entry 提交前只读取和验证文件系统，不改变外部世界，因此属于可安全重试的 runtime capability。若崩溃发生在 `ToolExecutionStarted` 后、outcome 前，恢复可以重新验证 target；不能把这项例外推广到任意 effectful/unknown tool。

用户 `/cd` 不需要伪造 tool message。Runtime 在验证目标后直接追加 `CwdChanged` entry，再发布 `WorkingDirectoryChanged` event。无论来源是用户还是模型，最终 branch state 的语义相同，并在 entry 中保留 source 以便展示和审计。

命令输入历史与 session tree 是两个不同投影。Composer 可以在本地 command history 中保存 `/cd ../project-b` 以支持上下键；session tree 保存的是成功后形成的 typed `CwdChanged`。`/help`、`/pwd`、`/tree` 等只查询或只改变 UI/navigation 的命令不因此成为 branch node；`/model`、`/thinking`、`/compact` 和未来 `/context` 这类改变后续模型语义的命令，应产生各自的 typed branch entry。Session name、locator rename/move 等则属于 session-global metadata。

## Execution journal 仍然是线性的

Session tree 描述可选择的历史，execution journal 描述实际发生过的存储和外部动作。后者不能因为 conversation 分支而变成多条 WAL：

```text
journal sequence: 1 -> 2 -> 3 -> 4 -> 5
session entries:       A
                     /   \
                    B     D
                    |     |
                    C     E
```

所有 append、head move、turn、tool execution 和 reconciliation record 仍共享一个单调 `JournalSequence`。Snapshot 保存完整 entry index、durable head、派生所需状态和 active turn；恢复先加载 snapshot，再顺序 replay WAL。

显式 session relocation 也必须进入这条事实序列，但不成为 branch entry：

```text
SessionRelocationPrepared { from, to, locator_generation }
SessionRelocationCommitted { to, locator_generation }
```

目标 artifact 只有在完整复制、校验并 atomic publish 后才能成为新 owner；source 随后保留 redirect/tombstone，使打开旧 locator 的客户端能够找到新位置而不能继续写旧副本。Tree checkout 不解释这些 records，它只使用恢复完成后选出的当前 locator。

现有 conversation revision 引用需要升级为稳定 head 引用：

```rust
struct StableHead {
    entry_id: Option<EntryId>,
    revision: HeadRevision,
    directory_revision: DirectoryRevision,
}
```

`TurnOpened`、`ModelStepPrepared`、tool intent 和 directory effect 都引用它们观察到的 stable head；`ModelStepPrepared` 还持久化 `TurnContextSnapshot`。这样恢复可以判断 record 属于哪条 branch、证明请求实际使用的 cwd，也能拒绝把旧 attempt 的结果提交到已经 checkout 的新 head。

### 从线性 session 迁移

旧 session 的 committed messages 按原顺序转换为单子链：第一条 parent 为 `None`，后续 entry parent 指向前一条，head 指向最后一条。初始 working directory 从旧 manifest/header 能证明的信息建立；不能证明的路径不得从进程当前目录或当前 locator 猜测。

旧 journal 的 sequence、tool execution 和 outcome records 保持原顺序。Migration 只增加 entry/head identity 并重写 snapshot 或新格式文件，不改变已经记录的 tool execution 事实。

## 恢复目录失败时保持事实而不是回退

恢复 session 时，recorded cwd 可能已被删除、重命名、卸载或变得不可访问。Rua 仍可以加载和展示 session tree，但不能静默退回 locator project root 后继续执行，因为相同相对路径会指向另一批文件。

这种状态表示为：

```text
DirectoryUnavailable {
    recorded_path,
    reason,
}
```

在目录不可用期间：

- tree 浏览、session export 和只读历史查看仍可用；
- 新 model turn 和 filesystem tool execution 被阻止；
- checkout 到一个目录仍有效的 branch 可以解除阻止；
- 用户可以显式 `/cd <replacement>`，产生新的 branch entry；
- Rua 不修改旧 `CwdChanged` entry，也不声称 replacement 就是原目录。

由于 recorded cwd 已不可用，此时 replacement 必须是绝对路径，或使用明确以 session locator project root 为基准的语法；Rua 不能再用不存在的 cwd 解释普通相对路径。

如果崩溃发生在已记录 typed outcome、尚未提交 directory entry 之间，恢复从 outcome 补交同一个 directory effect；如果 effect entry 已提交但 tool result 尚未提交，则从该 entry 和 recorded outcome 补写 result。若无法证明 effect 是否提交，恢复进入明确协调状态，不能重复猜测路径转换。

## Tree 交互区分 checkout 与重新编辑

`/tree` 打开当前 session 的 entry tree。首版交互提供两个明确动作，而不根据 entry 类型隐藏改变语义：

- `checkout`：把 head 移到选中的 entry，从该状态继续；
- `edit from here`：对 user/custom prompt，移动到它的 parent，并把原文本放回 composer，提交后形成 sibling branch。

选择 root 前的位置等价于 `head = None`，下一条输入创建新的 root。Tree 默认突出 active path，并允许过滤 tool、directory 和其他 context entries；directory change 应显示为类似 `cwd -> ../project-b` 的结构化节点，而不是普通聊天气泡。

Head move 是 application/runtime command，不是纯 UI selection。Controller 先检查无 active turn，再提交 expected head revision；runtime durable commit 成功后发布新的 branch snapshot，projection 才切换。这样 stale overlay 或重复按键不会在用户看见旧状态时悄悄切到另一条 branch。

Tree navigation 只重建 branch-local agent state，不回滚外部世界。Checkout 到一次 `/cd` 或文件修改之前，可以恢复旧 cwd 和旧模型上下文，但不会撤销已经写入磁盘的文件、已经执行的 shell 命令或远端副作用。TUI 必须把这种边界表达清楚，不能把 tree 描述成 filesystem snapshot 或 transaction rollback。

Branch summary 与 compaction 都会影响模型上下文，但解决的问题不同：compaction 压缩当前 path，branch summary 把离开路径的重要信息带到新路径。它们需要单独定义生成时机、文件跟踪和可编辑性；session tree 不依赖自动 summary 才能成立。

## Session fork 是第二层 tree

同一 session 内的 entry tree 用于保留探索路径；新的 session artifact 则用于形成独立工作单元。未来 fork manifest 至少记录：

```rust
struct ParentSessionRef {
    session_id: SessionId,
    fork_entry_id: Option<EntryId>,
}
```

默认 fork 仍写入当前 locator 的 `.rua/sessions`，并 materialize 所选 root-to-head path，使子 session 不依赖父文件才能恢复。`parent_session` 只表达 lineage、selector 分组和审计关系，不替代复制后的 durable state。

若用户希望在当前 cwd 所属的另一个项目创建或移动 session，应使用显式 new/move 操作。Rua 不因为一次 `/cd` 自动把后续 fork 写到另一个 `.rua`，否则 session lineage 和存储锁会被隐式分散。

因此存在两层不同的 tree：

```text
entry tree       一个 session 内的 immutable history branches
session lineage  多个 session artifacts 之间的 parent/fork 关系
```

首个实现可以只提供 entry tree；manifest 中的 parent identity 预留 session lineage，而全局或跨项目 selector 仍属于后续交互设计。

## 重要决策与权衡

### 不让 cwd 隐式决定 session 在哪里

把 session 自动存到“当前目录的 `.rua`”看似符合 shell 直觉，但每次 `cd` 都会改变持久化目标，使一个 turn 甚至可能跨两个 store。独立 locator 多引入一个概念，却让普通工作目录变化不干扰 session identity、锁和恢复；需要移动时再以显式、可恢复的 session 文件操作完成。

### 不截断 conversation 来实现分支

截断数组实现简单，但会永久删除用户离开的路径，也无法安全承载 label、branch summary 和未来 extension state。不可变 entry tree 需要 stable ID 和 path fold，却能让 checkout、fork 和审计共享一个模型。

### 不把 WAL 本身做成树

WAL 的职责是证明实际写入和外部动作顺序。让 journal 跟着用户 branch 会产生多个互不完整的 sequence，并削弱 tool outcome recovery。Tree 是 journal replay 出来的 session state，journal 自身继续保持全序。

### 不使用进程全局 chdir

全局 cwd 对单进程单任务程序很方便，但会让后台任务和未来多 session runtime 产生数据竞争式语义。显式 directory snapshot 需要所有工具正确传递路径，却能测试、恢复并审计每次执行的真实上下文。

### Change-directory 是 context effect，不是普通文本

仅把“已进入某目录”写成 system text 无法驱动工具，也无法在 checkout 时可靠恢复。把它建模为 typed session entry，使模型上下文、tool cwd、TUI 展示和 journal recovery 从同一个事实派生。
