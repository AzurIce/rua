# rua 项目说明

# 协作约定

- **永远不静默替用户做决策。** 任何影响数据格式、外部行为、API、架构方向的决策点，必须显式提出来询问用户，等用户拍板后再动手；拿不准算不算决策点时按"是"处理。派 subagent 实现时同样适用：指示未覆盖的决策点必须停下回报，不得自行拍板、事后"知会"。
- **永远不使用 plan 模式。** 直接对话讨论，确认后实现。

会话图模型的 agent（daemon + Dioxus Web UI）。Workspace 四 crate：

- `crates/rua-graph` — 图模型定义 + 图数据能力（纯库，不依赖 rig/axum/tokio，依赖仅 serde/serde_json/ulid/thiserror；不含 LLM/工具消费）：不可变节点（Input/Turn/Context；Input 携带展开后的工具覆盖列表、Turn 记录该轮有效工具集，旧数据缺字段读成空数组 = 未记录）、类型层 = `Kind` trait（只有 `type Data`，无方法无常量）+ 泛型 `Node<T>`（信封 + meta `kind: T` + 正文 `data: Option<T::Data>`，`None` = 未加载）+ 唯一和类型 `AnyNode`（journal/chain 端点的 `Serialize` = header 形状，data 永不落盘；`Deserialize` → `data: None`；kind 知识在各模块固有函数，`Node::<Turn>`/`Node::<Context>` 有类型化固有 impl）、cursor 注册表、journal 事件溯源、`Graph::load_chain`（链回溯 + 材料解析）；`Graph` 单 `nodes` map（重放填 header，`node()` 懒加载正文进同一条目）
- `crates/rua-engine` — agent loop：rig-core 0.42（OpenAI-compatible / DeepSeek；多 provider 经 `[[providers]]` 具名注册，model ref = `"provider/model"`，裸名走默认 provider）、LLM provider 配置（`config.rs`）与图→消息历史投影装配纯函数（`assemble.rs`，消费 `Graph::load_chain` 的输出）、工具（bash，行为对齐 pi：尾部截断 2000 行/50KB + 全文落盘、`exec 2>&1` 交错、无默认超时、进程组击杀；spawn_turn/inspect 图生长工具经 `TurnSpawner` trait 注入）、系统提示词由 engine 按 `EffectiveTools` 动态组装（`prompt.rs`，schema 注册/提示词/执行分发共用一份有效集；模型调未启用工具被软拒绝为 `error: tool not enabled`）、spawn_turn 支持 tools 子集委托（默认继承父 turn 有效集，沿 spawn 边单调衰减，显式子集严格校验）、`run_turn(...) -> Node<Turn>`
- `crates/rua-server` — daemon（二进制 `rua`）：独占图 + 运行时，REST/WS API，只绑 127.0.0.1；wire 层 tools=None（UI 全勾）在 commit Input 前就地展开为全量显式列表；`ServerSpawner` 实现 spawn_turn/inspect（spawn 复用 `/api/inputs` 原子路径，递归上限 4）；`GET /api/cursors/:id/chain` 回轻量 header（信封 + kind tag + meta 平铺，不含 steps，Input 正文内联 header.text；Context 行带 `distilled_from`、不带 actor/tools 假值），Turn 详情（steps + 由 turns/*.jsonl 的 init 锚点 + steps 折叠重建的 LlmCall `request`，图上不再逐字存 k 份）只在 `GET /api/nodes/:id`（view.rs 重放物化，kind 形状 = `{type, ...meta, ...data}`）按需取；`GET /api/cursors/:id/context_preview?tools=a,b` 实时装配下一轮请求（链消息 + 按覆盖动态组装的系统提示词，无 token 估计）
- `crates/rua-ui` — Dioxus 0.7 web UI：聊天视图（markdown 渲染）+ 图视图（基于 `../dioxus-flow` 画布库）。API/WS 走页面同源（daemon 同时 serve UI 与 API）；`dx serve`（默认 8080）没有 /api，`api.rs` 检测该端口时把 API/WS 改指 `127.0.0.1:3080`（daemon 已放开 CORS）；聊天视图按 Turn 容器组织（`turn-<node_id>`，输入气泡直接用 meta.text），steps 经 IntersectionObserver（±400px 预取）懒加载进 `turn_details` 缓存；滚动跟随（贴底阈值 40px，上滚解除并显示「回到底部」浮钮，切会话/新提交强制回底）；左缘常显 Turn 导航条（刻度宽/透明度映射该轮 usage，hover 出 Input preview，点击/条上滚轮按轮定位）；右侧 inspect 侧栏与滚动联动（focused_turn = 视口最靠上的可见容器；聚焦最新轮/进行中轮 = context_preview 预览，旧轮 = 该轮快照：系统提示词 + usage + steps 概要，Turn 气泡「上下文」按钮 = 定位并聚焦）；图视图 DetailPanel 同套贴底跟随；usage 显示统一走 chat.rs `usage_label`（聊天气泡 footer / 节点详情汇总行 / LlmCall step 行三处共用，节点卡片底行另有缓存 %）

存储：`<project>/.rua/graphs/<name>/`（多图；每图 `journal.jsonl` 是结构唯一事实源（节点 meta + cursor/turn 事件，Input 正文内联 meta.text），Turn 正文为 `turns/<ulid>.jsonl` 轮内事件流（init 锚点 + llm_call/tool_exec 逐行，engine 经 `TurnParams.sink` 增量落盘），Context 正文为 `contexts/<ulid>.md`；旧 `nodes/<ulid>.json` 单文件布局 open 时自动迁移并备份进图内 `.trash/migration-*`，旧 `.rua/graph/` 启动时迁移为 `graphs/default`；删除图移入 `graphs/.trash/`）。
测试遵守资源约束：`cargo test -j8 -- --test-threads=4`。
UI 构建：`nix develop` 的 devShell 已 pin dioxus-cli 0.7.10 + wasm-bindgen-cli 0.2.121（dx 0.7.10 只接受此版本）+ wasm32 target；`dx build/serve -p rua-ui --platform web` 直接用，dioxus 依赖必须保持 `=0.7.10`。
本机默认 `cc` 是 nix gcc（缺 `-liconv`），跑含 C 依赖的 cargo 命令要用 `nix develop --command ...` 或 `PATH="/usr/bin:/bin:$PATH"`。

# External Resources

When relevant context is needed beyond this workspace, the following sibling directories under `../` are available for reference.

## `../codex/`

OpenAI's **Codex CLI** and **codex-rs** (Rust-based TUI agent).

- **Languages**: Rust, TypeScript
- **Key areas**: Terminal UI (ratatui), sandboxed execution (Seatbelt), LLM protocol, app-server API, MCP tool calls
- **Build tools**: Bazel, Cargo, `just`
- **Reference for**: Rust TUI patterns, sandbox architecture, LLM streaming protocol design, snapshot testing with `insta`
- **Entry docs**: `codex-rs/` (Rust workspace), `codex-cli/` (TypeScript CLI), `docs/`

## `../claude-code-sourcemap/`

Extracted source of **Claude Code** (Anthropic's agentic coding tool).

- **Languages**: TypeScript
- **Key areas**: Agent loop, tool use, codebase exploration, context management
- **Reference for**: Claude Code's internal architecture, tool definitions, and interaction patterns
- **Entry docs**: `restored-src/`, `README.md`

## `../pi-mono/`

**pi** — a terminal-based AI coding agent (monorepo).

- **Languages**: TypeScript
- **Key areas**: TUI, coding agent, AI streaming abstractions (`packages/ai`), model providers, keybindings
- **Build tools**: npm, Vitest
- **Reference for**: Multi-provider LLM streaming design, agent test harness, TUI keybinding patterns, monorepo structure
- **Entry docs**: `packages/ai/`, `packages/coding-agent/`, `packages/tui/`

## `../opencode/`

**OpenCode** — open-source AI coding assistant.

- **Languages**: TypeScript
- **Key areas**: Agent configuration, SDK generation, session management, Drizzle schemas
- **Build tools**: Bun, Turbo
- **Reference for**: Agent config patterns, SDK build pipelines, functional TS style, schema design
- **Entry docs**: `packages/`, `specs/`, `sdks/`

---

> **Note**: These directories are read-only references. Do not modify them. If you need to borrow patterns or verify implementation details, read the relevant files directly from `../<project>/`.
