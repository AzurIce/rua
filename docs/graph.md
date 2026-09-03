# 图模型与存储设计

> implementation: aligned

rua 的图是智能体的长期记忆载体：节点不可变、只增不改，加载只依赖盘上的
字节，绝不依赖当前可用的工具、provider 或任何运行时能力。本文描述图的数据
模型、存储布局，以及由此得出的 crate 边界。

## 最小模型

```
journal.jsonl        结构唯一事实源：节点信封 + cursor 状态 + turn 生命周期
turns/<ulid>.jsonl   Turn 正文：轮内事件流（llm_call / tool_exec 逐行追加）
contexts/…           Context 正文：文本材料（.md/.not），节点只存引用
```

两种边：`parent`（唯一结构边，决定会话链）、`context_refs`（材料引用边）。
三种节点：Input（一次输入）、Turn（一轮完整 agent 交互）、Context（distill
材料，永不在链上）。Cursor 是会话指针，只活在 journal 里。

## 核心不变量

1. **加载 ≠ 能力。** 图的加载、replay、装配历史，从不校验工具名是否存在于
   当前注册表，从不读取 provider 配置。存储数据里的工具名是不透明字符串。
   可用工具集只在两个时机起作用：commit 新 Input 时校验/展开覆盖列表；
   turn 执行时软拒绝未启用的工具（`error: tool not enabled`）。历史是数据，
   能力是运行时。
2. **journal 是结构的唯一事实源。** 节点信封（id、parent、context_refs、
   created_by、kind、outcome、actor、usage、model、tools、preview）只存在
   journal 的 `node_committed` 事件里，不存在第二份拷贝。扫描正文重建结构
   这条退路被显式放弃——cursor 状态与 turn 生命周期本来就只能从 journal
   恢复，journal 的耐久性（append + `sync_data` + 容忍撕裂尾行）按一等
   公民对待。
3. **存储 schema 自有。** 落盘的每一个字节由 rua 自己的类型定义，不从
   rig-core 等库借类型——库的公共类型随版本变动，而图是要长期存活的
   数据。provider 特有的往返数据（reasoning 签名、provider 侧 call id 等）
   通过显式的 `provider_data: Option<serde_json::Value>` 逃生舱原样存取，
   不进入规范化字段。

## Turn 正文：jsonl 轮内事件流

Turn 是唯一"数据一边产生一边需要落盘"的节点：agent loop 每完成一个 step
就追加一行。这给出两个性质——崩溃不丢轮内进度（已执行的工具调用和 LLM
响应都在盘上）；封口语义免费（journal 的 `turn_finished` 即封口标记，
缺封口 = Interrupted，但内容可恢复）。

行格式：

```jsonl
{"type":"init","request":[...]}        ← 可选锚点，一轮至多一份
{"type":"llm_call","response_text":"...","tool_calls":[...],"reasoning":"...","usage":{...}}
{"type":"tool_exec","call_id":"...","name":"bash","args":{...},"output":"...","duration_ms":12}
```

**`llm_call` 不存 `request`。** 旧设计每个 LlmCall step 逐字内嵌当次完整
请求消息列表：一轮 k 次调用存 k 份不断增长的历史，存储量 ≈ 上下文大小 ×
调用次数（O(k·m + k²)），是图中唯一随使用加速膨胀的数据。而轮内该信息
完全可重建——系统提示词一轮算一次（不变）、轮内历史严格 append-only，
所以 request_k ≡ 系统提示 + assemble(链) + 本轮前序 steps 重放。需要展示
"实际发了什么"（UI 快照 tab）时现场重放；若担心未来装配逻辑变化导致重建
漂移，用首轮 `init` 锚点行兜底（一轮一份，不是 k 份）。将来若引入轮内
上下文改写（compaction 等），改写本身成为新行类型，重放语义依然闭合。

## Input 与 Context 正文

- **Input 内联进 journal。** text + actor + tools 只有几百字节，不值得一个
  正文文件；`node_committed` 的 meta 直接携带。
- **Context 正文外部化为文本文件。** distill 材料的正文是文本，不是 JSON：
  存为 `.md`/`.not` 文件，节点只存路径引用。默认落在图目录
  `contexts/<ulid>.md`；引用是路径这一事实为将来直接指向 Obsidian vault /
  Notist 库（数字生命的记忆与人的笔记共用载体）留出位置。`context_refs`
  的 sources 等元信息留在 journal 信封，不污染正文文件。

## crate 边界

```
rua-graph   模型定义 + 图能力：id / node / message / graph / store /
            journal / events / cursor。依赖仅 serde、serde_json、ulid、
            thiserror。不含任何 LLM 客户端、工具注册、异步运行时。
rua-engine  消费侧：assemble（图 → 消息历史的投影）、config（provider
            配置）、agent loop、工具。rua_graph 的下游。
rua-server  daemon：REST/WS、图目录管理。rua_graph + rua_engine 的下游。
```

`CoreMessage`/`CoreToolCall` 留在 rua-graph：它们是 `llm_call` 行与装配输出
共用的**落盘 schema**，属于模型定义；`assemble()` 是投影即消费，移到 engine。
engine 保持"不接触 `Graph`"的约定：graph 暴露 `load_chain(tip)` 返回
`(chain, materials)`，engine 的 `assemble` 消费它。

这条边界是不变量 1 的编译期强制：rua-graph 物理上不可能依赖工具注册表或
rig。rig 类型只出现在 engine 的运行时边界（`to_rig` 映射），其形状可作
设计参考（如 `Usage` 的缓存字段划分），但不落盘。

## 被否决的备选

- **全部数据并入一个大 jsonl。** 读取退化为全扫或需要 id→偏移索引；
  按 id 随机访问、按节点粒度迁移/删除全部复杂化。jsonl 的价值在增量追加，
  已提交节点没有可追加的东西（进行中的 turn 除外——那正是 turns/*.jsonl）。
- **Turn 正文保持单 JSON 文档。** 轮内无增量持久化，daemon 中途死亡丢失
  整轮已执行工作；"节点存在 = 完整"的简洁性由 journal 封口标记等价给出。
- **直接复用 rig 类型落盘。** 见不变量 3：依赖边界与版本耦合。
- **保留 request 逐字存档。** O(k·m + k²) 冗余，轮内无保真需求（见上节）。

## 迁移

旧格式（`nodes/<ulid>.json` 单文件含信封+正文、LlmCall 内嵌 request）启动时
自动迁移：Input 正文并入 journal meta；Turn steps 转写为 `turns/<ulid>.jsonl`
（request 丢弃，各 LlmCall 首份 request 保留为 init 锚点）；Context body 落
`contexts/<ulid>.md`。迁移后原 `nodes/` 移入 `.trash` 留底。
