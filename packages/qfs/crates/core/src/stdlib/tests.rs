//! Unit tests for the stdlib (t08): scalar/path/date/number/conditional built-ins,
//! context fns, aggregates (+ `COUNT(DISTINCT)` and aggregate-vs-scalar dispatch),
//! capability-gated `env`/`READ`/`http.get`, the function registry (register/resolve,
//! unknown → structured error), and the driver-prelude mechanism end-to-end. All pure:
//! no I/O, no network, no live wall clock, no credentials.

use super::registry::{classify_fn, AliasDecl};
use super::*;
use qfs_types::{ColumnType, DriverId, Value};

/// A deterministic pure-eval context: frozen now/current_date, deny-all env.
fn ctx() -> EvalCtx<'static> {
    static ENV: NoEnv = NoEnv;
    EvalCtx::pure(1_700_000_000, 20_000, &ENV)
}

/// Invoke a scalar built-in by name with the pure context.
fn scalar(reg: &StdlibRegistry, name: &str, args: &[Value]) -> Result<Value, FnError> {
    let c = ctx();
    match &reg.builtin(name).expect("registered builtin").eval {
        BuiltinEval::Scalar(f) => f(args, &c),
        _ => panic!("{name} is not scalar"),
    }
}

#[test]
fn registry_with_core_registers_the_stdlib_and_resolves_by_name() {
    let reg = StdlibRegistry::with_core();
    assert!(!reg.is_empty());
    // A representative sample across families resolves.
    for name in [
        "UPPER",
        "SUBSTR",
        "BASENAME",
        "PARSE_DATE",
        "ROUND",
        "COALESCE",
        "IF",
        "NOW",
        "CURRENT_DATE",
        "LAST_RUN",
        "env",
        "COUNT",
        "SUM",
        "AVG",
        "MIN",
        "MAX",
        "READ",
        "http.get",
    ] {
        assert!(reg.is_builtin(name), "{name} should be registered");
    }
    // An unknown fn is a structured error, not a panic.
    assert!(matches!(
        classify_fn(&reg, "NOPE"),
        Err(FnError::UnknownFunction { .. })
    ));
}

// ---- string / path scalars ----

#[test]
fn string_scalars_have_correct_results_and_type_errors() {
    let reg = StdlibRegistry::with_core();
    assert_eq!(
        scalar(&reg, "UPPER", &[Value::Text("abc".into())]).unwrap(),
        Value::Text("ABC".into())
    );
    assert_eq!(
        scalar(&reg, "TRIM", &[Value::Text("  x  ".into())]).unwrap(),
        Value::Text("x".into())
    );
    // LENGTH is Unicode scalar count, not bytes.
    assert_eq!(
        scalar(&reg, "LENGTH", &[Value::Text("café".into())]).unwrap(),
        Value::Int(4)
    );
    // A type error is structured (no panic, no value content).
    let err = scalar(&reg, "UPPER", &[Value::Int(1)]).unwrap_err();
    assert_eq!(err.code(), "fn_type");
    // Null propagates.
    assert_eq!(scalar(&reg, "UPPER", &[Value::Null]).unwrap(), Value::Null);
}

#[test]
fn substr_unicode_bounds_and_domain() {
    let reg = StdlibRegistry::with_core();
    // 1-based, Unicode-safe.
    assert_eq!(
        scalar(
            &reg,
            "SUBSTR",
            &[Value::Text("héllo".into()), Value::Int(2), Value::Int(3)]
        )
        .unwrap(),
        Value::Text("éll".into())
    );
    // Out-of-range length clamps to the tail (no error).
    assert_eq!(
        scalar(
            &reg,
            "SUBSTR",
            &[Value::Text("ab".into()), Value::Int(1), Value::Int(99)]
        )
        .unwrap(),
        Value::Text("ab".into())
    );
    // start beyond the end yields empty, not a panic.
    assert_eq!(
        scalar(&reg, "SUBSTR", &[Value::Text("ab".into()), Value::Int(5)]).unwrap(),
        Value::Text("".into())
    );
    // start < 1 is a domain error.
    let err = scalar(&reg, "SUBSTR", &[Value::Text("ab".into()), Value::Int(0)]).unwrap_err();
    assert_eq!(err.code(), "fn_domain");
}

#[test]
fn replace_split_concat_like() {
    let reg = StdlibRegistry::with_core();
    assert_eq!(
        scalar(
            &reg,
            "REPLACE",
            &[
                Value::Text("a.b.c".into()),
                Value::Text(".".into()),
                Value::Text("/".into())
            ]
        )
        .unwrap(),
        Value::Text("a/b/c".into())
    );
    assert_eq!(
        scalar(
            &reg,
            "SPLIT",
            &[Value::Text("a,b".into()), Value::Text(",".into())]
        )
        .unwrap(),
        Value::Array(vec![Value::Text("a".into()), Value::Text("b".into())])
    );
    // CONCAT coerces and skips nulls.
    assert_eq!(
        scalar(
            &reg,
            "CONCAT",
            &[Value::Text("x".into()), Value::Null, Value::Int(2)]
        )
        .unwrap(),
        Value::Text("x2".into())
    );
    assert_eq!(
        scalar(
            &reg,
            "LIKE",
            &[Value::Text("hello".into()), Value::Text("h%o".into())]
        )
        .unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        scalar(
            &reg,
            "LIKE",
            &[Value::Text("hello".into()), Value::Text("h_llo".into())]
        )
        .unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        scalar(
            &reg,
            "LIKE",
            &[Value::Text("hello".into()), Value::Text("z%".into())]
        )
        .unwrap(),
        Value::Bool(false)
    );
}

#[test]
fn path_scalars_basename_dirname_ext() {
    let reg = StdlibRegistry::with_core();
    assert_eq!(
        scalar(&reg, "BASENAME", &[Value::Text("/a/b/c.txt".into())]).unwrap(),
        Value::Text("c.txt".into())
    );
    assert_eq!(
        scalar(&reg, "DIRNAME", &[Value::Text("/a/b/c.txt".into())]).unwrap(),
        Value::Text("/a/b".into())
    );
    assert_eq!(
        scalar(&reg, "EXT", &[Value::Text("/a/b/c.txt".into())]).unwrap(),
        Value::Text("txt".into())
    );
    // A leading-dot file has no extension.
    assert_eq!(
        scalar(&reg, "EXT", &[Value::Text(".gitignore".into())]).unwrap(),
        Value::Text("".into())
    );
}

// ---- date scalars ----

#[test]
fn date_round_trip_and_arithmetic() {
    let reg = StdlibRegistry::with_core();
    let parsed = scalar(&reg, "PARSE_DATE", &[Value::Text("2026-06-22".into())]).unwrap();
    // FORMAT_DATE(PARSE_DATE(s)) == s (round-trip).
    let formatted = scalar(&reg, "FORMAT_DATE", std::slice::from_ref(&parsed)).unwrap();
    assert_eq!(formatted, Value::Text("2026-06-22".into()));
    // DATE is the same as PARSE_DATE on a canonical string.
    assert_eq!(
        scalar(&reg, "DATE", &[Value::Text("2026-06-22".into())]).unwrap(),
        parsed
    );
    // DATE_ADD / DATE_DIFF in whole days.
    let plus10 = scalar(&reg, "DATE_ADD", &[parsed.clone(), Value::Int(10)]).unwrap();
    assert_eq!(
        scalar(&reg, "FORMAT_DATE", std::slice::from_ref(&plus10)).unwrap(),
        Value::Text("2026-07-02".into())
    );
    assert_eq!(
        scalar(&reg, "DATE_DIFF", &[plus10, parsed]).unwrap(),
        Value::Int(10)
    );
    // A malformed date is a domain error, not a panic.
    let err = scalar(&reg, "PARSE_DATE", &[Value::Text("nope".into())]).unwrap_err();
    assert_eq!(err.code(), "fn_domain");
    // Leap-day round-trips.
    let leap = scalar(&reg, "PARSE_DATE", &[Value::Text("2024-02-29".into())]).unwrap();
    assert_eq!(
        scalar(&reg, "FORMAT_DATE", &[leap]).unwrap(),
        Value::Text("2024-02-29".into())
    );
    // A non-leap Feb 29 is rejected.
    assert!(scalar(&reg, "PARSE_DATE", &[Value::Text("2025-02-29".into())]).is_err());
}

/// Regression (ticket t08): the date conversions must be **total** on any `i64`
/// epoch-day — extreme inputs that previously panicked (`attempt to add with overflow`
/// in `civil_from_days`, debug) or silently wrapped to a wrong date (release) must now
/// surface a structured `date_out_of_range` domain error instead.
#[test]
fn format_date_extreme_epoch_days_are_domain_errors_not_panics() {
    let reg = StdlibRegistry::with_core();

    // FORMAT_DATE(i64::MAX) previously panicked / wrapped — now a structured domain error.
    let err = scalar(&reg, "FORMAT_DATE", &[Value::Int(i64::MAX)]).unwrap_err();
    assert_eq!(err.code(), "fn_domain");
    assert!(
        matches!(&err, FnError::Domain { reason, .. } if *reason == "date_out_of_range"),
        "expected date_out_of_range, got {err:?}"
    );

    // FORMAT_DATE(i64::MIN) previously emitted a junk negative-year string — now rejected.
    let err = scalar(&reg, "FORMAT_DATE", &[Value::Int(i64::MIN)]).unwrap_err();
    assert!(
        matches!(&err, FnError::Domain { reason, .. } if *reason == "date_out_of_range"),
        "expected date_out_of_range, got {err:?}"
    );

    // Just past the supported upper bound (9999-12-31 == epoch-day 2_932_896).
    let err = scalar(&reg, "FORMAT_DATE", &[Value::Int(2_932_897)]).unwrap_err();
    assert!(matches!(&err, FnError::Domain { reason, .. } if *reason == "date_out_of_range"));
    // Just past the supported lower bound (0001-01-01 == epoch-day -719_162).
    let err = scalar(&reg, "FORMAT_DATE", &[Value::Int(-719_163)]).unwrap_err();
    assert!(matches!(&err, FnError::Domain { reason, .. } if *reason == "date_out_of_range"));

    // The boundary days themselves still format correctly (inclusive range).
    assert_eq!(
        scalar(&reg, "FORMAT_DATE", &[Value::Int(2_932_896)]).unwrap(),
        Value::Text("9999-12-31".into())
    );
    assert_eq!(
        scalar(&reg, "FORMAT_DATE", &[Value::Int(-719_162)]).unwrap(),
        Value::Text("0001-01-01".into())
    );

    // A normal in-range date still formats correctly (no regression).
    let parsed = scalar(&reg, "PARSE_DATE", &[Value::Text("2026-06-22".into())]).unwrap();
    assert_eq!(
        scalar(&reg, "FORMAT_DATE", std::slice::from_ref(&parsed)).unwrap(),
        Value::Text("2026-06-22".into())
    );
}

/// Regression (ticket t08): `DATE_ADD` near the `i64` boundary must not overflow — both an
/// out-of-range base and an in-range base shifted out of range are structured domain
/// errors, while an in-range shift still computes the right day.
#[test]
fn date_add_boundary_is_total() {
    let reg = StdlibRegistry::with_core();

    // An out-of-range base is rejected before any arithmetic.
    let err = scalar(&reg, "DATE_ADD", &[Value::Int(i64::MAX), Value::Int(1)]).unwrap_err();
    assert!(matches!(&err, FnError::Domain { reason, .. } if *reason == "date_out_of_range"));

    // An in-range base whose result would overflow `i64` does not panic.
    let err = scalar(&reg, "DATE_ADD", &[Value::Int(0), Value::Int(i64::MAX)]).unwrap_err();
    assert!(matches!(&err, FnError::Domain { reason, .. } if *reason == "date_out_of_range"));

    // Adding off the top of the supported range is a domain error (no silent wrap).
    let err = scalar(&reg, "DATE_ADD", &[Value::Int(2_932_896), Value::Int(1)]).unwrap_err();
    assert!(matches!(&err, FnError::Domain { reason, .. } if *reason == "date_out_of_range"));

    // A normal in-range shift still works.
    let parsed = scalar(&reg, "PARSE_DATE", &[Value::Text("2026-06-22".into())]).unwrap();
    let plus10 = scalar(&reg, "DATE_ADD", &[parsed, Value::Int(10)]).unwrap();
    assert_eq!(
        scalar(&reg, "FORMAT_DATE", &[plus10]).unwrap(),
        Value::Text("2026-07-02".into())
    );
}

// ---- number scalars ----

#[test]
fn number_scalars_and_casts() {
    let reg = StdlibRegistry::with_core();
    assert_eq!(
        scalar(&reg, "ABS", &[Value::Int(-5)]).unwrap(),
        Value::Int(5)
    );
    assert_eq!(
        scalar(&reg, "ABS", &[Value::Float(-2.5)]).unwrap(),
        Value::Float(2.5)
    );
    assert_eq!(
        scalar(&reg, "ROUND", &[Value::Float(2.5)]).unwrap(),
        Value::Int(3)
    );
    assert_eq!(
        scalar(&reg, "FLOOR", &[Value::Float(2.9)]).unwrap(),
        Value::Int(2)
    );
    assert_eq!(
        scalar(&reg, "CEIL", &[Value::Float(2.1)]).unwrap(),
        Value::Int(3)
    );
    assert_eq!(
        scalar(&reg, "INT", &[Value::Text("42".into())]).unwrap(),
        Value::Int(42)
    );
    assert_eq!(
        scalar(&reg, "FLOAT", &[Value::Int(3)]).unwrap(),
        Value::Float(3.0)
    );
    assert_eq!(
        scalar(&reg, "TEXT", &[Value::Int(7)]).unwrap(),
        Value::Text("7".into())
    );
    // Non-numeric cast is a domain error.
    assert_eq!(
        scalar(&reg, "INT", &[Value::Text("xx".into())])
            .unwrap_err()
            .code(),
        "fn_domain"
    );
}

#[test]
fn coalesce_and_if_semantics() {
    let reg = StdlibRegistry::with_core();
    assert_eq!(
        scalar(&reg, "COALESCE", &[Value::Null, Value::Null, Value::Int(3)]).unwrap(),
        Value::Int(3)
    );
    assert_eq!(
        scalar(&reg, "COALESCE", &[Value::Null, Value::Null]).unwrap(),
        Value::Null
    );
    assert_eq!(
        scalar(
            &reg,
            "IF",
            &[Value::Bool(true), Value::Int(1), Value::Int(2)]
        )
        .unwrap(),
        Value::Int(1)
    );
    assert_eq!(
        scalar(
            &reg,
            "IF",
            &[Value::Bool(false), Value::Int(1), Value::Int(2)]
        )
        .unwrap(),
        Value::Int(2)
    );
    // Null condition selects the else branch.
    assert_eq!(
        scalar(&reg, "IF", &[Value::Null, Value::Int(1), Value::Int(2)]).unwrap(),
        Value::Int(2)
    );
}

// ---- context fns ----

#[test]
fn now_and_current_date_are_frozen_per_statement() {
    let reg = StdlibRegistry::with_core();
    // Two calls in one statement (one ctx) are EQUAL — determinism.
    let a = scalar(&reg, "NOW", &[]).unwrap();
    let b = scalar(&reg, "NOW", &[]).unwrap();
    assert_eq!(a, b);
    assert_eq!(a, Value::Timestamp(1_700_000_000));
    assert_eq!(
        scalar(&reg, "CURRENT_DATE", &[]).unwrap(),
        Value::Int(20_000)
    );
}

#[test]
fn last_run_returns_injected_value_or_null() {
    let reg = StdlibRegistry::with_core();
    static ENV: NoEnv = NoEnv;
    // Unset → Null.
    let c_unset = EvalCtx::pure(1, 1, &ENV);
    if let BuiltinEval::Scalar(f) = &reg.builtin("LAST_RUN").unwrap().eval {
        assert_eq!(f(&[], &c_unset).unwrap(), Value::Null);
        // Injected → that value.
        let c_set = EvalCtx::pure(1, 1, &ENV).with_last_run(Some(999));
        assert_eq!(f(&[], &c_set).unwrap(), Value::Timestamp(999));
    } else {
        panic!("LAST_RUN is scalar");
    }
}

#[test]
fn env_is_capability_gated_and_secret_free() {
    let reg = StdlibRegistry::with_core();
    let env = MapEnv::new().with("TOKEN", "s3cr3t");
    // Gate OFF (default): denied — and the error carries only the NAME, never the value.
    let denied = EvalCtx::pure(1, 1, &env);
    if let BuiltinEval::Scalar(f) = &reg.builtin("env").unwrap().eval {
        let err = f(&[Value::Text("TOKEN".into())], &denied).unwrap_err();
        assert_eq!(err.code(), "capability_denied");
        match &err {
            FnError::CapabilityDenied { requested, .. } => {
                assert_eq!(requested, "TOKEN");
                // The secret value must NOT appear anywhere in the error.
                assert!(!format!("{err:?}").contains("s3cr3t"));
            }
            other => panic!("expected CapabilityDenied, got {other:?}"),
        }
        // Gate ON: resolves through the stub EnvSource.
        let allowed = EvalCtx::pure(1, 1, &env).with_capabilities(true);
        assert_eq!(
            f(&[Value::Text("TOKEN".into())], &allowed).unwrap(),
            Value::Text("s3cr3t".into())
        );
        // An unset name with the gate on → Null.
        assert_eq!(
            f(&[Value::Text("MISSING".into())], &allowed).unwrap(),
            Value::Null
        );
    } else {
        panic!("env is scalar");
    }
}

// ---- aggregates ----

#[test]
fn aggregates_over_a_fixture_group() {
    let vals = [
        Value::Int(3),
        Value::Int(1),
        Value::Null,
        Value::Int(5),
        Value::Int(1),
    ];
    // COUNT (non-null) = 4.
    assert_eq!(
        run_agg(AggregateKind::Count { distinct: false }, &vals),
        Value::Int(4)
    );
    // COUNT(DISTINCT) over {3,1,5,1} = 3.
    assert_eq!(
        run_agg(AggregateKind::Count { distinct: true }, &vals),
        Value::Int(3)
    );
    // SUM = 10.0, AVG = 2.5.
    assert_eq!(run_agg(AggregateKind::Sum, &vals), Value::Float(10.0));
    assert_eq!(run_agg(AggregateKind::Avg, &vals), Value::Float(2.5));
    // MIN = 1, MAX = 5.
    assert_eq!(run_agg(AggregateKind::Min, &vals), Value::Int(1));
    assert_eq!(run_agg(AggregateKind::Max, &vals), Value::Int(5));
}

#[test]
fn aggregates_over_empty_or_all_null_group() {
    // SUM/AVG/MIN/MAX over all-null → Null; COUNT → 0.
    let nulls = [Value::Null, Value::Null];
    assert_eq!(run_agg(AggregateKind::Sum, &nulls), Value::Null);
    assert_eq!(run_agg(AggregateKind::Avg, &nulls), Value::Null);
    assert_eq!(run_agg(AggregateKind::Min, &nulls), Value::Null);
    assert_eq!(
        run_agg(AggregateKind::Count { distinct: false }, &nulls),
        Value::Int(0)
    );
}

#[test]
fn sum_over_non_numeric_is_a_typed_error() {
    let mut st = AggregateFactory::new(AggregateKind::Sum).init();
    let err = st.accumulate(&Value::Text("x".into())).unwrap_err();
    assert_eq!(err.code(), "fn_type");
}

#[test]
fn aggregate_vs_scalar_dispatch_is_typed() {
    let reg = StdlibRegistry::with_core();
    // The registry distinguishes aggregate from scalar so misuse is a typed decision.
    assert!(reg.is_aggregate("SUM"));
    assert!(reg.is_aggregate("COUNT"));
    assert!(!reg.is_aggregate("UPPER"));
    assert!(!reg.is_aggregate("NOW"));
}

/// Run an aggregate over a slice of values (init → accumulate* → finalize).
fn run_agg(kind: AggregateKind, vals: &[Value]) -> Value {
    let mut st = AggregateFactory::new(kind).init();
    for v in vals {
        st.accumulate(v).unwrap();
    }
    st.finalize()
}

// ---- table-valued (READ / http.get) ----

#[test]
fn read_and_http_get_are_deferred_nodes_gated_and_io_free() {
    let reg = StdlibRegistry::with_core();
    static ENV: NoEnv = NoEnv;
    // Gate OFF: denied (no node built, no I/O).
    let denied = EvalCtx::pure(1, 1, &ENV);
    if let BuiltinEval::TableValued(f) = &reg.builtin("READ").unwrap().eval {
        assert_eq!(
            f(&[Value::Text("/blob/x".into())], &denied)
                .unwrap_err()
                .code(),
            "capability_denied"
        );
        // Gate ON: a deferred READ source node (a description, not a read).
        let allowed = EvalCtx::pure(1, 1, &ENV).with_capabilities(true);
        let node = f(&[Value::Text("/blob/x".into())], &allowed).unwrap();
        assert_eq!(node, PlanNode::read("/blob/x"));
        assert!(matches!(node.kind, PlanNodeKind::Read { .. }));
    } else {
        panic!("READ is table-valued");
    }
    if let BuiltinEval::TableValued(f) = &reg.builtin("http.get").unwrap().eval {
        let allowed = EvalCtx::pure(1, 1, &ENV).with_capabilities(true);
        let node = f(&[Value::Text("https://api/x".into())], &allowed).unwrap();
        assert_eq!(node, PlanNode::http_get("https://api/x"));
    } else {
        panic!("http.get is table-valued");
    }
}

// ---- prelude mechanism ----

#[test]
fn prelude_round_trips_and_namespaces_aliases() {
    let mut reg = StdlibRegistry::with_core();
    let mail = DriverId::new("mail");
    // A test mail driver prelude: SEND(d) desugars to a single CALL mail.send.
    let prelude = Prelude::new(
        mail.clone(),
        vec![AliasDecl::new(
            "SEND",
            "FROM /mail/drafts |> CALL mail.send",
        )],
    );
    reg.register_prelude(&prelude).unwrap();

    // The alias is registered, namespaced by driver, and desugars to mail.send.
    let aliases = reg.prelude_aliases(&mail);
    assert_eq!(aliases.len(), 1);
    assert_eq!(aliases[0].name, "SEND");
    assert_eq!(aliases[0].desugars_to, "mail.send");
    // The AliasFn view (t06's resolution surface) matches.
    let fns = reg.prelude_alias_fns(&mail);
    assert_eq!(fns[0].name, "SEND");
    assert_eq!(fns[0].desugars_to, "mail.send");
    // classify_fn now recognises SEND as a (prelude) function, not unknown.
    assert!(classify_fn(&reg, "SEND").is_ok());
}

#[test]
fn same_alias_on_two_drivers_stays_scoped() {
    let mut reg = StdlibRegistry::with_core();
    let mail = DriverId::new("mail");
    let chat = DriverId::new("chat");
    reg.register_prelude(&Prelude::new(
        mail.clone(),
        vec![AliasDecl::new(
            "SEND",
            "FROM /mail/drafts |> CALL mail.send",
        )],
    ))
    .unwrap();
    reg.register_prelude(&Prelude::new(
        chat.clone(),
        vec![AliasDecl::new("SEND", "FROM /chat/room |> CALL chat.send")],
    ))
    .unwrap();
    // Both drivers ship SEND — no global clash; each desugars to its own proc.
    assert_eq!(reg.prelude_aliases(&mail)[0].desugars_to, "mail.send");
    assert_eq!(reg.prelude_aliases(&chat)[0].desugars_to, "chat.send");
    // Both are providers (the t06 ambiguity input).
    let mut providers = reg.alias_providers("SEND");
    providers.sort();
    assert_eq!(providers, vec!["chat".to_string(), "mail".to_string()]);
}

#[test]
fn within_prelude_duplicate_is_rejected() {
    let mut reg = StdlibRegistry::with_core();
    let err = reg
        .register_prelude(&Prelude::new(
            DriverId::new("mail"),
            vec![
                AliasDecl::new("SEND", "FROM /mail/drafts |> CALL mail.send"),
                AliasDecl::new("SEND", "FROM /mail/drafts |> CALL mail.send2"),
            ],
        ))
        .unwrap_err();
    assert_eq!(err.code(), "prelude_duplicate_alias");
}

#[test]
fn impure_alias_body_is_rejected() {
    let mut reg = StdlibRegistry::with_core();
    // A body with no CALL (just a scan) does not desugar to a plan-constructing CALL.
    let err = reg
        .register_prelude(&Prelude::new(
            DriverId::new("mail"),
            vec![AliasDecl::new("BAD", "FROM /mail/drafts |> WHERE x = 1")],
        ))
        .unwrap_err();
    assert_eq!(err.code(), "prelude_impure_alias");
}

#[test]
fn unparseable_alias_body_is_a_structured_parse_error() {
    let mut reg = StdlibRegistry::with_core();
    let err = reg
        .register_prelude(&Prelude::new(
            DriverId::new("mail"),
            vec![AliasDecl::new("BAD", "this is not qfs (((")],
        ))
        .unwrap_err();
    assert_eq!(err.code(), "prelude_alias_parse");
}

#[test]
fn fn_sig_arity_policy() {
    // Fixed, range, and variadic arity acceptance.
    assert!(FnSig::fixed(2, ColumnType::Text).accepts_arity(2));
    assert!(!FnSig::fixed(2, ColumnType::Text).accepts_arity(3));
    assert!(FnSig::range(2, 3, ColumnType::Text).accepts_arity(3));
    assert!(!FnSig::range(2, 3, ColumnType::Text).accepts_arity(1));
    assert!(FnSig::variadic(1, ColumnType::Text).accepts_arity(100));
    assert!(!FnSig::variadic(1, ColumnType::Text).accepts_arity(0));
}
