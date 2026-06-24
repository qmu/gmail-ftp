//! The [`Plan`]: a typed DAG of [`EffectNode`]s with dependency edges, plus its
//! construction combinators (`leaf`/`pure`/`then`/`merge`/`depends_on`) and the DAG
//! invariant check ([`Plan::validate`]). RFD-0001 §6 (runtime / effects-as-data).
//!
//! A `Plan` is **pure data**: building one performs no I/O. The only impure operation
//! is the interpreter ([`commit`](crate::commit)), which walks an already-built plan.

use qfs_types::Schema;
use serde::Serialize;

use crate::ids::NodeId;
use crate::node::EffectNode;

/// An error from [`Plan::validate`]: the plan violated a DAG invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlanError {
    /// A dependency edge references a node id that is not in the plan.
    DanglingDep {
        /// The dependent (child) node id.
        child: NodeId,
        /// The depended-on (parent) node id that does not exist.
        parent: NodeId,
    },
    /// The dependency graph contains a cycle (no topological order exists).
    Cyclic,
    /// Two nodes share a [`NodeId`] (ids must be unique within a plan).
    DuplicateId(NodeId),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::DanglingDep { child, parent } => {
                write!(
                    f,
                    "dependency {child} -> {parent}: parent node does not exist"
                )
            }
            PlanError::Cyclic => f.write_str("plan dependency graph is cyclic"),
            PlanError::DuplicateId(id) => write!(f, "duplicate node id {id}"),
        }
    }
}

impl std::error::Error for PlanError {}

/// A typed DAG of effects: the value a write statement evaluates to (RFD §6).
///
/// `nodes` are the effects; `deps` are edges `(parent, child)` meaning `parent` must
/// be applied **before** `child`. The invariant ([`Plan::validate`]): every edge
/// references existing nodes, ids are unique, and the graph is acyclic. `returning`
/// carries the optional `RETURNING` projection schema (RFD §3 effect statements).
///
/// Built immutably: every combinator returns a new `Plan`, so "constructing a plan
/// does I/O" is unrepresentable (RFD §3 purity invariant).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Plan {
    /// The effect nodes (insertion order; topo order is computed separately).
    pub nodes: Vec<EffectNode>,
    /// Dependency edges `(parent, child)`: `parent` applies before `child`.
    pub deps: Vec<(NodeId, NodeId)>,
    /// The optional `RETURNING` projection schema, if the statement declared one.
    pub returning: Option<Schema>,
}

impl Plan {
    /// An empty plan for a query-only (effect-free) statement — `PREVIEW`/`COMMIT` of a
    /// pure read produces this. Reversible, no nodes, no edges (RFD §3: pure functions
    /// return a `Plan`; a read's plan is empty).
    #[must_use]
    pub fn pure() -> Self {
        Self::default()
    }

    /// A single-effect plan from one node (no dependencies).
    #[must_use]
    pub fn leaf(node: EffectNode) -> Self {
        Self {
            nodes: vec![node],
            deps: Vec::new(),
            returning: None,
        }
    }

    /// Attach a `RETURNING` projection schema (builder).
    #[must_use]
    pub fn returning(mut self, schema: Schema) -> Self {
        self.returning = Some(schema);
        self
    }

    /// The effect nodes of this plan.
    #[must_use]
    pub fn nodes(&self) -> &[EffectNode] {
        &self.nodes
    }

    /// The dependency edges `(parent, child)` of this plan.
    #[must_use]
    pub fn deps(&self) -> &[(NodeId, NodeId)] {
        &self.deps
    }

    /// Whether this plan contains any irreversible effect (RFD §6/§10 seam).
    #[must_use]
    pub fn is_irreversible(&self) -> bool {
        self.nodes.iter().any(|n| n.irreversible)
    }

    /// Look up a node by id.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&EffectNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Independent union: merge `other`'s nodes and edges with no new dependency. The
    /// two subgraphs may be applied in any interleaving (the future interpreter may
    /// parallelise them, RFD §6). Node ids must already be disjoint (the evaluator
    /// allocates ids densely via a [`PlanBuilder`]); a clash is caught by `validate`.
    #[must_use]
    pub fn merge(mut self, other: Plan) -> Plan {
        self.nodes.extend(other.nodes);
        self.deps.extend(other.deps);
        // Keep self's RETURNING; an independent union has no single result schema.
        self
    }

    /// Sequence: every "root" node of `other` (a node with no parent in `other`)
    /// depends on every "sink" node of `self` (a node with no child in `self`), so all
    /// of `self`'s effects apply before any of `other`'s. The canonical `INSERT then
    /// CALL` combinator.
    #[must_use]
    pub fn then(mut self, other: Plan) -> Plan {
        let self_sinks = self.sink_ids();
        let other_roots = other.root_ids();
        self.nodes.extend(other.nodes.iter().cloned());
        self.deps.extend(other.deps.iter().copied());
        for parent in &self_sinks {
            for child in &other_roots {
                self.deps.push((*parent, *child));
            }
        }
        self.returning = other.returning.or(self.returning);
        self
    }

    /// The ids of nodes with no parent within this plan (graph roots).
    fn root_ids(&self) -> Vec<NodeId> {
        self.nodes
            .iter()
            .map(|n| n.id)
            .filter(|id| !self.deps.iter().any(|(_, child)| child == id))
            .collect()
    }

    /// The ids of nodes with no child within this plan (graph sinks).
    fn sink_ids(&self) -> Vec<NodeId> {
        self.nodes
            .iter()
            .map(|n| n.id)
            .filter(|id| !self.deps.iter().any(|(parent, _)| parent == id))
            .collect()
    }

    /// Validate the DAG invariants: unique ids, every edge endpoint exists, and the
    /// graph is acyclic. Cheap and pure; called at construction boundaries and
    /// `debug_assert`-ed by the topo walk.
    ///
    /// # Errors
    /// - [`PlanError::DuplicateId`] if two nodes share an id.
    /// - [`PlanError::DanglingDep`] if an edge references a missing node.
    /// - [`PlanError::Cyclic`] if no topological order exists.
    pub fn validate(&self) -> Result<(), PlanError> {
        // Unique ids.
        for (i, a) in self.nodes.iter().enumerate() {
            for b in &self.nodes[i + 1..] {
                if a.id == b.id {
                    return Err(PlanError::DuplicateId(a.id));
                }
            }
        }
        // Every edge endpoint exists.
        for (parent, child) in &self.deps {
            if self.node(*parent).is_none() {
                return Err(PlanError::DanglingDep {
                    child: *child,
                    parent: *parent,
                });
            }
            if self.node(*child).is_none() {
                return Err(PlanError::DanglingDep {
                    child: *child,
                    parent: *parent,
                });
            }
        }
        // Acyclic: a successful topological sort proves it.
        if crate::topo::topo_order(self).is_none() {
            return Err(PlanError::Cyclic);
        }
        Ok(())
    }
}

/// A small builder that allocates dense, unique [`NodeId`]s and accumulates a [`Plan`].
/// The evaluator (E1/E2) uses this so node ids are unique by construction; `merge`/
/// `then` then compose builder-produced plans without id clashes.
#[derive(Debug, Default)]
pub struct PlanBuilder {
    next: u32,
    plan: Plan,
}

impl PlanBuilder {
    /// A fresh builder starting node ids at `0`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next dense [`NodeId`].
    pub fn next_id(&mut self) -> NodeId {
        let id = NodeId(self.next);
        self.next = self.next.saturating_add(1);
        id
    }

    /// Append a node (already carrying an id allocated via [`PlanBuilder::next_id`]).
    pub fn push(&mut self, node: EffectNode) -> NodeId {
        let id = node.id;
        self.plan.nodes.push(node);
        id
    }

    /// Add a dependency edge `parent -> child` (`parent` applies first).
    pub fn depends_on(&mut self, child: NodeId, parent: NodeId) {
        self.plan.deps.push((parent, child));
    }

    /// Finish, returning the accumulated [`Plan`].
    #[must_use]
    pub fn build(self) -> Plan {
        self.plan
    }
}

/// Free-function form of [`PlanBuilder::depends_on`] for an already-built plan: append
/// a `parent -> child` edge. The result still needs [`Plan::validate`] to confirm both
/// ids exist and no cycle was introduced.
#[must_use]
pub fn depends_on(mut plan: Plan, child: NodeId, parent: NodeId) -> Plan {
    plan.deps.push((parent, child));
    plan
}
