# 图模型与存储设计

存储目录结构：

```
.rua/graphs/<name>/
  ├── turns/
  │   └── <ulid>.jsonl
  └── journal.jsonl
```

- `journal.jsonl`: 图的 journal

  不存储具体节点信息，只存储元信息和引用，append only。
  有如下条目：
  - `{ event: "node_committed", meta: Meta }`：提交节点。
- `turns/<ulid>.jsonl`：存储 Turn 节点的详细信息，对应传统的 session 内的消息记录。

## 疑问

## Turn 节点

以 `turns/<ulid>.jsonl` 存储对话数据。

每行一个 JSON 对象，行类型由 `"type"` 字段判别（serde 内部 tag，值 `snake_case`）：`init` / `llm_call` / `tool_exec`。各字段与 `type` 平铺在同一层，不嵌套 payload：

```json
{"type":"llm_call","response_text":"","tool_calls":[…],"usage":{…}}
```

### init

首次 LLM 调用的完整请求快照（系统提示词 + 初始历史）。锚点行，不折叠为 step。

| 字段 | 类型 | 说明 |
|---|---|---|
| `request` | `Vec<CoreMessage>` | 请求快照 |

`CoreMessage`：

| variant | 字段 | 说明 |
|---|---|---|
| `system` | `content: String` | 系统提示词 |
| `user` | `content: String` | 用户输入 |
| `assistant` | `content: String`、`tool_calls: Vec<CoreToolCall>` | 助手消息 |
| `tool_result` | `call_id: String`、`name: String`、`output: String` | 工具结果 |
| `context` | `body: String`、`sources: Vec<Ulid>` | 蒸馏材料，带溯源 |

### llm_call

一次 LLM 往返的终态。

| 字段 | 类型 | 说明 |
|---|---|---|
| `response_text` | `String` | 本响应的 assistant 文本，可为空（纯工具调用） |
| `tool_calls` | `Vec<CoreToolCall>` | 本响应请求的工具调用 |
| `reasoning` | `Vec<ReasoningBlock>` | 推理内容；只作审计 / UI，不回放进上下文 |
| `usage` | `Usage` | 本次调用的 token 用量 |
| `provider_data` | `Option<Value>` | provider 原样载荷（签名、加密推理、响应 id 等），落盘不解释 |

`CoreToolCall`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | `String` | wire call id 原样保留 |
| `name` | `String` | 工具名 |
| `args` | `Value` | 工具入参 |

`ReasoningBlock`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `text` | `String` | 明文推理 |
| `signature` | `Option<String>` | provider 签名 / reasoning item id |
| `encrypted` | `Option<String>` | 加密载荷原样 |

`Usage`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `input_tokens` | `u64` | 输入 token 数 |
| `output_tokens` | `u64` | 输出 token 数 |
| `reasoning_tokens` | `u64` | 推理 token 数 |
| `cached_input_tokens` | `u64` | 缓存命中的输入 token 数 |

### tool_exec

一次工具执行的终态（含 distill 调用）。

| 字段 | 类型 | 说明 |
|---|---|---|
| `call_id` | `String` | 对应的 `tool_calls[].id` |
| `name` | `String` | 工具名 |
| `args` | `Value` | 工具入参 |
| `output` | `String` | 喂给模型的输出（超限时尾部截断后的那份） |
| `duration_ms` | `u64` | 执行耗时 |
| `details` | `Option<Value>` | 结构化元数据：截断信息（行数 / 字节）、全文输出路径 |
| `is_error` | `bool` | 工具级失败标记 |

## Input 节点

Input 无独立正文文件：整个节点（信封 + 正文）作为 journal 的 `node_committed` 条目内联存储（`text`/`actor`/`tools` 只有几百字节）。信封 `id`/`created_at`/`preview` 与 meta 字段平铺同一层，kind 判别键是 `"kind"`（`input`/`turn`/`context`）——注意与 Turn 正文行的 `"type"` 是两个层级的判别：

```json
{"event":"node_committed","meta":{"id":"…","created_at":…,"preview":"…","kind":"input","text":"…","actor":"human","tools":["bash"]}}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `parent` | `Option<NodeId<Turn>>` | 相继边：上一个 Turn；根输入为 None。链严格交替 Input ↔ Turn |
| `context_refs` | `Vec<NodeId<Context>>` | 材料边：本输入引用的蒸馏材料，装配时注入请求 |
| `created_by` | `Option<NodeId<Turn>>` | 因果边：spawn 溯源（由哪个 Turn 的 spawn_turn 产生）；None = 用户直接输入 |
| `text` | `String` | 正文，内联（journal header.text） |
| `actor` | `String` | 输入者（如 `human`） |
| `tools` | `Vec<String>` | 本轮工具覆盖（wire 层的 None 在 commit 前展开为显式列表；空 = 未记录或无工具） |
