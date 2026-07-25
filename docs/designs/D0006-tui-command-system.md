# D0006：TUI 命令系统、补全与输入提示

## 背景

Rua 的 composer 同时承担两种入口：发送给模型的普通输入，以及只由客户端或 runtime 解释的控制命令。早期实现只在 `AppController` 中识别 `/recovery` 和 `/approval`，并通过字符串分割直接构造 `AppCommand`。这种做法足以打通恢复路径，却无法回答命令如何被发现、参数如何补全、什么状态下可用、未知命令是否会误发给模型，以及新的命令应当在哪一层注册。

[D0003](D0003-tui-architecture.md) 已经确定 terminal event 经 input action 进入 controller，controller 产生 command，runtime event 只用于 projection；composer 只拥有 draft、cursor 和编辑历史。本设计在这些边界内定义一套统一的 TUI 命令语言和交互协议。[D0001](D0001-agent-runtime.md) 与 [D0005](D0005-execution-journal-and-session-recovery.md) 对 session、turn 和恢复事实的所有权保持不变：TUI 可以发起操作，但不能通过解析展示状态来替 runtime 作决定。

## 目标

- 给内建 slash commands 一个可枚举、可校验、可扩展的定义来源，使解析、帮助、补全和可用性判断共享同一份语义。
- 明确普通 prompt、命令输入、未完成命令、非法命令和转义后的字面 `/` 输入之间的区别。
- 为命令名、子命令、选项、枚举值和动态资源提供一致的补全与参数提示模型。
- 定义 completion overlay、ghost text、usage、错误反馈与 composer 的焦点和按键优先级。
- 让异步候选不会阻塞输入、不会覆盖更新后的 draft，也不会在补全阶段产生外部副作用。
- 保持 TUI、应用装配、runtime 和 tool runtime 的职责边界，所有有副作用的操作继续服从既有 approval 与 journal 语义。

## 非目标

- 本设计不枚举 Rua 最终会拥有的全部命令，也不承诺文中的示例命令已实现。
- 本设计不定义 CLI 参数系统；启动参数与 TUI 内部命令可以指向相同应用能力，但不共享一段面向 argv 的解析代码。
- 本设计不定义配置、插件、skills 或 MCP 资源的发现与信任模型。它只预留受控注册入口，不允许扩展绕过未来的装配边界。
- 本设计不把命令变成 tool call，也不允许命令提示直接读取 execution journal、provider secrets 或任意文件系统位置。
- 本设计不规定具体颜色、边框字符或像素级布局。

## 输入先被分类，再被执行

slash command 只在 draft 的第一个字节是 `/` 时生效。行中出现的 `/`、URL、以空白开头的文本和后续行都不会开启命令模式。这样，命令识别不依赖 trim 后的文本，也不会改变普通 prompt 的前导或尾随空白。

`//` 是显式的字面转义：提交 `//explain this path` 时，Rua 向模型发送 `/explain this path`。未知的单 slash 输入不会静默降级为 prompt；Enter 保留 draft 并显示“未知命令”以及最接近的候选。这个选择把拼写错误留在本地，避免看似执行控制操作的文字意外进入模型上下文。

分类结果不是一个布尔值，而是能够表达编辑中状态的结构：

```text
InputClassification
  Prompt
  EscapedPrompt { submitted_text }
  Command {
    name_range,
    command_id?,
    parse_state,
  }

ParseState
  Incomplete { expected }
  Complete { invocation }
  Invalid { diagnostic }
  Unavailable { reason }
```

命令名和参数都保留其原始 byte range；所有 range 必须位于 UTF-8 字符边界，composer 的替换操作还必须把 range 扩展到 grapheme boundary。parser 不修改 draft，也不执行命令。编辑期间允许 `Incomplete`，只有提交时才把它作为需要用户补全的错误处理。

命令名采用 ASCII 小写字母、数字和连字符，匹配大小写敏感。参数 lexer 支持空白分隔、单引号、双引号和反斜杠转义，并保留 token 的原始 range 与解码值。定义为 `RestText` 的末尾参数接收剩余文本，可以包含空格和换行；其他命令出现换行时产生明确诊断。借此，恢复原因或其他自由文本无需依赖不一致的 `splitn` 规则。

## CommandRegistry 是交互语义的单一来源

应用装配阶段创建一个不可变的 `CommandRegistry` snapshot，并交给 controller 使用。每条定义至少包含：

```text
CommandDefinition
  id                 稳定的内部标识
  name               canonical slash name
  aliases            仅用于解析的兼容名称
  summary            候选列表中的短说明
  help               详细帮助
  grammar            参数与子命令结构
  visibility         默认列表是否展示
  availability       当前上下文中的可用条件
  completion_sources 各参数位置的候选来源
  history_policy     是否及如何进入本地输入历史
  target             Local | Application | Runtime
```

registry 在构建时拒绝 canonical name、alias 和稳定 ID 冲突。alias 默认不在空筛选列表中重复展示，但用户输入 alias 时可以匹配并在提示中显示 canonical name。隐藏只影响发现，不等于无法执行；当用户精确输入一个已知但当前不可用的命令时，系统返回具体原因，而不是谎称命令不存在。

`availability` 根据一个只读的 `CommandContext` 判断，例如当前是否有 active turn、是否存在 pending approval、当前 session 是否允许切换。它不能捕获 runtime、provider、store 或 tool 的可变引用。动态状态变化后，controller 生成新的 context 并重新求值，因此 popup 与 Enter 使用同一时刻的可用性规则。

grammar 是结构化参数模式，而不是 usage 字符串。它能够描述必填与可选参数、子命令、互斥选项、枚举值、资源引用和末尾自由文本。usage、当前参数提示、解析诊断和静态补全都从 grammar 派生，避免帮助文本接受一种语法而执行路径解释另一种语法。

扩展来源将来可以向应用装配层贡献声明式定义，但不能覆盖内建名称，也不能向 registry 塞入任意执行 closure。来源与信任等级必须可见；同名冲突使该扩展定义不可用并产生诊断。插件命令真正如何获得能力，留给扩展资源设计决定。

## 从 invocation 到 AppCommand

完整解析产生 `CommandInvocation`，其中包含稳定 `CommandId`、类型已验证的参数值和提交时的 context revision。它随后进入 `AppController`，由 controller 映射成已有的应用动作：

```text
Composer draft
      |
      v
Recognizer + CommandRegistry
      | Prompt ----------------------> SubmitUserInput
      |
      ` CommandInvocation
              |
              v
         AppController
          /    |     \
         v     v      v
     local UI  app    runtime command
```

`Local` 命令只改变 projection、focus、overlay 或 viewport，例如打开帮助和清理本地显示。`Application` 命令请求装配层协调跨组件操作，例如列出或加载 session。`Runtime` 命令表达 runtime 已定义的业务决定，例如 approval resolution 或 reconciliation。registry 的 `target` 用于帮助、可用性和路由校验；真正的 effect 仍由 controller 输出的显式 command 承载。

命令永远不会因为执行方便而直接调用 provider、tool 或 `SessionStore`。`/session load <id>` 可以成为应用命令，但 session 的验证、锁、恢复和 canonical conversation 仍由 D0005 所定义的 owner 完成。`/approval` 和 `/recovery` 也只提交显式 decision，不能从 TUI history 推断结果。

命令提交后，controller 清空 draft 的时机取决于是否已经接受 invocation：解析失败或上下文已过期时保留原文；同步 local command 成功接受后可以立即清空；异步 application/runtime command 一旦进入命令通道便清空，并在 projection 中显示 accepted、pending、completed 或 failed。local command 的输出是 UI projection item，不自动成为 canonical model message。

## 补全是一种带替换范围的查询

每次补全都针对 draft revision、cursor 和当前 parse state 创建查询：

```text
CompletionRequest {
  request_id,
  draft_revision,
  cursor,
  command_id?,
  argument_path,
  query_text,
  replacement_range,
  context_revision,
}

CompletionItem {
  stable_key,
  label,
  detail?,
  replacement,
  replacement_range,
  kind,
  disabled_reason?,
}
```

候选不是一段应该附加到 draft 末尾的字符串。它必须携带精确 replacement range，因此在命令名中间编辑、替换带引号参数或保留后续参数时都不会破坏文本。接受候选前，composer 再次验证 range 属于当前 revision、处于 UTF-8 与 grapheme 边界，并把 cursor 放到替换文本之后。

补全分为三类：

- registry 可同步给出命令名、alias、子命令、选项和枚举候选；
- application 可以异步给出 session ID、model、pending tool call ID 等动态资源；
- 文件路径等环境资源只能通过具名、受 scope 限制的 provider 查询，parser 本身不能遍历文件系统。

候选排序必须可预测：精确前缀优先，其次是稳定的 fuzzy match，之后按定义顺序和 canonical name 打破平局。默认列表可以隐藏兼容 alias 或诊断命令；一旦用户输入足以明确匹配，它们仍可出现。候选数量、label 和 detail 长度都有上限，避免大 registry 或长资源名称拖垮 frame scheduling。

动态查询通过应用事件通道返回，并携带原 request ID、draft revision 和 context revision。任一 revision 不匹配时直接丢弃结果；draft 改变、overlay 关闭或命令切换时取消未完成查询。补全 provider 只能读取明确授权的索引或服务，超时和失败会降级为短提示，不阻塞输入，也不使整个 TUI 失败。

## 提示层不拥有 draft

composer 继续是文本和 cursor 的唯一 owner。命令交互状态属于 projection 中独立的 `CommandAssistState`：

```text
CommandAssistState
  classification
  selected_candidate
  candidates + scroll
  active_request?
  signature_hint?
  diagnostic?
```

界面按信息密度使用三种表现：只有一个可靠补全时，可在 cursor 后显示 ghost text；有多个候选时，在 composer 邻近位置打开 completion overlay；cursor 落在参数位置时，显示紧凑的 signature/usage，并标出当前参数。解析错误与异步查询错误使用独立 diagnostic 行，不把文字插入 draft。

窄终端优先保留 draft、当前参数和首条诊断；候选列表可以缩到一行或完全隐藏，并显示匹配数量。含义不能只靠颜色表达，选中项、禁用原因和候选计数都要有文本或符号上的区分。

`/help` 使用同一个 registry 生成可浏览帮助。空参数展示当前可发现命令；`/help <command>` 展示 canonical name、aliases、usage、说明、可用性与来源。这样帮助不会成为另一份手写且会漂移的命令清单。

## 焦点与按键优先级

当 completion overlay 可见时，它先于普通 composer 和全局快捷键处理与其相关的按键：

- `Up` / `Down` 改变候选选择，而不是滚动 transcript；
- `Tab` 接受当前候选但不执行命令；
- `Enter` 接受候选。如果接受后 invocation 已完整，仍只更新 draft；用户再次按 Enter 才提交，避免候选选择同时触发有副作用的命令；
- `Esc` 先关闭 overlay 并保留 draft，第二次 Esc 才交给外层 cancel/focus 语义；
- 普通字符、删除与 cursor 移动仍编辑 composer，随后重新分类并刷新候选。

没有 overlay 时，Enter 对 `Complete` invocation 执行命令，对 `Incomplete`、`Invalid` 或 `Unavailable` 显示诊断并保留 draft，对 `Prompt` 和 `EscapedPrompt` 才产生 user input。paste 始终作为一次原子编辑进入 composer，不会因为粘贴内容恰好是完整命令而自动执行。

命令模式下的 history navigation 只检索本地 command entries；普通模式检索 prompt entries。二者可以共享一个带类型标签的本地 ring，但不能从 `AppState.history` 或 canonical conversation 反向构造。包含 secret 类型参数的定义必须选择 `Redact` 或 `Omit` history policy；帮助、诊断和 completion detail 同样不能回显 secret 值。

## 并发、状态变化与安全

command popup 可以在 model stream 期间继续工作，但每条命令的 availability 决定能否提交。全局 cancel、terminal failure 等高优先级事件仍遵循 D0003 的调度规则；overlay 不得吞掉 `Ctrl+C` 这类必须立即处理的信号。

从候选被显示到 Enter 提交之间，active turn、approval 或 session 可能已经变化。因此 invocation 带 context revision，controller 在路由前重新验证 availability；过期操作不执行并返回新的原因。该校验不是 runtime 的最终授权，有副作用的 runtime 操作仍需执行时校验、approval 和 durable journal。

补全和帮助严格无副作用。展示一个 session、tool call 或路径候选不等于加载、批准、执行或读取其内容。候选来源不得包含凭据、完整环境变量或未经信任的隐藏资源；diagnostic 对底层错误执行与其他 TUI 错误相同的脱敏规则。

## 命名空间与演进

命令优先采用少量稳定顶层名和结构化子命令，例如：

```text
/help [command]
/session list
/session load <session-id>
/approval inspect
/approval approve <tool-call-id>
/recovery inspect
/recovery retry <tool-call-id>
```

相关操作放在同一顶层命名空间，避免为每个动作占用一个全局 slash name。高频且无歧义的 local command 可以保留独立名称。重命名通过 alias 维持兼容，并在帮助中标记 canonical name；删除或改变语义需要显式的弃用期，不能让旧名称悄悄指向不同 effect。

内建 registry 是命令面的基线；动态资源、完整帮助浏览和外部扩展沿同一协议加入，而不改变 composer、controller 与 runtime 的所有权边界。

## 结果

采用本设计后，Rua 的 slash command 不再是散落在 Enter 分支中的特殊字符串。命令定义同时驱动解析、发现、帮助、补全和可用性；assist state 提供丰富交互但不拥有 draft；controller 仍是 invocation 到显式应用动作的唯一入口；runtime、session store 与 tool runtime 继续拥有各自的业务事实和副作用。

代价是应用层需要维护结构化 grammar、context revision 与异步 completion 生命周期，简单命令也不能只增加一个字符串判断。但这些成本换来了可预测的输入语义、不会误发给模型的失败行为，以及无需破坏 D0003 边界即可扩展的 TUI 命令面。
