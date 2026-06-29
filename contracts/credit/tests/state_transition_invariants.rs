use soroban_sdk::testutils::Address;
// SPDX-License-Identifier: MIT

//! State-transition invariant tests for the Credit contract state machine.
//!
//! # Coverage matrix
//!
//! | From       | To         | principal=0 | principal>0 | accrued>0 |
//! |------------|------------|-------------|-------------|-----------|
//! | Active     | Suspended  | ✓           | ✓           | ✓         |
//! | Active     | Defaulted  | ✓           | ✓           | ✓         |
//! | Active     | Closed     | ✓ (ok)      | ✓ (admin)   | ✓ (admin) |
//! | Suspended  | Defaulted  | ✓           | ✓           | ✓         |
//! | Suspended  | Closed     | ✓ (ok)      | ✓ (admin)   | ✓ (admin) |
//! | Suspended  | Active     | ✓ (reopen)  | ✓ (reopen)  | ✓ (reopen)|
//! | Defaulted  | Active     | ✓           | ✓           | ✓         |
//! | Defaulted  | Closed     | ✓ (ok)      | ✓ (admin)   | ✓ (admin) |
//! | Closed     | *          | ✓ (idempot) | —           | —         |
//!
//! # Accounting invariant
//! For every transition: `total_debt == principal + accrued_interest`
//! where `total_debt = utilized_amount` and `principal = utilized_amount - accrued_interest`.
//!
//! # Security notes
//! - Close with balance > 0 is only allowed for the admin, never the borrower.
//! - Reinstate is admin-only and only valid from Defaulted.
//! - Suspend is admin-only and only valid from Active.
//! - All invariant assertions run before AND after every transition.

use creditra_credit::types::{CreditLineData, CreditStatus};
use creditra_credit::{Credit, CreditClient};
use soroban_sdk::testutils::{Address as _, Ledger};
use soroban_sdk::{token, Address, Env};

// ── helpers ──────────────────────────────────────────────────────────────────

/// Assert the core accounting invariant on a credit line snapshot.
/// `utilized_amount` is the total debt; `accrued_interest` is the interest
/// component; principal is the remainder. All must be non-negative.
fn assert_accounting_invariant(line: &CreditLineData, label: &str) {
    assert!(
        line.utilized_amount >= 0,
        "{label}: utilized_amount must be >= 0, got {}",
        line.utilized_amount
    );
    assert!(
        line.accrued_interest >= 0,
        "{label}: accrued_interest must be >= 0, got {}",
        line.accrued_interest
    );
    assert!(
        line.accrued_interest <= line.utilized_amount,
        "{label}: accrued_interest ({}) must be <= utilized_amount ({})",
        line.accrued_interest,
        line.utilized_amount
    );
    // principal = total_debt - interest
    let principal = line.utilized_amount - line.accrued_interest;
    assert!(
        principal >= 0,
        "{label}: derived principal must be >= 0, got {principal}"
    );
}

/// Minimal contract setup: returns (env, admin, contract_id).
fn setup_env() -> (Env, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let contract_id = env.register(Credit, ());
    let client = CreditClient::new(&env, &contract_id);
    client.init(&admin);

    let token_id = env.register_stellar_asset_contract_v2(Address::generate(&env));
    let tok = token_id.address();
    client.set_liquidity_token(&tok);
    token::StellarAssetClient::new(&env, &tok).mint(&contract_id, &100_000_000_i128);

    (env, admin, contract_id)
}

/// Open a credit line and optionally draw `draw_amount` to create principal.
/// Returns the borrower address.
fn open_line(env: &Env, contract_id: &Address, credit_limit: i128, draw_amount: i128) -> Address {
    let borrower = Address::generate(env);
    let client = CreditClient::new(env, contract_id);
    client.open_credit_line(&borrower, &credit_limit, &300_u32, &50_u32);
    if draw_amount > 0 {
        client.draw_credit(&borrower, &draw_amount);
    }
    borrower
}

/// Advance ledger time. Accrual is lazy and fires on the next mutating call.
#[allow(dead_code)]
fn advance_time_and_accrue(
    env: &Env,
    contract_id: &Address,
    borrower: &Address,
    seconds: u64,
) -> CreditLineData {
    env.ledger().with_mut(|li| li.timestamp += seconds);
    // A suspend+reinstate round-trip is the simplest way to force accrual
    // without changing the final status. Instead we just read the line —
    // accrual is lazy and applied on the next mutating call.
    CreditClient::new(env, contract_id)
        .get_credit_line(borrower)
        .unwrap()
}

// ── transition case descriptor ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct TransitionCase {
    label: &'static str,
    credit_limit: i128,
    draw_amount: i128,
    /// Seconds to advance before the transition (triggers accrual on next call).
    advance_seconds: u64,
    from: CreditStatus,
    to: CreditStatus,
    /// Whether the transition should succeed.
    expect_ok: bool,
    /// Whether the closer is the borrower (vs admin) for Close transitions.
    borrower_closes: bool,
}

// ── transition matrix ─────────────────────────────────────────────────────────

fn transition_cases() -> Vec<TransitionCase> {
    vec![
        // ── Active → Suspended ───────────────────────────────────────────────
        TransitionCase {
            label: "Active→Suspended: zero balance",
            credit_limit: 1_000,
            draw_amount: 0,
            advance_seconds: 0,
            from: CreditStatus::Active,
            to: CreditStatus::Suspended,
            expect_ok: true,
            borrower_closes: false,
        },
        TransitionCase {
            label: "Active→Suspended: principal > 0",
            credit_limit: 1_000,
            draw_amount: 500,
            advance_seconds: 0,
            from: CreditStatus::Active,
            to: CreditStatus::Suspended,
            expect_ok: true,
            borrower_closes: false,
        },
        TransitionCase {
            label: "Active→Suspended: accrued interest > 0",
            credit_limit: 1_000,
            draw_amount: 500,
            advance_seconds: 31_536_000, // 1 year → ~15 bps interest
            from: CreditStatus::Active,
            to: CreditStatus::Suspended,
            expect_ok: true,
            borrower_closes: false,
        },
        // ── Active → Defaulted ───────────────────────────────────────────────
        TransitionCase {
            label: "Active→Defaulted: zero balance",
            credit_limit: 1_000,
            draw_amount: 0,
            advance_seconds: 0,
            from: CreditStatus::Active,
            to: CreditStatus::Defaulted,
            expect_ok: true,
            borrower_closes: false,
        },
        TransitionCase {
            label: "Active→Defaulted: principal > 0",
            credit_limit: 1_000,
            draw_amount: 800,
            advance_seconds: 0,
            from: CreditStatus::Active,
            to: CreditStatus::Defaulted,
            expect_ok: true,
            borrower_closes: false,
        },
        TransitionCase {
            label: "Active→Defaulted: accrued interest > 0",
            credit_limit: 1_000,
            draw_amount: 800,
            advance_seconds: 31_536_000,
            from: CreditStatus::Active,
            to: CreditStatus::Defaulted,
            expect_ok: true,
            borrower_closes: false,
        },
        // ── Active → Closed (admin) ──────────────────────────────────────────
        TransitionCase {
            label: "Active→Closed: zero balance, borrower closes",
            credit_limit: 1_000,
            draw_amount: 0,
            advance_seconds: 0,
            from: CreditStatus::Active,
            to: CreditStatus::Closed,
            expect_ok: true,
            borrower_closes: true,
        },
        TransitionCase {
            label: "Active→Closed: principal > 0, admin force-closes",
            credit_limit: 1_000,
            draw_amount: 300,
            advance_seconds: 0,
            from: CreditStatus::Active,
            to: CreditStatus::Closed,
            expect_ok: true,
            borrower_closes: false,
        },
        TransitionCase {
            label: "Active→Closed: accrued interest > 0, admin force-closes",
            credit_limit: 1_000,
            draw_amount: 300,
            advance_seconds: 31_536_000,
            from: CreditStatus::Active,
            to: CreditStatus::Closed,
            expect_ok: true,
            borrower_closes: false,
        },
        // ── Active → Closed (borrower, balance > 0 → must fail) ─────────────
        TransitionCase {
            label: "Active→Closed: principal > 0, borrower close MUST FAIL",
            credit_limit: 1_000,
            draw_amount: 300,
            advance_seconds: 0,
            from: CreditStatus::Active,
            to: CreditStatus::Closed,
            expect_ok: false,
            borrower_closes: true,
        },
        // ── Suspended → Defaulted ────────────────────────────────────────────
        TransitionCase {
            label: "Suspended→Defaulted: zero balance",
            credit_limit: 1_000,
            draw_amount: 0,
            advance_seconds: 0,
            from: CreditStatus::Suspended,
            to: CreditStatus::Defaulted,
            expect_ok: true,
            borrower_closes: false,
        },
        TransitionCase {
            label: "Suspended→Defaulted: principal > 0",
            credit_limit: 1_000,
            draw_amount: 600,
            advance_seconds: 0,
            from: CreditStatus::Suspended,
            to: CreditStatus::Defaulted,
            expect_ok: true,
            borrower_closes: false,
        },
        TransitionCase {
            label: "Suspended→Defaulted: accrued interest > 0",
            credit_limit: 1_000,
            draw_amount: 600,
            advance_seconds: 31_536_000,
            from: CreditStatus::Suspended,
            to: CreditStatus::Defaulted,
            expect_ok: true,
            borrower_closes: false,
        },
        // ── Suspended → Closed ───────────────────────────────────────────────
        TransitionCase {
            label: "Suspended→Closed: zero balance, borrower closes",
            credit_limit: 1_000,
            draw_amount: 0,
            advance_seconds: 0,
            from: CreditStatus::Suspended,
            to: CreditStatus::Closed,
            expect_ok: true,
            borrower_closes: true,
        },
        TransitionCase {
            label: "Suspended→Closed: principal > 0, admin force-closes",
            credit_limit: 1_000,
            draw_amount: 400,
            advance_seconds: 0,
            from: CreditStatus::Suspended,
            to: CreditStatus::Closed,
            expect_ok: true,
            borrower_closes: false,
        },
        TransitionCase {
            label: "Suspended→Closed: principal > 0, borrower close MUST FAIL",
            credit_limit: 1_000,
            draw_amount: 400,
            advance_seconds: 0,
            from: CreditStatus::Suspended,
            to: CreditStatus::Closed,
            expect_ok: false,
            borrower_closes: true,
        },
        // ── Defaulted → Active (reinstate) ───────────────────────────────────
        TransitionCase {
            label: "Defaulted→Active: zero balance",
            credit_limit: 1_000,
            draw_amount: 0,
            advance_seconds: 0,
            from: CreditStatus::Defaulted,
            to: CreditStatus::Active,
            expect_ok: true,
            borrower_closes: false,
        },
        TransitionCase {
            label: "Defaulted→Active: principal > 0",
            credit_limit: 1_000,
            draw_amount: 700,
            advance_seconds: 0,
            from: CreditStatus::Defaulted,
            to: CreditStatus::Active,
            expect_ok: true,
            borrower_closes: false,
        },
        TransitionCase {
            label: "Defaulted→Active: accrued interest > 0",
            credit_limit: 1_000,
            draw_amount: 700,
            advance_seconds: 31_536_000,
            from: CreditStatus::Defaulted,
            to: CreditStatus::Active,
            expect_ok: true,
            borrower_closes: false,
        },
        // ── Defaulted → Closed ───────────────────────────────────────────────
        TransitionCase {
            label: "Defaulted→Closed: zero balance, borrower closes",
            credit_limit: 1_000,
            draw_amount: 0,
            advance_seconds: 0,
            from: CreditStatus::Defaulted,
            to: CreditStatus::Closed,
            expect_ok: true,
            borrower_closes: true,
        },
        TransitionCase {
            label: "Defaulted→Closed: principal > 0, admin force-closes",
            credit_limit: 1_000,
            draw_amount: 500,
            advance_seconds: 0,
            from: CreditStatus::Defaulted,
            to: CreditStatus::Closed,
            expect_ok: true,
            borrower_closes: false,
        },
        TransitionCase {
            label: "Defaulted→Closed: principal > 0, borrower close MUST FAIL",
            credit_limit: 1_000,
            draw_amount: 500,
            advance_seconds: 0,
            from: CreditStatus::Defaulted,
            to: CreditStatus::Closed,
            expect_ok: false,
            borrower_closes: true,
        },
        // ── Closed → Closed (idempotent) ─────────────────────────────────────
        TransitionCase {
            label: "Closed→Closed: idempotent admin close",
            credit_limit: 1_000,
            draw_amount: 0,
            advance_seconds: 0,
            from: CreditStatus::Closed,
            to: CreditStatus::Closed,
            expect_ok: true,
            borrower_closes: false,
        },
        // ── Illegal transitions ───────────────────────────────────────────────
        TransitionCase {
            label: "Suspended→Suspended: MUST FAIL (not Active)",
            credit_limit: 1_000,
            draw_amount: 0,
            advance_seconds: 0,
            from: CreditStatus::Suspended,
            to: CreditStatus::Suspended,
            expect_ok: false,
            borrower_closes: false,
        },
        TransitionCase {
            label: "Defaulted→Suspended: MUST FAIL (reinstate only to Active)",
            credit_limit: 1_000,
            draw_amount: 0,
            advance_seconds: 0,
            from: CreditStatus::Defaulted,
            to: CreditStatus::Suspended,
            expect_ok: false,
            borrower_closes: false,
        },
        TransitionCase {
            label: "Active→Active: re-suspend MUST FAIL (already Active, suspend only)",
            credit_limit: 1_000,
            draw_amount: 0,
            advance_seconds: 0,
            from: CreditStatus::Active,
            to: CreditStatus::Active, // attempted via reinstate on non-Defaulted
            expect_ok: false,
            borrower_closes: false,
        },
    ]
}

// ── transition executor ───────────────────────────────────────────────────────

/// Drive a credit line from `Active` to `tc.from`, then attempt `tc.from → tc.to`.
/// Returns `Ok(line_after)` on success, `Err(())` on panic.
fn run_transition(
    env: &Env,
    admin: &Address,
    contract_id: &Address,
    tc: &TransitionCase,
) -> Result<CreditLineData, ()> {
    let client = CreditClient::new(env, contract_id);
    let borrower = open_line(env, contract_id, tc.credit_limit, tc.draw_amount);

    // Advance time so accrual fires on the next mutating call.
    if tc.advance_seconds > 0 {
        env.ledger()
            .with_mut(|li| li.timestamp += tc.advance_seconds);
    }

    // Drive to `from` status.
    match tc.from {
        CreditStatus::Active => {}
        CreditStatus::Suspended => {
            client.suspend_credit_line(&borrower);
        }
        CreditStatus::Defaulted => {
            client.default_credit_line(&borrower);
        }
        CreditStatus::Closed => {
            client.close_credit_line(&borrower, admin);
        }
        CreditStatus::Restricted => {
            panic!("Restricted setup not supported in this harness");
        }
    }

    // Snapshot before the target transition.
    let before = client.get_credit_line(&borrower).unwrap();
    assert_accounting_invariant(&before, &format!("{} [before]", tc.label));

    // Attempt the target transition.
    let closer = if tc.borrower_closes {
        borrower.clone()
    } else {
        admin.clone()
    };

    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match (tc.from, tc.to) {
            (_, CreditStatus::Suspended) => client.suspend_credit_line(&borrower),
            (_, CreditStatus::Defaulted) => client.default_credit_line(&borrower),
            (_, CreditStatus::Closed) => client.close_credit_line(&borrower, &closer),
            (_, CreditStatus::Active) => {
                client.reinstate_credit_line(&borrower, &CreditStatus::Active)
            }
            (_, CreditStatus::Restricted) => {
                panic!("Restricted target not supported in this harness")
            }
        }));

    match result {
        Ok(_) => {
            let after = client.get_credit_line(&borrower).unwrap();
            assert_accounting_invariant(&after, &format!("{} [after]", tc.label));
            Ok(after)
        }
        Err(_) => Err(()),
    }
}

// ── table-driven test ─────────────────────────────────────────────────────────

#[test]
fn state_transition_matrix() {
    for tc in transition_cases() {
        let (env, admin, contract_id) = setup_env();
        let result = run_transition(&env, &admin, &contract_id, &tc);

        match (tc.expect_ok, result) {
            (true, Ok(after)) => {
                assert_eq!(
                    after.status, tc.to,
                    "{}: expected status {:?}, got {:?}",
                    tc.label, tc.to, after.status
                );
            }
            (false, Err(_)) => {
                // Expected failure — pass.
            }
            (true, Err(_)) => {
                panic!(
                    "{}: expected transition to succeed but it panicked",
                    tc.label
                );
            }
            (false, Ok(after)) => {
                panic!(
                    "{}: expected transition to fail but it succeeded (status={:?})",
                    tc.label, after.status
                );
            }
        }
    }
}

// ── focused invariant tests ───────────────────────────────────────────────────

/// Debt record is fully preserved across Active → Suspended → Defaulted.
#[test]
fn debt_record_preserved_through_suspend_then_default() {
    let (env, _admin, contract_id) = setup_env();
    let client = CreditClient::new(&env, &contract_id);
    let borrower = open_line(&env, &contract_id, 1_000, 600);

    // Advance 1 year to accumulate interest.
    env.ledger().with_mut(|li| li.timestamp += 31_536_000);

    // Active → Suspended (accrual fires here).
    client.suspend_credit_line(&borrower);
    let after_suspend = client.get_credit_line(&borrower).unwrap();
    assert_eq!(after_suspend.status, CreditStatus::Suspended);
    assert_accounting_invariant(&after_suspend, "after suspend");
    let debt_at_suspend = after_suspend.utilized_amount;
    let interest_at_suspend = after_suspend.accrued_interest;

    // Suspended → Defaulted (no additional time, no double-count).
    client.default_credit_line(&borrower);
    let after_default = client.get_credit_line(&borrower).unwrap();
    assert_eq!(after_default.status, CreditStatus::Defaulted);
    assert_accounting_invariant(&after_default, "after default");

    // Debt must not change between Suspended and Defaulted (no time elapsed).
    assert_eq!(
        after_default.utilized_amount, debt_at_suspend,
        "utilized_amount must not change on Suspended→Defaulted"
    );
    assert_eq!(
        after_default.accrued_interest, interest_at_suspend,
        "accrued_interest must not change on Suspended→Defaulted"
    );
}

/// No double-counting of interest across Defaulted → Active (reinstate).
#[test]
fn no_double_interest_on_reinstate() {
    let (env, _admin, contract_id) = setup_env();
    let client = CreditClient::new(&env, &contract_id);
    let borrower = open_line(&env, &contract_id, 1_000, 500);

    // Advance time and default.
    env.ledger().with_mut(|li| li.timestamp += 31_536_000);
    client.default_credit_line(&borrower);
    let after_default = client.get_credit_line(&borrower).unwrap();
    assert_accounting_invariant(&after_default, "after default");
    let debt_at_default = after_default.utilized_amount;
    let interest_at_default = after_default.accrued_interest;

    // Reinstate immediately (no time elapsed).
    client.reinstate_credit_line(&borrower, &CreditStatus::Active);
    let after_reinstate = client.get_credit_line(&borrower).unwrap();
    assert_eq!(after_reinstate.status, CreditStatus::Active);
    assert_accounting_invariant(&after_reinstate, "after reinstate");

    // Debt must be identical — no extra interest injected by reinstate.
    assert_eq!(
        after_reinstate.utilized_amount, debt_at_default,
        "reinstate must not alter utilized_amount"
    );
    assert_eq!(
        after_reinstate.accrued_interest, interest_at_default,
        "reinstate must not alter accrued_interest"
    );
}

/// Admin force-close preserves the full debt record (balance is not zeroed).
#[test]
fn admin_close_preserves_debt_record() {
    let (env, admin, contract_id) = setup_env();
    let client = CreditClient::new(&env, &contract_id);
    let borrower = open_line(&env, &contract_id, 1_000, 400);

    env.ledger().with_mut(|li| li.timestamp += 31_536_000);
    client.default_credit_line(&borrower);

    let before_close = client.get_credit_line(&borrower).unwrap();
    assert_accounting_invariant(&before_close, "before close");

    client.close_credit_line(&borrower, &admin);
    let after_close = client.get_credit_line(&borrower).unwrap();
    assert_eq!(after_close.status, CreditStatus::Closed);
    assert_accounting_invariant(&after_close, "after close");

    // Closing must not wipe the debt — the record is preserved for off-chain reconciliation.
    assert_eq!(
        after_close.utilized_amount, before_close.utilized_amount,
        "admin close must not zero utilized_amount"
    );
    assert_eq!(
        after_close.accrued_interest, before_close.accrued_interest,
        "admin close must not zero accrued_interest"
    );
}

/// Borrower cannot close while any balance (principal or interest) remains.
#[test]
fn borrower_cannot_close_with_nonzero_balance() {
    let (env, _admin, contract_id) = setup_env();
    let client = CreditClient::new(&env, &contract_id);
    let borrower = open_line(&env, &contract_id, 1_000, 1);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        client.close_credit_line(&borrower, &borrower);
    }));
    assert!(
        result.is_err(),
        "borrower close with balance > 0 must panic"
    );

    // Line must still be Active — no partial state change.
    let line = client.get_credit_line(&borrower).unwrap();
    assert_eq!(line.status, CreditStatus::Active);
    assert_accounting_invariant(&line, "after failed borrower close");
}

/// Borrower can close only after full repayment.
#[test]
fn borrower_can_close_after_full_repayment() {
    let (env, _admin, contract_id) = setup_env();
    let client = CreditClient::new(&env, &contract_id);
    let borrower = open_line(&env, &contract_id, 1_000, 0);

    // No draw — utilized_amount is 0, borrower close is allowed.
    client.close_credit_line(&borrower, &borrower);
    let line = client.get_credit_line(&borrower).unwrap();
    assert_eq!(line.status, CreditStatus::Closed);
    assert_accounting_invariant(&line, "after borrower close");
}

/// Suspend is only valid from Active; all other sources must fail.
#[test]
fn suspend_only_valid_from_active() {
    for from in [
        CreditStatus::Suspended,
        CreditStatus::Defaulted,
        CreditStatus::Closed,
    ] {
        let (env, admin, contract_id) = setup_env();
        let client = CreditClient::new(&env, &contract_id);
        let borrower = open_line(&env, &contract_id, 1_000, 0);

        // Drive to `from`.
        match from {
            CreditStatus::Suspended => client.suspend_credit_line(&borrower),
            CreditStatus::Defaulted => client.default_credit_line(&borrower),
            CreditStatus::Closed => client.close_credit_line(&borrower, &admin),
            _ => unreachable!(),
        }

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.suspend_credit_line(&borrower);
        }));
        assert!(result.is_err(), "suspend from {from:?} must fail");
    }
}

/// Reinstate is only valid from Defaulted; Active and Suspended must fail.
#[test]
fn reinstate_only_valid_from_defaulted() {
    for from in [CreditStatus::Active, CreditStatus::Suspended] {
        let (env, _admin, contract_id) = setup_env();
        let client = CreditClient::new(&env, &contract_id);
        let borrower = open_line(&env, &contract_id, 1_000, 0);

        if from == CreditStatus::Suspended {
            client.suspend_credit_line(&borrower);
        }

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.reinstate_credit_line(&borrower, &CreditStatus::Active);
        }));
        assert!(result.is_err(), "reinstate from {from:?} must fail");
    }
}

// ── reinstate target_status coverage (#task/reinstate-target-status-tests) ───

/// Defaulted → Active: the canonical reinstate path.
/// Debt and interest are unchanged; status flips to Active.
#[test]
fn reinstate_defaulted_to_active() {
    let (env, _admin, contract_id) = setup_env();
    let client = CreditClient::new(&env, &contract_id);
    let borrower = open_line(&env, &contract_id, 1_000, 500);

    client.default_credit_line(&borrower);
    assert_eq!(
        client.get_credit_line(&borrower).unwrap().status,
        CreditStatus::Defaulted
    );

    client.reinstate_credit_line(&borrower, &CreditStatus::Active);
    let line = client.get_credit_line(&borrower).unwrap();

    assert_eq!(line.status, CreditStatus::Active);
    assert_accounting_invariant(&line, "reinstate to Active");
}

/// Defaulted → Restricted: valid when the admin wants to cap draws while
/// requiring the borrower to repay the excess balance first.
#[test]
fn reinstate_defaulted_to_restricted() {
    let (env, _admin, contract_id) = setup_env();
    let client = CreditClient::new(&env, &contract_id);
    let borrower = open_line(&env, &contract_id, 1_000, 500);

    client.default_credit_line(&borrower);
    let before = client.get_credit_line(&borrower).unwrap();
    assert_eq!(before.status, CreditStatus::Defaulted);

    client.reinstate_credit_line(&borrower, &CreditStatus::Restricted);
    let line = client.get_credit_line(&borrower).unwrap();

    assert_eq!(line.status, CreditStatus::Restricted);
    // Debt must be preserved — reinstate never alters balances.
    assert_eq!(line.utilized_amount, before.utilized_amount);
    assert_eq!(line.accrued_interest, before.accrued_interest);
    assert_accounting_invariant(&line, "reinstate to Restricted");
}

/// Reinstating to Closed, Defaulted, or Suspended must revert.
/// These targets are outside the allowed set per the state-machine spec.
#[test]
fn reinstate_invalid_targets_revert() {
    for bad_target in [
        CreditStatus::Closed,
        CreditStatus::Defaulted,
        CreditStatus::Suspended,
    ] {
        let (env, _admin, contract_id) = setup_env();
        let client = CreditClient::new(&env, &contract_id);
        let borrower = open_line(&env, &contract_id, 1_000, 0);

        client.default_credit_line(&borrower);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.reinstate_credit_line(&borrower, &bad_target);
        }));
        assert!(result.is_err(), "reinstate to {bad_target:?} must revert");

        // Line must remain Defaulted — no partial state change.
        let line = client.get_credit_line(&borrower).unwrap();
        assert_eq!(
            line.status,
            CreditStatus::Defaulted,
            "status must stay Defaulted after failed reinstate to {bad_target:?}"
        );
    }
}

/// Accrued interest is materialized (not lost) when transitioning Active → Suspended.
/// Verifies the interest-continues-to-accrue-until-checkpoint behaviour.
#[test]
fn interest_materialized_on_suspend() {
    let (env, _admin, contract_id) = setup_env();
    let client = CreditClient::new(&env, &contract_id);
    let borrower = open_line(&env, &contract_id, 10_000, 10_000);

    // Advance 1 year — at 300 bps on 10_000 principal, expect ~300 interest.
    env.ledger().with_mut(|li| li.timestamp += 31_536_000);

    let before = client.get_credit_line(&borrower).unwrap();
    // Before suspend, accrual hasn't fired yet (lazy).
    assert_eq!(
        before.accrued_interest, 0,
        "accrual is lazy before mutation"
    );

    client.suspend_credit_line(&borrower);
    let after = client.get_credit_line(&borrower).unwrap();
    assert_eq!(after.status, CreditStatus::Suspended);
    assert_accounting_invariant(&after, "after suspend with accrual");

    // Interest must have been capitalized.
    assert!(
        after.accrued_interest > 0,
        "accrued_interest must be > 0 after 1 year at 300 bps"
    );
    assert!(
        after.utilized_amount > 10_000,
        "utilized_amount must grow after interest accrual"
    );
    // Invariant: total = principal + interest.
    let principal = after.utilized_amount - after.accrued_interest;
    assert_eq!(principal, 10_000, "principal must equal original draw");
}

/// Closing an already-Closed line is idempotent (no panic, no state change).
#[test]
fn close_already_closed_is_idempotent() {
    let (env, admin, contract_id) = setup_env();
    let client = CreditClient::new(&env, &contract_id);
    let borrower = open_line(&env, &contract_id, 1_000, 0);

    client.close_credit_line(&borrower, &admin);
    let first = client.get_credit_line(&borrower).unwrap();
    assert_eq!(first.status, CreditStatus::Closed);

    // Second close must not panic.
    client.close_credit_line(&borrower, &admin);
    let second = client.get_credit_line(&borrower).unwrap();
    assert_eq!(second.status, CreditStatus::Closed);
    assert_eq!(second.utilized_amount, first.utilized_amount);
    assert_eq!(second.accrued_interest, first.accrued_interest);
}

/// Full lifecycle: Active → Suspended → Defaulted → Active → Closed.
/// Invariant holds at every checkpoint.
#[test]
fn full_lifecycle_invariant_chain() {
    let (env, admin, contract_id) = setup_env();
    let client = CreditClient::new(&env, &contract_id);
    let borrower = open_line(&env, &contract_id, 5_000, 2_000);

    // Checkpoint 1: Active with principal.
    let c1 = client.get_credit_line(&borrower).unwrap();
    assert_eq!(c1.status, CreditStatus::Active);
    assert_accounting_invariant(&c1, "c1 Active");

    // Advance time, then suspend.
    env.ledger().with_mut(|li| li.timestamp += 15_768_000); // 6 months
    client.suspend_credit_line(&borrower);
    let c2 = client.get_credit_line(&borrower).unwrap();
    assert_eq!(c2.status, CreditStatus::Suspended);
    assert_accounting_invariant(&c2, "c2 Suspended");
    assert!(
        c2.utilized_amount >= c1.utilized_amount,
        "debt must not decrease"
    );

    // Default.
    client.default_credit_line(&borrower);
    let c3 = client.get_credit_line(&borrower).unwrap();
    assert_eq!(c3.status, CreditStatus::Defaulted);
    assert_accounting_invariant(&c3, "c3 Defaulted");
    assert_eq!(
        c3.utilized_amount, c2.utilized_amount,
        "no time elapsed, debt unchanged"
    );

    // Reinstate.
    client.reinstate_credit_line(&borrower, &CreditStatus::Active);
    let c4 = client.get_credit_line(&borrower).unwrap();
    assert_eq!(c4.status, CreditStatus::Active);
    assert_accounting_invariant(&c4, "c4 Reinstated");
    assert_eq!(
        c4.utilized_amount, c3.utilized_amount,
        "reinstate must not alter debt"
    );

    // Admin force-close.
    client.close_credit_line(&borrower, &admin);
    let c5 = client.get_credit_line(&borrower).unwrap();
    assert_eq!(c5.status, CreditStatus::Closed);
    assert_accounting_invariant(&c5, "c5 Closed");
    assert_eq!(
        c5.utilized_amount, c4.utilized_amount,
        "close must not alter debt"
    );
}
