use std::collections::HashMap;

use serde_json::json;

use crate::cursor::{CursorRegistry, TurnHandle};
use crate::error::{Error, Result};
use crate::id::{CursorId, NodeId};
use crate::journal::JournalEvent;
use crate::message::CoreMessage;
use crate::node::{AnyNode, Outcome, Turn};
use crate::store::Store;

/// The graph engine's state. All committed nodes live in one map: journal
/// replay fills them as headers (`data: None`), bodies load lazily into the
/// same entry (the index entry *is* the body cache). The daemon owns the
/// single writer; all mutations go through these methods and are journaled.
pub struct Graph {
    store: Store,
    nodes: HashMap<NodeId, AnyNode>,
    children: HashMap<NodeId, Vec<NodeId>>,
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
        self.interrupted = self.cursors.in_flight_all().cloned().collect();
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

    // ---- data plane ----

    /// Validate and commit an immutable node: finalize the body (idempotent,
    /// per-kind), journal the header, update the index. This is the only way
    /// nodes come into existence; committing a header without a loaded body
    /// is rejected.
    pub fn commit(&mut self, node: impl Into<AnyNode>) -> Result<()> {
        let node = node.into();
        let id = node.id();
        if self.nodes.contains_key(&id) {
            return Err(Error::NodeAlreadyCommitted(id));
        }
        if let Some(parent) = node.parent()
            && !self.nodes.contains_key(&parent)
        {
            return Err(Error::ParentNotCommitted(parent));
        }
        let structural = node.is_structural();
        if !structural && node.parent().is_some() {
            return Err(Error::ContextNodeHasParent(id));
        }
        for r in node.context_refs() {
            let meta = self
                .nodes
                .get(r)
                .ok_or(Error::ContextRefNotCommitted(*r))?;
            // Context boundary: structural nodes may only pull in distilled
            // material (context nodes). Context nodes may trace any node.
            if structural && !meta.is_context() {
                return Err(Error::ContextRefNotContextNode(*r));
            }
        }

        // 正文收尾按 kind 分派（幂等）：Input no-op；Context 原子写；
        // Turn 为 turns/<id>.jsonl——sink 已增量写入（文件存在）时不重写，
        // 直接提交路径（无 sink，如测试）把 steps 整体转出（无 Init 行）。
        node.write_data(&self.store)?;

        if let Some(parent) = node.parent() {
            self.children.entry(parent).or_default().push(id);
        }
        self.nodes.insert(id, node);
        // journal 只落 header（data 永不进 journal）；从 map 序列化即得
        // header 行，避免为落盘深拷贝一次正文。
        self.store.append_journal_value(&json!({
            "event": "node_committed",
            "meta": self.nodes[&id].header_value(),
        }))?;
        Ok(())
    }

    /// 节点头（信封 + meta，`data` 可能未加载）。结构检查与轻量读用。
    pub fn meta(&self, id: NodeId) -> Option<&AnyNode> {
        self.nodes.get(&id)
    }

    pub fn metas(&self) -> Vec<&AnyNode> {
        let mut v: Vec<_> = self.nodes.values().collect();
        v.sort_by_key(|n| (n.created_at(), n.id()));
        v
    }

    /// Load a node body (cached): Input is always loaded (meta inline),
    /// Turn from `turns/<id>.jsonl` (`Init` lines are anchors, not steps),
    /// Context from `contexts/<id>.md`. The index entry is the cache.
    pub fn node(&mut self, id: NodeId) -> Result<&AnyNode> {
        if !self.nodes.contains_key(&id) {
            return Err(Error::NodeNotFound(id));
        }
        let node = self.nodes.get_mut(&id).expect("checked above");
        if !node.loaded() {
            node.read_data(&self.store)?;
        }
        Ok(self.nodes.get(&id).expect("checked above"))
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        self.children.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Walk parent pointers from `tip` to the root. Returned root → tip.
    pub fn chain_to_root(&self, tip: NodeId) -> Result<Vec<NodeId>> {
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
    pub fn move_cursor(&mut self, cursor_id: CursorId, node: NodeId) -> Result<()> {
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
    /// conversation tree).
    pub fn detach_cursor(&mut self, cursor_id: CursorId) -> Result<()> {
        self.cursors.detach(cursor_id)?;
        self.store
            .append_journal(&JournalEvent::CursorDetached { cursor_id })?;
        Ok(())
    }

    /// Begin a turn on a cursor: enforces "at most one in-flight turn per
    /// cursor" and pre-allocates the landing node id.
    pub fn begin_turn(&mut self, cursor_id: CursorId) -> Result<TurnHandle> {
        let handle = self.cursors.begin_turn(cursor_id, NodeId::new())?;
        self.store.append_journal(&JournalEvent::TurnStarted {
            cursor_id,
            node_id: handle.node_id,
            started_at: handle.started_at,
        })?;
        Ok(handle)
    }

    pub fn finish_turn(&mut self, cursor_id: CursorId, outcome: Outcome) -> Result<TurnHandle> {
        let handle = self.cursors.finish_turn(cursor_id)?;
        self.store.append_journal(&JournalEvent::TurnFinished {
            cursor_id,
            node_id: handle.node_id,
            outcome,
        })?;
        Ok(handle)
    }

    // ---- chain loading ----

    /// Resolve the chain + referenced material for `tip`: backtrack to root,
    /// load the chain nodes and their `context_refs` targets (bodies all
    /// loaded — the projection layer may rely on `data: Some`).
    pub fn load_chain(&mut self, tip: NodeId) -> Result<(Vec<AnyNode>, HashMap<NodeId, AnyNode>)> {
        let chain_ids = self.chain_to_root(tip)?;
        let mut chain = Vec::with_capacity(chain_ids.len());
        let mut materials = HashMap::new();
        for id in chain_ids {
            let node = self.node(id)?.clone();
            for r in node.context_refs() {
                materials.insert(*r, self.node(*r)?.clone());
            }
            chain.push(node);
        }
        Ok((chain, materials))
    }

    /// The turn's init anchor: the first LLM call's full request snapshot
    /// (`None` when the body has no `Init` line, e.g. direct-commit paths).
    pub fn turn_init(&self, id: NodeId) -> Result<Option<Vec<CoreMessage>>> {
        Turn::init_anchor(&self.store, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{Context, Input, Node, TurnData, Usage, now_millis};
    use crate::node::{Step, TurnLine};

    fn input_node(text: &str, parent: Option<NodeId>) -> Node<Input> {
        let mut n = Input::node(NodeId::new(), parent, text, "human", vec![], None);
        n.created_at = now_millis();
        n
    }

    fn turn_node(parent: NodeId, outcome: Outcome) -> Node<Turn> {
        Turn::node(
            NodeId::new(),
            Some(parent),
            vec![],
            None,
            outcome,
            "agent",
            "deepseek-v4-pro",
            Usage::default(),
            vec![],
            TurnData {
                steps: vec![Step::LlmCall {
                    response_text: "reply".into(),
                    tool_calls: vec![],
                    reasoning: None,
                    usage: Usage::default(),
                    provider_data: None,
                }],
            },
        )
    }

    fn context_node(body: &str, sources: Vec<NodeId>, distilled_from: NodeId) -> Node<Context> {
        Context::node(NodeId::new(), body, sources, distilled_from, "deepseek-v4-pro")
    }

    #[test]
    fn commit_and_chain() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let i1 = input_node("hello", None);
        let t1 = turn_node(i1.id, Outcome::Completed);
        let i2 = input_node("next", Some(t1.id));
        g.commit(i1.clone()).unwrap();
        g.commit(t1.clone()).unwrap();
        g.commit(i2.clone()).unwrap();

        assert_eq!(g.chain_to_root(i2.id).unwrap(), vec![i1.id, t1.id, i2.id]);
        assert_eq!(g.children(i1.id), &[t1.id]);
        assert_eq!(g.children(t1.id), &[i2.id]);
    }

    #[test]
    fn context_tokens_from_last_llm_call() {
        let mut steps = vec![Step::LlmCall {
            response_text: "reply".into(),
            tool_calls: vec![],
            reasoning: None,
            usage: Usage::default(),
            provider_data: None,
        }];
        steps.push(Step::LlmCall {
            response_text: "final".into(),
            tool_calls: vec![],
            reasoning: None,
            usage: Usage {
                input_tokens: 12_345,
                ..Usage::default()
            },
            provider_data: None,
        });
        let t = Turn::node(
            NodeId::new(),
            None,
            vec![],
            None,
            Outcome::Completed,
            "agent",
            "m",
            Usage::default(),
            vec![],
            TurnData { steps },
        );
        assert_eq!(t.kind.context_tokens, Some(12_345));
        // 无 LLM 调用的回合没有上下文量
        let empty = Turn::node(
            NodeId::new(),
            None,
            vec![],
            None,
            Outcome::Failed,
            "agent",
            "m",
            Usage::default(),
            vec![],
            TurnData { steps: vec![] },
        );
        assert_eq!(empty.kind.context_tokens, None);
        // 空正文也是 Some（零 LLM 调用失败轮），不是「未加载」
        assert!(empty.data.is_some());
    }

    #[test]
    fn fork_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let i1 = input_node("root", None);
        g.commit(i1.clone()).unwrap();
        let a = input_node("branch a", Some(i1.id));
        let b = input_node("branch b", Some(i1.id));
        g.commit(a.clone()).unwrap();
        g.commit(b.clone()).unwrap();
        assert_eq!(g.children(i1.id), &[a.id, b.id]);
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

        // Structural node referencing a non-context node: rejected.
        let mut bad = input_node("bad", Some(i1.id));
        bad.context_refs = vec![i1.id];
        assert!(matches!(
            g.commit(bad),
            Err(Error::ContextRefNotContextNode(_))
        ));

        // Context node with provenance refs to any committed node: ok.
        let c = context_node("summary", vec![i1.id], i1.id);
        g.commit(c.clone()).unwrap();

        // Structural node referencing the context node: ok.
        let mut ok = input_node("with material", Some(i1.id));
        ok.context_refs = vec![c.id];
        g.commit(ok).unwrap();

        // Context node with a parent: rejected.
        let mut badc = context_node("orphan?", vec![], i1.id);
        badc.parent = Some(i1.id);
        assert!(matches!(
            g.commit(badc),
            Err(Error::ContextNodeHasParent(_))
        ));

        // Cursor cannot land on a context node.
        let cur = g.create_cursor("human", vec![]);
        g.commit(input_node("tip", None)).unwrap();
        assert!(matches!(
            g.move_cursor(cur.id, c.id),
            Err(Error::CursorOnContextNode(_))
        ));
    }

    #[test]
    fn one_in_flight_per_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let cur = g.create_cursor("human", vec![]);
        let h = g.begin_turn(cur.id).unwrap();
        assert!(matches!(
            g.begin_turn(cur.id),
            Err(Error::CursorBusy(_))
        ));
        g.finish_turn(cur.id, Outcome::Cancelled).unwrap();
        let h2 = g.begin_turn(cur.id).unwrap();
        assert_ne!(h.node_id, h2.node_id);
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
            g.move_cursor(cur.id, i1.id).unwrap();
            (cur.id, i1.id)
        };

        let mut g2 = Graph::open(&root).unwrap();
        assert_eq!(g2.cursors.get(cur_id).unwrap().node, Some(tip_id));
        // Input 正文内联进 header：重放即已「加载」，只读 journal 就有全量正文。
        assert!(g2.meta(tip_id).unwrap().loaded());
        let loaded = g2.node(tip_id).unwrap().input().unwrap();
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
            g.move_cursor(cur.id, i1.id).unwrap();
            g.detach_cursor(cur.id).unwrap();
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
            g.begin_turn(cur.id).unwrap();
            cur.id // no finish_turn: daemon "died"
        };
        let g2 = Graph::open(&root).unwrap();
        assert_eq!(g2.interrupted().len(), 1);
        assert_eq!(g2.interrupted()[0].cursor_id, cur_id);
        // The interrupted cursor is usable again.
        let mut g2 = g2;
        assert!(g2.begin_turn(cur_id).is_ok());
    }

    /// 旧格式节点（无 tools 字段）header 行直读：缺字段读成空数组。
    #[test]
    fn legacy_header_without_tools_deserializes() {
        let input_json = serde_json::json!({
            "id": NodeId::new().to_string(),
            "kind": "input",
            "actor": "human",
            "created_at": 0,
            "preview": "hi",
            "text": "hi",
        });
        let node: AnyNode = serde_json::from_value(input_json).unwrap();
        let input = node.input().unwrap();
        assert!(input.kind.tools.is_empty());
        assert_eq!(input.kind.text, "hi");

        let turn_json = serde_json::json!({
            "id": NodeId::new().to_string(),
            "kind": "turn",
            "outcome": "completed",
            "actor": "agent",
            "model": "m",
            "created_at": 0,
            "preview": "p",
        });
        let node: AnyNode = serde_json::from_value(turn_json).unwrap();
        let turn = node.turn().unwrap();
        assert!(turn.kind.tools.is_empty());
        assert_eq!(turn.kind.outcome, Outcome::Completed);
        assert!(turn.data.is_none());
    }

    /// header 序列化 = 信封 + kind tag + meta 平铺；data（steps）永不进
    /// journal，空 tools 不落盘（skip 生效）。
    #[test]
    fn header_serialization_shape() {
        let mut input = input_node("hi", None);
        input.kind.tools = vec!["bash".into()];
        let v = AnyNode::from(input.clone()).header_value();
        assert_eq!(v["kind"], "input");
        assert_eq!(v["text"], "hi");
        assert_eq!(v["actor"], "human");
        assert_eq!(v["tools"], serde_json::json!(["bash"]));
        assert!(v.get("data").is_none());

        let turn = turn_node(NodeId::new(), Outcome::Completed);
        let v = AnyNode::from(turn.clone()).header_value();
        assert_eq!(v["kind"], "turn");
        assert_eq!(v["outcome"], "completed");
        assert_eq!(v["model"], "deepseek-v4-pro");
        assert!(v.get("steps").is_none(), "正文永不进 header");
        assert!(v.get("parent").is_some()); // parent 有值

        // 空 tools 的 Input：tools 键不出现。
        let bare = input_node("bare", None);
        let v = AnyNode::from(bare).header_value();
        assert!(v.get("tools").is_none());
    }

    /// 旧 journal Context 行（created_by + actor）直读：别名生效、多余
    /// 字段忽略，distilled_from 就位。
    #[test]
    fn legacy_context_header_reads_with_alias() {
        let src = NodeId::new();
        let ctx_json = serde_json::json!({
            "id": NodeId::new().to_string(),
            "kind": "context",
            "actor": "",
            "created_by": src.to_string(),
            "model": "m",
            "created_at": 0,
            "preview": "p",
        });
        let node: AnyNode = serde_json::from_value(ctx_json).unwrap();
        let ctx = node.context().unwrap();
        assert_eq!(ctx.kind.distilled_from, src);
        // 信封 created_by 是单义的 spawn 溯源：Context 恒 None。
        assert!(node.created_by().is_none());
    }

    /// 新写入的 header（distilled_from）直读回去一致。
    #[test]
    fn context_header_round_trips() {
        let src = input_node("src", None);
        let c = context_node("body", vec![src.id], src.id);
        let v = AnyNode::from(c.clone()).header_value();
        assert_eq!(v["distilled_from"], serde_json::json!(src.id.to_string()));
        let back: AnyNode = serde_json::from_value(v).unwrap();
        assert_eq!(back.context().unwrap().kind.distilled_from, src.id);
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
        let mut g = Graph::open(&root).unwrap();
        let node = g.node(id).unwrap();
        let input = node.input().unwrap();
        assert_eq!(input.kind.text, "a full input body, not just a preview");
        assert_eq!(input.kind.actor, "human");
        assert_eq!(input.kind.tools, vec!["bash".to_string()]);
        // meta 带全量 text，preview 仍是 80 字符截断。
        let meta = g.meta(id).unwrap().input().unwrap();
        assert_eq!(meta.kind.text, "a full input body, not just a preview");
        assert!(meta.preview.chars().count() <= 81);
        // 无正文文件。
        assert!(!root.join("nodes").exists());
    }

    /// Turn 的两条落盘路径读回一致：直接 commit（无 sink，steps 整体
    /// 转出）vs sink 增量 append 后 commit（不重写正文）。
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

        // (a) 直接 commit：commit 把 steps 整体写入 turns/<id>.jsonl。
        let dir_a = tempfile::tempdir().unwrap();
        let mut ga = Graph::open(dir_a.path().join("g")).unwrap();
        let input = input_node("hi", None);
        ga.commit(input.clone()).unwrap();
        let turn_a = Turn::node(
            NodeId::new(),
            Some(input.id),
            vec![],
            None,
            Outcome::Completed,
            "agent",
            "deepseek-v4-pro",
            Usage::default(),
            vec![],
            TurnData { steps: steps.clone() },
        );
        ga.commit(turn_a.clone()).unwrap();

        // (b) sink 路径：先增量 append（含 Init 锚点），commit 时正文已存在
        // → 只追加 journal header，不重写。
        let dir_b = tempfile::tempdir().unwrap();
        let root_b = dir_b.path().join("g");
        let mut gb = Graph::open(&root_b).unwrap();
        let input = input_node("hi", None);
        gb.commit(input.clone()).unwrap();
        let turn_b_id = NodeId::new();
        let turn_b = Turn::node(
            turn_b_id,
            Some(input.id),
            vec![],
            None,
            Outcome::Completed,
            "agent",
            "deepseek-v4-pro",
            Usage::default(),
            vec![],
            TurnData { steps: steps.clone() },
        );
        let init = vec![
            CoreMessage::System {
                content: "sys".into(),
            },
            CoreMessage::User {
                content: "hi".into(),
            },
        ];
        gb.store()
            .append_turn_line(turn_b_id, &TurnLine::Init { request: init.clone() })
            .unwrap();
        for step in &steps {
            gb.store()
                .append_turn_line(turn_b_id, &TurnLine::from(step.clone()))
                .unwrap();
        }
        gb.commit(turn_b.clone()).unwrap();

        // 两条路径重建出的 steps 一致（Init 不进 steps）。
        let mut ga = Graph::open(dir_a.path().join("g")).unwrap();
        let mut gb = Graph::open(&root_b).unwrap();
        assert_eq!(
            ga.node(turn_a.id).unwrap().turn_data().unwrap().steps,
            steps
        );
        assert_eq!(
            gb.node(turn_b_id).unwrap().turn_data().unwrap().steps,
            steps
        );

        // Init 锚点：sink 路径有、直接 commit 路径无。
        assert_eq!(gb.turn_init(turn_b_id).unwrap(), Some(init));
        assert_eq!(ga.turn_init(turn_a.id).unwrap(), None);
    }

    /// read_turn_lines 与 read_journal 同策略：容忍截断的尾行（崩溃在
    /// append 中途），但中间坏行报错。
    #[test]
    fn read_turn_lines_tolerates_truncated_tail() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("g")).unwrap();
        let id = NodeId::new();
        let line = TurnLine::ToolExec {
            call_id: "c1".into(),
            name: "bash".into(),
            args: serde_json::json!({}),
            output: "ok".into(),
            duration_ms: 1,
        };
        store.append_turn_line(id, &line).unwrap();
        // 追加半行撕裂字节。
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(store.root().join("turns").join(format!("{id}.jsonl")))
            .unwrap();
        f.write_all(b"{\"type\":\"tool_exec\",\"call_id\":\"c2")
            .unwrap();
        drop(f);
        assert_eq!(store.read_turn_lines(id).unwrap(), vec![line]);
    }

    /// Context 正文外部化：commit 写 contexts/<id>.md，重开后重建一致；
    /// distilled_from 经 header 存活重建。
    #[test]
    fn context_body_lives_in_contexts_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("g");
        let id = {
            let mut g = Graph::open(&root).unwrap();
            let i = input_node("src", None);
            g.commit(i.clone()).unwrap();
            let c = context_node("distilled body", vec![i.id], i.id);
            g.commit(c.clone()).unwrap();
            c.id
        };
        assert_eq!(
            std::fs::read_to_string(root.join("contexts").join(format!("{id}.md"))).unwrap(),
            "distilled body"
        );
        let mut g = Graph::open(&root).unwrap();
        let src = g.meta(id).unwrap().context_refs()[0];
        let node = g.node(id).unwrap().context().unwrap();
        assert_eq!(node.data.as_ref().unwrap().body, "distilled body");
        assert_eq!(node.kind.model, "deepseek-v4-pro");
        assert_eq!(node.kind.distilled_from, src);
    }

    /// 旧布局（nodes/<ulid>.json 单文件 + LlmCall 内嵌 request）在 open 时
    /// 自动迁移：正文转写新布局、journal header 重生成、旧文件进 .trash。
    #[test]
    fn migrates_legacy_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("g");
        let nodes = root.join("nodes");
        std::fs::create_dir_all(&nodes).unwrap();

        let input_id = NodeId::new();
        let turn_id = NodeId::new();
        let ctx_id = NodeId::new();
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
        let mut g = Graph::open(&root).unwrap();

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
        let turn = g.node(turn_id).unwrap().turn().unwrap();
        assert_eq!(turn.kind.outcome, Outcome::Completed);
        assert_eq!(turn.kind.tools, vec!["bash".to_string()]);
        let steps = &turn.data.as_ref().unwrap().steps;
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
        // distilled_from。
        assert_eq!(
            std::fs::read_to_string(root.join("contexts").join(format!("{ctx_id}.md"))).unwrap(),
            "legacy ctx"
        );
        let ctx = g.node(ctx_id).unwrap().context().unwrap();
        assert_eq!(ctx.data.as_ref().unwrap().body, "legacy ctx");
        assert_eq!(ctx.kind.distilled_from, turn_id);

        // cursor/turn 事件原样保留。
        assert_eq!(g.cursors.get(cursor_id).unwrap().node, Some(turn_id));
    }

    /// commit 一个 header（data: None）被拒绝：未加载的节点不许落盘。
    #[test]
    fn committing_unloaded_body_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let input = input_node("root", None);
        g.commit(input.clone()).unwrap();
        let header = Node::<crate::node::Turn>::header(
            NodeId::new(),
            Some(input.id),
            vec![],
            None,
            now_millis(),
            "p".into(),
            crate::node::Turn {
                outcome: Outcome::Completed,
                actor: "agent".into(),
                model: "m".into(),
                usage: Usage::default(),
                context_tokens: None,
                tools: vec![],
            },
        );
        assert!(matches!(
            g.commit(header),
            Err(Error::DataNotLoaded(_))
        ));
    }
}
