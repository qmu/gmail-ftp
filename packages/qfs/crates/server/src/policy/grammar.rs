//! Build a [`Policy`] from the parsed `CREATE POLICY` DDL, and round-trip it through the
//! `/server/policies` row representation (RFD §8). The closed-core grammar (the `ALLOW`/`DENY`
//! token parsing) lives in `qfs-parser` (no new frozen keyword); THIS module is the semantic
//! layer that turns the shape-only [`qfs_parser::ServerDdl`] / [`qfs_parser::PolicyRuleAst`]
//! into the owned [`Policy`] DTO and renders rules back to the storable string form.
//!
//! ## Round-trip (CREATE ≡ INSERT INTO /server/policies)
//! A [`Policy`] serializes to a [`crate::PolicyDef`] whose `allow: Vec<String>` holds one
//! canonical rule string per rule (e.g. `ALLOW SELECT`, `DENY INSERT,UPDATE,REMOVE,CALL`,
//! `ALLOW ALL ON mail`). [`policy_from_def`] re-parses those strings back into [`Rule`]s, so a
//! `CREATE POLICY` desugars to an `INSERT INTO /server/policies` that rehydrates to an EQUAL
//! `Policy` (the acceptance round-trip). No schema change — the existing `allow` array column
//! carries the rules.

use qfs_parser::{Expr, Literal, PolicyRuleAst, PolicySubjectAst, ServerDdl};

use super::model::{
    Condition, DriverGlob, Effectivity, Policy, Rule, ScopeGlob, Subject, Verb, VerbSet,
};
use crate::state::PolicyDef;

/// Build a [`Policy`] from a parsed `CREATE POLICY` [`ServerDdl`] (t35). The `name` is the row
/// key; each [`PolicyRuleAst`] becomes a [`Rule`]. Default-deny is the fixed baseline.
///
/// # Errors
/// A secret-free detail string if a verb token is unrecognized (defensive — the grammar only
/// emits known verb labels, but the boundary is validated).
pub fn policy_from_ddl(ddl: &ServerDdl) -> Result<Policy, String> {
    let mut policy = Policy::new(ddl.name.clone());
    for ast in &ddl.policy_rules {
        policy.rules.push(rule_from_ast(ast)?);
    }
    Ok(policy)
}

/// Convert one [`PolicyRuleAst`] to a [`Rule`].
fn rule_from_ast(ast: &PolicyRuleAst) -> Result<Rule, String> {
    let effect = if ast.allow {
        Effectivity::Allow
    } else {
        Effectivity::Deny
    };
    let verbs = if ast.all_token {
        VerbSet::all()
    } else {
        let mut parsed = Vec::new();
        for tok in &ast.verbs {
            let v = Verb::from_label(tok).ok_or_else(|| format!("unknown policy verb `{tok}`"))?;
            parsed.push(v);
        }
        VerbSet::from_verbs(&parsed)
    };
    let driver = ast
        .driver
        .as_deref()
        .map(DriverGlob::new)
        .unwrap_or_else(DriverGlob::any);
    // t57: the optional `FOR <subject>` / `AT <scope>` / `WHERE <condition>` axes.
    let subject = match &ast.subject {
        Some(s) => subject_from_ast(s)?,
        None => Subject::Anyone,
    };
    let scope = match &ast.scope {
        Some(raw) => Some(
            ScopeGlob::parse(raw).ok_or_else(|| format!("malformed policy scope path `{raw}`"))?,
        ),
        None => None,
    };
    let condition = match &ast.condition {
        Some(expr) => condition_from_expr(expr)?,
        None => Condition::Always,
    };
    Ok(Rule {
        effect,
        verbs,
        driver,
        all_token: ast.all_token,
        subject,
        scope,
        condition,
    })
}

/// Convert a parsed `FOR <subject>` clause ([`PolicySubjectAst`]) into a [`Subject`]. The `kind`
/// token is one of `user`/`role`/`group` (case-insensitive); anything else is a secret-free error.
fn subject_from_ast(ast: &PolicySubjectAst) -> Result<Subject, String> {
    let name = ast.name.clone();
    Ok(match ast.kind.to_ascii_lowercase().as_str() {
        "user" => Subject::User(name),
        "role" => Subject::Role(name),
        "group" => Subject::Group(name),
        other => return Err(format!("unknown policy subject kind `{other}`")),
    })
}

/// Interpret a parsed `WHERE <expr>` conditional grant. t57 supports exactly the
/// `member_of('/directories/...')` predicate — a **function-valued** predicate (the "functions
/// are values" seam, [`Expr::Fn`]), NOT a new keyword. Any other shape is a secret-free error so
/// an unsupported condition is never silently dropped (which would *widen* the grant).
fn condition_from_expr(expr: &Expr) -> Result<Condition, String> {
    let Expr::Fn(call) = expr else {
        return Err("policy WHERE condition must be a `member_of('/directories/...')` call".into());
    };
    if !call.name.eq_ignore_ascii_case("member_of") {
        return Err(format!(
            "unsupported policy condition function `{}`",
            call.name
        ));
    }
    let [Expr::Lit(Literal::Str(dir))] = call.args.as_slice() else {
        return Err("member_of(...) takes one string directory ref argument".into());
    };
    Ok(Condition::MemberOf(dir.clone()))
}

/// Render one [`Rule`] to its canonical storable string (the round-trip form). E.g.
/// `ALLOW SELECT`, `DENY INSERT,UPDATE,REMOVE,CALL`, `ALLOW ALL ON mail`.
#[must_use]
pub fn rule_to_string(rule: &Rule) -> String {
    let verbs = if rule.all_token {
        "ALL".to_string()
    } else {
        rule.verbs
            .verbs()
            .iter()
            .map(|v| v.label())
            .collect::<Vec<_>>()
            .join(",")
    };
    let mut s = format!("{} {verbs}", rule.effect.label());
    if !rule.driver.is_any() {
        s.push_str(" ON ");
        s.push_str(rule.driver.as_str());
    }
    // t57: the richer axes round-trip as space-free trailing tokens (`FOR role:admin`,
    // `AT /members/alice/**`, `WHERE member_of('/directories/...')`). Each is omitted at its
    // unscoped default, so a pre-t57 rule renders byte-for-byte as before.
    if !rule.subject.is_anyone() {
        s.push_str(" FOR ");
        s.push_str(&rule.subject.label());
    }
    if let Some(scope) = &rule.scope {
        s.push_str(" AT ");
        s.push_str(&scope.render());
    }
    if let Some(cond) = rule.condition.label() {
        s.push_str(" WHERE ");
        s.push_str(&cond);
    }
    s
}

/// The canonical `allow`-array rendering of a [`Policy`]: one rule string per rule. This is
/// exactly what the `CREATE POLICY` desugar writes into `/server/policies.allow` and what
/// [`policy_from_def`] re-parses.
#[must_use]
pub fn policy_to_rule_strings(policy: &Policy) -> Vec<String> {
    policy.rules.iter().map(rule_to_string).collect()
}

/// Rehydrate a [`Policy`] from a stored [`PolicyDef`] (`/server/policies` row). The `allow`
/// array holds the canonical rule strings; each is re-parsed. An empty `allow` array yields a
/// default-deny policy with no rules (the fail-closed default). Unparseable rule strings are
/// **skipped** defensively (a malformed stored rule must never silently *widen* the policy —
/// dropping it keeps the policy at least as strict, fail-closed).
#[must_use]
pub fn policy_from_def(def: &PolicyDef) -> Policy {
    let mut policy = Policy::new(def.name.clone());
    for s in &def.allow {
        if let Some(rule) = parse_rule_string(s) {
            policy.rules.push(rule);
        }
    }
    policy
}

/// Parse one canonical stored rule string (`ALLOW SELECT`, `DENY INSERT,UPDATE ON mail`, …)
/// back into a [`Rule`]. `None` if the string is not a well-formed rule (skipped fail-closed).
fn parse_rule_string(s: &str) -> Option<Rule> {
    let mut parts = s.split_whitespace();
    let effect = Effectivity::from_label(parts.next()?)?;
    let verbs_tok = parts.next()?;
    let (verbs, all_token) = if verbs_tok == "ALL" {
        (VerbSet::all(), true)
    } else {
        let mut parsed = Vec::new();
        for v in verbs_tok.split(',') {
            parsed.push(Verb::from_label(v)?);
        }
        (VerbSet::from_verbs(&parsed), false)
    };
    // The optional trailing clauses, in canonical order: `ON <glob>`, then the t57 axes
    // `FOR <subject>`, `AT <scope>`, `WHERE <condition>`. Each clause keyword is followed by one
    // space-free token. An unrecognized token ⇒ malformed ⇒ the whole rule is dropped fail-closed.
    let mut driver = DriverGlob::any();
    let mut subject = Subject::Anyone;
    let mut scope = None;
    let mut condition = Condition::Always;
    while let Some(clause) = parts.next() {
        match clause {
            "ON" => driver = DriverGlob::new(parts.next()?.to_string()),
            "FOR" => subject = Subject::from_label(parts.next()?)?,
            "AT" => scope = Some(ScopeGlob::parse(parts.next()?)?),
            "WHERE" => condition = condition_from_canonical(parts.next()?)?,
            _ => return None, // unexpected trailing token ⇒ malformed
        }
    }
    Some(Rule {
        effect,
        verbs,
        driver,
        all_token,
        subject,
        scope,
        condition,
    })
}

/// Parse a canonical `WHERE` condition token back into a [`Condition`]. t57 supports the single
/// `member_of('/directories/...')` form (the same surface [`rule_to_string`] renders). `None` for
/// anything else (malformed ⇒ the rule is dropped fail-closed — a broken condition must never
/// silently *widen* the grant by becoming unconditional).
fn condition_from_canonical(tok: &str) -> Option<Condition> {
    let inner = tok.strip_prefix("member_of('")?.strip_suffix("')")?;
    if inner.is_empty() {
        return None;
    }
    Some(Condition::MemberOf(inner.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_parser::{parse_statement, Statement};

    fn policy_of(src: &str) -> Policy {
        let Statement::Ddl(ddl) = parse_statement(src).unwrap() else {
            panic!("not a DDL")
        };
        policy_from_ddl(&ddl).unwrap()
    }

    #[test]
    fn rfd_section8_golden_example() {
        // The RFD §8 acceptance golden: `CREATE POLICY api ALLOW SELECT DENY INSERT,UPDATE,
        // REMOVE,CALL` → the expected Policy DTO.
        let p = policy_of("CREATE POLICY api ALLOW SELECT DENY INSERT,UPDATE,REMOVE,CALL");
        let expected = Policy::new("api")
            .with_rule(Rule::allow(VerbSet::one(Verb::Select), DriverGlob::any()))
            .with_rule(Rule::deny(
                VerbSet::from_verbs(&[Verb::Insert, Verb::Update, Verb::Remove, Verb::Call]),
                DriverGlob::any(),
            ));
        assert_eq!(p, expected);
    }

    #[test]
    fn roundtrip_through_def() {
        let p = policy_of("CREATE POLICY api ALLOW SELECT DENY INSERT,UPDATE,REMOVE,CALL");
        let def = PolicyDef {
            name: p.name.clone(),
            handler: String::new(),
            allow: policy_to_rule_strings(&p),
        };
        let back = policy_from_def(&def);
        assert_eq!(p, back, "CREATE POLICY round-trips through PolicyDef");
    }

    #[test]
    fn roundtrip_all_token_and_glob() {
        let p = policy_of("CREATE POLICY broad ALLOW ALL ON mail DENY REMOVE,CALL");
        let strings = policy_to_rule_strings(&p);
        assert_eq!(strings[0], "ALLOW ALL ON mail");
        assert_eq!(strings[1], "DENY REMOVE,CALL");
        let def = PolicyDef {
            name: p.name.clone(),
            handler: String::new(),
            allow: strings,
        };
        assert_eq!(p, policy_from_def(&def));
        // The ALL token survives the round-trip (irreversible strictness depends on it).
        assert!(policy_from_def(&def).rules[0].all_token);
    }

    #[test]
    fn empty_allow_array_is_default_deny() {
        let def = PolicyDef {
            name: "empty".to_string(),
            handler: String::new(),
            allow: Vec::new(),
        };
        let p = policy_from_def(&def);
        assert!(p.rules.is_empty());
        assert_eq!(p.default, Effectivity::Deny);
    }

    #[test]
    fn malformed_rule_string_is_skipped() {
        let def = PolicyDef {
            name: "x".to_string(),
            handler: String::new(),
            allow: vec!["GARBAGE".to_string(), "ALLOW SELECT".to_string()],
        };
        let p = policy_from_def(&def);
        // The garbage rule is dropped (fail-closed); only the valid ALLOW SELECT survives.
        assert_eq!(p.rules.len(), 1);
        assert_eq!(p.rules[0].effect, Effectivity::Allow);
    }

    // ---- t57: the richer axes parse + round-trip through `/sys/policies` -------------------

    #[test]
    fn ddl_parses_actor_scope_and_condition() {
        let p = policy_of(
            "CREATE POLICY eng ALLOW INSERT ON mail FOR role admin AT /members/alice/** \
             WHERE member_of('/directories/google/groups/eng')",
        );
        assert_eq!(p.rules.len(), 1);
        let r = &p.rules[0];
        assert_eq!(r.subject, super::Subject::Role("admin".into()));
        assert_eq!(
            r.scope.as_ref().map(super::ScopeGlob::render).as_deref(),
            Some("/members/alice/**")
        );
        assert_eq!(
            r.condition,
            super::Condition::MemberOf("/directories/google/groups/eng".into())
        );
    }

    #[test]
    fn richer_rule_round_trips_through_def() {
        let p = policy_of(
            "CREATE POLICY eng ALLOW INSERT ON mail FOR group eng AT /me/mail/* \
             WHERE member_of('/directories/x')",
        );
        let strings = policy_to_rule_strings(&p);
        assert_eq!(
            strings[0],
            "ALLOW INSERT ON mail FOR group:eng AT /me/mail/* WHERE member_of('/directories/x')",
            "canonical rule string carries every axis"
        );
        let def = PolicyDef {
            name: p.name.clone(),
            handler: String::new(),
            allow: strings,
        };
        // The whole richer model round-trips through the `/sys/policies` row representation.
        assert_eq!(p, policy_from_def(&def));
    }

    #[test]
    fn member_of_must_be_a_call_expression() {
        // A non-call WHERE body is rejected (a stray column reference is not a condition).
        let Statement::Ddl(ddl) =
            parse_statement("CREATE POLICY bad ALLOW INSERT WHERE foo").unwrap()
        else {
            panic!("not a DDL")
        };
        assert!(policy_from_ddl(&ddl).is_err());
    }

    #[test]
    fn unsupported_condition_function_is_rejected() {
        let Statement::Ddl(ddl) =
            parse_statement("CREATE POLICY bad ALLOW INSERT WHERE is_admin('x')").unwrap()
        else {
            panic!("not a DDL")
        };
        assert!(policy_from_ddl(&ddl).is_err());
    }

    #[test]
    fn malformed_condition_string_drops_rule_fail_closed() {
        // A stored WHERE token that is not a well-formed `member_of('...')` drops the rule (never
        // becomes an unconditional grant, which would widen the policy).
        let def = PolicyDef {
            name: "x".to_string(),
            handler: String::new(),
            allow: vec!["ALLOW INSERT WHERE member_of()".to_string()],
        };
        assert!(policy_from_def(&def).rules.is_empty());
    }
}
