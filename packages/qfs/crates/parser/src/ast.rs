//! The owned, vendor-free qfs AST (RFD-0001 §2.2 pipe-SQL, §3 closed core, §4 data
//! model). This is the **full** grammar surface (t04), promoted from the E0 spike
//! subset: every downstream subsystem (effect-plan, runtime, drivers, server DDL)
//! consumes these sum types.
//!
//! ## Closed core, structurally enforced (RFD §3)
//! The closed-core thesis — "new backend = zero new keywords" — is enforced *by the
//! shape of these enums*: [`Statement`] and [`PipeOp`] have **no** per-driver,
//! per-action variant, and they are NOT `#[non_exhaustive]` here precisely so a
//! governance test (`grammar`/`lib` tests) can lock their variant set. Everything a
//! driver contributes flows through exactly three **string-named** open seams:
//! [`PathExpr`] (the path/mount registry), [`CallRef`]/[`FnRef`] (the
//! function/procedure registry), and [`Codec`] (the codec registry). A driver can
//! never add an AST node; it can only supply a name inside one of these.
//!
//! ## Owned DTOs / no vendor leak (RFD §9)
//! Nothing here depends on winnow or any driver/vendor crate. Spans are the
//! `qfs_lang::Span` byte-range primitive (shared with the lexer); literals are owned
//! `std` types. `serde::Serialize` powers `-json` AST dumps and the golden tests.
//!
//! ## Purity (RFD §3 purity invariant)
//! The AST is **data**: it describes a statement, it does not execute one. `INSERT`
//! vs `UPSERT` is preserved as a distinct [`EffectVerb`] so the runtime can pick a
//! retry-safe verb (RFD §6); `CALL` is a plan-constructing reference node, never an
//! effect.

use qfs_lang::Span;
use serde::{Deserialize, Serialize};

/// Serialize a `qfs_lang::Span` as a `[start, end]` byte-range pair.
///
/// `qfs_lang::Span` is intentionally `serde`-free (the lexer crate stays zero-dep,
/// B7), so the AST supplies its own projection rather than adding serde to
/// `qfs-lang`. This keeps the span legible in `-json` AST dumps and golden tests
/// without leaking a serde dependency into the closed-core crate.
fn serialize_span<S>(span: &Span, ser: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeTuple;
    let mut t = ser.serialize_tuple(2)?;
    t.serialize_element(&span.start)?;
    t.serialize_element(&span.end)?;
    t.end()
}

/// Deserialize a `qfs_lang::Span` from a `[start, end]` byte-range pair (the inverse
/// of [`serialize_span`]). The AST owns this projection because `qfs_lang::Span` is
/// serde-free (the lexer crate stays zero-dep). This makes the owned AST a fully
/// round-trippable serializable value: the server-DDL deferred body (t31
/// `StatementSpec`/`PlanSpec`) is rehydrated from its serialized form WITHOUT
/// re-running the parser — so the runtime cannot hit a parse error at fire time.
fn deserialize_span<'de, D>(de: D) -> Result<Span, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let [start, end] = <[u32; 2]>::deserialize(de)?;
    Ok(Span::new(start, end))
}

/// An identifier name (a path segment, a driver/action name, a column, a codec
/// format). Always a raw string — names are *registry* concerns resolved in a later
/// semantic phase (E2), never grammar (RFD §3).
pub type Ident = String;

/// The top-level statement sum type (RFD §3). **Closed core**: exactly these five
/// forms. Not `#[non_exhaustive]` — the governance test locks this variant set so a
/// later ticket cannot smuggle in a per-driver statement form. The fifth form,
/// [`Statement::Let`], is the **deliberate** M6 functional-core addition (ticket t60):
/// it is gated by exactly the same governance tripwire as the keyword freeze (the
/// variant-count lock in `tests`), updated in step so the addition is reviewed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Statement {
    /// `FROM <source> |> op |> op …` — a pure read pipeline.
    Query(Pipeline),
    /// `INSERT/UPSERT INTO … | UPDATE … | REMOVE …` — an effect statement.
    Effect(EffectStmt),
    /// `CREATE ENDPOINT|TRIGGER|JOB|VIEW|… ` — server DDL sugar (RFD §8).
    Ddl(ServerDdl),
    /// `PREVIEW <stmt>` / `COMMIT <stmt>` — a plan wrapper (RFD §6).
    Plan(PlanWrap),
    /// `LET <name> = <pipeline>` — a relation binding (M6 functional core, ticket t60).
    ///
    /// The program model the roadmap settled on (§1.2): a program is a sequence of
    /// statements with **no terminator**, and a `LET` names an intermediate relation that
    /// stays in scope for everything after it. This is encoded as a **let-in nesting**: the
    /// `body` is the rest of the program (the next `LET`, or the final statement that uses
    /// the binding). Scoping is therefore lexical and conservative — a binding is visible to
    /// its `body` only, shadowing is allowed (an inner `LET` of the same name wins for its
    /// own `body`), and there are no recursive/forward references (`value` is resolved
    /// without `name` in scope). `value` is restricted by the grammar to a **relation**
    /// (a `Statement::Query` pipeline), never an effect — so a `LET` can never smuggle a
    /// write into a pure context (the safety floor holds trivially).
    Let {
        /// The bound name, referenced later as a bare-identifier [`Source::Name`].
        name: Ident,
        /// The bound relation — always a [`Statement::Query`] (grammar-enforced).
        value: Box<Statement>,
        /// The rest of the program, with `name` in scope.
        body: Box<Statement>,
    },
}

/// A `PREVIEW`/`COMMIT` wrapper around an inner statement (RFD §3 plan keywords).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanWrap {
    /// `true` for `COMMIT`, `false` for `PREVIEW`.
    pub commit: bool,
    /// The wrapped statement.
    pub inner: Box<Statement>,
    /// Source span of the `PREVIEW`/`COMMIT` keyword.
    #[serde(
        serialize_with = "serialize_span",
        deserialize_with = "deserialize_span"
    )]
    pub span: Span,
}

/// A read pipeline: a source followed by zero or more `|>`-separated ops.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pipeline {
    /// What the pipeline reads from.
    pub source: Source,
    /// The chain of pipe operations.
    pub ops: Vec<PipeOp>,
}

/// The source of a pipeline (RFD §2.2). Either a `/driver/...` path, an inline
/// `VALUES` block, or a parenthesised sub-pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Source {
    /// `/driver/seg/seg` (the open path registry).
    Path(PathExpr),
    /// `FROM VALUES (..),(..)` — an inline literal relation.
    Values(Values),
    /// `FROM ( <pipeline> )` — a sub-query.
    Subquery(Box<Pipeline>),
    /// `FROM <name>` — a bare identifier naming a `LET`-bound relation (M6, ticket t60).
    /// Unresolved here (the parser validates shape only); a name with no matching binding
    /// in scope is a structured resolve/eval error, never a silent empty relation.
    Name(Ident),
}

/// One pipe operation following `|>` (RFD §3 query/transform + codec + call).
///
/// **Closed core**: exactly one variant per closed-core query/transform keyword,
/// plus the three registry seams ([`PipeOp::Decode`]/[`PipeOp::Encode`] = codec
/// registry, [`PipeOp::Call`] = procedure registry). There is deliberately **no**
/// per-action variant (no `Send`, no `Merge`): those are pure registry functions
/// that desugar to `CALL` (RFD §3). The governance test locks this variant set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PipeOp {
    /// `WHERE <expr>`
    Where(Expr),
    /// `SELECT <proj>, …`
    Select(Vec<Projection>),
    /// `EXTEND <name> = <expr>, …` (add columns, keep the rest).
    Extend(Vec<Assignment>),
    /// `SET <name> = <expr>, …` (overwrite columns in place).
    Set(Vec<Assignment>),
    /// `AGGREGATE <expr> [AS <name>], …` (the aggregate projections).
    Aggregate(Vec<Projection>),
    /// `GROUP BY <expr>, …`
    GroupBy(Vec<Expr>),
    /// `ORDER BY <expr> [ASC|DESC], …` — modelled as expr + descending flag.
    OrderBy(Vec<OrderKey>),
    /// `LIMIT <n>`
    Limit(i64),
    /// `DISTINCT`
    Distinct,
    /// `JOIN <source> ON <expr>`
    Join(JoinOp),
    /// `UNION <pipeline>`
    Union(Box<Pipeline>),
    /// `EXCEPT <pipeline>`
    Except(Box<Pipeline>),
    /// `INTERSECT <pipeline>`
    Intersect(Box<Pipeline>),
    /// `AS <alias>` (name the current relation).
    As(Ident),
    /// `EXPAND <field>` (explode a nested collection into rows, RFD §4).
    Expand(PathRef),
    /// `DECODE <fmt>` (codec registry seam, RFD §4).
    Decode(Codec),
    /// `ENCODE <fmt>` (codec registry seam, RFD §4).
    Encode(Codec),
    /// `CALL driver.action(args)` (procedure registry seam, RFD §3).
    Call(CallRef),
}

/// A `JOIN <source> ON <expr>` operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JoinOp {
    /// The joined relation.
    pub source: Source,
    /// The `ON` predicate.
    pub on: Expr,
}

/// One `ORDER BY` sort key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderKey {
    /// The sort expression.
    pub expr: Expr,
    /// `true` for `DESC`, `false` for the `ASC` default.
    pub descending: bool,
}

/// One `SELECT`/`AGGREGATE` projection: an expression with an optional `AS` alias,
/// or a bare `*`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Projection {
    /// `*` — project everything.
    Star,
    /// `<expr> [AS <alias>]`
    Expr {
        /// The projected expression.
        expr: Expr,
        /// An optional `AS <alias>`.
        alias: Option<Ident>,
    },
}

/// One `EXTEND`/`SET` assignment: `<name> = <expr>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    /// The target column name.
    pub name: Ident,
    /// The value expression.
    pub value: Expr,
}

/// An effect statement (RFD §3 effects). `INSERT`/`UPSERT` are kept distinct via
/// [`EffectVerb`] so the runtime can choose a retry-safe verb (RFD §6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectStmt {
    /// Which effect verb (`INSERT`/`UPSERT`/`UPDATE`/`REMOVE`).
    pub verb: EffectVerb,
    /// The target path the effect writes to.
    pub target: PathExpr,
    /// The data being written (`VALUES`, a sub-pipeline, or `SET`/`WHERE` clauses).
    pub body: EffectBody,
    /// An optional `RETURNING <expr>, …` projection.
    pub returning: Option<Vec<Projection>>,
}

/// The effect verb. `Insert` and `Upsert` are distinct (idempotency, RFD §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectVerb {
    /// `INSERT INTO`
    Insert,
    /// `UPSERT INTO`
    Upsert,
    /// `UPDATE`
    Update,
    /// `REMOVE`
    Remove,
}

/// The data portion of an effect statement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EffectBody {
    /// `VALUES (..),(..)` — inline literal rows.
    Values(Values),
    /// A sub-pipeline source (`INSERT INTO x FROM y |> …`).
    Pipeline(Box<Pipeline>),
    /// `UPDATE … SET a = b [WHERE …]` — column assignments + optional filter.
    SetWhere {
        /// The `SET` assignments (empty for a bare `REMOVE`).
        set: Vec<Assignment>,
        /// An optional `WHERE` filter.
        filter: Option<Expr>,
    },
}

/// An inline `VALUES` relation: an optional column list plus one or more rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Values {
    /// Optional explicit column names: `VALUES (a, b) (1, 2)`.
    pub columns: Option<Vec<Ident>>,
    /// The literal rows; each row is a list of expressions.
    pub rows: Vec<Vec<Expr>>,
}

/// A server-DDL statement (RFD §8). Each form is **sugar** that desugars downstream
/// to `INSERT INTO /server/...`; the [`ServerDdl::target`] records that path. The
/// parser only validates shape — desugaring lives in a later epic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerDdl {
    /// Which DDL kind (`ENDPOINT`/`TRIGGER`/`JOB`/`VIEW`/…).
    pub kind: DdlKind,
    /// The handler/object name.
    pub name: Ident,
    /// The `/server/...` path this DDL desugars to (RFD §8).
    pub target: Vec<Ident>,
    /// The optional `DO <plan>` clause (the effect-plan body).
    pub do_plan: Option<Box<Statement>>,
    /// The optional `AS <query>` clause (the backing query for `ENDPOINT`/`VIEW`).
    pub as_query: Option<Box<Statement>>,
    /// The optional `WHERE <pred>` clause (the trigger guard for `TRIGGER`, t34). `WHERE` is a
    /// frozen keyword, so wiring `CREATE TRIGGER … ON <event> WHERE <pred> DO <plan>` adds NO new
    /// keyword — the clause is captured here as a `Statement::Query` wrapping the predicate over an
    /// empty `VALUES` source (so it round-trips through the downstream `StatementSpec`).
    pub where_pred: Option<Box<Statement>>,
    /// The optional `EVERY <interval>` clause (cron interval for `JOB`).
    pub every: Option<String>,
    /// The optional `ON <event>` clause (trigger event / route).
    pub on: Option<String>,
    /// The `ALLOW`/`DENY` rule clauses of a `CREATE POLICY` form (t35). Empty for every
    /// non-POLICY DDL. `ALLOW`/`DENY` are **not** frozen keywords (RFD §3 freeze) — they are
    /// parsed as contextual UPPERCASE identifiers within the POLICY form (the t31 `AT` lesson,
    /// see the grammar), so wiring `CREATE POLICY … ALLOW … DENY …` adds NO new keyword.
    #[serde(default)]
    pub policy_rules: Vec<PolicyRuleAst>,
    /// The optional `POLICY <name>` **attachment** clause (t35): the `/server/policies` row a
    /// binding (`ENDPOINT`/`TRIGGER`/`JOB`/…) commits its fired plan under (least privilege).
    /// `POLICY` IS a frozen keyword, so this adds none. `None` = no policy attached (fail-closed
    /// default-deny at fire time). Distinct from `policy_rules`, which are the rule body of a
    /// `CREATE POLICY` statement itself.
    #[serde(default)]
    pub policy: Option<String>,
}

/// One `ALLOW`/`DENY` rule clause inside a `CREATE POLICY` form (t35). A shape-only AST node —
/// the verb-set / driver-glob semantics live in `qfs-server::policy`. Owned, vendor-free.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyRuleAst {
    /// Whether this is an `ALLOW` (`true`) or `DENY` (`false`) rule.
    pub allow: bool,
    /// The verb tokens (`SELECT`/`INSERT`/`UPSERT`/`UPDATE`/`REMOVE`/`CALL`), uppercased — or
    /// a single `ALL` token captured here as the literal `"ALL"`.
    pub verbs: Vec<String>,
    /// Whether the verb list was the bare `ALL` token (vs an explicit list).
    pub all_token: bool,
    /// The optional `ON <driver-glob>` scope (e.g. `mail`, `s3/*`); `None` = every driver.
    pub driver: Option<String>,
}

/// The kind of a server-DDL statement (RFD §8). Frozen, driver-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DdlKind {
    /// `CREATE ENDPOINT`
    Endpoint,
    /// `CREATE TRIGGER`
    Trigger,
    /// `CREATE JOB`
    Job,
    /// `CREATE VIEW`
    View,
    /// `CREATE MATERIALIZED VIEW`
    MaterializedView,
    /// `CREATE WEBHOOK`
    Webhook,
    /// `CREATE POLICY`
    Policy,
}

/// An expression (RFD §3 operators, frozen). The boolean structure (`AND`/`OR`/
/// `NOT`) and comparison/predicate forms are all closed core; the only open seam is
/// [`Expr::Fn`] (the function registry) and column/path references.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    /// A literal value.
    Lit(Literal),
    /// A bare column reference (a single identifier).
    Col(Ident),
    /// A struct-navigation path `a.b.c` (RFD §4 path access, no flattening).
    Path(Vec<Ident>),
    /// A registry function call `fn(args)` (the function registry seam, RFD §3).
    Fn(FnRef),
    /// A binary operation `<lhs> <op> <rhs>` (comparison / logical).
    Binary {
        /// The operator.
        op: Op,
        /// Left operand.
        lhs: Box<Expr>,
        /// Right operand.
        rhs: Box<Expr>,
    },
    /// A unary operation (`NOT <expr>`).
    Unary {
        /// The unary operator.
        op: Op,
        /// The operand.
        expr: Box<Expr>,
    },
    /// `<expr> IN (<list>)`.
    In {
        /// The tested expression.
        expr: Box<Expr>,
        /// The candidate set.
        set: Vec<Expr>,
    },
    /// `<expr> BETWEEN <low> AND <high>`.
    Between {
        /// The tested expression.
        expr: Box<Expr>,
        /// Lower bound (inclusive).
        low: Box<Expr>,
        /// Upper bound (inclusive).
        high: Box<Expr>,
    },
    /// `<expr> LIKE <pattern>`.
    Like {
        /// The tested expression.
        expr: Box<Expr>,
        /// The LIKE pattern.
        pattern: Box<Expr>,
    },
    /// `<expr> <op> ANY (<set>)` — the quantified comparison (RFD §3 `ANY`).
    AnyOp {
        /// The comparison operator applied against the set.
        op: Op,
        /// The tested expression.
        expr: Box<Expr>,
        /// The candidate set.
        set: Vec<Expr>,
    },
}

/// The frozen operator set (RFD §3). No operator can be added without editing this
/// enum (and the keyword/operator freeze tests in `qfs-lang`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    Le,
    /// `>=`
    Ge,
    /// `AND`
    And,
    /// `OR`
    Or,
    /// `NOT`
    Not,
    /// `LIKE`
    Like,
    /// `~` (regex match)
    Match,
}

/// A literal value (RFD §4 data model).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Literal {
    /// A string literal.
    Str(String),
    /// An integer literal.
    Int(i64),
    /// A floating-point literal.
    Float(f64),
    /// A boolean literal.
    Bool(bool),
    /// The null literal.
    Null,
    /// A size literal (`25 MB`): magnitude + canonical unit text.
    Size {
        /// The numeric magnitude.
        value: u64,
        /// The unit text (`B`/`KB`/`MB`/`GB`/`TB`).
        unit: String,
    },
    /// A typed literal (`DATE '…'`): the introducer keyword text + raw inner string.
    Typed {
        /// The introducer (`DATE`/`TIME`/`TIMESTAMP`).
        ty: String,
        /// The raw, unvalidated inner string content.
        raw: String,
    },
}

/// A `/driver/seg/seg` path expression — the open path/mount registry seam (RFD §3,
/// §4). Driver and segments are raw strings; `@version` / `AS OF` are temporal
/// coordinates (RFD §4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathExpr {
    /// The path segments (raw text; first is conventionally the driver).
    pub segments: Vec<PathSegment>,
    /// An optional `AS OF '<ts>'` temporal coordinate (RFD §4).
    pub as_of: Option<String>,
    /// Source span of the whole path.
    #[serde(
        serialize_with = "serialize_span",
        deserialize_with = "deserialize_span"
    )]
    pub span: Span,
}

/// One segment of a [`PathExpr`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathSegment {
    /// The raw segment name.
    pub name: Ident,
    /// An optional `@version` ref bound to this segment (RFD §4), raw text.
    pub version: Option<String>,
    /// Whether the segment carried a glob character.
    pub glob: bool,
}

/// A path reference used in expression position (e.g. the target of `EXPAND`), where
/// the path is dotted struct navigation rather than a `/driver/...` mount path.
pub type PathRef = Vec<Ident>;

/// A `CALL driver.action(args)` reference — the procedure registry seam (RFD §3).
/// All names are strings resolved later; the parser validates *shape* only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallRef {
    /// The driver namespace (e.g. `mail`).
    pub driver: Ident,
    /// The action name (e.g. `send`).
    pub action: Ident,
    /// The named/positional arguments.
    pub args: Vec<NamedArg>,
    /// Source span of the call.
    #[serde(
        serialize_with = "serialize_span",
        deserialize_with = "deserialize_span"
    )]
    pub span: Span,
}

/// One argument to a [`CallRef`]: either positional or `name => value`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedArg {
    /// The argument name, if given as `name => value`.
    pub name: Option<Ident>,
    /// The argument value.
    pub value: Expr,
}

/// A `fn(args)` registry function reference — the function registry seam (RFD §3).
/// The name is a string resolved later (receiver-typed alias resolution is E2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FnRef {
    /// The function name.
    pub name: Ident,
    /// The positional arguments.
    pub args: Vec<Expr>,
    /// Source span of the call.
    #[serde(
        serialize_with = "serialize_span",
        deserialize_with = "deserialize_span"
    )]
    pub span: Span,
}

/// A `DECODE fmt` / `ENCODE fmt` codec reference — the codec registry seam (RFD §4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Codec {
    /// The codec format name (`json`/`yaml`/`csv`/…), resolved later.
    pub fmt: Ident,
    /// Source span of the codec format token.
    #[serde(
        serialize_with = "serialize_span",
        deserialize_with = "deserialize_span"
    )]
    pub span: Span,
}
