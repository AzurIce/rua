# D0003：TUI 架构与终端交互

## 背景

Rua 当前的 TUI 是一个可运行的原型：`main.rs` 同时负责终端模式、事件源、应用循环和 session 调用，`AppState` 同时持有业务投影、输入草稿和展示状态，渲染函数则通过在字符串中插入空格模拟输入光标。这种结构足以验证流式对话，但无法稳定承载 Unicode 编辑、Windows IME、粘贴、取消、runtime retry、overlay 或未来的非交互客户端。

[D0001：Agent Runtime 与会话所有权](d0001-agent-runtime.md) 已规定 TUI 不是 canonical conversation 的所有者；[D0002：Provider 边界与标准消息模型](d0002-provider-model.md) 已规定 provider wire chunks 不能直接成为 UI 的长期状态。本设计继续定义 terminal 到 display projection 之间的边界。

对 Codex 与 pi 的调查得到三个直接结论：

- Codex 在 terminal event 边界过滤 key release，将 paste 与 key 分流，并使用独立 textarea 维护 Unicode cursor 与显示宽度。
- Codex 和 pi 都把真实硬件光标定位到编辑位置；pi 明确将其用于 IME candidate window 定位。
- 两者都把 terminal lifecycle 和文本编辑从 agent/session 状态中分离。Codex 还用 restore guard 处理错误与 panic unwind。

同时，Crossterm 0.28/0.29 的 Windows legacy input parser 在 `UnicodeChar == 0` 时可能通过 `ToUnicodeEx` 合成字符。应用收到普通 `KeyCode::Char` 后已失去 composition provenance。因此，本设计不能声称仅靠 composer 层就能完整解决 Windows 原生 IME 的拼音泄漏。

## 目标

- 将终端模式、输入传输、按键语义、文本编辑、display projection 和 agent runtime 分成可独立测试的边界。
- 让 TUI 只消费 runtime events 并产生 commands，不拥有 canonical conversation。
- 在 terminal ingress 统一过滤无效或重复事件，使下游只处理规范化事件。
- 提供 grapheme-safe、display-width-aware 的 composer，并始终使用真实硬件光标。
- 将 paste 作为原子输入，而不是伪装成高速按键序列。
- 让 terminal modes 在正常退出、错误和 panic unwind 时都尽力恢复。
- 为 Windows VT input、legacy input patch 或 cooked-input compatibility mode 保留后端扩展点。
- 允许未来引入 dirty-frame scheduling、overlay、keymap、历史与多行编辑，而无需改写 agent runtime。

## 非目标

本文不决定：

- AgentRuntime 的 turn/retry/tool execution 语义；
- canonical conversation 的持久化格式；
- 最终主题、配色和消息排版；
- 完整的 Vim/Emacs 编辑模式；
- 在首个实现阶段自行实现 Windows TSF/IMM composition engine；
- 复制 Codex 的 custom terminal diff 或 paste-burst heuristic，除非 Rua 能稳定复现其对应问题。

## 约束与事实

### Terminal 不是可靠的统一字节流

Unix PTY、Windows Terminal VT input、ConPTY 与 legacy Win32 console records 的事件语义并不相同。Bracketed paste、modified Enter、key release 和 IME composition 在不同组合下可能缺失或表示不同。

Rua 必须把 terminal backend 当作能力提供者，而不是假设所有平台产生相同事件。首版仍使用 Crossterm，但 Crossterm 类型不得扩散到 composer、projection 或 runtime。

### Unicode cursor 有三种坐标

输入编辑至少同时涉及：

- UTF-8 byte offset：用于修改 Rust `String`；
- grapheme boundary：用于用户可感知的左右移动和删除；
- terminal cell column：用于折行、裁剪和硬件光标定位。

三者不能互换。Composer 内部保存 byte offset，但保证它始终是 grapheme boundary；view 通过 Unicode display width 转换为 cell column。

### IME preedit 不等于 committed text

理想输入模型包含 composition start、update、commit 和 cancel。但 Crossterm 的通用 `KeyEvent` 不提供这一模型。当前 Rua 的可靠责任范围是：

- 不重复处理 release；
- 不破坏已经提交的 Unicode text；
- 将真实硬件光标放在正确 cell，帮助 terminal/IME 定位候选窗口；
- 不把非 ASCII 输入误判成 paste；
- 能替换 terminal backend，而不修改 composer 或 runtime。

若 backend 已把 preedit 物理键错误转换为普通字符，通用应用层不能可靠猜测并删除它们。

## 目标架构

```text
TerminalSession             AgentRuntime
  modes/capabilities             |
         |                 RuntimeEvent
         v                        |
TerminalEventSource              v
  Crossterm/WinVT/...       AppController
         |                  /     |      \
     TuiEvent              /      |       \ AppCommand
         |                v       v
         +----------> Projection  Composer
                            \       /
                             v     v
                               View
                                |
                         buffer + CursorIntent
```

### TerminalSession

`TerminalSession` 是 terminal modes 的唯一所有者。它负责：

- raw mode；
- alternate/inline screen policy；
- bracketed paste；
- keyboard enhancement 与 focus reporting（能力允许时）；
- cursor visibility/style；
- suspend、外部编辑器和子进程前后的 pause/restore；
- 正常退出与 unwind 时的幂等恢复。

进入模式必须逐步记录成功状态。任一步失败时，已启用的模式由 guard 回滚。恢复过程采用 best effort，并保留第一个可报告错误；`Drop` 不 panic。

首个实现切片提供 raw mode、alternate screen、bracketed paste 和 RAII restore。inline mode、capability probe、suspend/resume 与 panic hook 后续补充。

### TerminalEventSource 与 TuiEvent

Terminal transport 只向应用发布规范化事件：

```rust
enum TuiEvent {
    Key(KeyEvent),
    Paste(String),
    Resize { columns: u16, rows: u16 },
}
```

边界规则：

- 只发布 `Press | Repeat`，丢弃 `Release`；
- bracketed paste 保持为一个 `Paste` payload；
- focus、mouse、capability reply 等事件不能伪造成 tick 或 key；
- backend error 必须成为显式 terminal failure，而不是静默结束 stream；
- 后续可在不改变下游的情况下增加 `FocusChanged`、`Composition` 或 capability events。

首个实现切片仍将 `TuiEvent` 包装进现有 `UiEvent` channel。目标状态会把 terminal、runtime 和 frame request 作为 controller 的独立输入源，避免一个无界 channel 混合不同优先级的流量。

### InputAction 与 keymap

原始 `KeyEvent` 不应直接修改 `AppState`。目标结构先由 context-aware keymap 转换为 `InputAction`：

```text
Insert(text)  DeleteBackward  MoveLeft  Submit
Paste(text)   Scroll          Cancel    ToggleReasoning
```

Keymap 需要知道当前 focus、overlay 和 turn phase。全局快捷键不得吞掉普通可输入字符。原实现使用裸 `r` 展开 reasoning，使用户无法输入字母 `r`；首个切片已改为 `Ctrl+R`。完整 `InputAction` 层在 overlay 与 cancellation 接入时实施。

### Composer

Composer 只拥有 draft text、cursor 和编辑历史，不拥有 conversation 或 turn 状态。基本不变量：

- cursor 始终位于 UTF-8 与 grapheme boundary；
- backward/forward delete 删除一个 grapheme，而不是一个 code point；
- insert/paste 不改变 Unicode 内容；
- viewport 按 terminal cells 裁剪，永远为硬件光标保留可见位置；
- 提交返回 draft snapshot，由 controller 决定能否产生 runtime command。

首个切片实现单个可视编辑行和水平 viewport；paste 中的换行保留在 draft，viewport 显示 cursor 所在逻辑行。后续完整多行 composer 应增加 visual-line cache、vertical preferred column、scroll state、undo/redo、kill ring、history search 与 paste placeholders；这些能力仍位于 composer，不进入 `AppState`。

### Projection

Projection 是 runtime state 到 display state 的可丢弃映射，包含：

- committed transcript items；
- 当前 turn 的 provisional text/reasoning/tool activity；
- retry、failure、cancellation 和 reconnection 状态；
- selection、fold、scroll anchor 等纯 UI 状态。

Runtime 是真相源。Projection 可以从 runtime snapshot 与 events 重建；不得反向编译展示历史来构造下一次 model request。当前 `AppState.history -> DeepSeek Message` 是迁移前行为，等 D0001 runtime 接入后删除。

### AppController 与 commands

Controller 串行处理 UI 语义，保证状态转换有单一所有者。它接收 terminal events、runtime events、frame requests 与 lifecycle signals，产生：

- `SubmitUserInput`；
- `CancelTurn`；
- `RetryModelStep`；
- `Approve/RejectTool`；
- `Quit`；
- 纯本地 view/composer actions。

Controller 不能直接调用 provider 或 tool。Commands 进入 AgentRuntime，runtime events 再更新 projection。

活动 turn 期间仍允许编辑下一份 draft；Enter 是 submit、queue follow-up 还是 disabled，由明确的 turn policy 决定，不能由 `is_streaming` 布尔值偶然决定。

### View 与 CursorIntent

View 是纯渲染。每帧根据 projection、composer、terminal size 和 focus 生成 buffer，并在有输入焦点时返回真实 cursor intent。

硬件光标位置按实际输入 viewport、prompt width、grapheme display width 和滚动计算。不得通过向展示字符串插入空格模拟 cursor，因为这会改变布局，并使 Windows IME candidate window 无法锚定到编辑位置。

首个切片已改为 `Frame::set_cursor_position`。多行 composer 到来时，cursor 计算必须与文本折行使用同一套 layout 结果，不能分别实现两份宽度算法。

### Frame scheduling 与背压

固定 80ms tick 只应用于确实活动的动画。目标调度器合并 dirty requests：

- key、paste、resize 和可见 runtime delta 标记 dirty；
- 同一时间窗内多个 delta 只触发一次 draw；
- spinner 使用 deadline 驱动，不产生永久空转 tick；
- terminal input 与 cancel 的优先级高于大量 stream delta；
- 无界 stream delta 不应无限积压；允许在 projection 边界合并连续 text delta。

首个切片保留现有 loop，以降低与 AgentRuntime 迁移的冲突。frame scheduler 在 runtime event 接入时一并实施。

## Windows 输入策略

Windows 支持分三级推进：

### Level 1：通用 TUI 正确性

当前阶段实施：过滤 release、grapheme-safe composer、真实光标、paste 分流和 terminal guard。这些修复独立于 composition backend，并能解决重复字符、候选框错位、Unicode 删除以及退出后终端损坏等问题。

### Level 2：输入可观测性与 backend 选择

增加可选诊断模式，记录脱敏后的 terminal event kind/code/modifiers/timing 和环境能力，不记录用户完整 prompt。建立测试矩阵：

- Windows Terminal stable/preview；
- PowerShell、cmd、Git Bash/ConPTY；
- Microsoft Pinyin、Japanese IME、Korean IME；
- native legacy records 与 VT input。

根据结果评估 Crossterm Windows VT input、一个最小 legacy parser patch，或独立 `WindowsEventSource`。不能仅根据 ASCII/non-ASCII 和时间猜测 composition。

### Level 3：兼容模式

如果 raw TUI backend 在某些 Windows/IME 组合中无法保真，提供明确的 cooked-input compatibility mode：让系统完成整行 IME 编辑后再提交给 controller。它会牺牲实时 keymap 和部分 overlay 能力，但比静默污染 prompt 更可靠。

## Paste 语义

Bracketed paste payload 直接插入 composer，并保留换行。无 bracketed paste 的平台不能永久依赖固定毫秒阈值作为唯一真相；如果引入 Codex 风格 paste burst，它必须：

- 位于 terminal adapter，而不是 composer；
- 对 non-ASCII typing 保持低延迟；
- 有平台化参数与 deterministic tests；
- 允许关闭；
- 不改变最终文本顺序。

## 错误与生命周期

- terminal 初始化失败：回滚已启用模式，在 TUI 外报告错误；
- terminal event source EOF/error：请求 controller 退出并恢复 terminal；
- render error：停止 loop，guard 恢复 terminal；
- runtime error：成为 projection item，不导致 terminal teardown；
- panic unwind：guard 尽力恢复；后续增加 panic hook 覆盖 guard 之前或多线程异常路径；
- 外部编辑器/交互子进程：暂停 event source，恢复 terminal，子进程结束后重新探测 size/capabilities 并 redraw。

## 测试策略

### 纯单元测试

- grapheme movement/delete：CJK、combining marks、ZWJ emoji、regional indicators；
- byte boundary 与 viewport cell width 不变量；
- key release normalization；
- paste 原子性；
- keymap 不吞普通字符；
- projection 对重复/迟到 runtime event 的处理。

### 渲染测试

使用 Ratatui `TestBackend` 验证 buffer 和 cursor position，覆盖窄窗口、双宽字符、resize、scroll 与 overlay。若引入 terminal diff 优化，再添加真实 ANSI output snapshot，特别验证 wide grapheme 后的 clear-to-end。

### 集成与实机测试

- 伪 terminal event source 驱动 controller，不依赖真实 stdin；
- fake runtime 验证 submit/cancel/retry command；
- PTY/ConPTY smoke tests 验证模式进入与恢复；
- Windows IME 测试保留事件 trace 和最终 committed text，明确区分“自动测试覆盖”和“人工验证”。

## 迁移计划

### Phase A：输入与 terminal 基础

本设计首个实现切片：

- 对齐到单一 Crossterm 0.29 依赖；
- 增加 `TerminalSession`；
- 增加规范化 `TuiEvent`，过滤 release 并分离 paste；
- 增加独立、grapheme-safe `Composer`；
- 使用 display-width-aware 水平 viewport 与真实硬件光标；
- 修复裸 `r` 吞字快捷键。

### Phase B：Controller 与 runtime projection

- 用 D0001 runtime commands/events 替换 `Session -> UiEvent`；（已完成）
- 将 `AppState.history` 拆为可重建 projection；
- 引入 `InputAction`、turn-aware commands 与 cancel/retry；
- 分离 terminal/runtime/frame 输入源。

### Phase C：完整 composer 与调度

- 多行编辑、visual-line layout cache、vertical navigation；
- history、undo/redo、paste placeholders；
- dirty-frame coalescing 和 stream delta 背压；
- overlay/focus stack 与 context keymap。

### Phase D：平台后端

- Windows event trace 与测试矩阵；
- VT/legacy/cooked backend 决策；
- suspend/resume、inline mode、external editor；
- capability negotiation 与配置。

## 重要决策与权衡

### 保留 Ratatui，隔离 Crossterm

当前问题不要求更换 Ratatui。Ratatui 适合作为 buffer/layout/view 层；平台差异主要集中在 terminal input 与 lifecycle。Rua 将 Crossterm 限制在 `tui` transport 边界，使未来替换 Windows backend 不影响 composer、controller 或 runtime。

### 不直接复制 Codex TextArea

Codex TextArea 已覆盖大量编辑语义，但与其 placeholders、paste burst、Vim mode 和 rendering contract 紧密耦合。Rua 先实现小而有不变量的 Composer，并按真实需求扩展；算法和测试模式可以借鉴，但不复制项目特有状态。

### 不把 paste timing 当作 IME 模型

高速 non-ASCII 序列可能是 IME commit，也可能是 paste。时间启发式只能作为缺少协议时的 adapter fallback，不能进入 canonical composer semantics。

### 先使用单行 viewport

真实光标和 Unicode 正确性优先于立即实现完整多行编辑器。单个可视编辑行和水平 viewport 能建立正确的坐标与边界模型，并安全展示 multiline paste 的 cursor 所在行；后续多行能力复用同一 Composer，而不是继续在 render 中拼字符串。

## 当前实现状态

Phase A 已完成。Phase B 已完成 runtime event 接线和单一 canonical history：`AppState` 只消费 `RuntimeEvent`，不再构造 provider messages；provider failure 后可通过 `Ctrl+R` 请求 `resume_turn`。完整 `AppController`、`InputAction`、cancel command、独立事件源优先级和 dirty-frame scheduler 仍未实现。
