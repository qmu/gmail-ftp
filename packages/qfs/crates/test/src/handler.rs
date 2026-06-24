//! Handler PREVIEW fixtures (t38, RFD §8): `preview_handler(ddl_src) -> Plan` — drive a
//! `CREATE ENDPOINT/TRIGGER/JOB` binding to the `Plan` a fired binding would COMMIT, **no
//! socket, no live backend**.
//!
//! ## PREVIEW-as-test (RFD §8)
//! A server binding (`CREATE ENDPOINT`/`TRIGGER`/`JOB`) **desugars** to exactly one
//! `/server/<kind>` config-write effect-plan (the canonical closed-core rule, RFD §6/§8). That
//! desugar is **pure** — it builds effects-as-data and runs no I/O — so the strongest test of a
//! handler is to assert the `Plan` it produces, never opening a listen socket or hitting a
//! backend. This consolidates the t30–t34 binding-seam tests: instead of standing up an HTTP
//! listener / cron interval / event bus, assert the plan the binding would commit.
//!
//! The fixture "event" is the binding DDL itself — driving `CREATE ENDPOINT … ON GET /x`
//! through the desugar is exactly what the runtime does when the binding is created, and the
//! resulting plan is what every later firing reconciles against.

use qfs_core::{desugar_to_insert, parse_server_binding_ddl, Plan};

/// Desugar a `CREATE ENDPOINT/TRIGGER/JOB/VIEW/WEBHOOK …` binding to the single `/server/*`
/// config-write [`Plan`] it would COMMIT. Pure: no socket, no backend, no creds.
///
/// # Panics
/// Panics (test-only) if `ddl_src` is not a valid server-binding DDL statement or the desugared
/// row violates the `/server/*` schema — a fixture that does not desugar is a test-author error.
#[must_use]
pub fn preview_handler(ddl_src: &str) -> Plan {
    let ddl = parse_server_binding_ddl(ddl_src)
        .unwrap_or_else(|e| panic!("qfs-test preview_handler: `{ddl_src}` is not valid DDL: {e}"));
    desugar_to_insert(&ddl)
        .unwrap_or_else(|e| panic!("qfs-test preview_handler: `{ddl_src}` did not desugar: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_core::EffectKind;

    #[test]
    fn create_endpoint_previews_a_single_server_config_write() {
        // No socket is opened — the binding's plan is asserted directly.
        let plan = preview_handler("CREATE ENDPOINT hello ON 'GET /hello' AS FROM /mail/inbox");
        assert_eq!(
            plan.nodes().len(),
            1,
            "exactly one /server config-write node"
        );
        assert!(
            matches!(plan.nodes()[0].kind, EffectKind::ServerConfigWrite { .. }),
            "the endpoint desugars to a /server config write"
        );
        // A config write is reversible (RFD §6) — no irreversible node, nothing to warn on.
        assert!(!plan.is_irreversible());
        // Building the plan did no I/O — it is a valid DAG and that is all that exists.
        plan.validate().expect("desugared plan is a valid DAG");
    }
}
