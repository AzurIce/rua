# TODO

## 节点正文的内存淘汰（LRU）

- **现状**（2026-09-04 类型层 + DataStore 落地后）：meta 索引（`Graph.nodes`，
  信封 + kind meta）常驻内存是设计如此；正文（`TurnData`/`ContextData`）住在
  `DataStore` 的条目里（`Arc<RwLock<NodeData<T>>>`），`entry()` 惰性加载后永久
  驻留，目前没有任何淘汰机制。主要增长路径：每次提交 input 时装配经
  `load_chain` + `entry()` 加载整条链 + 引用的材料；UI 点开详情
  （`GET /api/nodes/:id`）、`inspect` 轮询也会各自加载。长 turn 的 steps
  （bash 输出截断 50KB/次、最多 32 个 tool round + response/reasoning/
  provider_data）可到几百 KB～MB 级，随链长线性上涨。唯一的整体清空点是
  切换图（旧 `Graph` 连同其 `DataStore` 整个 drop）。
- **方向**：DataStore 的条目结构天然支持淘汰——从 map 摘 Arc 即可，持有
  `Entry<T>` 句柄的调用方无感（结构保证，见 `datastore.rs` 模块文档）；淘汰
  后下次 `entry()` 自动惰性重载（single-flight 已就位）。加一个访问时间戳
  / LRU 策略即可，读路径不用改。
- **已解决顺带项**：装配不再对整链正文做深拷贝——`load_chain` 只回 meta，
  正文经 `Entry::cloned()` 按节点读取（`assemble` 投影时逐条取，无整链
  clone）。
