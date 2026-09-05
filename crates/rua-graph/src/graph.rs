use std::collections::HashMap;

use serde_json::json;
use ulid::Ulid;

use crate::cursor::{CursorRegistry, TurnHandle};
use crate::datastore::{DataStore, Entry};
use crate::error::{Error, Result};
use crate::id::{CursorId, NodeId};
use crate::journal::JournalEvent;
use crate::message::CoreMessage;
use crate::node::{Context, Meta, Node, Outcome, Turn};
use crate::store::Store;

/// The graph engine's state — 结构世界。`nodes` 是 meta 索引（信封 + kind
/// meta，重放自 journal，journal 是唯一事实源）；正文不在图里——数据面归
/// [`DataStore`]（`Graph::data`），按需惰性加载。The daemon owns the single
/// writer; all mutations go through these methods and are journaled.
pub struct Graph {
    store: Store,
    data: DataStore,
    nodes: HashMap<Ulid, Meta>,
    children: HashMap<Ulid, Vec<Ulid>>,
    pub cursors: CursorRegistry,
    /// Handles left in-flight when the daemon last stopped (journal replay
    /// found `TurnStarted` without `TurnFinished`).
    interrupted: Vec<TurnHandle>,
}

impl Graph {
    /// Open (or create) the graph at `root`, replaying the journal.
    /// A legacy `nodes/` layout (single-file bodies) is migrated first.
    pub fn open(root: impl Into<std::path::PathBuf>) -> Result<Self> {
        let store = Store::new(root)?;
        crate::migrate::migrate_legacy_nodes(&store)?;
        let events = store.read_journal()?;
        let mut graph = Graph {
            data: DataStore::new(store.root().to_path_buf()),
            store,
            nodes: HashMap::new(),
            children: HashMap::new(),
            cursors: CursorRegistry::new(),
            interrupted: Vec::new(),
        };
        graph.replay(events)?;
        Ok(graph)
    }

    fn replay(&mut self, events: Vec<JournalEvent>) -> Result<()> {
        for ev in events {
            match ev {
                JournalEvent::NodeCommitted { meta } => {
                    if let Some(parent) = meta.parent() {
                        self.children.entry(parent).or_default().push(meta.id());
                    }
                    self.nodes.insert(meta.id(), meta);
                }
                JournalEvent::CursorCreated { cursor } => self.cursors.restore(cursor),
                JournalEvent::CursorMoved { cursor_id, node } => {
                    // Tolerate moves for unknown cursors only if journal is corrupt.
                    self.cursors.move_to(cursor_id, node)?;
                }
                JournalEvent::CursorDetached { cursor_id } => {
                    self.cursors.detach(cursor_id)?;
                }
                JournalEvent::TurnStarted {
                    cursor_id,
                    node_id,
                    started_at,
                } => {
                    self.cursors.restore_in_flight(TurnHandle {
                        cursor_id,
                        node_id,
                        started_at,
                    });
                }
                JournalEvent::TurnFinished { cursor_id, .. } => {
                    let _ = self.cursors.finish_turn(cursor_id);
                }
            }
        }
        // Anything still in-flight after replay was interrupted mid-turn.
        self.interrupted = self.cursors.in_flight_all().copied().collect();
        for h in self.interrupted.clone() {
            let _ = self.cursors.finish_turn(h.cursor_id);
        }
        Ok(())
    }

    pub fn interrupted(&self) -> &[TurnHandle] {
        &self.interrupted
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 数据面入口：正文读取 / 进行中轮的增量写入都走这里。
    pub fn data(&self) -> &DataStore {
        &self.data
    }

    // ---- structure ----

    /// Validate and commit an immutable node's structure: journal the header,
    /// update the index. 正文不归这里管——有正文的 kind（Turn/Context）必须
    /// 已经在数据面有条目（`open_turn` 注册的空条目 / `DataStore::create`
    /// 写入的条目），否则拒绝（等价于旧的"不许提交未加载正文"）。
    pub fn commit(&mut self, node: impl Into<Meta>) -> Result<()> {
        let meta = node.into();
        let id = meta.id();
        if self.nodes.contains_key(&id) {
            return Err(Error::NodeAlreadyCommitted(id));
        }
        // 相继边：存在性 + 严格交替（Input ← Turn，Turn ← Input）。
        match &meta {
            Meta::Input(n) => {
                if let Some(parent) = n.kind.parent {
                    match self.nodes.get(&parent.raw()) {
                        None => return Err(Error::ParentNotCommitted(parent.raw())),
                        Some(Meta::Turn(_)) => {}
                        Some(_) => {
                            return Err(Error::ParentKindMismatch {
                                node: id,
                                parent: parent.raw(),
                            });
                        }
                    }
                }
            }
            Meta::Turn(n) => match self.nodes.get(&n.kind.parent.raw()) {
                None => return Err(Error::ParentNotCommitted(n.kind.parent.raw())),
                Some(Meta::Input(_)) => {}
                Some(_) => {
                    return Err(Error::ParentKindMismatch {
                        node: id,
                        parent: n.kind.parent.raw(),
                    });
                }
            },
            Meta::Context(_) => {}
        }
        // 材料边：目标必须已提交且是 Context。（`Context.sources` 是展示用
        // 溯源，异构、可悬空，不在此校验。）
        for r in meta.material_refs() {
            match self.nodes.get(&r.raw()) {
                None => return Err(Error::ContextRefNotCommitted(r.raw())),
                Some(Meta::Context(_)) => {}
                Some(_) => return Err(Error::ContextRefNotContextNode(r.raw())),
            }
        }
        // 数据面：有正文的 kind 必须先有条目。
        let body_missing = match &meta {
            Meta::Turn(n) => !self.data.contains::<Turn>(n.id),
            Meta::Context(n) => !self.data.contains::<Context>(n.id),
            Meta::Input(_) => false,
        };
        if body_missing {
            return Err(Error::DataNotLoaded(id));
        }

        if let Some(parent) = meta.parent() {
            self.children.entry(parent).or_default().push(id);
        }
        self.nodes.insert(id, meta);
        self.store.append_journal_value(&json!({
            "event": "node_committed",
            "meta": self.nodes[&id].header_value(),
        }))?;
        Ok(())
    }

    /// 节点头（信封 + meta）。结构检查与轻量读用；正文走 [`Graph::data`]。
    pub fn meta(&self, id: Ulid) -> Option<&Meta> {
        self.nodes.get(&id)
    }

    /// 受检类型恢复：把裸 Ulid 确认为 Turn 的 typed id。crate 之外重建
    /// typed id 的唯一通道（`NodeId::from_raw` 是 `pub(crate)`）——从此
    /// 任何 typed id 都是图亲手发出来的。
    pub fn expect_turn(&self, id: Ulid) -> Result<NodeId<Turn>> {
        match self.nodes.get(&id) {
            Some(Meta::Turn(n)) => Ok(n.id),
            Some(_) => Err(Error::WrongKind {
                id,
                expected: "turn",
            }),
            None => Err(Error::NodeNotFound(id)),
        }
    }

    pub fn metas(&self) -> Vec<&Meta> {
        let mut v: Vec<_> = self.nodes.values().collect();
        v.sort_by_key(|n| (n.created_at(), n.id()));
        v
    }

    pub fn children(&self, id: Ulid) -> &[Ulid] {
        self.children.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Walk parent pointers from `tip` to the root. Returned root → tip.
    pub fn chain_to_root(&self, tip: Ulid) -> Result<Vec<Ulid>> {
        if !self.nodes.contains_key(&tip) {
            return Err(Error::NodeNotFound(tip));
        }
        let mut chain = Vec::new();
        let mut cur = Some(tip);
        while let Some(id) = cur {
            let meta = self.nodes.get(&id).ok_or(Error::NodeNotFound(id))?;
            chain.push(id);
            cur = meta.parent();
        }
        chain.reverse();
        Ok(chain)
    }

    // ---- control plane ----

    pub fn create_cursor(&mut self, actor: impl Into<String>, capabilities: Vec<String>) -> crate::cursor::Cursor {
        let cursor = self.cursors.create(actor, capabilities);
        // Best-effort journal; cursor creation is cheap to lose on crash but
        // we still record it so replay sees the session.
        let _ = self
            .store
            .append_journal(&JournalEvent::CursorCreated {
                cursor: cursor.clone(),
            });
        cursor
    }

    /// Move a cursor's tip. Landing points are structural nodes only;
    /// context nodes are material, never conversation footholds.
    /// （私有原语，外部经 [`CursorMut::move_to`]。）
    fn move_cursor(&mut self, cursor_id: CursorId, node: Ulid) -> Result<()> {
        let meta = self.nodes.get(&node).ok_or(Error::NodeNotFound(node))?;
        if meta.is_context() {
            return Err(Error::CursorOnContextNode(node));
        }
        self.cursors.move_to(cursor_id, node)?;
        self.store
            .append_journal(&JournalEvent::CursorMoved { cursor_id, node })?;
        Ok(())
    }

    /// Detach a cursor: its next input starts a new root (a disconnected
    /// conversation tree).（私有原语，外部经 [`CursorMut::detach`]。）
    fn detach_cursor(&mut self, cursor_id: CursorId) -> Result<()> {
        self.cursors.detach(cursor_id)?;
        self.store
            .append_journal(&JournalEvent::CursorDetached { cursor_id })?;
        Ok(())
    }

}

/// 进行中的一轮：控制面句柄 + 数据面正文条目绑在一个 ticket 上。收尾只有
/// 两条路——[`Graph::commit_turn`]（有节点）/ [`Graph::abort_turn`]（无
/// 节点），都按 move 消费它：ticket 存在 ≙ in-flight 已注册，不存在
/// "丢了一半"或"忘了清账"的中间态。
pub struct OpenTurn {
    pub handle: TurnHandle,
    entry: Entry<Turn>,
}

impl OpenTurn {
    /// 正文条目句柄（廉价 Clone）：交给 engine 的 sink 做增量落盘。
    pub fn entry(&self) -> Entry<Turn> {
        self.entry.clone()
    }
}

/// 会话操作的受检视图：id 在创建时验证存在，之后的调用不会再返回
/// `CursorNotFound`。生命周期 = 一次 `&mut Graph` 借用（server 的锁持有
/// 期）；跨锁存活的状态由 [`OpenTurn`] 承载，轮的收尾仍在 [`Graph`] 上。
pub struct CursorMut<'a> {
    graph: &'a mut Graph,
    id: CursorId,
}

impl CursorMut<'_> {
    /// 推进会话 tip（[`Graph::move_cursor`] 语义：落点须是结构节点）。
    pub fn move_to(&mut self, node: Ulid) -> Result<()> {
        self.graph.move_cursor(self.id, node)
    }

    /// 脱离图：下一个 input 开新根（[`Graph::detach_cursor`] 语义）。
    pub fn detach(&mut self) -> Result<()> {
        self.graph.detach_cursor(self.id)
    }

    /// 开一轮（每 cursor 同时至多一轮在飞，返回 [`OpenTurn`] ticket）。
    pub fn open_turn(&mut self) -> Result<OpenTurn> {
        self.graph.open_turn(self.id)
    }
}

impl Graph {
    /// 受检会话视图（[`CursorMut`]）。cursor 不存在 → `CursorNotFound`。
    pub fn cursor_mut(&mut self, id: CursorId) -> Result<CursorMut<'_>> {
        self.cursors.get(id)?;
        Ok(CursorMut { graph: self, id })
    }
}

impl Graph {
    /// Begin a turn on a cursor: enforces "at most one in-flight turn per
    /// cursor", pre-allocates the landing node id, and registers the empty
    /// data-plane entry（语义 = 已加载但为空；进行中的轮只对数据面可见，
    /// meta 索引里没有它）。返回的 ticket 自带正文条目，收尾必须走
    /// [`Graph::commit_turn`] / [`Graph::abort_turn`]。
    /// （私有原语，外部经 [`CursorMut::open_turn`]。）
    fn open_turn(&mut self, cursor_id: CursorId) -> Result<OpenTurn> {
        let id: NodeId<Turn> = self.data.allocate();
        let handle = self.cursors.begin_turn(cursor_id, id)?;
        // 注册空条目：正文文件尚不存在 ≡ 空 TurnData（读路径闭环）。
        let entry = self.data.entry(id)?;
        self.store.append_journal(&JournalEvent::TurnStarted {
            cursor_id,
            node_id: id,
            started_at: handle.started_at,
        })?;
        Ok(OpenTurn { handle, entry })
    }

    /// 有节点收尾（成功 Completed / 取消 Cancelled，outcome 取自节点
    /// meta）：commit header（结构校验同 [`Graph::commit`]）→
    /// turn_finished → 游标推进到新 Turn，一次调用闭完全部账。commit
    /// 失败也会以 Failed 闭账清掉 in-flight，游标不会被一次失败卡死。
    pub fn commit_turn(&mut self, turn: OpenTurn, node: Node<Turn>) -> Result<NodeId<Turn>> {
        let cursor_id = turn.handle.cursor_id;
        let node_id = node.id;
        let outcome = node.kind.outcome;
        if let Err(e) = self.commit(node) {
            // header 没进去也要闭账，否则重放把这轮记成 interrupted。
            let _ = self.close_turn(cursor_id, Outcome::Failed);
            return Err(e);
        }
        self.close_turn(cursor_id, outcome)?;
        self.move_cursor(cursor_id, node_id.raw())?;
        Ok(node_id)
    }

    /// 无节点收尾（engine 起跑即败等）：只清 in-flight + journal
    /// turn_finished，游标不动。
    pub fn abort_turn(&mut self, turn: OpenTurn, outcome: Outcome) -> Result<()> {
        self.close_turn(turn.handle.cursor_id, outcome)?;
        Ok(())
    }

    /// 清 in-flight + journal turn_finished（journal/registry 一致的闭账
    /// 原语，commit/abort 共用）。
    fn close_turn(&mut self, cursor_id: CursorId, outcome: Outcome) -> Result<TurnHandle> {
        let handle = self.cursors.finish_turn(cursor_id)?;
        self.store.append_journal(&JournalEvent::TurnFinished {
            cursor_id,
            node_id: handle.node_id,
            outcome,
        })?;
        Ok(handle)
    }

    // ---- chain loading ----

    /// Resolve the chain + referenced material metas for `tip`: backtrack to
    /// root, collect the chain nodes and their `context_refs` targets。
    /// 只回结构（meta）；正文由装配层经 [`Graph::data`] 按 id 取。
    pub fn load_chain(&self, tip: Ulid) -> Result<(Vec<Meta>, HashMap<Ulid, Meta>)> {
        let chain_ids = self.chain_to_root(tip)?;
        let mut chain = Vec::with_capacity(chain_ids.len());
        let mut materials = HashMap::new();
        for id in chain_ids {
            let meta = self.nodes.get(&id).ok_or(Error::NodeNotFound(id))?.clone();
            for r in meta.material_refs() {
                let m = self
                    .nodes
                    .get(&r.raw())
                    .ok_or(Error::NodeNotFound(r.raw()))?;
                materials.insert(r.raw(), m.clone());
            }
            chain.push(meta);
        }
        Ok((chain, materials))
    }

    /// The turn's init anchor: the first LLM call's full request snapshot
    /// (`None` when the body has no `Init` line, e.g. direct-commit paths).
    pub fn turn_init(&self, id: Ulid) -> Result<Option<Vec<CoreMessage>>> {
        crate::node::turn::init_anchor(self.store.root(), id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{Context, ContextData, Data, Input, Node, Step, TurnData, TurnLine, Usage};

    fn input_node(text: &str, parent: Option<NodeId<crate::node::Turn>>) -> Node<Input> {
        Input::node(NodeId::new(), parent, text, "human", vec![], None)
    }

    fn sample_steps() -> Vec<Step> {
        vec![Step::LlmCall {
            response_text: "reply".into(),
            tool_calls: vec![],
            reasoning: None,
            usage: Usage::default(),
            provider_data: None,
        }]
    }

    fn turn_node(parent: NodeId<Input>, outcome: Outcome) -> Node<Turn> {
        Turn::node(
            NodeId::new(),
            parent,
            outcome,
            "agent",
            "deepseek-v4-pro",
            Usage::default(),
            vec![],
            &sample_steps(),
        )
    }

    /// 直接提交路径：数据面建条目（逐行 append，无 sink 也同一通道）+
    /// commit header。
    fn commit_turn(g: &mut Graph, node: Node<Turn>, steps: &[Step]) {
        let entry = g.data().entry(node.id).unwrap();
        for step in steps {
            entry.append(TurnLine::from(step.clone())).unwrap();
        }
        g.commit(node).unwrap();
    }

    fn commit_context(g: &mut Graph, body: &str, sources: Vec<Ulid>, distilled_from: Option<NodeId<Turn>>) -> Node<Context> {
        let id: NodeId<Context> = g.data().allocate();
        g.data()
            .create(id, ContextData { body: body.into() })
            .unwrap();
        let node = Context::node(id, sources, distilled_from, "deepseek-v4-pro", body);
        g.commit(node.clone()).unwrap();
        node
    }

    #[test]
    fn commit_and_chain() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let i1 = input_node("hello", None);
        let t1 = turn_node(i1.id, Outcome::Completed);
        let i2 = input_node("next", Some(t1.id));
        g.commit(i1.clone()).unwrap();
        commit_turn(&mut g, t1.clone(), &sample_steps());
        g.commit(i2.clone()).unwrap();

        assert_eq!(
            g.chain_to_root(i2.id.raw()).unwrap(),
            vec![i1.id.raw(), t1.id.raw(), i2.id.raw()]
        );
        assert_eq!(g.children(i1.id.raw()), &[t1.id.raw()]);
        assert_eq!(g.children(t1.id.raw()), &[i2.id.raw()]);
    }

    #[test]
    fn context_tokens_from_last_llm_call() {
        let i = input_node("hi", None);
        let steps = vec![
            Step::LlmCall {
                response_text: "reply".into(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage::default(),
                provider_data: None,
            },
            Step::LlmCall {
                response_text: "final".into(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage {
                    input_tokens: 12_345,
                    ..Usage::default()
                },
                provider_data: None,
            },
        ];
        let t = Turn::node(
            NodeId::new(),
            i.id,
            Outcome::Completed,
            "agent",
            "m",
            Usage::default(),
            vec![],
            &steps,
        );
        assert_eq!(t.kind.context_tokens, Some(12_345));
        // 无 LLM 调用的回合没有上下文量
        let empty = Turn::node(
            NodeId::new(),
            i.id,
            Outcome::Failed,
            "agent",
            "m",
            Usage::default(),
            vec![],
            &[],
        );
        assert_eq!(empty.kind.context_tokens, None);
    }

    #[test]
    fn fork_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let i1 = input_node("root", None);
        g.commit(i1.clone()).unwrap();
        // Input 的父必须是 Turn：直接挂在另一个 Input 下会被交替性拒绝。
        let wrong = input_node("wrong attach", Some(NodeId::from_raw(i1.id.raw())));
        assert!(matches!(
            g.commit(wrong),
            Err(Error::ParentKindMismatch { .. })
        ));
        let t1 = turn_node(i1.id, Outcome::Completed);
        commit_turn(&mut g, t1.clone(), &sample_steps());
        let a = input_node("branch a", Some(t1.id));
        let b = input_node("branch b", Some(t1.id));
        g.commit(a.clone()).unwrap();
        g.commit(b.clone()).unwrap();
        assert_eq!(g.children(t1.id.raw()), &[a.id.raw(), b.id.raw()]);
    }

    #[test]
    fn nodes_are_immutable() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let n = input_node("x", None);
        g.commit(n.clone()).unwrap();
        assert!(matches!(
            g.commit(n),
            Err(Error::NodeAlreadyCommitted(_))
        ));
    }

    #[test]
    fn context_boundary_rules() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let i1 = input_node("root", None);
        g.commit(i1.clone()).unwrap();
        let t1 = turn_node(i1.id, Outcome::Completed);
        commit_turn(&mut g, t1.clone(), &sample_steps());

        // 材料边指向未提交节点：拒绝。
        let mut bad = input_node("bad", Some(t1.id));
        bad.kind.context_refs = vec![NodeId::from_raw(i1.id.raw())];
        assert!(matches!(
            g.commit(bad),
            Err(Error::ContextRefNotContextNode(_))
        ));

        // Context 节点：sources 是展示用溯源，异构、不校验存在性。
        let c = commit_context(&mut g, "summary", vec![i1.id.raw(), Ulid::new()], Some(t1.id));

        // Input 引用 Context：ok。
        let mut ok = input_node("with material", Some(t1.id));
        ok.kind.context_refs = vec![c.id];
        g.commit(ok).unwrap();

        // Cursor cannot land on a context node.
        let cur = g.create_cursor("human", vec![]);
        assert!(matches!(
            g.cursor_mut(cur.id).unwrap().move_to(c.id.raw()),
            Err(Error::CursorOnContextNode(_))
        ));
    }

    /// 交替性：Turn 的父指向 Turn / Input 的父指向 Input，都被拒绝。
    #[test]
    fn chain_must_alternate() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let i1 = input_node("root", None);
        g.commit(i1.clone()).unwrap();
        let t1 = turn_node(i1.id, Outcome::Completed);
        commit_turn(&mut g, t1.clone(), &sample_steps());

        // Turn 的父是 Turn（伪造 typed id 指向 t1）。
        let bad_t = Turn::node(
            NodeId::new(),
            NodeId::from_raw(t1.id.raw()),
            Outcome::Completed,
            "agent",
            "m",
            Usage::default(),
            vec![],
            &sample_steps(),
        );
        let entry = g.data().entry(bad_t.id).unwrap();
        for s in &sample_steps() {
            entry.append(TurnLine::from(s.clone())).unwrap();
        }
        assert!(matches!(
            g.commit(bad_t),
            Err(Error::ParentKindMismatch { .. })
        ));

        // Input 的父是 Input。
        let bad_i = input_node("bad", Some(NodeId::from_raw(i1.id.raw())));
        assert!(matches!(
            g.commit(bad_i),
            Err(Error::ParentKindMismatch { .. })
        ));
    }

    #[test]
    fn one_in_flight_per_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let cur = g.create_cursor("human", vec![]);
        let mut c = g.cursor_mut(cur.id).unwrap();
        let t1 = c.open_turn().unwrap();
        assert!(matches!(c.open_turn(), Err(Error::CursorBusy(_))));
        let first_id = t1.handle.node_id;
        g.abort_turn(t1, Outcome::Cancelled).unwrap();
        let t2 = g.cursor_mut(cur.id).unwrap().open_turn().unwrap();
        assert_ne!(first_id, t2.handle.node_id);
    }

    #[test]
    fn journal_replay_rebuilds_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("g");
        let (cur_id, tip_id) = {
            let mut g = Graph::open(&root).unwrap();
            let cur = g.create_cursor("human", vec![]);
            let i1 = input_node("hello", None);
            g.commit(i1.clone()).unwrap();
            g.cursor_mut(cur.id)
                .unwrap()
                .move_to(i1.id.raw())
                .unwrap();
            (cur.id, i1.id)
        };

        let g2 = Graph::open(&root).unwrap();
        assert_eq!(g2.cursors.get(cur_id).unwrap().node, Some(tip_id.raw()));
        // Input 正文内联进 header：重放即得全量正文，无需触碰数据面。
        let loaded = g2.meta(tip_id.raw()).unwrap().input().unwrap();
        assert_eq!(loaded.kind.text, "hello");
        assert!(g2.interrupted().is_empty());
    }

    #[test]
    fn detach_and_replay() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("g");
        let cur_id = {
            let mut g = Graph::open(&root).unwrap();
            let cur = g.create_cursor("human", vec![]);
            let i1 = input_node("hello", None);
            g.commit(i1.clone()).unwrap();
            g.cursor_mut(cur.id)
                .unwrap()
                .move_to(i1.id.raw())
                .unwrap();
            g.cursor_mut(cur.id).unwrap().detach().unwrap();
            assert_eq!(g.cursors.get(cur.id).unwrap().node, None);
            cur.id
        };
        let g2 = Graph::open(&root).unwrap();
        assert_eq!(g2.cursors.get(cur_id).unwrap().node, None);
    }

    #[test]
    fn unfinished_turn_is_interrupted_on_replay() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("g");
        let cur_id = {
            let mut g = Graph::open(&root).unwrap();
            let cur = g.create_cursor("human", vec![]);
            g.cursor_mut(cur.id).unwrap().open_turn().unwrap();
            cur.id // ticket dropped without closing: daemon "died"
        };
        let mut g2 = Graph::open(&root).unwrap();
        assert_eq!(g2.interrupted().len(), 1);
        assert_eq!(g2.interrupted()[0].cursor_id, cur_id);
        // The interrupted cursor is usable again.
        assert!(g2.cursor_mut(cur_id).unwrap().open_turn().is_ok());
    }

    /// 旧格式节点（无 tools 字段）header 行直读：缺字段读成空数组。
    #[test]
    fn legacy_header_without_tools_deserializes() {
        let input_json = serde_json::json!({
            "id": Ulid::new().to_string(),
            "kind": "input",
            "actor": "human",
            "created_at": 0,
            "preview": "hi",
            "text": "hi",
        });
        let node: Meta = serde_json::from_value(input_json).unwrap();
        let input = node.input().unwrap();
        assert!(input.kind.tools.is_empty());
        assert_eq!(input.kind.text, "hi");
        assert_eq!(input.kind.parent, None);

        let turn_json = serde_json::json!({
            "id": Ulid::new().to_string(),
            "kind": "turn",
            "parent": Ulid::new().to_string(),
            "outcome": "completed",
            "actor": "agent",
            "model": "m",
            "created_at": 0,
            "preview": "p",
        });
        let node: Meta = serde_json::from_value(turn_json).unwrap();
        let turn = node.turn().unwrap();
        assert!(turn.kind.tools.is_empty());
        assert_eq!(turn.kind.outcome, Outcome::Completed);
    }

    /// header 序列化 = 信封 + kind tag + meta 平铺（边字段随 meta 平铺）；
    /// 正文永不进 header，空 tools 不落盘（skip 生效）。
    #[test]
    fn header_serialization_shape() {
        let mut input = input_node("hi", None);
        input.kind.tools = vec!["bash".into()];
        let v = Meta::from(input.clone()).header_value();
        assert_eq!(v["kind"], "input");
        assert_eq!(v["text"], "hi");
        assert_eq!(v["actor"], "human");
        assert_eq!(v["tools"], serde_json::json!(["bash"]));
        assert!(v.get("data").is_none());
        assert!(v.get("parent").is_none()); // 根 input 无 parent

        let i = input_node("p", None);
        let turn = turn_node(i.id, Outcome::Completed);
        let v = Meta::from(turn.clone()).header_value();
        assert_eq!(v["kind"], "turn");
        assert_eq!(v["outcome"], "completed");
        assert_eq!(v["model"], "deepseek-v4-pro");
        assert_eq!(v["parent"], serde_json::json!(i.id.raw().to_string()));
        assert!(v.get("steps").is_none(), "正文永不进 header");

        // 空 tools 的 Input：tools 键不出现。
        let bare = input_node("bare", None);
        let v = Meta::from(bare).header_value();
        assert!(v.get("tools").is_none());
    }

    /// 旧 journal Context 行（created_by + context_refs + actor）直读：
    /// 别名生效、多余字段忽略，distilled_from / sources 就位。
    #[test]
    fn legacy_context_header_reads_with_alias() {
        let src = Ulid::new();
        let refs = vec![Ulid::new()];
        let ctx_json = serde_json::json!({
            "id": Ulid::new().to_string(),
            "kind": "context",
            "actor": "",
            "created_by": src.to_string(),
            "context_refs": refs.iter().map(Ulid::to_string).collect::<Vec<_>>(),
            "model": "m",
            "created_at": 0,
            "preview": "p",
        });
        let node: Meta = serde_json::from_value(ctx_json).unwrap();
        let ctx = node.context().unwrap();
        assert_eq!(ctx.kind.distilled_from.map(|d| d.raw()), Some(src));
        assert_eq!(ctx.kind.sources, refs);
    }

    /// 新写入的 header（sources + distilled_from）直读回去一致。
    #[test]
    fn context_header_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let i1 = input_node("src", None);
        g.commit(i1.clone()).unwrap();
        let t1 = turn_node(i1.id, Outcome::Completed);
        commit_turn(&mut g, t1.clone(), &sample_steps());
        let c = commit_context(&mut g, "body", vec![i1.id.raw()], Some(t1.id));
        let v = Meta::from(c.clone()).header_value();
        assert_eq!(
            v["distilled_from"],
            serde_json::json!(t1.id.raw().to_string())
        );
        // 落盘/wire 键名沿用 context_refs（UI 与旧 journal 兼容）。
        assert_eq!(
            v["context_refs"],
            serde_json::json!([i1.id.raw().to_string()])
        );
        let back: Meta = serde_json::from_value(v).unwrap();
        assert_eq!(back.context().unwrap().kind.distilled_from, Some(t1.id));
    }

    /// Input 正文内联进 journal header：重开后只读 journal 即可重建完整正文。
    #[test]
    fn input_rebuilds_from_journal_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("g");
        let id = {
            let mut g = Graph::open(&root).unwrap();
            let mut n = input_node("a full input body, not just a preview", None);
            n.kind.tools = vec!["bash".into()];
            g.commit(n.clone()).unwrap();
            n.id
        };
        let g = Graph::open(&root).unwrap();
        let node = g.meta(id.raw()).unwrap().input().unwrap();
        assert_eq!(node.kind.text, "a full input body, not just a preview");
        assert_eq!(node.kind.actor, "human");
        assert_eq!(node.kind.tools, vec!["bash".to_string()]);
        // meta 带全量 text，preview 仍是 80 字符截断。
        assert!(node.preview.chars().count() <= 81);
        // 无正文文件。
        assert!(!root.join("nodes").exists());
    }

    /// Turn 的两条落盘路径读回一致：直接提交（entry 逐行 append，无 Init）
    /// vs open_turn 的 sink 路径（含 Init 锚点）。
    #[test]
    fn turn_commit_paths_read_back_identical() {
        let steps = vec![
            Step::LlmCall {
                response_text: String::new(),
                tool_calls: vec![crate::message::CoreToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    args: serde_json::json!({"command": "ls"}),
                }],
                reasoning: None,
                usage: Usage::default(),
                provider_data: None,
            },
            Step::ToolExec {
                call_id: "c1".into(),
                name: "bash".into(),
                args: serde_json::json!({"command": "ls"}),
                output: "file.txt".into(),
                duration_ms: 5,
            },
            Step::LlmCall {
                response_text: "done".into(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage::default(),
                provider_data: None,
            },
        ];

        // (a) 直接 commit：entry + 逐行 append（无 Init 行）。
        let dir_a = tempfile::tempdir().unwrap();
        let mut ga = Graph::open(dir_a.path().join("g")).unwrap();
        let input = input_node("hi", None);
        ga.commit(input.clone()).unwrap();
        let turn_a = turn_node(input.id, Outcome::Completed);
        commit_turn(&mut ga, turn_a.clone(), &steps);

        // (b) sink 路径：open_turn 注册空条目，append Init 锚点 + 逐行 steps。
        let dir_b = tempfile::tempdir().unwrap();
        let root_b = dir_b.path().join("g");
        let mut gb = Graph::open(&root_b).unwrap();
        let cur = gb.create_cursor("agent", vec![]);
        let input = input_node("hi", None);
        gb.commit(input.clone()).unwrap();
        let mut cview = gb.cursor_mut(cur.id).unwrap();
        cview.move_to(input.id.raw()).unwrap();
        let turn = cview.open_turn().unwrap();
        let entry = turn.entry();
        let init = vec![
            CoreMessage::System {
                content: "sys".into(),
            },
            CoreMessage::User {
                content: "hi".into(),
            },
        ];
        entry.append(TurnLine::Init { request: init.clone() }).unwrap();
        for step in &steps {
            entry.append(TurnLine::from(step.clone())).unwrap();
        }
        let node_id = turn.handle.node_id;
        let turn_b = Turn::node(
            node_id,
            input.id,
            Outcome::Completed,
            "agent",
            "deepseek-v4-pro",
            Usage::default(),
            vec![],
            &steps,
        );
        // ticket 收尾：header 落账 + turn_finished + 游标推进一次到位。
        gb.commit_turn(turn, turn_b).unwrap();

        // 两条路径重建出的 steps 一致（Init 不进 steps）。
        let ga2 = Graph::open(dir_a.path().join("g")).unwrap();
        let gb2 = Graph::open(&root_b).unwrap();
        let steps_a = ga2.data().entry(turn_a.id).unwrap().cloned().unwrap().steps;
        let steps_b = gb2
            .data()
            .entry(node_id)
            .unwrap()
            .cloned()
            .unwrap()
            .steps;
        assert_eq!(steps_a, steps);
        assert_eq!(steps_b, steps);

        // Init 锚点：sink 路径有、直接 commit 路径无。
        assert_eq!(gb2.turn_init(node_id.raw()).unwrap(), Some(init));
        assert_eq!(ga2.turn_init(turn_a.id.raw()).unwrap(), None);
    }

    /// 正文读取容忍截断的尾行（崩溃在 append 中途），中间坏行报错。
    #[test]
    fn turn_body_tolerates_truncated_tail() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("g");
        std::fs::create_dir_all(root.join("turns")).unwrap();
        let store = DataStore::new(root.clone());
        let id: NodeId<Turn> = store.allocate();
        let line = TurnLine::ToolExec {
            call_id: "c1".into(),
            name: "bash".into(),
            args: serde_json::json!({}),
            output: "ok".into(),
            duration_ms: 1,
        };
        let entry = store.entry(id).unwrap();
        entry.append(line.clone()).unwrap();
        // 追加半行撕裂字节。
        use std::io::Write;
        let path = TurnData::path(root.as_path(), id.raw());
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"{\"type\":\"tool_exec\",\"call_id\":\"c2").unwrap();
        drop(f);
        // 新条目（换进程 = 新 DataStore）加载时容忍撕裂尾行。
        let store2 = DataStore::new(root.clone());
        let data = store2.entry(id).unwrap().cloned().unwrap();
        assert_eq!(data.steps, vec![line.into_step().unwrap()]);
    }

    /// append 的临界区性质：文件写失败 → 内存不动，两侧账本一致。
    #[cfg(unix)]
    #[test]
    fn failed_append_leaves_both_ledgers_untouched() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("g");
        std::fs::create_dir_all(root.join("turns")).unwrap();
        let store = DataStore::new(root.clone());
        let id: NodeId<Turn> = store.allocate();
        let entry = store.entry(id).unwrap();

        // 把正文文件预置为只读：append 打开即失败。
        let path = TurnData::path(&root, id.raw());
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let line = TurnLine::LlmCall {
            response_text: "x".into(),
            tool_calls: vec![],
            reasoning: None,
            usage: Usage::default(),
            provider_data: None,
        };
        assert!(entry.append(line.clone()).is_err());
        // 内存一侧没有被污染。
        assert!(entry.cloned().unwrap().steps.is_empty());
        // 文件一侧也没有（只读的预置内容仍是空）。
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");

        // 恢复可写后 append 正常工作。
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        entry.append(line.clone()).unwrap();
        assert_eq!(entry.cloned().unwrap().steps.len(), 1);
    }

    /// commit 一个数据面没有条目的 Turn/Context：拒绝（等价于旧的"不许
    /// 提交未加载正文"）。
    #[test]
    fn committing_without_data_entry_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let input = input_node("root", None);
        g.commit(input.clone()).unwrap();

        let turn = turn_node(input.id, Outcome::Completed);
        assert!(matches!(
            g.commit(turn),
            Err(Error::DataNotLoaded(_))
        ));

        let ctx = Context::node(
            NodeId::new(),
            vec![],
            None,
            "m",
            "body",
        );
        assert!(matches!(
            g.commit(ctx),
            Err(Error::DataNotLoaded(_))
        ));
    }

    /// Context 正文外部化：create 写 contexts/<id>.md，重开后经数据面
    /// 惰性加载重建一致；distilled_from 经 header 存活重建。
    #[test]
    fn context_body_lives_in_contexts_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("g");
        let id = {
            let mut g = Graph::open(&root).unwrap();
            let i = input_node("src", None);
            g.commit(i.clone()).unwrap();
            let t = turn_node(i.id, Outcome::Completed);
            commit_turn(&mut g, t.clone(), &sample_steps());
            let c = commit_context(&mut g, "distilled body", vec![i.id.raw()], Some(t.id));
            c.id
        };
        assert_eq!(
            std::fs::read_to_string(root.join("contexts").join(format!("{id}.md"))).unwrap(),
            "distilled body"
        );
        let g = Graph::open(&root).unwrap();
        let meta = g.meta(id.raw()).unwrap().context().unwrap();
        let body = g.data().entry(id).unwrap().cloned().unwrap().body;
        assert_eq!(body, "distilled body");
        assert_eq!(meta.kind.model, "deepseek-v4-pro");
    }

    /// 旧布局（nodes/<ulid>.json 单文件 + LlmCall 内嵌 request）在 open 时
    /// 自动迁移：正文转写新布局、journal header 重生成、旧文件进 .trash。
    #[test]
    fn migrates_legacy_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("g");
        let nodes = root.join("nodes");
        std::fs::create_dir_all(&nodes).unwrap();

        let input_id = Ulid::new();
        let turn_id = Ulid::new();
        let ctx_id = Ulid::new();
        std::fs::write(
            nodes.join(format!("{input_id}.json")),
            serde_json::to_string_pretty(&serde_json::json!({
                "id": input_id.to_string(),
                "created_at": 1,
                "kind": {"type": "input", "text": "legacy hello", "actor": "human", "tools": ["bash"]},
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            nodes.join(format!("{turn_id}.json")),
            serde_json::to_string_pretty(&serde_json::json!({
                "id": turn_id.to_string(),
                "parent": input_id.to_string(),
                "created_at": 2,
                "kind": {
                    "type": "turn",
                    "steps": [
                        {"type": "llm_call", "request": [
                            {"role": "system", "content": "sys"},
                            {"role": "user", "content": "legacy hello"}
                        ], "response_text": "", "tool_calls": [
                            {"id": "c1", "name": "bash", "args": {"command": "ls"}}
                        ], "usage": {"input_tokens": 7}},
                        {"type": "tool_exec", "call_id": "c1", "name": "bash",
                         "args": {"command": "ls"}, "output": "f.txt", "duration_ms": 3},
                        {"type": "llm_call", "request": [
                            {"role": "system", "content": "sys"},
                            {"role": "user", "content": "legacy hello"},
                            {"role": "assistant", "content": "", "tool_calls": [
                                {"id": "c1", "name": "bash", "args": {"command": "ls"}}
                            ]},
                            {"role": "tool_result", "call_id": "c1", "name": "bash", "output": "f.txt"}
                        ], "response_text": "legacy done", "usage": {"input_tokens": 9}}
                    ],
                    "outcome": "completed", "actor": "agent", "model": "m",
                    "usage": {"input_tokens": 16}, "tools": ["bash"]
                },
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            nodes.join(format!("{ctx_id}.json")),
            serde_json::to_string_pretty(&serde_json::json!({
                "id": ctx_id.to_string(),
                "context_refs": [turn_id.to_string()],
                "created_at": 3,
                "kind": {"type": "context", "body": "legacy ctx", "created_by": turn_id.to_string(), "model": "m"},
            }))
            .unwrap(),
        )
        .unwrap();
        // 旧 journal：三个 node_committed（旧 meta，无 text）+ 一条 cursor 事件。
        let cursor_id = crate::id::CursorId::new();
        let mut journal = String::new();
        for (id, parent, kind, extra) in [
            (input_id, None, "input", serde_json::json!({})),
            (turn_id, Some(input_id), "turn", serde_json::json!({
                "outcome": "completed", "model": "m",
                "usage": {"input_tokens": 16}, "tools": ["bash"]
            })),
            (ctx_id, None, "context", serde_json::json!({
                "context_refs": [turn_id.to_string()]
            })),
        ] {
            let mut meta = serde_json::json!({
                "id": id.to_string(),
                "kind": kind,
                "actor": "a",
                "created_at": 1,
                "preview": "p",
            });
            if let Some(p) = parent {
                meta["parent"] = serde_json::json!(p.to_string());
            }
            for (k, v) in extra.as_object().unwrap() {
                meta[k] = v.clone();
            }
            journal.push_str(&serde_json::json!({"event": "node_committed", "meta": meta}).to_string());
            journal.push('\n');
        }
        journal.push_str(
            &serde_json::json!({
                "event": "cursor_created",
                "cursor": {"id": cursor_id.to_string(), "node": null, "actor": "human", "capabilities": [], "created_at": 1},
            })
            .to_string(),
        );
        journal.push('\n');
        journal.push_str(
            &serde_json::json!({
                "event": "cursor_moved", "cursor_id": cursor_id.to_string(), "node": turn_id.to_string(),
            })
            .to_string(),
        );
        journal.push('\n');
        std::fs::write(root.join("journal.jsonl"), journal).unwrap();

        // open 触发迁移。
        let g = Graph::open(&root).unwrap();

        // 旧布局进了 .trash，新布局就位。
        assert!(!root.join("nodes").exists());
        let trash: Vec<_> = std::fs::read_dir(root.join(".trash"))
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        assert_eq!(trash.len(), 1);
        assert!(trash[0].file_name().to_string_lossy().starts_with("migration-"));
        assert!(trash[0].path().join("nodes").is_dir());
        assert!(trash[0].path().join("journal.jsonl").exists());

        // Input：text 内联进 header，重建出完整正文。
        let meta = g.meta(input_id).unwrap().input().unwrap();
        assert_eq!(meta.kind.text, "legacy hello");
        assert_eq!(meta.kind.tools, vec!["bash".to_string()]);

        // Turn：turns/<id>.jsonl = init 锚点（首份 request）+ 逐行 steps
        // （request 已丢）；重建的 steps 与旧数据一致。
        let init = g.turn_init(turn_id).unwrap().expect("init anchor");
        assert_eq!(init.len(), 2);
        assert!(matches!(&init[0], CoreMessage::System { content } if content == "sys"));
        let turn = g.meta(turn_id).unwrap().turn().unwrap();
        assert_eq!(turn.kind.outcome, Outcome::Completed);
        assert_eq!(turn.kind.tools, vec!["bash".to_string()]);
        assert_eq!(turn.kind.parent, NodeId::from_raw(input_id));
        let data = g.data().entry(turn.id).unwrap();
        let steps = data.cloned().unwrap().steps;
        assert_eq!(steps.len(), 3);
        let Step::LlmCall {
            response_text,
            tool_calls,
            usage: u0,
            ..
        } = &steps[0]
        else {
            panic!("expected llm call");
        };
        assert_eq!(response_text, "");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(u0.input_tokens, 7);
        assert!(matches!(&steps[1], Step::ToolExec { output, .. } if output == "f.txt"));
        assert!(matches!(&steps[2], Step::LlmCall { response_text, .. } if response_text == "legacy done"));
        assert_eq!(turn.kind.usage.input_tokens, 16);

        // Context：正文落 contexts/<id>.md，重建一致；旧 created_by 读成
        // distilled_from，旧 context_refs 读成 sources。
        assert_eq!(
            std::fs::read_to_string(root.join("contexts").join(format!("{ctx_id}.md"))).unwrap(),
            "legacy ctx"
        );
        let ctx = g.meta(ctx_id).unwrap().context().unwrap();
        assert_eq!(ctx.kind.distilled_from, Some(NodeId::from_raw(turn_id)));
        assert_eq!(ctx.kind.sources, vec![turn_id]);
        let body = g.data().entry(ctx.id).unwrap().cloned().unwrap().body;
        assert_eq!(body, "legacy ctx");

        // cursor/turn 事件原样保留。
        assert_eq!(g.cursors.get(cursor_id).unwrap().node, Some(turn_id));
    }
}
