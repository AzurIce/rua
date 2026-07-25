# Rua 架构

本文是 Rua 的架构索引，用于说明组件关系、当前实现与目标设计。具体决策、边界和权衡记录在 [`docs/designs`](designs/) 下的 design documents 中；本文不替代它们。

## 项目方向

Rua 是一个以终端为主要交互界面的 AI coding agent。目标架构由一个与 provider 和 UI 无关的 agent runtime 驱动：runtime 持有完整 conversation，向 provider 发起模型请求，通过 tool runtime 执行工具，并向 TUI 或其他客户端发布结构化事件。

```text
                         配置与应用装配
                                |
              +-----------------+-----------------+
              |                 |                 |
              v                 v                 v
          Provider <------ AgentRuntime ------> ToolRuntime
                                |
                     canonical conversation
                     + execution journal
                                |
                    +-----------+-----------+
                    |                       |
                    v                       v
                   TUI              未来的 print / RPC
```

AgentRuntime 是业务语义的中心，但不是所有功能的容器。Provider 负责协议转换，ToolRuntime 负责外部动作，客户端负责展示与输入，配置层负责选择并装配这些组件。

## 已记录的目标设计

### [D0001：Agent Runtime 与会话所有权](designs/D0001-agent-runtime.md)

定义：

- canonical conversation 的唯一所有权；
- user turn、model step 和 attempt 的关系；
- committed state 与 provisional stream output 的区别；
- 可重入 turn、稳定提交点和 execution journal；
- runtime、provider、tools 和 UI 的顶层职责边界。

### [D0002：Provider 边界与标准消息模型](designs/D0002-provider-model.md)

定义：

- instructions、user/assistant/tool-result messages 和 content parts；
- reasoning 与 opaque provider state 的无损保存；
- immutable model request snapshot；
- provider stream 的唯一 terminal event 和 completion validation；
- provider error classification、retry hint 和跨 provider history 转换。

### [D0003：TUI 架构与终端交互](designs/D0003-tui-architecture.md)

定义：

- terminal lifecycle、event normalization 与平台 backend 边界；
- grapheme-safe composer、paste 和真实硬件光标；
- AppController、display projection、commands 与 runtime events 的关系；
- frame scheduling、背压、错误恢复和 Windows IME 分阶段策略。

### [D0006：TUI 命令系统、补全与输入提示](designs/D0006-tui-command-system.md)

定义：

- prompt、命令、未完成输入、非法输入和字面 slash 转义的分类；
- CommandRegistry、结构化参数 grammar 与 invocation 路由；
- 命令名、参数和动态资源的补全查询及 stale result 丢弃；
- completion overlay、ghost text、usage、诊断与 composer 的焦点边界；
- 命令可用性、history 脱敏和有副作用操作的安全边界。

### [D0004：Tool Runtime 与执行语义](designs/D0004-tool-runtime.md)

定义：

- 开放的工具注册与 provider-neutral schema；
- tool call、执行事件和 canonical result 的 ID 关联；
- known failure 与 outcome unknown 的区别；
- 取消、输出上限、串行执行和跨平台 shell 策略。

### [D0005：执行 Journal 与 Session 恢复](designs/D0005-execution-journal-and-session-recovery.md)

定义：

- conversation 与 execution journal 的分工和持久化边界；
- external tool 之前的 write-ahead 记录顺序；
- crash recovery、已知 outcome 补交和 outcome unknown 的人工协调；
- snapshot/WAL、校验、锁与 schema migration。

D0001 回答“谁推进一个 turn”，D0002 回答“runtime 如何请求模型并提交响应”，D0003 回答“terminal 与 UI 如何投影 runtime”，D0004 回答“外部动作如何执行并记录结果”，D0005 回答“进程中断后如何安全恢复这些事实”，D0006 回答“TUI 如何发现、补全并安全路由内部命令”。

## 待设计领域

以下主题尚未形成 active design document。这里列出的是架构问题域，不代表编号或具体方案已经确定。

### 配置、凭据与应用装配

需要定义默认值、全局配置、项目配置、环境变量、CLI override 和 runtime override 的优先级；区分普通设置与 secrets；并决定如何组装 provider、model、tools、runtime 和 client。

### 项目上下文与扩展资源

需要定义 system prompt、项目规则、skills、prompt templates、自定义工具和其他资源的发现、信任、优先级及 reload 语义。

## 当前实现

当前运行路径已经切换到 provider-neutral runtime：

```text
main.rs
  |
  v
AgentRuntime ----> DeepSeekProvider
  |     |
  |     +-------> ToolRegistry -> BashTool
  v
canonical Conversation
  |
RuntimeEvent -> AppState projection -> TUI
```

- `AgentRuntime` 是 canonical conversation 的唯一写入者，并执行 model—tool—model loop。
- 每个 turn 同时受 provider attempt、model step 和累计 tool-call 上限约束；tool-call 超限在 assistant draft 提交前终止，避免留下缺少对应 results 的 canonical message。
- 同一 model step 的 retry 复用稳定 conversation snapshot 和 `StepId`，使用新的 `AttemptId`；失败 draft 不提交。
- provider failure 后 turn 可从稳定 revision 进程内 `resume`，TUI 通过 `Ctrl+R` 触发。
- `DeepSeekProvider` 把 canonical request 编译为 wire messages，并把 SSE 转换为经 accumulator 校验的 `ProviderEvent`。
- Provider accumulator 对 opaque provider state 执行 scope 与序列化大小校验；DeepSeek 的可展示 HTTP/transport 错误会移除已配置 API key。
- `ToolRegistry` 使用开放 trait 注册工具，在注册时编译 JSON Schema，并在调用进入具体工具前执行通用参数与大小校验；Bash 在 Windows 使用 PowerShell 和 Job Object，在 Unix 使用 `sh` 和 process group。取消或超时时会终止并回收整个进程树，stdout/stderr 会持续排空但只保留有界内容，启动后的等待或通信故障仍诚实地归类为 outcome unknown。
- `AppState.history` 仅是 `RuntimeEvent` 的展示投影，不再反向构造模型 history。
- 旧 `src/session.rs`、`src/deepseek.rs` 和 `src/tools.rs` 已删除，仓库只保留新的 runtime/provider/tool 路径。
- `src/tui` 已实现可报告首个恢复错误的 terminal guard、backend-neutral key event、grapheme-safe composer、paste 分流和真实硬件光标；Crossterm 类型不再进入 app 或 agent 层。
- `AppController` 已串行处理 terminal/runtime 事件并产生显式 commands；terminal input 与 runtime projection 使用独立通道并优先处理输入，相邻 text/reasoning deltas 在 lifecycle boundary 前合并；frame scheduler 合并 dirty draw，并仅在动画活动时按 deadline 推进 spinner。
- TUI 内部命令已由内建 `CommandRegistry` 统一解析 `/help`、`/clear`、`/quit`、`/session`、`/recovery` 与 `/approval`；结构化 grammar 同时驱动 usage、静态与动态资源补全、completion overlay 和 ghost text。命令与 prompt 使用分离的本地 history，availability 和 context revision 在 dispatch 前复核；session 列举与运行中加载由应用装配层协调，registry 不持有 store 或 runtime。
- Tool registry 除 Bash 外已提供 workspace-scoped read/write/edit/glob/grep；只读工具声明 `ReadOnly` replay class，写工具声明 `Effectful`，输出与搜索结果均有界。
- Effectful/Unknown 工具支持 durable `auto`/`ask`/`never` approval；pending approval 可跨进程恢复，且 approval resolution 在 `ToolExecutionStarted` 前落盘。
- 可选 `RUA_INPUT_TRACE` 记录脱敏的 terminal event 类型、修饰键、时间与环境能力，不记录键入字符或 paste 内容。

D0005 的核心恢复路径已经落地：`SessionStore` 端口、conversation/tool write-ahead 记录、带长度与 checksum framing 的本地 WAL、原子 snapshot、单 writer 锁、journal replay、raw export/validation、崩溃尾部自动截断，以及 outcome unknown 的显式 reconciliation。Session ID 不能逃逸项目内目录，Unix session 目录和文件会收紧为用户私有权限。完整 frame 的 checksum 或中间记录损坏仍会被拒绝，不能被 repair 命令静默截断。尚未实现的关键部分是 schema migration、针对中间损坏的只读报告/显式修复流程、强隔离 sandbox、Windows IME 测试矩阵，以及配置与资源装配设计。

## 推进顺序

当前建议顺序如下：

1. 记录配置、凭据与应用装配设计，解除 `main.rs` 对单一 DeepSeek 配置的硬编码。
2. 在 Tool Runtime 已有 approval、coding tools、workspace cwd、timeout 和进程树清理基础上，继续补强 OS sandbox 与 output streaming。
3. 为 D0005 增加 schema migration，并维持对非尾部损坏的拒绝与原始证据保留。
4. 完成 Windows input backend trace 与 IME 测试矩阵，再决定 VT/legacy/cooked 模式。
5. 设计项目规则、skills、prompt resources 与 context compaction。

每项实现都应明确区分当前行为和目标设计。Active design 与实现尚未对齐期间，提交应按项目采用的 design transition 协议标记 WIP，直至实现经过验证。
