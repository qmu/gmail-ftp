//! The account-resolution ladder (RFD-0001 §10): turn a statement's context into a
//! concrete [`AccountId`] for a driver, recording *why* (the [`AccountSource`]) so the
//! decision can be logged to the audit ledger ("who ran as whom") without ever touching
//! a credential.
//!
//! ## Precedence (highest wins)
//! 1. **`--account` flag** — an explicit CLI override.
//! 2. **`AT 'acct'` clause** — the statement-level selector (its AST node comes from the
//!    grammar ticket t04; here we consume the resolved [`AccountId`]).
//! 3. **persistent active** — the `qfs account use` choice ([`ActiveAccounts`]).
//! 4. **sole account** — if the driver has exactly one configured account, use it.
//! 5. **error** — zero accounts → [`ResolveError::NoneConfigured`]; more than one with no
//!    selector → [`ResolveError::Ambiguous`] listing the candidates (never a silent pick).
//!
//! "Sole account" means *sole for that driver*, scoped via the `available` slice the
//! caller filters to the driver — not globally. Resolution is pure (no I/O); the caller
//! reads `available` from the store once at COMMIT time and passes it in.

use crate::active::ActiveAccounts;
use crate::key::{AccountId, AccountRecord, DriverId};

/// Which rung of the ladder chose the account — recorded for the audit ledger and for
/// AI-readable "who ran as whom". Secret-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSource {
    /// The `--account` flag.
    Flag,
    /// The `AT 'acct'` clause.
    AtClause,
    /// The persistent active account (`account use`).
    Active,
    /// The driver's sole configured account.
    Sole,
}

impl AccountSource {
    /// A short, stable label for logs / audit / `-json`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            AccountSource::Flag => "flag",
            AccountSource::AtClause => "at_clause",
            AccountSource::Active => "active",
            AccountSource::Sole => "sole",
        }
    }
}

/// A resolved account decision: the chosen account + the rung that chose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The account to bind the driver leg to.
    pub account: AccountId,
    /// Why it was chosen.
    pub source: AccountSource,
}

/// Resolution failed — structured and secret-free, listing candidates so the AI/user can
/// pick (RFD §10: never a silent pick).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// No account is configured for this driver — run `qfs account add`.
    #[error("no account configured for driver {}", .driver.as_str())]
    NoneConfigured {
        /// The driver that has no accounts.
        driver: DriverId,
    },
    /// More than one account exists and none was selected. The candidate list is the
    /// actionable recovery: pass `--account` / `AT` / set an active one.
    #[error(
        "ambiguous account for driver {}: candidates {} — pass --account or AT 'acct'",
        .driver.as_str(),
        .candidates.iter().map(AccountId::as_str).collect::<Vec<_>>().join(", ")
    )]
    Ambiguous {
        /// The driver with the ambiguity.
        driver: DriverId,
        /// The candidate accounts to choose among.
        candidates: Vec<AccountId>,
    },
    /// An explicit selector (flag / AT / active) named an account that is not configured
    /// for the driver — a typo or a removed account. Lists the available candidates.
    #[error(
        "selected account {} not configured for driver {} (available: {})",
        .selected.as_str(),
        .driver.as_str(),
        .candidates.iter().map(AccountId::as_str).collect::<Vec<_>>().join(", ")
    )]
    UnknownSelection {
        /// The driver.
        driver: DriverId,
        /// The account that was selected but does not exist.
        selected: AccountId,
        /// The accounts that do exist.
        candidates: Vec<AccountId>,
    },
}

impl ResolveError {
    /// A short, stable error code for structured surfaces / AI feedback.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            ResolveError::NoneConfigured { .. } => "account_none_configured",
            ResolveError::Ambiguous { .. } => "account_ambiguous",
            ResolveError::UnknownSelection { .. } => "account_unknown_selection",
        }
    }
}

/// Resolve the account to use for `driver`, following the precedence ladder.
///
/// - `flag` is the `--account` override; `at_clause` is the `AT 'acct'` selector.
/// - `active` is the persistent `account use` map.
/// - `available` is the full account list **already filtered to this driver** by the
///   caller (a driver only sees its own accounts — the capability boundary).
///
/// An explicit selector (flag > at > active) is honoured only if it names a configured
/// account; otherwise [`ResolveError::UnknownSelection`] fires (no silent fallthrough to
/// "sole", which would mask a typo). With no selector: the sole account wins, zero is
/// [`ResolveError::NoneConfigured`], and many is [`ResolveError::Ambiguous`].
///
/// # Errors
/// [`ResolveError`] per the rules above.
pub fn resolve(
    driver: &DriverId,
    flag: Option<&AccountId>,
    at_clause: Option<&AccountId>,
    active: &ActiveAccounts,
    available: &[AccountRecord],
) -> Result<Resolution, ResolveError> {
    let candidates: Vec<AccountId> = available
        .iter()
        .filter(|r| &r.driver == driver)
        .map(|r| r.account.clone())
        .collect();

    let exists = |a: &AccountId| candidates.iter().any(|c| c == a);

    // An explicit selector, in precedence order. Each must name a configured account.
    let selector: Option<(AccountId, AccountSource)> = flag
        .map(|a| (a.clone(), AccountSource::Flag))
        .or_else(|| at_clause.map(|a| (a.clone(), AccountSource::AtClause)))
        .or_else(|| {
            active
                .get(driver)
                .map(|a| (a.clone(), AccountSource::Active))
        });

    if let Some((account, source)) = selector {
        if exists(&account) {
            return Ok(Resolution { account, source });
        }
        return Err(ResolveError::UnknownSelection {
            driver: driver.clone(),
            selected: account,
            candidates,
        });
    }

    // No selector: fall to the sole account, else structured ambiguity / emptiness.
    // We move the single element out by value (no unwrap/expect: the slice-pattern
    // binds it directly, satisfying the no-panic lib policy).
    match candidates.as_slice() {
        [] => Err(ResolveError::NoneConfigured {
            driver: driver.clone(),
        }),
        [_only] => {
            let mut candidates = candidates;
            let account = candidates.remove(0);
            Ok(Resolution {
                account,
                source: AccountSource::Sole,
            })
        }
        _ => Err(ResolveError::Ambiguous {
            driver: driver.clone(),
            candidates,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn acct(s: &str) -> AccountId {
        AccountId::new(s).unwrap()
    }

    fn rec(driver: &str, account: &str) -> AccountRecord {
        AccountRecord::new(
            DriverId::new(driver),
            acct(account),
            OffsetDateTime::UNIX_EPOCH,
        )
    }

    /// Precedence: flag wins over AT clause wins over active wins over sole.
    #[test]
    fn precedence_ladder_full_order() {
        let mail = DriverId::new("mail");
        let available = vec![rec("mail", "work"), rec("mail", "personal")];
        let mut active = ActiveAccounts::new();
        active.set(&mail, acct("personal"));

        // Flag beats everything.
        let r = resolve(
            &mail,
            Some(&acct("work")),
            Some(&acct("personal")),
            &active,
            &available,
        )
        .unwrap();
        assert_eq!(r.account, acct("work"));
        assert_eq!(r.source, AccountSource::Flag);

        // No flag: AT clause beats active.
        let r = resolve(&mail, None, Some(&acct("work")), &active, &available).unwrap();
        assert_eq!(r.account, acct("work"));
        assert_eq!(r.source, AccountSource::AtClause);

        // No flag/AT: active wins.
        let r = resolve(&mail, None, None, &active, &available).unwrap();
        assert_eq!(r.account, acct("personal"));
        assert_eq!(r.source, AccountSource::Active);
    }

    /// Sole account: exactly one configured account, no selector -> Sole.
    #[test]
    fn sole_account_for_driver_only() {
        let s3 = DriverId::new("s3");
        // Two drivers configured globally, but s3 has a SOLE account -> sole, not ambiguous.
        let available = vec![
            rec("s3", "prod"),
            rec("mail", "work"),
            rec("mail", "personal"),
        ];
        let r = resolve(&s3, None, None, &ActiveAccounts::new(), &available).unwrap();
        assert_eq!(r.account, acct("prod"));
        assert_eq!(r.source, AccountSource::Sole);
    }

    /// Zero accounts for the driver -> NoneConfigured.
    #[test]
    fn zero_accounts_is_none_configured() {
        let mail = DriverId::new("mail");
        let available = vec![rec("s3", "prod")];
        let err = resolve(&mail, None, None, &ActiveAccounts::new(), &available).unwrap_err();
        assert_eq!(err.code(), "account_none_configured");
        assert!(matches!(err, ResolveError::NoneConfigured { .. }));
    }

    /// Many accounts, no selector -> Ambiguous listing the candidates.
    #[test]
    fn many_accounts_no_selector_is_ambiguous() {
        let mail = DriverId::new("mail");
        let available = vec![rec("mail", "work"), rec("mail", "personal")];
        let err = resolve(&mail, None, None, &ActiveAccounts::new(), &available).unwrap_err();
        assert_eq!(err.code(), "account_ambiguous");
        match err {
            ResolveError::Ambiguous { candidates, .. } => {
                assert_eq!(candidates.len(), 2);
                assert!(candidates.contains(&acct("work")));
                assert!(candidates.contains(&acct("personal")));
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    /// An explicit selector that names a non-existent account -> UnknownSelection (never a
    /// silent fall-through to sole, which would mask a typo).
    #[test]
    fn selector_for_unknown_account_is_rejected() {
        let mail = DriverId::new("mail");
        let available = vec![rec("mail", "work")];
        let err = resolve(
            &mail,
            Some(&acct("typo")),
            None,
            &ActiveAccounts::new(),
            &available,
        )
        .unwrap_err();
        assert_eq!(err.code(), "account_unknown_selection");
        match err {
            ResolveError::UnknownSelection {
                selected,
                candidates,
                ..
            } => {
                assert_eq!(selected, acct("typo"));
                assert_eq!(candidates, vec![acct("work")]);
            }
            other => panic!("expected UnknownSelection, got {other:?}"),
        }
    }

    /// AccountSource labels are stable for the audit ledger.
    #[test]
    fn account_source_labels_are_stable() {
        assert_eq!(AccountSource::Flag.label(), "flag");
        assert_eq!(AccountSource::AtClause.label(), "at_clause");
        assert_eq!(AccountSource::Active.label(), "active");
        assert_eq!(AccountSource::Sole.label(), "sole");
    }
}
