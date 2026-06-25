//! The **pure evaluator** (RFD-0001 §3 purity invariant, §6 effect-plan, ticket t07):
//! the pass that turns a parsed + resolved [`qfs_parser::Statement`] into a
//! [`qfs_plan::Plan`] (the effect-plan) and a logical query [`PlanSource`] — performing
//! **no I/O**.
//!
//! "A statement is a plan." Query stages fold left into a logical relation description
//! ([`PlanSource`]) whose output [`Schema`](qfs_types::Schema) is threaded stage by
//! stage (using the t05 schema algebra as the single source of truth); write operators
//! (`INSERT`/`UPSERT`/`UPDATE`/`REMOVE`) and `CALL driver.action(...)` **construct**
//! [`EffectNode`](qfs_plan::EffectNode)s and assemble them into a [`Plan`] DAG with
//! declared dependencies and the `irreversible` flag set. The single impure boundary —
//! `COMMIT : Plan -> World` — is t10's interpreter and is explicitly out of scope here.
//!
//! ## Purity invariant (the safety property, RFD §3)
//! No function in this module takes or returns a `World`, an HTTP client, or a token; it
//! reads only the read-only [`MountRegistry`] (longest-prefix path router → driver) and
//! the driver's pure [`describe`](qfs_driver::Driver::describe) schema. Building and
//! previewing a plan therefore perform **no** network calls — this is what makes every
//! statement dry-runnable and golden-testable without credentials.
//!
//! ## Verb pipeline (t06 carry-over O2 + t09 mirror)
//! The AST-verb translation goes through the canonical
//! [`write_verb_for`](crate::write_verb_for) ∘ [`kind_for_verb`](qfs_plan::kind_for_verb)
//! pipeline — an **exhaustive match with no `_` arm** at each hop. `qfs_parser::EffectVerb`
//! must therefore stay **non-`#[non_exhaustive]`** (it is, per t04) so that adding a verb
//! breaks the cross-crate match and forces this evaluator and t06's resolver to be
//! updated rather than silently dropping the new verb. The [`WriteVerb`](qfs_plan::WriteVerb)
//! / `EffectVerb` mirror is intentional (the t06 Architect note): `qfs-plan` mirrors the
//! four verbs so it stays parser-free (acyclic spine), and the evaluator bridges the two.

use qfs_driver::{Driver, Path};
use qfs_parser::{
    EffectBody, EffectStmt, EffectVerb, Expr, Literal, PipeOp, Pipeline, PlanWrap, Projection,
    Source, Statement, Values,
};
use qfs_plan::{
    kind_for_verb, Affected, EffectKind, EffectNode, NodeId, Plan, PlanBuilder, ProcId, Target,
    VfsPath,
};
use qfs_types::{Column, ColumnType, DriverId, Name, Row, RowBatch, Schema, Value};

use crate::registry::MountRegistry;
use crate::resolve::{write_verb_for, ResolveError, Resolver};
use crate::stdlib::{BuiltinEval, FnError, StdlibRegistry};

/// A logical relation node — a **description** of how a query produces rows, never an
/// executed scan (RFD §3 purity). The fold of a `FROM` source + `|>` pipe ops produces
/// a tree of these; each node carries its computed output [`Schema`] so the next stage
/// (and a `RETURNING`/write that consumes it) types against one source of truth.
///
/// t09 deferred the relational node enum to the evaluator (no `PlanSource` type exists in
/// `qfs-plan`); this is that owned, vendor-free description. It references paths and column
/// names only — never a driver SDK struct (RFD §9).
#[derive(Debug, Clone, PartialEq)]
pub enum PlanSource {
    /// `FROM /driver/...` — a logical scan of a path (a description, not a read).
    Scan {
        /// The driver this relation reads from.
        driver: DriverId,
        /// The virtual path scanned.
        path: VfsPath,
        /// The node's output schema (from the driver's pure `describe`).
        schema: Schema,
    },
    /// `FROM VALUES (..),(..)` — an inline literal relation.
    Values {
        /// The inferred schema of the literal rows.
        schema: Schema,
    },
    /// A `WHERE` filter over an input relation (schema-preserving).
    Filter {
        /// The filtered input.
        input: Box<PlanSource>,
    },
    /// A `SELECT`/projection narrowing the input columns.
    Project {
        /// The projected input.
        input: Box<PlanSource>,
        /// The projected output schema.
        schema: Schema,
    },
    /// An `EXTEND`/`SET` adding or overwriting columns.
    Extend {
        /// The extended input.
        input: Box<PlanSource>,
        /// The output schema after extension.
        schema: Schema,
    },
    /// `LIMIT`/`DISTINCT`/`ORDER BY`/`GROUP BY`/`AS` — a schema-preserving shaping op.
    Shape {
        /// The shaped input.
        input: Box<PlanSource>,
    },
    /// `EXPAND <field>` — explode a nested collection into rows (RFD §4).
    Expand {
        /// The expanded input.
        input: Box<PlanSource>,
        /// The output schema after the field is exploded.
        schema: Schema,
    },
    /// `DECODE`/`ENCODE <fmt>` — a codec-registry seam node (a description only; the
    /// codec lookup is the registry's concern, the schema is late-bound).
    Codec {
        /// The decoded/encoded input.
        input: Box<PlanSource>,
        /// The codec format name.
        fmt: String,
    },
    /// `UNION`/`EXCEPT`/`INTERSECT` — a set operation over two relations; the output
    /// schema is the column-wise `unify` of the two sides (RFD §4).
    SetOp {
        /// The left input.
        lhs: Box<PlanSource>,
        /// The right input.
        rhs: Box<PlanSource>,
        /// The unified output schema.
        schema: Schema,
    },
    /// `JOIN <source> ON <expr>` — the concatenation of both sides' columns.
    Join {
        /// The left input.
        lhs: Box<PlanSource>,
        /// The right input.
        rhs: Box<PlanSource>,
        /// The joined output schema.
        schema: Schema,
    },
}

impl PlanSource {
    /// The output schema of this relation node — the contract the next stage and any
    /// `RETURNING`/consuming write types against.
    #[must_use]
    pub fn schema(&self) -> &Schema {
        match self {
            PlanSource::Scan { schema, .. }
            | PlanSource::Values { schema }
            | PlanSource::Project { schema, .. }
            | PlanSource::Extend { schema, .. }
            | PlanSource::Expand { schema, .. }
            | PlanSource::SetOp { schema, .. }
            | PlanSource::Join { schema, .. } => schema,
            PlanSource::Filter { input }
            | PlanSource::Shape { input }
            | PlanSource::Codec { input, .. } => input.schema(),
        }
    }
}

/// The value a statement evaluates to (RFD §3): a query yields a logical relation; a
/// write/`CALL` yields a [`Plan`]. Owned, no vendor types.
#[derive(Debug, Clone, PartialEq)]
pub enum EvalValue {
    /// A pure read pipeline evaluated to its logical relation description.
    Relation(PlanSource),
    /// A write/effect statement evaluated to its effect-plan DAG.
    Plan(Plan),
}

impl EvalValue {
    /// The relation, if this value is a query result.
    #[must_use]
    pub fn as_relation(&self) -> Option<&PlanSource> {
        match self {
            EvalValue::Relation(r) => Some(r),
            EvalValue::Plan(_) => None,
        }
    }

    /// The plan, if this value is a write/effect result.
    #[must_use]
    pub fn as_plan(&self) -> Option<&Plan> {
        match self {
            EvalValue::Plan(p) => Some(p),
            EvalValue::Relation(_) => None,
        }
    }
}

/// The structured, AI-consumable evaluation error (RFD §5). Resolution failures (unknown
/// driver/proc, capability denial, ambiguous alias) surface as [`EvalError::Resolve`];
/// schema/type failures (unknown column, ill-typed projection) surface as
/// [`EvalError::Type`]; an unrouted target path is its own arm. Credentials never appear.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum EvalError {
    /// Name resolution rejected the statement (delegated to the t06 resolver): unknown
    /// driver/procedure, capability denial, ambiguous/unprovided alias, arity/arg mismatch.
    Resolve(ResolveError),
    /// A schema/type rule rejected the statement (delegated to the t05 type model):
    /// unknown column, non-expandable field, incomparable types.
    Type(qfs_types::TypeError),
    /// A `fn(...)` registry-function call was ill-formed (delegated to the t08 stdlib):
    /// unknown function, wrong arity, or an aggregate used outside an `AGGREGATE` context.
    Fn(FnError),
    /// A `FROM` / effect-target path did not route to any registered driver mount, so no
    /// schema could be described for it. Carries the path for AI recovery.
    UnroutedPath {
        /// The path that failed to route to a mounted driver.
        path: String,
    },
    /// An `INSERT … VALUES` cell was not a constant the planner can lower to a literal effect
    /// payload (a function call, column reference, or other runtime expression). VALUES must be
    /// constants — use an `INSERT … FROM <query>` for computed rows. Carries a secret-free detail.
    NonLiteralValues {
        /// A machine-facing description of the offending expression form.
        detail: String,
    },
}

impl EvalError {
    /// A stable, machine-readable code an AI-facing caller branches on (RFD §5).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            EvalError::Resolve(e) => e.code(),
            EvalError::Type(e) => e.code(),
            EvalError::Fn(e) => e.code(),
            EvalError::UnroutedPath { .. } => "unrouted_path",
            EvalError::NonLiteralValues { .. } => "non_literal_values",
        }
    }
}

impl From<ResolveError> for EvalError {
    fn from(e: ResolveError) -> Self {
        EvalError::Resolve(e)
    }
}

impl From<qfs_types::TypeError> for EvalError {
    fn from(e: qfs_types::TypeError) -> Self {
        EvalError::Type(e)
    }
}

impl From<FnError> for EvalError {
    fn from(e: FnError) -> Self {
        EvalError::Fn(e)
    }
}

/// The pure evaluator (RFD §3, t07): a read-only pass over a parsed [`Statement`] that
/// folds the query side into a [`PlanSource`] and the write side into a [`Plan`]. Holds
/// the [`MountRegistry`] read-only (for `describe` schema + capability/procedure
/// resolution) and a [`Resolver`] for the t06 name/capability gate; performs no I/O.
pub struct Evaluator<'r> {
    mounts: &'r MountRegistry,
    resolver: Resolver<'r>,
    /// The function registry (t08), consulted to type `fn(...)` calls in expression
    /// position. `None` keeps t07's late-bound behaviour (a `fn(...)` projects an
    /// `Unknown` column and is not registry-checked); `Some` tightens it — the function's
    /// declared return type drives the projected column type and an unknown function /
    /// aggregate-in-`WHERE` becomes a structured [`EvalError::Fn`].
    stdlib: Option<&'r StdlibRegistry>,
}

impl<'r> Evaluator<'r> {
    /// Build an evaluator over a mount registry (no function registry — `fn(...)` calls
    /// stay late-bound, t07 behaviour).
    #[must_use]
    pub fn new(mounts: &'r MountRegistry) -> Self {
        Self {
            mounts,
            resolver: Resolver::new(mounts),
            stdlib: None,
        }
    }

    /// Build an evaluator wired to the function registry (t08): `fn(...)` calls are typed
    /// against the stdlib, so a projection over a built-in carries the built-in's declared
    /// return type and an unknown / mis-contexted function is a structured error.
    #[must_use]
    pub fn with_stdlib(mounts: &'r MountRegistry, stdlib: &'r StdlibRegistry) -> Self {
        Self {
            mounts,
            resolver: Resolver::new(mounts),
            stdlib: Some(stdlib),
        }
    }

    /// The declared return type of a `fn(name, ...)` call against the function registry,
    /// or a structured [`FnError`] (unknown function / wrong arity / aggregate misuse). The
    /// `under_aggregate` flag enforces the aggregate-vs-scalar dispatch rule (RFD §3): an
    /// aggregate is legal only under `AGGREGATE`, a scalar is rejected where an aggregate
    /// is required. A prelude-alias name (receiver-scoped) types as `Unknown` here — its
    /// resolution is t06's receiver-typing concern, not the function registry's.
    ///
    /// # Errors
    /// [`FnError`] for an unknown function, an arity mismatch, or an aggregate/scalar used
    /// in the wrong context.
    pub fn type_of_fn(
        &self,
        name: &str,
        argc: usize,
        under_aggregate: bool,
    ) -> Result<ColumnType, FnError> {
        let Some(reg) = self.stdlib else {
            return Ok(ColumnType::Unknown);
        };
        let Some(builtin) = reg.builtin(name) else {
            // Not a core built-in. If a driver ships it as a prelude alias, it is a
            // receiver-typed callable (t06), late-bound here; otherwise it is unknown.
            if reg.alias_providers(name).is_empty() {
                return Err(FnError::UnknownFunction {
                    name: name.to_string(),
                });
            }
            return Ok(ColumnType::Unknown);
        };
        if !builtin.sig.accepts_arity(argc) {
            return Err(FnError::Arity {
                name: name.to_string(),
                expected: builtin.sig.min_args,
                found: argc,
            });
        }
        // Aggregate-vs-scalar dispatch (RFD §3): a typed error, never a panic.
        let is_aggregate = matches!(builtin.eval, BuiltinEval::Aggregate(_));
        if is_aggregate && !under_aggregate {
            return Err(FnError::AggregateOutsideAggregate {
                name: name.to_string(),
            });
        }
        if !is_aggregate && under_aggregate && matches!(builtin.eval, BuiltinEval::Scalar(_)) {
            // A bare scalar directly under AGGREGATE (not wrapping/aggregating) is the
            // dual misuse — but scalars are legal *inside* an aggregate's argument, so we
            // only flag a top-level scalar projection. The caller passes `under_aggregate`
            // only for the projection head, so this stays correct.
            return Err(FnError::ScalarInAggregate {
                name: name.to_string(),
            });
        }
        Ok(builtin.sig.returns.clone())
    }

    /// Evaluate a statement to its [`EvalValue`] (RFD §3 entry point). Resolution (the
    /// t06 capability/procedure gate) runs **first**, so a denied verb or unknown
    /// procedure never reaches a plan; then the pure fold builds the relation/plan.
    ///
    /// # Errors
    /// [`EvalError`] for any unresolvable name, capability denial, ill-typed stage, or
    /// unrouted path.
    pub fn eval(&self, stmt: &Statement) -> Result<EvalValue, EvalError> {
        // Resolve-time gate first (RFD §5): denied verbs / unknown procs fail before a
        // plan exists.
        self.resolver.resolve_statement(stmt)?;
        self.eval_inner(stmt)
    }

    fn eval_inner(&self, stmt: &Statement) -> Result<EvalValue, EvalError> {
        match stmt {
            Statement::Query(pipeline) => Ok(EvalValue::Relation(self.fold_query(pipeline)?)),
            Statement::Effect(effect) => Ok(EvalValue::Plan(self.eval_write(effect)?)),
            // PREVIEW/COMMIT are transparent to evaluation: the plan they wrap is the
            // inner statement's plan (the dry-run/apply distinction is t10's interpreter).
            Statement::Plan(PlanWrap { inner, .. }) => self.eval_inner(inner),
            // Server DDL desugars to `/server/...` effects in a later epic; here it
            // evaluates to an empty plan (no effect node to construct yet, ticket scope).
            Statement::Ddl(_) => Ok(EvalValue::Plan(Plan::pure())),
        }
    }

    // ---- Query side: fold pipe stages into a logical relation ----

    /// Left-fold a read pipeline into a [`PlanSource`], threading the output schema
    /// through each `|>` stage (RFD §2.2). The source schema comes from the driver's
    /// pure `describe`; each op transforms it via the t05 schema algebra.
    fn fold_query(&self, pipeline: &Pipeline) -> Result<PlanSource, EvalError> {
        let mut src = self.eval_source(&pipeline.source)?;
        for op in &pipeline.ops {
            src = self.fold_op(src, op)?;
        }
        Ok(src)
    }

    /// Evaluate a pipeline source into the base [`PlanSource`] (RFD §2.2).
    fn eval_source(&self, source: &Source) -> Result<PlanSource, EvalError> {
        match source {
            Source::Path(path) => {
                let full = render_path(
                    &path
                        .segments
                        .iter()
                        .map(|s| s.name.clone())
                        .collect::<Vec<_>>(),
                );
                let (driver, sub) = self
                    .mounts
                    .resolve_path(&full)
                    .ok_or_else(|| EvalError::UnroutedPath { path: full.clone() })?;
                let vfs = format!("/{}/{}", driver.id().as_str(), sub);
                let schema = describe_schema(driver.as_ref(), &vfs)?;
                Ok(PlanSource::Scan {
                    driver: driver.id(),
                    path: VfsPath::new(vfs),
                    schema,
                })
            }
            Source::Values(values) => Ok(PlanSource::Values {
                schema: values_schema(values),
            }),
            Source::Subquery(inner) => self.fold_query(inner),
        }
    }

    /// Fold one pipe op onto the current relation, computing its output schema (RFD §3).
    fn fold_op(&self, input: PlanSource, op: &PipeOp) -> Result<PlanSource, EvalError> {
        match op {
            // Schema-preserving filter.
            PipeOp::Where(_) => Ok(PlanSource::Filter {
                input: Box::new(input),
            }),
            // Projection narrows to the named columns (t05 `project` is the source of truth).
            // `SELECT` types `fn(...)` projections as scalars; `AGGREGATE` types them under
            // the aggregate-context rule (t08 dispatch).
            PipeOp::Select(projs) => {
                let schema = self.project_schema(input.schema(), projs, false)?;
                Ok(PlanSource::Project {
                    input: Box::new(input),
                    schema,
                })
            }
            PipeOp::Aggregate(projs) => {
                let schema = self.project_schema(input.schema(), projs, true)?;
                Ok(PlanSource::Project {
                    input: Box::new(input),
                    schema,
                })
            }
            // EXTEND/SET add or overwrite columns (Unknown-typed: pure expr typing is
            // late-bound here; the column env carries names so the next stage resolves).
            PipeOp::Extend(asgns) | PipeOp::Set(asgns) => {
                let mut schema = input.schema().clone();
                for a in asgns {
                    match schema.columns.iter_mut().find(|c| c.name == a.name) {
                        Some(col) => col.ty = ColumnType::Unknown,
                        None => schema.columns.push(Column::new(
                            a.name.clone(),
                            ColumnType::Unknown,
                            true,
                        )),
                    }
                }
                Ok(PlanSource::Extend {
                    input: Box::new(input),
                    schema,
                })
            }
            // EXPAND explodes a collection column (t05 `expand` is the source of truth).
            PipeOp::Expand(field) => {
                let name = field.last().cloned().unwrap_or_default();
                let schema = input.schema().expand(&name)?;
                Ok(PlanSource::Expand {
                    input: Box::new(input),
                    schema,
                })
            }
            PipeOp::Decode(codec) | PipeOp::Encode(codec) => Ok(PlanSource::Codec {
                input: Box::new(input),
                fmt: codec.fmt.clone(),
            }),
            // Set operations: unify the two sides' schemas column-wise (RFD §4).
            PipeOp::Union(p) | PipeOp::Except(p) | PipeOp::Intersect(p) => {
                let rhs = self.fold_query(p)?;
                let schema = Schema::unify(input.schema(), rhs.schema())?;
                Ok(PlanSource::SetOp {
                    lhs: Box::new(input),
                    rhs: Box::new(rhs),
                    schema,
                })
            }
            PipeOp::Join(join) => {
                let rhs = self.eval_source(&join.source)?;
                let mut cols = input.schema().columns.clone();
                cols.extend(rhs.schema().columns.clone());
                let schema = Schema::new(cols);
                Ok(PlanSource::Join {
                    lhs: Box::new(input),
                    rhs: Box::new(rhs),
                    schema,
                })
            }
            // A `CALL` in a read pipeline is a procedure node; resolution already vetted
            // it, and the query relation is schema-preserving for the fold's purpose
            // (the call's effect, if any, is materialised on the write side).
            PipeOp::Call(_) => Ok(PlanSource::Shape {
                input: Box::new(input),
            }),
            // Schema-preserving shaping ops.
            PipeOp::Limit(_)
            | PipeOp::Distinct
            | PipeOp::OrderBy(_)
            | PipeOp::GroupBy(_)
            | PipeOp::As(_) => Ok(PlanSource::Shape {
                input: Box::new(input),
            }),
        }
    }

    // ---- Write side: construct effect-plan nodes ----

    /// Evaluate an effect statement (`INSERT`/`UPSERT`/`UPDATE`/`REMOVE`) into a [`Plan`]
    /// (RFD §6). The verb goes through the canonical
    /// [`write_verb_for`](crate::write_verb_for) ∘ [`kind_for_verb`](qfs_plan::kind_for_verb)
    /// pipeline (no `_` arm). The effect node depends on any input relation (an
    /// `INSERT … FROM <query>` body), `REMOVE` is flagged inherently irreversible, and the
    /// optional `RETURNING` projection schema is attached.
    fn eval_write(&self, effect: &EffectStmt) -> Result<Plan, EvalError> {
        let full = render_path(
            &effect
                .target
                .segments
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
        );
        let (driver, vfs) = match self.mounts.resolve_path(&full) {
            Some((driver, sub)) => (driver.id(), format!("/{}/{}", driver.id().as_str(), sub)),
            // An unrouted target is a path/mount concern; we still build a plan against
            // the literal path so the verb/irreversible semantics are testable without a
            // mount (deferred path resolution, ticket scope).
            None => (DriverId::new(first_segment(&full)), full.clone()),
        };
        let target = Target::new(driver, VfsPath::new(vfs));

        // The canonical verb pipeline (t06 O2 → t09 mirror): EffectVerb → WriteVerb →
        // EffectKind, each an exhaustive no-`_`-arm match.
        let kind = kind_for_verb(write_verb_for(effect.verb));

        let mut builder = PlanBuilder::new();

        // If the body is a sub-pipeline, evaluate it to a relation first and emit a Read
        // dependency the write hangs off (the `INSERT … FROM <query>` case).
        let dep: Option<NodeId> = match &effect.body {
            EffectBody::Pipeline(p) => {
                let rel = self.fold_query(p)?;
                let read =
                    EffectNode::new(builder.next_id(), EffectKind::Read, source_target(&rel))
                        .with_affected(Affected::Unknown);
                Some(builder.push(read))
            }
            EffectBody::Values(_) | EffectBody::SetWhere { .. } => None,
        };

        // The literal `VALUES` row payload the effect WRITES — lowered here into the effect node's
        // `args` (a `RowBatch`) so the applier actually receives the rows. Column names come from
        // the explicit `VALUES (a,b)` list when present, else the target node's described columns
        // in order (the applier maps the row's named columns onto the backend columns). WITHOUT
        // this, an `INSERT … VALUES` reaches the applier with an empty payload — a silent no-op.
        let values_args: Option<RowBatch> = match &effect.body {
            EffectBody::Values(v) => Some(self.values_row_batch(effect, v)?),
            EffectBody::SetWhere { set, filter } => Some(setwhere_row_batch(set, filter.as_ref())?),
            EffectBody::Pipeline(_) => None,
        };

        // The write node itself. `REMOVE` is inherently irreversible (set in `new`). The row
        // payload is attached FIRST, then the honest affected estimate LAST: `INSERT … VALUES` is
        // `Exact(n)`, while a filter-driven `UPDATE`/`REMOVE` stays `Unknown` (the synthetic
        // key+set row is NOT the affected-row count) — so `with_affected` must win over the
        // `with_args` row-count refinement.
        let write_id = builder.next_id();
        let mut write = EffectNode::new(write_id, kind, target);
        if let Some(args) = values_args {
            write = write.with_args(args);
        }
        write = write.with_affected(write_affected(effect));
        builder.push(write);
        if let Some(parent) = dep {
            builder.depends_on(write_id, parent);
        }

        let mut plan = builder.build();

        // The RETURNING projection schema, computed against the effect's input schema.
        if let Some(returning) = &effect.returning {
            let input_schema = self.effect_input_schema(effect)?;
            let schema = self.project_schema(&input_schema, returning, false)?;
            plan = plan.returning(schema);
        }

        Ok(plan)
    }

    /// Lower an `INSERT … VALUES` body into the effect's row payload [`RowBatch`]. Each cell is a
    /// constant evaluated to a [`Value`]; the column names come from the explicit `VALUES (a,b)`
    /// list when present, else the target node's described columns (truncated to the row width) so
    /// the applier maps each named cell onto the right backend column. A non-constant cell is a
    /// structured [`EvalError::NonLiteralValues`] (use `INSERT … FROM <query>` for computed rows),
    /// never a silently-dropped value.
    fn values_row_batch(
        &self,
        effect: &EffectStmt,
        values: &Values,
    ) -> Result<RowBatch, EvalError> {
        let width = values.rows.first().map_or(0, Vec::len);
        // Column names: explicit list, else the target's described columns in order.
        let columns: Vec<Column> = match &values.columns {
            Some(cols) => cols
                .iter()
                .map(|name| Column::new(name.clone(), ColumnType::Unknown, true))
                .collect(),
            None => {
                // Prefer the target's described columns (so a routed table names each cell for the
                // applier); fall back to positional `col{i}` when the target is unrouted or narrower
                // than the row — so a PREVIEW works with no mount (mirroring `values_schema`), never
                // erroring just because the row payload is being built.
                let described = self
                    .effect_input_schema(effect)
                    .map(|s| s.columns)
                    .unwrap_or_default();
                (0..width)
                    .map(|i| {
                        described.get(i).cloned().unwrap_or_else(|| {
                            Column::new(format!("col{i}"), ColumnType::Unknown, true)
                        })
                    })
                    .collect()
            }
        };
        let schema = Schema::new(columns);
        let mut rows = Vec::with_capacity(values.rows.len());
        for row in &values.rows {
            let mut cells = Vec::with_capacity(row.len());
            for expr in row {
                cells.push(literal_value(expr)?);
            }
            rows.push(Row::new(cells));
        }
        Ok(RowBatch::new(schema, rows))
    }

    /// The schema the effect reads/writes against — the sub-pipeline's output schema for a
    /// `FROM`-bodied effect, otherwise the target node's described schema. Used to type a
    /// `RETURNING` projection.
    fn effect_input_schema(&self, effect: &EffectStmt) -> Result<Schema, EvalError> {
        if let EffectBody::Pipeline(p) = &effect.body {
            return Ok(self.fold_query(p)?.schema().clone());
        }
        let full = render_path(
            &effect
                .target
                .segments
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
        );
        match self.mounts.resolve_path(&full) {
            Some((driver, sub)) => {
                let vfs = format!("/{}/{}", driver.id().as_str(), sub);
                describe_schema(driver.as_ref(), &vfs)
            }
            None => Err(EvalError::UnroutedPath { path: full }),
        }
    }

    /// Project a schema by a list of [`Projection`]s (RFD §4 `SELECT`/`AGGREGATE`). `*`
    /// keeps the input schema; a bare `col`/`col AS a` resolves the real column type (t05
    /// `project`); a `fn(...)` expression is **typed against the function registry** (t08):
    /// the built-in's declared return type becomes the projected column type, and an
    /// unknown function / aggregate-misuse is a structured [`EvalError::Fn`]. Without a
    /// wired registry, a computed expression stays late-bound (`Unknown`, t07 behaviour).
    ///
    /// `under_aggregate` is `true` for an `AGGREGATE` projection (so a top-level aggregate
    /// `fn` is legal there and rejected in a `SELECT`).
    fn project_schema(
        &self,
        input: &Schema,
        projs: &[Projection],
        under_aggregate: bool,
    ) -> Result<Schema, EvalError> {
        // A bare `*` (alone) preserves the whole schema.
        if projs.iter().any(|p| matches!(p, Projection::Star)) && projs.len() == 1 {
            return Ok(input.clone());
        }
        let mut out = Vec::with_capacity(projs.len());
        for p in projs {
            match p {
                Projection::Star => out.extend(input.columns.clone()),
                Projection::Expr { expr, alias } => match (alias, expr) {
                    // `col` / `col AS a` → resolve the real column type (source of truth).
                    (alias, qfs_parser::Expr::Col(name)) => {
                        let col = input.project(std::slice::from_ref(name))?;
                        let mut c = col.columns.into_iter().next().unwrap_or_else(|| {
                            Column::new(name.clone(), ColumnType::Unknown, true)
                        });
                        if let Some(a) = alias {
                            c.name = a.clone();
                        }
                        out.push(c);
                    }
                    // A `fn(...)` projection → type it against the function registry (t08).
                    // The built-in's return type drives the column; an unknown/mis-contexted
                    // function is a structured error (not a silent `Unknown`).
                    (alias, qfs_parser::Expr::Fn(fnref)) => {
                        let ty = self.type_of_fn(&fnref.name, fnref.args.len(), under_aggregate)?;
                        let name = alias
                            .clone()
                            .unwrap_or_else(|| format!("expr{}", out.len()));
                        out.push(Column::new(name, ty, true));
                    }
                    // Any other computed/aliased expression → an Unknown-typed column under
                    // its alias (or a synthesised name); pure expr typing stays late-bound.
                    (Some(a), _) => out.push(Column::new(a.clone(), ColumnType::Unknown, true)),
                    (None, _) => {
                        out.push(Column::new(
                            format!("expr{}", out.len()),
                            ColumnType::Unknown,
                            true,
                        ));
                    }
                },
            }
        }
        Ok(Schema::new(out))
    }
}

// ---- Free helpers (pure) ----

/// The `Target` of a relation source (its scanned driver/path), or a synthetic empty
/// target for a non-scan relation (e.g. `VALUES`) — used to anchor a `Read` dependency.
fn source_target(rel: &PlanSource) -> Target {
    match rel {
        PlanSource::Scan { driver, path, .. } => Target::new(driver.clone(), path.clone()),
        PlanSource::Filter { input }
        | PlanSource::Project { input, .. }
        | PlanSource::Extend { input, .. }
        | PlanSource::Shape { input }
        | PlanSource::Expand { input, .. }
        | PlanSource::Codec { input, .. } => source_target(input),
        PlanSource::SetOp { lhs, .. } | PlanSource::Join { lhs, .. } => source_target(lhs),
        PlanSource::Values { .. } => Target::new(DriverId::new(""), VfsPath::new("")),
    }
}

/// Render a segment list into a `/seg/seg` mount path string for the router.
fn render_path(segments: &[Name]) -> String {
    let mut s = String::new();
    for seg in segments {
        s.push('/');
        s.push_str(seg);
    }
    if s.is_empty() {
        s.push('/');
    }
    s
}

/// The first path segment (the conventional driver namespace) of a `/seg/seg` path.
fn first_segment(path: &str) -> String {
    path.trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Describe a node's schema via the driver's **pure** `describe` (no I/O). An
/// undescribable node degrades to an empty schema rather than erroring — the path routed,
/// so the relation exists; its columns are simply late-bound (RFD §4).
fn describe_schema(driver: &dyn Driver, vfs: &str) -> Result<Schema, EvalError> {
    match driver.describe(&Path::new(vfs.to_string())) {
        Ok(desc) => Ok(desc.schema),
        // A driver that cannot describe a node yields a late-bound (empty) schema; this
        // is not a hard error (the path resolved). Keeps the evaluator total.
        Err(_) => Ok(Schema::empty()),
    }
}

/// Infer the schema of an inline `VALUES` relation from its first row (RFD §4). Explicit
/// column names are honoured; otherwise positional `col0, col1, …` names are synthesised.
/// Lower an `UPDATE … SET … WHERE …` / `REMOVE … WHERE …` body into the effect's row payload. The
/// row carries the `SET` columns (the new values) plus the equality-key columns extracted from the
/// `WHERE` (`col = <const>` leaves) — exactly what a key-addressed applier (e.g. the SQL driver)
/// splits into `SET <non-key>` + `WHERE <key>`. A bare `REMOVE … WHERE id = 1` yields a one-column
/// `[id]` row; a non-equality filter contributes no key column (the applier then rejects an
/// un-keyed whole-table write, honestly). Constants only — a non-literal `SET`/`WHERE` value is a
/// structured [`EvalError::NonLiteralValues`].
fn setwhere_row_batch(
    set: &[qfs_parser::Assignment],
    filter: Option<&Expr>,
) -> Result<RowBatch, EvalError> {
    let mut cols: Vec<Column> = Vec::new();
    let mut vals: Vec<Value> = Vec::new();
    for assign in set {
        cols.push(Column::new(assign.name.clone(), ColumnType::Unknown, true));
        vals.push(literal_value(&assign.value)?);
    }
    if let Some(f) = filter {
        for (name, value) in collect_eq_constants(f) {
            if !cols.iter().any(|c| c.name == name) {
                cols.push(Column::new(name, ColumnType::Unknown, true));
                vals.push(value);
            }
        }
    }
    Ok(RowBatch::new(Schema::new(cols), vec![Row::new(vals)]))
}

/// Collect `col = <const>` equality leaves from a `WHERE` predicate (recursing through `AND`),
/// returning each as `(column, value)`. Non-equality leaves, `OR`, and non-constant right-hand
/// sides are skipped — they carry no key the applier can address by, which the applier surfaces as
/// an honest "supply the key column(s)" rejection rather than a wrong-row write.
fn collect_eq_constants(expr: &Expr) -> Vec<(String, Value)> {
    use qfs_parser::Op;
    match expr {
        Expr::Binary {
            op: Op::And,
            lhs,
            rhs,
        } => {
            let mut out = collect_eq_constants(lhs);
            out.extend(collect_eq_constants(rhs));
            out
        }
        Expr::Binary {
            op: Op::Eq,
            lhs,
            rhs,
        } => match (lhs.as_ref(), rhs.as_ref()) {
            (Expr::Col(col), Expr::Lit(lit)) | (Expr::Lit(lit), Expr::Col(col)) => {
                vec![(col.clone(), literal_to_value(lit))]
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Evaluate one `VALUES` cell expression to a constant [`Value`]. VALUES cells are constants by
/// construction (RFD §4); a non-constant form (column ref, `fn(..)`, arithmetic) is rejected as a
/// structured [`EvalError::NonLiteralValues`] rather than silently coerced — computed rows belong
/// in an `INSERT … FROM <query>`.
fn literal_value(expr: &Expr) -> Result<Value, EvalError> {
    match expr {
        Expr::Lit(lit) => Ok(literal_to_value(lit)),
        // The lexer surfaces the bare keyword constants `true`/`false`/`null` as identifiers
        // (`Expr::Col`); in a VALUES/SET cell they are the boolean / null literal, not a column
        // reference (a real column ref in a constant cell is rejected below).
        Expr::Col(name) => match name.to_ascii_lowercase().as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            "null" => Ok(Value::Null),
            _ => Err(EvalError::NonLiteralValues {
                detail: format!("VALUES expects a constant, got column reference `{name}`"),
            }),
        },
        other => Err(EvalError::NonLiteralValues {
            detail: format!("VALUES expects a constant, got {other:?}"),
        }),
    }
}

/// Map a parser [`Literal`] to the canonical [`Value`]. `Size` lowers to its byte magnitude; a
/// `Typed` introducer (`DATE '…'`) keeps its raw inner text (the backend parses it) — both are
/// the honest, lossless lowering for the effect payload.
fn literal_to_value(lit: &Literal) -> Value {
    match lit {
        Literal::Str(s) => Value::Text(s.clone()),
        Literal::Int(n) => Value::Int(*n),
        Literal::Float(f) => Value::Float(*f),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
        Literal::Size { value, .. } => Value::Int(*value as i64),
        Literal::Typed { raw, .. } => Value::Text(raw.clone()),
    }
}

fn values_schema(values: &Values) -> Schema {
    let width = values.rows.first().map_or(0, Vec::len);
    let mut cols = Vec::with_capacity(width);
    for i in 0..width {
        let name = values
            .columns
            .as_ref()
            .and_then(|c| c.get(i).cloned())
            .unwrap_or_else(|| format!("col{i}"));
        // Literal column types are late-bound here (pure value typing is the runtime's
        // job); the schema carries the names + Unknown types so projection resolves.
        cols.push(Column::new(name, ColumnType::Unknown, true));
    }
    Schema::new(cols)
}

/// The honest affected estimate for an effect (RFD §10): an `INSERT … VALUES` of `n`
/// literal rows is `Exact(n)`; a filter-driven `UPDATE`/`REMOVE` is `Unknown` until apply.
fn write_affected(effect: &EffectStmt) -> Affected {
    match &effect.body {
        EffectBody::Values(v) => Affected::Exact(v.rows.len() as u64),
        EffectBody::Pipeline(_) | EffectBody::SetWhere { .. } => {
            if matches!(effect.verb, EffectVerb::Remove) {
                // A REMOVE over a set has an unknown count until apply.
                Affected::Unknown
            } else {
                Affected::Unknown
            }
        }
    }
}

/// Convenience: re-export the canonical AST verb → effect-kind translation so callers can
/// check the mapping without reaching into both `qfs-core` and `qfs-plan`.
#[must_use]
pub fn effect_kind_for(verb: EffectVerb) -> EffectKind {
    kind_for_verb(write_verb_for(verb))
}

/// A `CALL`-effect's [`ProcId`] from a resolved qualified name (`driver.proc`). Exposed so
/// E2 can build `Call` nodes consistently with the evaluator's identity scheme.
#[must_use]
pub fn call_proc_id(qualified: &str) -> ProcId {
    ProcId::new(qualified)
}

#[cfg(test)]
mod tests;
