//! Internal tests for `cfs-driver-sql` (t17). The **real** backend is an in-process / temp-file
//! SQLite via `rusqlite` (bundled = vendored C SQLite, no external server) — it proves catalog
//! introspection, parameterized SELECT/DML, injection safety, and ACID transaction commit/rollback
//! against a live engine. The postgres/mysql paths are identical compiled-SQL text and are covered
//! by per-dialect **golden SQL string** tests (no live server). Secret safety is asserted with a
//! planted-canary test over every `SqlError` surface.

use std::sync::{Arc, Mutex};

use cfs_driver::{check_capability, Driver, Path, PushdownProfile, Verb};
use cfs_plan::{DriverId as PlanDriverId, EffectKind, EffectNode, NodeId, Target, VfsPath};
use cfs_secrets::{
    AccountId, CredentialKey, DriverId as SecretDriverId, InMemoryStore, Secret, Secrets,
};
use cfs_types::{
    CmpOp, ColRef, Column, ColumnType, Literal, Predicate, Row, RowBatch, Schema, Value,
};

use crate::catalog::{Catalog, ColumnDef, RelationKind, TableCatalog};
use crate::conn::{resolve_dialect, ConnHandle, ConnRegistry, SqlBackend};
use crate::dialect::Dialect;
use crate::emit::{render_dml, render_select, DmlOp, OrderTerm, Param, SelectPlan, SqlPredicate};
use crate::{compile, QuerySpec, SqlApplier, SqlDriver, SqlError, SqlPath};

// ----------------------------------------------------------------------------------------------
// The real rusqlite-backed `SqlBackend` test impl.
// ----------------------------------------------------------------------------------------------

/// A live in-process SQLite backend wrapping a `rusqlite::Connection` behind a `Mutex` (the
/// connection is `!Sync`; the mutex gives the `Send + Sync` the trait requires). It is the REAL
/// engine the ACID / injection / introspection tests run against — no external server.
struct SqliteBackend {
    conn: Mutex<rusqlite::Connection>,
}

impl SqliteBackend {
    /// Open an in-memory database and run `setup` DDL/seed against it.
    fn in_memory(setup: &str) -> Arc<Self> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(setup).unwrap();
        Arc::new(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Count the rows in a table (a test helper, not part of the trait).
    fn count(&self, table: &str) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| {
            r.get(0)
        })
        .unwrap()
    }
}

/// Bind a `&[Param]` onto a rusqlite statement positionally — the proof that every value is BOUND,
/// never interpolated. The placeholder text in `sql` is `?`; rusqlite binds these positionally.
fn bind_params<'a>(params: &'a [Param]) -> Vec<Box<dyn rusqlite::ToSql + 'a>> {
    params
        .iter()
        .map(|p| -> Box<dyn rusqlite::ToSql + 'a> {
            match p {
                Param::Null => Box::new(rusqlite::types::Null),
                Param::Bool(b) => Box::new(*b),
                Param::Int(n) => Box::new(*n),
                Param::Float(f) => Box::new(*f),
                Param::Text(t) => Box::new(t.as_str()),
                Param::Bytes(b) => Box::new(b.as_slice()),
            }
        })
        .collect()
}

impl SqlBackend for SqliteBackend {
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    fn introspect(&self) -> Result<Catalog, SqlError> {
        let conn = self.conn.lock().unwrap();
        // Enumerate tables and views from sqlite_master.
        let mut tables = Vec::new();
        let mut stmt = conn
            .prepare("SELECT name, type FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .map_err(|e| SqlError::backend("sqlite", "introspect", e.to_string()))?;
        let rels: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| SqlError::backend("sqlite", "introspect", e.to_string()))?
            .collect::<Result<_, _>>()
            .map_err(|e| SqlError::backend("sqlite", "introspect", e.to_string()))?;
        drop(stmt);

        for (name, kind) in rels {
            let mut cols = Vec::new();
            let mut info = conn
                .prepare(&format!("PRAGMA table_info(\"{name}\")"))
                .map_err(|e| SqlError::backend("sqlite", "introspect", e.to_string()))?;
            // PRAGMA table_info columns: cid, name, type, notnull, dflt_value, pk.
            let rows: Vec<(String, String, i64, i64)> = info
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(5)?,
                    ))
                })
                .map_err(|e| SqlError::backend("sqlite", "introspect", e.to_string()))?
                .collect::<Result<_, _>>()
                .map_err(|e| SqlError::backend("sqlite", "introspect", e.to_string()))?;
            drop(info);
            for (col_name, ty, notnull, pk) in rows {
                let mapped = Dialect::Sqlite.map_type(&ty);
                let is_pk = pk > 0;
                cols.push(ColumnDef::new(col_name, mapped, notnull == 0, is_pk, is_pk));
            }
            let relkind = if kind == "view" {
                RelationKind::View
            } else {
                RelationKind::Table
            };
            tables.push(TableCatalog::new(name, relkind, cols));
        }
        Ok(Catalog::new(tables))
    }

    fn execute_read(&self, sql: &str, params: &[Param]) -> Result<Vec<Row>, SqlError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| SqlError::backend("sqlite", "select", e.to_string()))?;
        let col_count = stmt.column_count();
        let boxed = bind_params(params);
        let refs: Vec<&dyn rusqlite::ToSql> = boxed.iter().map(|b| b.as_ref()).collect();
        let mut rows = stmt
            .query(refs.as_slice())
            .map_err(|e| SqlError::backend("sqlite", "select", e.to_string()))?;
        let mut out = Vec::new();
        while let Some(r) = rows
            .next()
            .map_err(|e| SqlError::backend("sqlite", "select", e.to_string()))?
        {
            let mut values = Vec::with_capacity(col_count);
            for i in 0..col_count {
                let v = r
                    .get_ref(i)
                    .map_err(|e| SqlError::backend("sqlite", "select", e.to_string()))?;
                values.push(sqlite_value_to_cfs(v));
            }
            out.push(Row::new(values));
        }
        Ok(out)
    }

    fn commit_transaction(&self, ops: &[DmlOp]) -> Result<u64, SqlError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction()
            .map_err(|e| SqlError::backend("sqlite", "begin", e.to_string()))?;
        let mut affected = 0u64;
        for op in ops {
            let (sql, params) = render_dml(Dialect::Sqlite, op);
            let boxed = bind_params(&params);
            let refs: Vec<&dyn rusqlite::ToSql> = boxed.iter().map(|b| b.as_ref()).collect();
            // On ANY error the `tx` is dropped without commit → automatic ROLLBACK (zero rows
            // changed), satisfying the all-or-nothing guarantee.
            let n = tx
                .execute(&sql, refs.as_slice())
                .map_err(|e| SqlError::backend("sqlite", "dml", e.to_string()))?;
            affected += n as u64;
        }
        tx.commit()
            .map_err(|e| SqlError::backend("sqlite", "commit", e.to_string()))?;
        Ok(affected)
    }
}

/// Convert a rusqlite value reference into the canonical cfs [`Value`] (the owned-DTO boundary —
/// no rusqlite type crosses past here).
fn sqlite_value_to_cfs(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(n) => Value::Int(n),
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Bytes(b.to_vec()),
    }
}

/// Build a driver over one in-memory sqlite connection named `db`.
fn driver_over(setup: &str) -> (SqlDriver, Arc<SqliteBackend>) {
    let backend = SqliteBackend::in_memory(setup);
    let handle = ConnHandle::new(backend.clone() as Arc<dyn SqlBackend>).unwrap();
    let registry = ConnRegistry::new().with("db", handle);
    (SqlDriver::new(registry), backend)
}

const USERS_DDL: &str = "
    CREATE TABLE users (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        age INTEGER,
        active BOOLEAN
    );
    INSERT INTO users (id, name, age, active) VALUES (1, 'alice', 30, 1);
    INSERT INTO users (id, name, age, active) VALUES (2, 'bob', 25, 0);
    INSERT INTO users (id, name, age, active) VALUES (3, 'carol', 40, 1);
    CREATE VIEW adults AS SELECT id, name FROM users WHERE age >= 18;
";

// ----------------------------------------------------------------------------------------------
// 1. Catalog introspection -> DESCRIBE.
// ----------------------------------------------------------------------------------------------

#[test]
fn describe_returns_catalog_schema_with_types_and_pk() {
    let (driver, _be) = driver_over(USERS_DDL);
    let desc = driver.describe(&Path::new("/sql/db/users")).unwrap();
    assert_eq!(desc.archetype, cfs_driver::Archetype::RelationalTable);
    // Columns + mapped types.
    let schema = &desc.schema;
    assert_eq!(schema.column("id").unwrap().ty, ColumnType::Int);
    assert_eq!(schema.column("name").unwrap().ty, ColumnType::Text);
    assert_eq!(schema.column("age").unwrap().ty, ColumnType::Int);
    assert_eq!(schema.column("active").unwrap().ty, ColumnType::Bool);
    // NOT NULL name is non-nullable; age is nullable.
    assert!(!schema.column("name").unwrap().nullable);
    assert!(schema.column("age").unwrap().nullable);
    // Provenance carries the sql driver + source column.
    let id_prov = &schema.column("id").unwrap().provenance;
    assert_eq!(id_prov.driver.as_ref().unwrap().as_str(), "sql");
    assert_eq!(id_prov.source_col.as_deref(), Some("id"));

    // PK is recorded in the catalog (drives upsert conflict target / reversibility).
    let (_s, cat) = driver.resolve_table(&Path::new("/sql/db/users")).unwrap();
    assert!(cat.column("id").unwrap().pk);
    assert_eq!(cat.key_columns().len(), 1);
    assert_eq!(cat.key_columns()[0].name, "id");
}

#[test]
fn describe_unknown_table_is_structured_error() {
    let (driver, _be) = driver_over(USERS_DDL);
    let err = driver.describe(&Path::new("/sql/db/nope")).unwrap_err();
    assert_eq!(err.code(), "invalid_path");
}

// ----------------------------------------------------------------------------------------------
// 2. SELECT with WHERE/ORDER/LIMIT compiled to parameterized SQL -> rows.
// ----------------------------------------------------------------------------------------------

#[test]
fn select_where_order_limit_compiles_and_returns_rows() {
    let (driver, _be) = driver_over(USERS_DDL);
    // SELECT name FROM users WHERE active = true ORDER BY age DESC LIMIT 1  → carol (40).
    let spec = QuerySpec::new(vec!["name".to_string()])
        .with_predicate(Predicate::Cmp(
            ColRef::col("active"),
            CmpOp::Eq,
            Literal::Bool(true),
        ))
        .order_by("age", true)
        .with_limit(1);
    let (rows, residual) = driver
        .execute_query(&Path::new("/sql/db/users"), &spec)
        .unwrap();
    assert!(residual.is_none(), "an exact `=` pushes down fully");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Text("carol".to_string()));
}

#[test]
fn select_in_and_between_push_down_exactly() {
    let (driver, _be) = driver_over(USERS_DDL);
    // WHERE age BETWEEN 26 AND 45 AND id IN (1, 3)  → alice(30,id1), carol(40,id3).
    let spec = QuerySpec::new(vec!["name".to_string()]).with_predicate(Predicate::And(
        Box::new(Predicate::Between(
            ColRef::col("age"),
            Literal::Int(26),
            Literal::Int(45),
        )),
        Box::new(Predicate::In(
            ColRef::col("id"),
            vec![Literal::Int(1), Literal::Int(3)],
        )),
    ));
    let (mut rows, residual) = driver
        .execute_query(&Path::new("/sql/db/users"), &spec)
        .unwrap();
    assert!(residual.is_none());
    let mut names: Vec<String> = rows
        .drain(..)
        .map(|r| match &r.values[0] {
            Value::Text(t) => t.clone(),
            _ => panic!("expected text"),
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["alice".to_string(), "carol".to_string()]);
}

#[test]
fn like_predicate_is_kept_as_truthful_residual() {
    let (driver, _be) = driver_over(USERS_DDL);
    // LIKE glob semantics differ from SQL LIKE: the driver KEEPS the predicate as a residual and
    // pushes only the table scan, so the engine re-filters. No WHERE renders for the LIKE leaf.
    let spec = QuerySpec::new(vec!["name".to_string()]).with_predicate(Predicate::Like(
        ColRef::col("name"),
        cfs_types::Pattern("a%".to_string()),
    ));
    let (rows, residual) = driver
        .execute_query(&Path::new("/sql/db/users"), &spec)
        .unwrap();
    assert!(
        residual.is_some(),
        "LIKE is not faithfully renderable here → kept as residual (engine re-filters)"
    );
    // The driver over-fetched ALL rows (no WHERE pushed), so the engine has the full set to filter.
    assert_eq!(rows.len(), 3);
}

#[test]
fn mixed_and_pushes_exact_half_keeps_residual_half() {
    let (driver, _be) = driver_over(USERS_DDL);
    // `age > 20 AND name ~ 'a'`: the `>` pushes down exactly; the `~` regex is not portable, kept.
    let spec = QuerySpec::new(vec!["name".to_string()]).with_predicate(Predicate::And(
        Box::new(Predicate::Cmp(
            ColRef::col("age"),
            CmpOp::Gt,
            Literal::Int(20),
        )),
        Box::new(Predicate::Cmp(
            ColRef::col("name"),
            CmpOp::Match,
            Literal::Text("a".to_string()),
        )),
    ));
    let cat = driver.resolve_table(&Path::new("/sql/db/users")).unwrap().1;
    let result = compile::compile("", &cat, &spec).unwrap();
    // The compiled WHERE has exactly the `>` leaf; the `~` is the residual.
    assert!(matches!(
        result.plan.where_,
        Some(SqlPredicate::Cmp { op: CmpOp::Gt, .. })
    ));
    match result.residual {
        Some(Predicate::Cmp(_, CmpOp::Match, _)) => {}
        other => panic!("expected the ~ predicate kept as residual, got {other:?}"),
    }
}

// ----------------------------------------------------------------------------------------------
// 3 + 4. INSERT/UPDATE/DELETE effects + injection safety (against the live sqlite DB).
// ----------------------------------------------------------------------------------------------

/// Build an effect node carrying one row payload for a `/sql/db/users` write.
fn write_node(id: u32, kind: EffectKind, schema: Schema, values: Vec<Value>) -> EffectNode {
    let target = Target::new(PlanDriverId::new("sql"), VfsPath::new("/sql/db/users"));
    let batch = RowBatch::new(schema, vec![Row::new(values)]);
    EffectNode::new(NodeId(id), kind, target).with_args(batch)
}

fn users_full_schema() -> Schema {
    Schema::new(vec![
        Column::new("id", ColumnType::Int, false),
        Column::new("name", ColumnType::Text, false),
        Column::new("age", ColumnType::Int, true),
        Column::new("active", ColumnType::Bool, true),
    ])
}

#[test]
fn insert_update_delete_effects_change_the_right_rows() {
    use cfs_runtime::SharedApplier;
    let (driver, backend) = driver_over(USERS_DDL);
    let applier = SqlApplier::new(driver.registry().clone());

    // INSERT a new user (id 4).
    let insert = write_node(
        1,
        EffectKind::Insert,
        users_full_schema(),
        vec![
            Value::Int(4),
            Value::Text("dave".to_string()),
            Value::Int(50),
            Value::Bool(true),
        ],
    );
    let out = applier.apply_shared(&insert).unwrap();
    assert_eq!(out.affected, 1);
    assert_eq!(backend.count("users"), 4);

    // UPDATE: SET name where id=4 (key column id forms the WHERE, non-key cols are SET).
    let update = write_node(
        2,
        EffectKind::Update,
        users_full_schema(),
        vec![
            Value::Int(4),
            Value::Text("david".to_string()),
            Value::Int(51),
            Value::Bool(false),
        ],
    );
    applier.apply_shared(&update).unwrap();
    let (rows, _) = driver
        .execute_query(
            &Path::new("/sql/db/users"),
            &QuerySpec::new(vec!["name".to_string()]).with_predicate(Predicate::Cmp(
                ColRef::col("id"),
                CmpOp::Eq,
                Literal::Int(4),
            )),
        )
        .unwrap();
    assert_eq!(rows[0].values[0], Value::Text("david".to_string()));

    // DELETE (REMOVE) the row with id=4 (key filter from the row payload).
    let remove = write_node(
        3,
        EffectKind::Remove,
        Schema::new(vec![Column::new("id", ColumnType::Int, false)]),
        vec![Value::Int(4)],
    );
    applier.apply_shared(&remove).unwrap();
    assert_eq!(backend.count("users"), 3);
}

#[test]
fn injection_attempt_is_bound_as_a_parameter_not_executed() {
    use cfs_runtime::SharedApplier;
    let (driver, backend) = driver_over(USERS_DDL);
    let applier = SqlApplier::new(driver.registry().clone());

    // The classic injection payload as a NAME value. If it were string-interpolated, it would drop
    // the table. Because every value is BOUND, the literal lands as data and `users` survives.
    let evil = "'; DROP TABLE users; --";
    let insert = write_node(
        1,
        EffectKind::Insert,
        users_full_schema(),
        vec![
            Value::Int(99),
            Value::Text(evil.to_string()),
            Value::Int(1),
            Value::Bool(true),
        ],
    );
    applier.apply_shared(&insert).unwrap();

    // The table still exists (4 rows now), and the evil string is stored verbatim as data.
    assert_eq!(backend.count("users"), 4);
    let (rows, _) = driver
        .execute_query(
            &Path::new("/sql/db/users"),
            &QuerySpec::new(vec!["name".to_string()]).with_predicate(Predicate::Cmp(
                ColRef::col("id"),
                CmpOp::Eq,
                Literal::Int(99),
            )),
        )
        .unwrap();
    assert_eq!(rows[0].values[0], Value::Text(evil.to_string()));

    // Also prove injection-safety on the READ path: a WHERE value carrying a quote is bound.
    let (rows2, _) = driver
        .execute_query(
            &Path::new("/sql/db/users"),
            &QuerySpec::new(vec!["id".to_string()]).with_predicate(Predicate::Cmp(
                ColRef::col("name"),
                CmpOp::Eq,
                Literal::Text(evil.to_string()),
            )),
        )
        .unwrap();
    assert_eq!(rows2.len(), 1);
    assert_eq!(rows2[0].values[0], Value::Int(99));
    // The table is STILL there after the read with the evil bound value.
    assert_eq!(backend.count("users"), 4);
}

// ----------------------------------------------------------------------------------------------
// 5. Capability gating (table vs view).
// ----------------------------------------------------------------------------------------------

#[test]
fn view_is_select_only_table_is_full_crud() {
    let (driver, _be) = driver_over(USERS_DDL);
    let table = Path::new("/sql/db/users");
    let view = Path::new("/sql/db/adults");

    // Table: full CRUD passes the gate.
    for v in [
        Verb::Select,
        Verb::Insert,
        Verb::Upsert,
        Verb::Update,
        Verb::Remove,
    ] {
        assert!(
            check_capability(&driver, &table, v).is_ok(),
            "table allows {v:?}"
        );
    }
    // View: SELECT allowed; every write rejected structurally at the parse-time gate.
    assert!(check_capability(&driver, &view, Verb::Select).is_ok());
    for v in [Verb::Insert, Verb::Update, Verb::Remove] {
        let err = check_capability(&driver, &view, v).unwrap_err();
        assert_eq!(err.code(), "unsupported_verb");
    }
}

#[test]
fn applier_rejects_write_to_a_view_belt_and_suspenders() {
    use cfs_runtime::SharedApplier;
    let (driver, _be) = driver_over(USERS_DDL);
    let applier = SqlApplier::new(driver.registry().clone());
    // A hand-built plan that bypassed the parse-time gate is still rejected at apply.
    let target = Target::new(PlanDriverId::new("sql"), VfsPath::new("/sql/db/adults"));
    let batch = RowBatch::new(
        Schema::new(vec![Column::new("id", ColumnType::Int, false)]),
        vec![Row::new(vec![Value::Int(7)])],
    );
    let node = EffectNode::new(NodeId(1), EffectKind::Insert, target).with_args(batch);
    let err = applier.apply_shared(&node).unwrap_err();
    assert_eq!(err.code(), "capability_denied");
}

// ----------------------------------------------------------------------------------------------
// 6. Transaction commit / rollback (ACID, against the live sqlite engine).
// ----------------------------------------------------------------------------------------------

#[test]
fn multi_effect_commit_is_atomic_and_rolls_back_on_failure() {
    let (_driver, backend) = driver_over(USERS_DDL);

    // A clean two-INSERT commit is atomic: both land.
    let ok = vec![
        DmlOp::Insert {
            schema: String::new(),
            table: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
            values: vec![Param::Int(10), Param::Text("x".to_string())],
        },
        DmlOp::Insert {
            schema: String::new(),
            table: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
            values: vec![Param::Int(11), Param::Text("y".to_string())],
        },
    ];
    let affected = backend.commit_transaction(&ok).unwrap();
    assert_eq!(affected, 2);
    assert_eq!(backend.count("users"), 5);

    // A commit whose SECOND effect violates the PK (id 10 already exists) rolls back the FIRST:
    // zero rows changed (the count stays 5, not 6).
    let bad = vec![
        DmlOp::Insert {
            schema: String::new(),
            table: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
            values: vec![Param::Int(20), Param::Text("z".to_string())],
        },
        DmlOp::Insert {
            schema: String::new(),
            table: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
            // Duplicate PK 10 → constraint violation mid-transaction.
            values: vec![Param::Int(10), Param::Text("dup".to_string())],
        },
    ];
    let err = backend.commit_transaction(&bad).unwrap_err();
    assert_eq!(err.code(), "backend");
    assert_eq!(
        backend.count("users"),
        5,
        "ROLLBACK: the first INSERT of the failed transaction left zero rows changed"
    );
}

#[test]
fn upsert_is_retry_safe_running_twice_yields_one_row() {
    let (_driver, backend) = driver_over(USERS_DDL);
    let upsert = DmlOp::Upsert {
        schema: String::new(),
        table: "users".to_string(),
        columns: vec!["id".to_string(), "name".to_string()],
        values: vec![Param::Int(1), Param::Text("ALICE".to_string())],
        conflict_keys: vec!["id".to_string()],
    };
    // Run twice; both succeed (idempotent) and the row count is unchanged (id 1 already existed).
    backend
        .commit_transaction(std::slice::from_ref(&upsert))
        .unwrap();
    backend.commit_transaction(&[upsert]).unwrap();
    assert_eq!(
        backend.count("users"),
        3,
        "UPSERT did not insert a duplicate"
    );
    // And the value was updated to the upserted name.
    let conn = backend.conn.lock().unwrap();
    let name: String = conn
        .query_row("SELECT name FROM users WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(name, "ALICE");
}

// ----------------------------------------------------------------------------------------------
// 7. End-to-end through the runtime interpreter + bridge.
// ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn end_to_end_insert_through_runtime_bridge() {
    use cfs_runtime::{ApplyCx, ApplyDriver, EffectInput};
    let (driver, backend) = driver_over(USERS_DDL);
    let bridge = crate::sql_apply_driver(&driver);

    let insert = write_node(
        1,
        EffectKind::Insert,
        users_full_schema(),
        vec![
            Value::Int(7),
            Value::Text("eve".to_string()),
            Value::Int(22),
            Value::Bool(true),
        ],
    );
    let input = EffectInput::from_node(&insert);
    let out = bridge.apply_one(&input, &ApplyCx::default()).await.unwrap();
    assert_eq!(out.affected, 1);
    assert_eq!(backend.count("users"), 4);
}

// ----------------------------------------------------------------------------------------------
// 8. Secret safety — a planted credential never appears in any error/log surface.
// ----------------------------------------------------------------------------------------------

#[test]
fn connection_credential_is_never_leaked_in_an_error() {
    const PLANTED: &str = "postgres://user:PLANTED-PASSWORD-9f8e7d6c@db.internal:5432/app";
    let store = InMemoryStore::new();
    let key = CredentialKey::new(SecretDriverId::new("sql"), AccountId::new("db").unwrap());
    store.put(&key, Secret::from(PLANTED)).unwrap();

    // resolve_dialect reads ONLY the scheme; the dialect is postgres and the secret is returned
    // for the backend — but neither the returned dialect's Debug nor any SqlError carries the URI.
    let (dialect, secret) = resolve_dialect(&store, "db").unwrap();
    assert_eq!(dialect, Dialect::Postgres);

    // Drive every SqlError surface and assert none contains the planted password.
    let surfaces = vec![
        format!("{:?}", SqlError::UnknownConnection { conn: "db".into() }),
        SqlError::UnknownConnection { conn: "db".into() }.to_string(),
        format!(
            "{:?}",
            SqlError::backend("postgres", "select", "constraint violated")
        ),
        SqlError::backend("postgres", "select", "constraint violated").to_string(),
        format!("{:?}", SqlError::from(cfs_secrets::SecretError::Locked)),
        SqlError::from(cfs_secrets::SecretError::Locked).to_string(),
        // The Secret's own Debug/Display redact.
        format!("{secret:?}"),
        format!("{secret}"),
    ];
    for s in &surfaces {
        assert!(!s.contains("PLANTED-PASSWORD"), "SECRET LEAK in: {s}");
        assert!(!s.contains("9f8e7d6c"), "SECRET fragment leaked in: {s}");
    }
    // Sanity: the secret rendered its redaction marker, not the value.
    assert!(surfaces.iter().any(|s| s.contains("redacted")));
}

#[test]
fn unknown_scheme_credential_is_rejected_without_leaking() {
    const PLANTED: &str = "oracle://admin:TOPSECRET-abc123@host/db";
    let store = InMemoryStore::new();
    let key = CredentialKey::new(SecretDriverId::new("sql"), AccountId::new("ora").unwrap());
    store.put(&key, Secret::from(PLANTED)).unwrap();
    let err = resolve_dialect(&store, "ora").unwrap_err();
    assert_eq!(err.code(), "unknown_scheme");
    // The error carries the SCHEME token only ("oracle"), never the user/password.
    let text = format!("{err:?}{err}");
    assert!(text.contains("oracle"));
    assert!(!text.contains("TOPSECRET"));
    assert!(!text.contains("abc123"));
}

// ----------------------------------------------------------------------------------------------
// 9. Golden per-dialect SQL string tests (no live DB) — the primary plan/SQL gate.
// ----------------------------------------------------------------------------------------------

/// A fixed compiled SELECT used across all three dialects: SELECT id, name FROM public.users
/// WHERE age >= ? ORDER BY name ASC LIMIT 5.
fn golden_select_plan() -> SelectPlan {
    SelectPlan {
        schema: "public".to_string(),
        table: "users".to_string(),
        projection: vec!["id".to_string(), "name".to_string()],
        where_: Some(SqlPredicate::Cmp {
            col: "age".to_string(),
            op: CmpOp::Ge,
            param: 0,
        }),
        order_by: vec![OrderTerm {
            col: "name".to_string(),
            desc: false,
        }],
        limit: Some(5),
        params: vec![Param::Int(18)],
    }
}

#[test]
fn golden_select_renders_per_dialect_with_bound_params() {
    let plan = golden_select_plan();

    let (pg, pg_params) = render_select(Dialect::Postgres, &plan);
    assert_eq!(
        pg,
        "SELECT \"id\", \"name\" FROM \"public\".\"users\" WHERE \"age\" >= $1 ORDER BY \"name\" ASC LIMIT 5"
    );
    assert_eq!(pg_params, vec![Param::Int(18)]);

    let (my, _my_params) = render_select(Dialect::Mysql, &plan);
    assert_eq!(
        my,
        "SELECT `id`, `name` FROM `public`.`users` WHERE `age` >= ? ORDER BY `name` ASC LIMIT 5"
    );

    let (sq, _sq_params) = render_select(Dialect::Sqlite, &plan);
    assert_eq!(
        sq,
        "SELECT \"id\", \"name\" FROM \"public\".\"users\" WHERE \"age\" >= ? ORDER BY \"name\" ASC LIMIT 5"
    );

    // No dialect rendered the value `18` into the SQL text — it is bound.
    for sql in [&pg, &my, &sq] {
        assert!(!sql.contains("18"), "value leaked into SQL text: {sql}");
    }
}

#[test]
fn golden_upsert_renders_dialect_specific_conflict_clause() {
    let op = DmlOp::Upsert {
        schema: String::new(),
        table: "users".to_string(),
        columns: vec!["id".to_string(), "name".to_string()],
        values: vec![Param::Int(1), Param::Text("a".to_string())],
        conflict_keys: vec!["id".to_string()],
    };

    let (pg, _) = render_dml(Dialect::Postgres, &op);
    assert_eq!(
        pg,
        "INSERT INTO \"users\" (\"id\", \"name\") VALUES ($1, $2) ON CONFLICT (\"id\") DO UPDATE SET \"name\" = excluded.\"name\""
    );

    let (my, _) = render_dml(Dialect::Mysql, &op);
    assert_eq!(
        my,
        "INSERT INTO `users` (`id`, `name`) VALUES (?, ?) ON DUPLICATE KEY UPDATE `name` = VALUES(`name`)"
    );

    let (sq, _) = render_dml(Dialect::Sqlite, &op);
    assert_eq!(
        sq,
        "INSERT INTO \"users\" (\"id\", \"name\") VALUES (?, ?) ON CONFLICT (\"id\") DO UPDATE SET \"name\" = excluded.\"name\""
    );
}

#[test]
fn golden_update_and_delete_bind_where_params() {
    let update = DmlOp::Update {
        schema: String::new(),
        table: "users".to_string(),
        assignments: vec![("name".to_string(), Param::Text("new".to_string()))],
        where_: Some(SqlPredicate::Cmp {
            col: "id".to_string(),
            op: CmpOp::Eq,
            param: 0,
        }),
        where_params: vec![Param::Int(5)],
    };
    let (pg, pg_params) = render_dml(Dialect::Postgres, &update);
    assert_eq!(pg, "UPDATE \"users\" SET \"name\" = $1 WHERE \"id\" = $2");
    assert_eq!(
        pg_params,
        vec![Param::Text("new".to_string()), Param::Int(5)]
    );

    let delete = DmlOp::Delete {
        schema: String::new(),
        table: "users".to_string(),
        where_: Some(SqlPredicate::Cmp {
            col: "id".to_string(),
            op: CmpOp::Eq,
            param: 0,
        }),
        where_params: vec![Param::Int(9)],
    };
    let (my, my_params) = render_dml(Dialect::Mysql, &delete);
    assert_eq!(my, "DELETE FROM `users` WHERE `id` = ?");
    assert_eq!(my_params, vec![Param::Int(9)]);
}

#[test]
fn identifier_quoting_escapes_embedded_quote() {
    // A column name containing the quote character is doubled, never breaking out of the quoting.
    assert_eq!(Dialect::Postgres.quote_ident("a\"b"), "\"a\"\"b\"");
    assert_eq!(Dialect::Mysql.quote_ident("a`b"), "`a``b`");
}

// ----------------------------------------------------------------------------------------------
// Path + pushdown declaration.
// ----------------------------------------------------------------------------------------------

#[test]
fn path_parses_conn_schema_table_shapes() {
    assert_eq!(SqlPath::parse_str("/sql").unwrap(), SqlPath::Root);
    assert_eq!(
        SqlPath::parse_str("/sql/db").unwrap(),
        SqlPath::Connection { conn: "db".into() }
    );
    assert_eq!(
        SqlPath::parse_str("/sql/db/users").unwrap(),
        SqlPath::Table {
            conn: "db".into(),
            schema: String::new(),
            table: "users".into()
        }
    );
    assert_eq!(
        SqlPath::parse_str("/sql/db/public/users").unwrap(),
        SqlPath::Table {
            conn: "db".into(),
            schema: "public".into(),
            table: "users".into()
        }
    );
    assert_eq!(
        SqlPath::parse_str("/other").unwrap_err().code(),
        "invalid_path"
    );
    assert_eq!(
        SqlPath::parse_str("/sql/a/b/c/d").unwrap_err().code(),
        "invalid_path"
    );
}

#[test]
fn pushdown_declares_full_sql_vocabulary() {
    let (driver, _be) = driver_over(USERS_DDL);
    let pd = driver.pushdown();
    assert!(pd.supports_where());
    assert!(pd.supports_project());
    assert!(pd.supports_limit());
    assert!(pd.supports_order());
    assert!(pd.supports_join());
    assert!(pd.supports_aggregate());
    assert!(pd.supports_distinct());
    assert!(pd.supports_group_by());
    assert!(matches!(pd, PushdownProfile::Partial { .. }));
}

#[test]
fn driver_id_and_mount_are_sql() {
    let (driver, _be) = driver_over(USERS_DDL);
    assert_eq!(driver.mount(), "/sql");
    assert_eq!(driver.id().as_str(), "sql");
}
