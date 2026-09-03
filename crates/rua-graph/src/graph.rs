use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::cursor::{CursorRegistry, TurnHandle};
use crate::error::{Error, Result};
use crate::id::{CursorId, NodeId};
use crate::journal::JournalEvent;
use crate::message::CoreMessage;
use crate::node::{Node, NodeKind, NodeKindTag, Outcome, TurnLine};
use crate::store::Store;

/// Slim per-node record kept in memory and in the journal. The journal is
/// the single source of truth for structure; bodies live in
/// `turns/<ulid>.jsonl` / `contexts/<ulid>.md` (Input bodies are inlined
/// here as `text`).
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
    /// 创建者。Input/Turn：turn 的 spawn_turn 工具调用产生的溯源（None =
    /// 用户/直接操作）。Context 节点复用此字段存 kind 级的 `created_by`
    /// （它蒸馏自哪个节点）——Context 的信封 created_by 恒为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<NodeId>,
    /// 模型：turn 节点 = 该轮使用的模型；context 节点 = 蒸馏所用模型。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 工具集：Input 节点 = 展开后的请求列表；Turn 节点 = 该轮有效集。
    /// 空 = 未记录（旧数据）或无工具。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// Input 节点的完整正文（内联进 journal，无正文文件）；其它 kind 为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Short human-readable preview for graph UIs.
    pub preview: String,
}

impl NodeMeta {
    pub(crate) fn of(node: &Node) -> Self {
        let (outcome, actor, usage, context_tokens, model, tools, text, preview) = match &node.kind {
            NodeKind::Input { text, actor, tools } => (
                None,
                actor.clone(),
                None,
                None,
                None,
                tools.clone(),
                Some(text.clone()),
                truncate(text, 80),
            ),
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
                    None,
                    truncate(&final_text, 80),
                )
            }
            NodeKind::Context { body, model, .. } => (
                None,
                String::new(),
                None,
                None,
                Some(model.clone()),
                vec![],
                None,
                truncate(body, 80),
            ),
        };
        // Context 节点的 kind 级 created_by（蒸馏来源）进 meta.created_by；
        // 其余 kind 用信封的 created_by（spawn 溯源）。
        let created_by = match &node.kind {
            NodeKind::Context { created_by, .. } => Some(*created_by),
            _ => node.created_by,
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
            created_by,
            model,
            tools,
            text,
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
    /// A legacy `nodes/` layout (single-file bodies) is migrated first.
    pub fn open(root: impl Into<std::path::PathBuf>) -> Result<Self> {
        let store = Store::new(root)?;
        crate::migrate::migrate_legacy_nodes(&store)?;
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

    /// Validate and commit an immutable node: persist the body (dispatch by
    /// kind), journal the meta, update the index. This is the only way nodes
    /// come into existence.
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

        // 正文落盘按 kind 分派：Input 内联进 journal meta（无正文文件）；
        // Context 外部化为 contexts/<id>.md；Turn 为 turns/<id>.jsonl——
        // sink 已增量写入（文件存在）时不重写，直接提交路径（无 sink，如
        // 测试）把 steps 整体转出（无 Init 行）。
        match &node.kind {
            NodeKind::Input { .. } => {}
            NodeKind::Context { body, .. } => self.store.write_context(node.id, body)?,
            NodeKind::Turn { steps, .. } => {
                if !self.store.has_turn_lines(node.id) {
                    for step in steps {
                        self.store
                            .append_turn_line(node.id, &TurnLine::from(step.clone()))?;
                    }
                }
            }
        }
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

    /// Load a node body (cached), rebuilt from its journal meta + body files:
    /// Input from the meta (`text`/actor/tools all inline), Turn from
    /// `turns/<id>.jsonl` (`Init` lines are anchors, not steps), Context from
    /// `contexts/<id>.md`.
    pub fn node(&mut self, id: NodeId) -> Result<&Node> {
        if !self.index.contains_key(&id) {
            return Err(Error::NodeNotFound(id));
        }
        if !self.bodies.contains_key(&id) {
            let node = self.rebuild(id)?;
            self.bodies.insert(id, node);
        }
        Ok(self.bodies.get(&id).unwrap())
    }

    fn rebuild(&self, id: NodeId) -> Result<Node> {
        let meta = self.index.get(&id).ok_or(Error::NodeNotFound(id))?;
        let kind = match meta.kind {
            NodeKindTag::Input => NodeKind::Input {
                text: meta.text.clone().unwrap_or_default(),
                actor: meta.actor.clone(),
                tools: meta.tools.clone(),
            },
            NodeKindTag::Turn => NodeKind::Turn {
                steps: self
                    .store
                    .read_turn_lines(id)?
                    .into_iter()
                    .filter_map(TurnLine::into_step)
                    .collect(),
                // Committed turns always carry an outcome; the fallback only
                // guards a hand-corrupted journal.
                outcome: meta.outcome.unwrap_or(Outcome::Failed),
                actor: meta.actor.clone(),
                model: meta.model.clone().unwrap_or_default(),
                usage: meta.usage.unwrap_or_default(),
                tools: meta.tools.clone(),
            },
            NodeKindTag::Context => NodeKind::Context {
                body: self.store.read_context(id)?,
                // meta.created_by 对 Context 存的是 kind 级蒸馏来源（见
                // NodeMeta::of）；缺失兜底为自身 id（正常数据不可达）。
                created_by: meta.created_by.unwrap_or(id),
                model: meta.model.clone().unwrap_or_default(),
            },
        };
        Ok(Node {
            id: meta.id,
            parent: meta.parent,
            context_refs: meta.context_refs.clone(),
            // 信封 created_by（spawn 溯源）只有 Input 可能有；Context 的
            // meta.created_by 已被 kind 级字段复用，信封恒为 None。
            created_by: match meta.kind {
                NodeKindTag::Context => None,
                _ => meta.created_by,
            },
            created_at: meta.created_at,
            kind,
        })
    }

    /// The turn's init anchor: the first LLM call's full request snapshot
    /// (`None` when the body has no `Init` line, e.g. direct-commit paths).
    pub fn turn_init(&mut self, id: NodeId) -> Result<Option<Vec<CoreMessage>>> {
        Ok(self
            .store
            .read_turn_lines(id)?
            .into_iter()
            .find_map(|line| match line {
                TurnLine::Init { request } => Some(request),
                _ => None,
            }))
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

    // ---- chain loading ----

    /// Resolve the chain + referenced material for `tip`: backtrack to root,
    /// load the chain nodes and their `context_refs` targets.
    pub fn load_chain(&mut self, tip: NodeId) -> Result<(Vec<Node>, HashMap<NodeId, Node>)> {
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
        Ok((chain, materials))
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
                    response_text: "reply".into(),
                    tool_calls: vec![],
                    reasoning: None,
                    usage: Usage::default(),
                    provider_data: None,
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
                response_text: "final".into(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage {
                    input_tokens: 12_345,
                    ..Usage::default()
                },
                provider_data: None,
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

    /// Input 正文内联进 journal meta：重开后只读 journal 即可重建完整节点。
    #[test]
    fn input_rebuilds_from_journal_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("g");
        let id = {
            let mut g = Graph::open(&root).unwrap();
            let mut n = input_node("a full input body, not just a preview", None);
            if let NodeKind::Input { tools, .. } = &mut n.kind {
                *tools = vec!["bash".into()];
            }
            g.commit_node(n.clone()).unwrap();
            n.id
        };
        let mut g = Graph::open(&root).unwrap();
        let node = g.node(id).unwrap();
        let NodeKind::Input { text, actor, tools } = &node.kind else {
            panic!("expected input");
        };
        assert_eq!(text, "a full input body, not just a preview");
        assert_eq!(actor, "human");
        assert_eq!(tools, &vec!["bash".to_string()]);
        // meta 带全量 text，preview 仍是 80 字符截断。
        let meta = g.meta(id).unwrap();
        assert_eq!(meta.text.as_deref(), Some("a full input body, not just a preview"));
        assert!(meta.preview.chars().count() <= 81);
        // 无正文文件。
        assert!(!root.join("nodes").exists());
    }

    /// Turn 的两条落盘路径读回一致：直接 commit_node（无 sink，steps 整体
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

        // (a) 直接 commit：commit_node 把 steps 整体写入 turns/<id>.jsonl。
        let dir_a = tempfile::tempdir().unwrap();
        let mut ga = Graph::open(dir_a.path().join("g")).unwrap();
        let input = input_node("hi", None);
        ga.commit_node(input.clone()).unwrap();
        let mut turn_a = turn_node(input.id, Outcome::Completed);
        if let NodeKind::Turn { steps: s, .. } = &mut turn_a.kind {
            *s = steps.clone();
        }
        ga.commit_node(turn_a.clone()).unwrap();

        // (b) sink 路径：先增量 append（含 Init 锚点），commit 时正文已存在
        // → 只追加 journal meta，不重写。
        let dir_b = tempfile::tempdir().unwrap();
        let root_b = dir_b.path().join("g");
        let mut gb = Graph::open(&root_b).unwrap();
        let input = input_node("hi", None);
        gb.commit_node(input.clone()).unwrap();
        let mut turn_b = turn_node(input.id, Outcome::Completed);
        if let NodeKind::Turn { steps: s, .. } = &mut turn_b.kind {
            *s = steps.clone();
        }
        let init = vec![
            CoreMessage::System {
                content: "sys".into(),
            },
            CoreMessage::User {
                content: "hi".into(),
            },
        ];
        gb.store()
            .append_turn_line(turn_b.id, &TurnLine::Init { request: init.clone() })
            .unwrap();
        for step in &steps {
            gb.store()
                .append_turn_line(turn_b.id, &TurnLine::from(step.clone()))
                .unwrap();
        }
        gb.commit_node(turn_b.clone()).unwrap();

        // 两条路径重建出的 steps 一致（Init 不进 steps）。
        let mut ga = Graph::open(dir_a.path().join("g")).unwrap();
        let mut gb = Graph::open(&root_b).unwrap();
        let NodeKind::Turn { steps: sa, .. } = &ga.node(turn_a.id).unwrap().kind else {
            panic!("expected turn");
        };
        let NodeKind::Turn { steps: sb, .. } = &gb.node(turn_b.id).unwrap().kind else {
            panic!("expected turn");
        };
        assert_eq!(sa, &steps);
        assert_eq!(sb, &steps);

        // Init 锚点：sink 路径有、直接 commit 路径无。
        assert_eq!(gb.turn_init(turn_b.id).unwrap(), Some(init));
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

    /// Context 正文外部化：commit 写 contexts/<id>.md，重开后重建一致。
    #[test]
    fn context_body_lives_in_contexts_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("g");
        let id = {
            let mut g = Graph::open(&root).unwrap();
            let i = input_node("src", None);
            g.commit_node(i.clone()).unwrap();
            let c = context_node("distilled body", vec![i.id], i.id);
            g.commit_node(c.clone()).unwrap();
            c.id
        };
        assert_eq!(
            std::fs::read_to_string(root.join("contexts").join(format!("{id}.md"))).unwrap(),
            "distilled body"
        );
        let mut g = Graph::open(&root).unwrap();
        let src = g.meta(id).unwrap().context_refs[0];
        let NodeKind::Context {
            body,
            created_by,
            model,
        } = &g.node(id).unwrap().kind
        else {
            panic!("expected context");
        };
        assert_eq!(body, "distilled body");
        assert_eq!(model, "deepseek-v4-pro");
        // kind 级 created_by（蒸馏来源）经 meta 存活重建。
        assert_eq!(*created_by, src);
    }

    /// 旧布局（nodes/<ulid>.json 单文件 + LlmCall 内嵌 request）在 open 时
    /// 自动迁移：正文转写新布局、journal meta 重生成、旧文件进 .trash。
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

        // Input：text 内联进 meta，重建出完整正文。
        let meta = g.meta(input_id).unwrap();
        assert_eq!(meta.text.as_deref(), Some("legacy hello"));
        assert_eq!(meta.tools, vec!["bash".to_string()]);
        let NodeKind::Input { text, .. } = &g.node(input_id).unwrap().kind else {
            panic!("expected input");
        };
        assert_eq!(text, "legacy hello");

        // Turn：turns/<id>.jsonl = init 锚点（首份 request）+ 逐行 steps
        // （request 已丢）；重建的 steps 与旧数据一致。
        let init = g.turn_init(turn_id).unwrap().expect("init anchor");
        assert_eq!(init.len(), 2);
        assert!(matches!(&init[0], CoreMessage::System { content } if content == "sys"));
        let NodeKind::Turn {
            steps,
            outcome,
            tools,
            usage,
            ..
        } = &g.node(turn_id).unwrap().kind
        else {
            panic!("expected turn");
        };
        assert_eq!(*outcome, Outcome::Completed);
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
        assert_eq!(tools, &vec!["bash".to_string()]);
        assert_eq!(usage.input_tokens, 16);

        // Context：正文落 contexts/<id>.md，重建一致。
        assert_eq!(
            std::fs::read_to_string(root.join("contexts").join(format!("{ctx_id}.md"))).unwrap(),
            "legacy ctx"
        );
        let NodeKind::Context { body, created_by, .. } = &g.node(ctx_id).unwrap().kind else {
            panic!("expected context");
        };
        assert_eq!(body, "legacy ctx");
        assert_eq!(*created_by, turn_id);

        // cursor/turn 事件原样保留。
        assert_eq!(g.cursors.get(cursor_id).unwrap().node, Some(turn_id));
    }
}
