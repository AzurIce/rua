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

### [D0001：Agent Runtime 与会话所有权](designs/d0001-agent-runtime.md)

定义：

- canonical conversation 的唯一所有权；
- user turn、model step 和 attempt 的关系；
- committed state 与 provisional stream output 的区别；
- 可重入 turn、稳定提交点和 execution journal；
- runtime、provider、tools 和 UI 的顶层职责边界。

### [D0002：Provider 边界与标准消息模型](designs/d0002-provider-model.md)

定义：

- instructions、user/assistant/tool-result messages 和 content parts；
- reasoning 与 opaque provider state 的无损保存；
- immutable model request snapshot；
- provider stream 的唯一 terminal event 和 completion validation；
- provider error classification、retry hint 和跨 provider history 转换。

### [D0003：TUI 架构与终端交互](designs/d0003-tui-architecture.md)

定义：

- terminal lifecycle、event normalization 与平台 backend 边界；
- grapheme-safe composer、paste 和真实硬件光标；
- AppController、display projection、commands 与 runtime events 的关系；
- frame scheduling、背压、错误恢复和 Windows IME 分阶段策略。

### [D0004：Tool Runtime 与执行语义](designs/d0004-tool-runtime.md)

定义：

- 开放的工具注册与 provider-neutral schema；
- tool call、执行事件和 canonical result 的 ID 关联；
- known failure 与 outcome unknown 的区别；
- 取消、输出上限、串行执行和跨平台 shell 策略。

### [D0005：执行 Journal 与 Session 恢复](designs/d0005-execution-journal-and-session-recovery.md)

定义：

- conversation 与 execution journal 的分工和持久化边界；
- external tool 之前的 write-ahead 记录顺序；
- crash recovery、已知 outcome 补交和 outcome unknown 的人工协调；
- snapshot/WAL、校验、锁与 schema migration。

D0001 回答“谁推进一个 turn”，D0002 回答“runtime 如何请求模型并提交响应”，D0003 回答“terminal 与 UI 如何投影 runtime”，D0004 回答“外部动作如何执行并记录结果”，D0005 回答“进程中断后如何安全恢复这些事实”。

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
- 同一 model step 的 retry 复用稳定 conversation snapshot 和 `StepId`，使用新的 `AttemptId`；失败 draft 不提交。
- provider failure 后 turn 可从稳定 revision 进程内 `resume`，TUI 通过 `Ctrl+R` 触发。
- `DeepSeekProvider` 把 canonical request 编译为 wire messages，并把 SSE 转换为经 accumulator 校验的 `ProviderEvent`。
- `ToolRegistry` 使用开放 trait 注册工具；Bash 在 Windows 使用 PowerShell，在 Unix 使用 `sh`。
- `AppState.history` 仅是 `RuntimeEvent` 的展示投影，不再反向构造模型 history。
- 旧 `src/session.rs`、`src/deepseek.rs` 和 `src/tools.rs` 已删除，仓库只保留新的 runtime/provider/tool 路径。
- `src/tui` 已实现 terminal guard、规范化事件、grapheme-safe composer、paste 分流和真实硬件光标。

尚未实现的关键部分是 D0005 的持久化 execution journal/crash recovery、完整 AppController/frame scheduler、approval/sandbox，以及配置与资源装配设计。

## 推进顺序

当前建议顺序如下：

1. 按 D0005 实现 in-memory journal 与 runtime write-ahead 状态机，再实现本地 snapshot/WAL 和 crash recovery。
2. 记录配置、凭据与应用装配设计，解除 `main.rs` 对单一 DeepSeek 配置的硬编码。
3. 按 D0003 补齐 AppController、显式 command、cancel/retry 状态机和 frame scheduler。
4. 在 Tool Runtime 上增加 approval、sandbox、专用 coding tools 和 output streaming。
5. 完成 Windows input backend trace 与 IME 测试矩阵，再决定 VT/legacy/cooked 模式。
6. 设计项目规则、skills、prompt resources 与 context compaction。

每项实现都应明确区分当前行为和目标设计。Active design 与实现尚未对齐期间，提交应按项目采用的 design transition 协议标记 WIP，直至实现经过验证。
