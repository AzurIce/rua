use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::cursor::{CursorRegistry, TurnHandle};
use crate::error::{Error, Result};
use crate::id::{CursorId, NodeId};
use crate::journal::JournalEvent;
use crate::message::CoreMessage;
use crate::node::{Node, NodeKind, NodeKindTag, Outcome};
use crate::store::Store;

/// Slim per-node record kept in memory and in the journal. Bodies are loaded
/// lazily from `nodes/<ulid>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeMeta {
    pub id: NodeId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<NodeId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_refs: Vec<NodeId>,
    pub kind: NodeKindTag,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    pub actor: String,
    pub created_at: u64,
    /// Token usage (turn nodes only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::node::Usage>,
    /// 该回合最后一次 LLM 调用实际吃掉的上下文量（input tokens，
    /// 含缓存部分；turn nodes only）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// 创建者（turn 的 spawn_turn 工具调用产生）；None = 用户/直接操作。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<NodeId>,
    /// 该 turn 使用的模型（turn 节点才有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 工具集：Input 节点 = 展开后的请求列表；Turn 节点 = 该轮有效集。
    /// 空 = 未记录（旧数据）或无工具。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// Short human-readable preview for graph UIs.
    pub preview: String,
}

impl NodeMeta {
    fn of(node: &Node) -> Self {
        let (outcome, actor, usage, context_tokens, model, tools, preview) = match &node.kind {
            NodeKind::Input { text, actor, tools } => {
                (None, actor.clone(), None, None, None, tools.clone(), truncate(text, 80))
            }
            NodeKind::Turn {
                steps,
                outcome,
                actor,
                usage,
                model,
                tools,
                ..
            } => {
                let final_text = steps
                    .iter()
                    .rev()
                    .find_map(|s| match s {
                        crate::node::Step::LlmCall { response_text, .. }
                            if !response_text.is_empty() =>
                        {
                            Some(response_text.clone())
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                // 上下文量 = 最后一次调用的 input tokens（那一刻装配出的
                // 完整上下文）。无 LLM 调用（纯失败回合）时为 None。
                let ctx = steps.iter().rev().find_map(|s| match s {
                    crate::node::Step::LlmCall { usage, .. } if usage.input_tokens > 0 => {
                        Some(usage.input_tokens)
                    }
                    _ => None,
                });
                (
                    Some(*outcome),
                    actor.clone(),
                    Some(*usage),
                    ctx,
                    Some(model.clone()),
                    tools.clone(),
                    truncate(&final_text, 80),
                )
            }
            NodeKind::Context { body, .. } => {
                (None, String::new(), None, None, None, vec![], truncate(body, 80))
            }
        };
        NodeMeta {
            id: node.id,
            parent: node.parent,
            context_refs: node.context_refs.clone(),
            kind: node.kind_tag(),
            outcome,
            actor,
            created_at: node.created_at,
            usage,
            context_tokens,
            created_by: node.created_by,
            model,
            tools,
            preview,
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// The graph engine's state: data-plane index + control-plane registry.
/// The daemon owns the single writer; all mutations go through these methods
/// and are journaled.
pub struct Graph {
    store: Store,
    index: HashMap<NodeId, NodeMeta>,
    children: HashMap<NodeId, Vec<NodeId>>,
    bodies: HashMap<NodeId, Node>,
    pub cursors: CursorRegistry,
    /// Handles left in-flight when the daemon last stopped (journal replay
    /// found `TurnStarted` without `TurnFinished`).
    interrupted: Vec<TurnHandle>,
}

impl Graph {
    /// Open (or create) the graph at `root`, replaying the journal.
    pub fn open(root: impl Into<std::path::PathBuf>) -> Result<Self> {
        let store = Store::new(root)?;
        let events = store.read_journal()?;
        let mut graph = Graph {
            store,
            index: HashMap::new(),
            children: HashMap::new(),
            bodies: HashMap::new(),
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
                    if let Some(parent) = meta.parent {
                        self.children.entry(parent).or_default().push(meta.id);
                    }
                    self.index.insert(meta.id, meta);
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

    /// Validate and commit an immutable node: write body, journal the meta,
    /// update the index. This is the only way nodes come into existence.
    pub fn commit_node(&mut self, node: Node) -> Result<()> {
        if self.index.contains_key(&node.id) {
            return Err(Error::NodeAlreadyCommitted(node.id));
        }
        if let Some(parent) = node.parent
            && !self.index.contains_key(&parent)
        {
            return Err(Error::ParentNotCommitted(parent));
        }
        let structural = node.is_structural();
        if !structural && node.parent.is_some() {
            return Err(Error::ContextNodeHasParent(node.id));
        }
        for r in &node.context_refs {
            let meta = self
                .index
                .get(r)
                .ok_or(Error::ContextRefNotCommitted(*r))?;
            // Context boundary: structural nodes may only pull in distilled
            // material (context nodes). Context nodes may trace any node.
            if structural && meta.kind != NodeKindTag::Context {
                return Err(Error::ContextRefNotContextNode(*r));
            }
        }

        self.store.write_node(&node)?;
        let meta = NodeMeta::of(&node);
        self.store.append_journal(&JournalEvent::NodeCommitted {
            meta: meta.clone(),
        })?;
        if let Some(parent) = meta.parent {
            self.children.entry(parent).or_default().push(meta.id);
        }
        self.bodies.insert(node.id, node);
        self.index.insert(meta.id, meta);
        Ok(())
    }

    pub fn meta(&self, id: NodeId) -> Option<&NodeMeta> {
        self.index.get(&id)
    }

    pub fn metas(&self) -> Vec<&NodeMeta> {
        let mut v: Vec<_> = self.index.values().collect();
        v.sort_by_key(|m| (m.created_at, m.id));
        v
    }

    /// Load a node body (cached).
    pub fn node(&mut self, id: NodeId) -> Result<&Node> {
        if !self.index.contains_key(&id) {
            return Err(Error::NodeNotFound(id));
        }
        if !self.bodies.contains_key(&id) {
            let node = self.store.read_node(id)?;
            self.bodies.insert(id, node);
        }
        Ok(self.bodies.get(&id).unwrap())
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        self.children.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Walk parent pointers from `tip` to the root. Returned root → tip.
    pub fn chain_to_root(&self, tip: NodeId) -> Result<Vec<NodeId>> {
        if !self.index.contains_key(&tip) {
            return Err(Error::NodeNotFound(tip));
        }
        let mut chain = Vec::new();
        let mut cur = Some(tip);
        while let Some(id) = cur {
            let meta = self.index.get(&id).ok_or(Error::NodeNotFound(id))?;
            chain.push(id);
            cur = meta.parent;
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
        let meta = self.index.get(&node).ok_or(Error::NodeNotFound(node))?;
        if meta.kind == NodeKindTag::Context {
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

    // ---- assembly ----

    /// Resolve the chain + referenced material for `tip` and run the pure
    /// assembly function.
    pub fn assemble_chain(&mut self, tip: NodeId) -> Result<Vec<CoreMessage>> {
        let chain_ids = self.chain_to_root(tip)?;
        let mut chain = Vec::with_capacity(chain_ids.len());
        let mut materials = HashMap::new();
        for id in chain_ids {
            let node = self.node(id)?.clone();
            for r in &node.context_refs {
                materials.insert(*r, self.node(*r)?.clone());
            }
            chain.push(node);
        }
        crate::assemble::assemble(&chain, &materials)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::Step;

    fn input_node(text: &str, parent: Option<NodeId>) -> Node {
        Node {
            id: NodeId::new(),
            parent,
            context_refs: vec![],
            created_by: None,
            created_at: Node::now_millis(),
            kind: NodeKind::Input {
                text: text.into(),
                actor: "human".into(),
                tools: vec![],
            },
        }
    }

    fn turn_node(parent: NodeId, outcome: Outcome) -> Node {
        Node {
            id: NodeId::new(),
            parent: Some(parent),
            context_refs: vec![],
            created_by: None,
            created_at: Node::now_millis(),
            kind: NodeKind::Turn {
                steps: vec![Step::LlmCall {
                    request: vec![],
                    response_text: "reply".into(),
                    tool_calls: vec![],
                    reasoning: None,
                    usage: Usage::default(),
                }],
                outcome,
                actor: "agent".into(),
                model: "deepseek-v4-pro".into(),
                usage: Usage::default(),
                tools: vec![],
            },
        }
    }

    fn context_node(body: &str, sources: Vec<NodeId>, created_by: NodeId) -> Node {
        Node {
            id: NodeId::new(),
            parent: None,
            context_refs: sources,
            created_by: None,
            created_at: Node::now_millis(),
            kind: NodeKind::Context {
                body: body.into(),
                created_by,
                model: "deepseek-v4-pro".into(),
            },
        }
    }

    use crate::node::Usage;

    #[test]
    fn commit_and_chain() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let i1 = input_node("hello", None);
        let t1 = turn_node(i1.id, Outcome::Completed);
        let i2 = input_node("next", Some(t1.id));
        g.commit_node(i1.clone()).unwrap();
        g.commit_node(t1.clone()).unwrap();
        g.commit_node(i2.clone()).unwrap();

        assert_eq!(g.chain_to_root(i2.id).unwrap(), vec![i1.id, t1.id, i2.id]);
        assert_eq!(g.children(i1.id), &[t1.id]);
        assert_eq!(g.children(t1.id), &[i2.id]);
    }

    #[test]
    fn meta_context_tokens_from_last_llm_call() {
        let mut t = turn_node(NodeId::new(), Outcome::Completed);
        if let NodeKind::Turn { steps, .. } = &mut t.kind {
            steps.push(Step::LlmCall {
                request: vec![],
                response_text: "final".into(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage {
                    input_tokens: 12_345,
                    ..Usage::default()
                },
            });
        }
        let meta = NodeMeta::of(&t);
        assert_eq!(meta.context_tokens, Some(12_345));
        // 无 LLM 调用的回合没有上下文量
        let mut empty = turn_node(NodeId::new(), Outcome::Failed);
        if let NodeKind::Turn { steps, .. } = &mut empty.kind {
            steps.clear();
        }
        assert_eq!(NodeMeta::of(&empty).context_tokens, None);
    }

    #[test]
    fn fork_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let i1 = input_node("root", None);
        g.commit_node(i1.clone()).unwrap();
        let a = input_node("branch a", Some(i1.id));
        let b = input_node("branch b", Some(i1.id));
        g.commit_node(a.clone()).unwrap();
        g.commit_node(b.clone()).unwrap();
        assert_eq!(g.children(i1.id), &[a.id, b.id]);
    }

    #[test]
    fn nodes_are_immutable() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let n = input_node("x", None);
        g.commit_node(n.clone()).unwrap();
        assert!(matches!(
            g.commit_node(n),
            Err(Error::NodeAlreadyCommitted(_))
        ));
    }

    #[test]
    fn context_boundary_rules() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = Graph::open(dir.path().join("g")).unwrap();
        let i1 = input_node("root", None);
        g.commit_node(i1.clone()).unwrap();

        // Structural node referencing a non-context node: rejected.
        let mut bad = input_node("bad", Some(i1.id));
        bad.context_refs = vec![i1.id];
        assert!(matches!(
            g.commit_node(bad),
            Err(Error::ContextRefNotContextNode(_))
        ));

        // Context node with provenance refs to any committed node: ok.
        let c = context_node("summary", vec![i1.id], i1.id);
        g.commit_node(c.clone()).unwrap();

        // Structural node referencing the context node: ok.
        let mut ok = input_node("with material", Some(i1.id));
        ok.context_refs = vec![c.id];
        g.commit_node(ok).unwrap();

        // Context node with a parent: rejected.
        let mut badc = context_node("orphan?", vec![], i1.id);
        badc.parent = Some(i1.id);
        assert!(matches!(
            g.commit_node(badc),
            Err(Error::ContextNodeHasParent(_))
        ));

        // Cursor cannot land on a context node.
        let cur = g.create_cursor("human", vec![]);
        g.commit_node(input_node("tip", None)).unwrap();
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
            g.commit_node(i1.clone()).unwrap();
            g.move_cursor(cur.id, i1.id).unwrap();
            (cur.id, i1.id)
        };

        let mut g2 = Graph::open(&root).unwrap();
        assert_eq!(g2.cursors.get(cur_id).unwrap().node, Some(tip_id));
        assert!(g2.node(tip_id).is_ok()); // lazy body load
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
            g.commit_node(i1.clone()).unwrap();
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

    /// 旧格式节点（无 tools 字段）反序列化兼容：缺字段读成空数组。
    #[test]
    fn legacy_node_without_tools_deserializes() {
        let input_json = serde_json::json!({
            "id": NodeId::new().to_string(),
            "created_at": 0,
            "kind": { "type": "input", "text": "hi", "actor": "human" },
        });
        let node: Node = serde_json::from_value(input_json).unwrap();
        let NodeKind::Input { tools, .. } = &node.kind else {
            panic!("expected input");
        };
        assert!(tools.is_empty());

        let turn_json = serde_json::json!({
            "id": NodeId::new().to_string(),
            "created_at": 0,
            "kind": {
                "type": "turn",
                "steps": [],
                "outcome": "completed",
                "actor": "agent",
                "model": "m",
            },
        });
        let node: Node = serde_json::from_value(turn_json).unwrap();
        let NodeKind::Turn { tools, .. } = &node.kind else {
            panic!("expected turn");
        };
        assert!(tools.is_empty());
    }

    /// NodeMeta::of：Input 带请求列表、Turn 带有效集；空数组不序列化。
    #[test]
    fn meta_maps_tools_from_node_kind() {
        let mut input = input_node("hi", None);
        if let NodeKind::Input { tools, .. } = &mut input.kind {
            *tools = vec!["bash".into()];
        }
        let meta = NodeMeta::of(&input);
        assert_eq!(meta.tools, vec!["bash".to_string()]);

        let mut turn = turn_node(input.id, Outcome::Completed);
        if let NodeKind::Turn { tools, .. } = &mut turn.kind {
            *tools = vec!["bash".into(), "spawn_turn".into(), "inspect".into()];
        }
        let meta = NodeMeta::of(&turn);
        assert_eq!(
            meta.tools,
            vec!["bash".to_string(), "spawn_turn".to_string(), "inspect".to_string()]
        );
        // 空数组不进 JSON（skip_serializing_if）。
        let empty_meta = NodeMeta::of(&input_node("bare", None));
        assert!(serde_json::to_value(&empty_meta).unwrap()["tools"].is_null());

        // 旧格式 meta（无 tools 字段）同样兼容。
        let mut legacy = serde_json::to_value(&meta).unwrap();
        legacy.as_object_mut().unwrap().remove("tools");
        let meta: NodeMeta = serde_json::from_value(legacy).unwrap();
        assert!(meta.tools.is_empty());
    }
}
