extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events, Ledger},
    token::StellarAssetClient,
    token::TokenClient,
    IntoVal, Symbol, TryFromVal, Val,
};

/// The review window most tests use: the one-hour minimum.
const WINDOW: u64 = MIN_REVIEW_WINDOW_SECS;

/// A non-zero starting ledger time, so `submitted_at` is never confused with the
/// `0` that marks a `Pending` milestone.
const T0: u64 = 1_000_000;

struct Setup<'a> {
    env: Env,
    escrow: EscrowClient<'a>,
    escrow_id: Address,
    token: TokenClient<'a>,
    client: Address,
    freelancer: Address,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(T0);
    let admin = Address::generate(&env);
    let client = Address::generate(&env);
    let freelancer = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    StellarAssetClient::new(&env, &sac.address()).mint(&client, &1_000);
    let id = env.register(Escrow, ());
    let escrow = EscrowClient::new(&env, &id);
    escrow.initialize(&admin);
    let token = TokenClient::new(&env, &sac.address());
    Setup {
        env,
        escrow,
        escrow_id: id,
        token,
        client,
        freelancer,
    }
}

/// Build the `Vec<i128>` the contract takes from a plain slice.
fn amounts(env: &Env, xs: &[i128]) -> Vec<i128> {
    let mut v = Vec::new(env);
    for x in xs {
        v.push_back(*x);
    }
    v
}

/// Create an escrow with the given milestone amounts and review window.
fn create_with_window(s: &Setup, xs: &[i128], window: u64) -> u64 {
    s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &amounts(&s.env, xs),
        &window,
    )
}

/// Create an escrow with the default review window.
fn create(s: &Setup, xs: &[i128]) -> u64 {
    create_with_window(s, xs, WINDOW)
}

/// Create an escrow, fund it fully, and return its id.
fn funded(s: &Setup, xs: &[i128]) -> u64 {
    let id = create(s, xs);
    let total: i128 = xs.iter().sum();
    s.escrow.deposit_funds(&id, &total);
    id
}

/// The custody invariant: the tokens the contract actually holds must equal
/// `funded_amount - released_amount`. Asserted after every money-moving call, so
/// a payout that forgets to update the accounting (or the reverse) is caught.
fn assert_custody(s: &Setup, id: u64) {
    let c = s.escrow.get_contract(&id);
    assert_eq!(
        s.token.balance(&s.escrow_id),
        c.funded_amount - c.released_amount,
        "contract custody must equal funded_amount - released_amount",
    );
}

fn status_of(s: &Setup, id: u64, index: u32) -> MilestoneStatus {
    s.escrow.get_contract(&id).milestones.get(index).unwrap().status
}

// ---------------------------------------------------------------------------
// Pre-existing behaviour, updated only for the `create_contract` signature.
// ---------------------------------------------------------------------------

#[test]
fn initialize_only_once() {
    let s = setup();
    let other = Address::generate(&s.env);
    assert_eq!(
        s.escrow.try_initialize(&other),
        Err(Ok(EscrowError::AlreadyInitialized))
    );
}

#[test]
fn full_happy_path() {
    let s = setup();
    let id = create(&s, &[100, 200]);
    assert_eq!(id, 1);
    s.escrow.deposit_funds(&id, &300);
    assert_eq!(s.token.balance(&s.client), 700);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);
    s.escrow.release_milestone(&id, &0);
    s.escrow.release_milestone(&id, &1);
    assert_eq!(s.token.balance(&s.freelancer), 300);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Completed);
    s.escrow.issue_reputation(&id, &5);
    let rep = s.escrow.get_reputation(&s.freelancer);
    assert_eq!(rep.completed_contracts, 1);
    assert_eq!(rep.total_rating, 5);
}

#[test]
fn rejects_overfunding_and_double_release() {
    let s = setup();
    let id = create(&s, &[100, 100]);
    assert_eq!(
        s.escrow.try_deposit_funds(&id, &201),
        Err(Ok(EscrowError::OverFunded))
    );
    s.escrow.deposit_funds(&id, &200);

    s.escrow.release_milestone(&id, &0);
    // One milestone is still outstanding, so the escrow is active and the
    // milestone-level guard is the one that fires.
    assert_eq!(
        s.escrow.try_release_milestone(&id, &0),
        Err(Ok(EscrowError::MilestoneAlreadyReleased))
    );

    s.escrow.release_milestone(&id, &1);
    // Now the escrow is Completed, and the status gate fires before the
    // milestone is ever inspected.
    assert_eq!(
        s.escrow.try_release_milestone(&id, &1),
        Err(Ok(EscrowError::InvalidStatus))
    );
}

#[test]
fn dispute_split_and_cancel() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.escrow.open_dispute(&id, &s.freelancer);
    s.escrow.resolve_dispute(&id, &2_500);
    assert_eq!(s.token.balance(&s.freelancer), 100);
    assert_eq!(s.token.balance(&s.client), 900);
    assert_custody(&s, id);

    let id2 = funded(&s, &[50]);
    s.escrow.cancel_contract(&id2);
    assert_eq!(s.token.balance(&s.client), 900);
    assert_eq!(s.escrow.get_contract(&id2).status, ContractStatus::Cancelled);
    assert_custody(&s, id2);
}

#[test]
fn pause_blocks_mutations() {
    let s = setup();
    let id = funded(&s, &[100]);
    s.escrow.set_paused(&true);
    assert_eq!(
        s.escrow.try_create_contract(
            &s.client,
            &s.freelancer,
            &s.token.address,
            &amounts(&s.env, &[1]),
            &WINDOW,
        ),
        Err(Ok(EscrowError::Paused))
    );
    assert_eq!(
        s.escrow.try_submit_milestone(&id, &0),
        Err(Ok(EscrowError::Paused))
    );
    assert_eq!(
        s.escrow.try_claim_milestone(&id, &0),
        Err(Ok(EscrowError::Paused))
    );
    assert_eq!(
        s.escrow.try_cancel_contract(&id),
        Err(Ok(EscrowError::Paused))
    );
    assert_eq!(
        s.escrow.try_release_milestone(&id, &0),
        Err(Ok(EscrowError::Paused))
    );
}

// ---------------------------------------------------------------------------
// The attack this issue closes.
// ---------------------------------------------------------------------------

/// Freelancer delivers milestone 0, the client copies the work and cancels in the
/// same ledger. Before the submission state the client took 100% back and the
/// freelancer got nothing; the only recourse was to race the client with
/// `open_dispute`.
///
/// `submit_milestone` does not exist on `main`, so this cannot compile there.
/// The main-side demonstration is in the PR description.
#[test]
fn cancel_after_delivery_cannot_claw_back_submitted_work() {
    let s = setup();
    let id = funded(&s, &[400, 600]);
    assert_custody(&s, id);

    // Freelancer delivers the first milestone.
    s.escrow.submit_milestone(&id, &0);
    assert_eq!(status_of(&s, id, 0), MilestoneStatus::Submitted);

    // Client cancels immediately, in the same ledger as the submission.
    s.escrow.cancel_contract(&id);
    assert_eq!(
        s.escrow.get_contract(&id).status,
        ContractStatus::Cancelled
    );

    // Only the milestone that was never delivered is refunded.
    assert_eq!(s.token.balance(&s.client), 1_000 - 400);
    assert_eq!(s.token.balance(&s.freelancer), 0);
    assert_custody(&s, id);

    // The delivered work is still payable: the window lapses and the freelancer
    // claims it without the client's cooperation.
    s.env.ledger().set_timestamp(T0 + WINDOW);
    s.escrow.claim_milestone(&id, &0);
    assert_eq!(s.token.balance(&s.freelancer), 400);
    assert_eq!(s.token.balance(&s.client), 600);
    assert_eq!(status_of(&s, id, 0), MilestoneStatus::Released);
    assert_custody(&s, id);

    // The never-delivered milestone is not claimable: it was refunded.
    assert_eq!(
        s.escrow.try_claim_milestone(&id, &1),
        Err(Ok(EscrowError::MilestoneNotSubmitted))
    );
}

/// Cancelling with one submitted and one pending milestone refunds only the
/// pending amount, and the submitted one stays claimable afterwards.
#[test]
fn cancel_refunds_only_pending_backing() {
    let s = setup();
    let id = funded(&s, &[250, 250, 500]);
    s.escrow.submit_milestone(&id, &0);
    s.escrow.submit_milestone(&id, &2);

    s.escrow.cancel_contract(&id);

    // 750 was delivered (250 + 500), so only the 250 pending milestone is refunded.
    assert_eq!(s.token.balance(&s.client), 250);
    assert_custody(&s, id);
    let c = s.escrow.get_contract(&id);
    assert_eq!(c.funded_amount, 750);
    assert_eq!(c.released_amount, 0);

    // Both submitted milestones remain claimable after the cancel.
    s.env.ledger().set_timestamp(T0 + WINDOW);
    s.escrow.claim_milestone(&id, &0);
    s.escrow.claim_milestone(&id, &2);
    assert_eq!(s.token.balance(&s.freelancer), 750);
    assert_custody(&s, id);
}

/// With nothing submitted, cancellation behaves exactly as it did before.
#[test]
fn cancel_with_no_submitted_milestones_is_unchanged() {
    let s = setup();
    let id = funded(&s, &[400, 600]);
    s.escrow.cancel_contract(&id);
    assert_eq!(s.token.balance(&s.client), 1_000);
    assert_eq!(s.token.balance(&s.freelancer), 0);
    let c = s.escrow.get_contract(&id);
    assert_eq!(c.funded_amount, 0);
    assert_custody(&s, id);
}

// ---------------------------------------------------------------------------
// submit_milestone
// ---------------------------------------------------------------------------

#[test]
fn submit_milestone_records_delivery_and_the_timestamp() {
    let s = setup();
    let id = funded(&s, &[400, 600]);
    s.env.ledger().set_timestamp(T0 + 77);

    s.escrow.submit_milestone(&id, &0);

    let m = s.escrow.get_contract(&id).milestones.get(0).unwrap();
    assert_eq!(m.status, MilestoneStatus::Submitted);
    assert_eq!(m.submitted_at, T0 + 77);
    // The other milestone is untouched.
    assert_eq!(status_of(&s, id, 1), MilestoneStatus::Pending);
    // No money moved.
    assert_eq!(s.token.balance(&s.freelancer), 0);
    assert_custody(&s, id);
}

/// The last event the contract published, as `(contract, topics, data)`.
fn last_event(s: &Setup) -> (Address, Vec<Val>, Val) {
    let events = s.env.events().all();
    events.get(events.len() - 1).unwrap()
}

/// Element `i` of an event's topic list, converted to `T`.
fn topic<T: IntoVal<Env, Val> + TryFromVal<Env, Val>>(s: &Setup, topics: &Vec<Val>, i: u32) -> T {
    topics.get(i).unwrap().into_val(&s.env)
}

#[test]
fn submit_milestone_emits_the_submission_event() {
    let s = setup();
    let id = funded(&s, &[400, 600]);
    s.env.ledger().set_timestamp(T0 + 5);
    s.escrow.submit_milestone(&id, &0);

    let (contract, topics, data) = last_event(&s);
    assert_eq!(contract, s.escrow_id);
    assert_eq!(topic::<Symbol>(&s, &topics, 0), symbol_short!("submit"));
    assert_eq!(topic::<u64>(&s, &topics, 1), id);

    let payload: Vec<Val> = data.into_val(&s.env);
    assert_eq!(topic::<u32>(&s, &payload, 0), 0);
    assert_eq!(topic::<u64>(&s, &payload, 1), T0 + 5);
}

#[test]
fn claim_milestone_emits_the_claim_event() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.escrow.submit_milestone(&id, &0);
    s.env.ledger().set_timestamp(T0 + WINDOW);
    s.escrow.claim_milestone(&id, &0);

    let (contract, topics, data) = last_event(&s);
    assert_eq!(contract, s.escrow_id);
    assert_eq!(topic::<Symbol>(&s, &topics, 0), symbol_short!("claim"));
    assert_eq!(topic::<u64>(&s, &topics, 1), id);

    let payload: Vec<Val> = data.into_val(&s.env);
    assert_eq!(topic::<u32>(&s, &payload, 0), 0);
    assert_eq!(topic::<i128>(&s, &payload, 1), 400);
}

/// `cancel_contract` reports the amount it actually refunded, which is the
/// pending backing and not the whole unreleased balance.
#[test]
fn cancel_event_reports_only_the_refund() {
    let s = setup();
    let id = funded(&s, &[400, 600]);
    s.escrow.submit_milestone(&id, &0);
    s.escrow.cancel_contract(&id);

    let (contract, topics, data) = last_event(&s);
    assert_eq!(contract, s.escrow_id);
    assert_eq!(topic::<Symbol>(&s, &topics, 0), symbol_short!("cancel"));
    assert_eq!(topic::<u64>(&s, &topics, 1), id);

    let refund: i128 = data.into_val(&s.env);
    assert_eq!(refund, 600, "only the pending milestone is refunded");
}

#[test]
fn submit_milestone_rejects_the_client() {
    let s = setup();
    let id = funded(&s, &[400]);
    // Only the freelancer may declare work delivered.
    assert!(s.escrow.try_submit_milestone(&id, &0).is_ok());

    // With auths mocked for everyone, prove the check by asserting the freelancer
    // is the party the contract authenticates: drop the mock and sign as the
    // client instead, which must fail.
    let s2 = setup();
    let id2 = funded(&s2, &[400]);
    s2.env.mock_auths(&[soroban_sdk::testutils::MockAuth {
        address: &s2.client,
        invoke: &soroban_sdk::testutils::MockAuthInvoke {
            contract: &s2.escrow_id,
            fn_name: "submit_milestone",
            args: (&id2, 0u32).into_val(&s2.env),
            sub_invokes: &[],
        },
    }]);
    assert!(s2.escrow.try_submit_milestone(&id2, &0).is_err());
}

#[test]
fn submit_milestone_requires_a_funded_escrow() {
    let s = setup();
    let id = create(&s, &[400]);
    assert_eq!(
        s.escrow.try_submit_milestone(&id, &0),
        Err(Ok(EscrowError::InvalidStatus))
    );

    // Partially funded is still not `Funded`, so submission is refused.
    s.escrow.deposit_funds(&id, &100);
    assert_eq!(
        s.escrow.try_submit_milestone(&id, &0),
        Err(Ok(EscrowError::InvalidStatus))
    );

    s.escrow.deposit_funds(&id, &300);
    assert!(s.escrow.try_submit_milestone(&id, &0).is_ok());
}

#[test]
fn submit_milestone_rejects_a_second_submission() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.escrow.submit_milestone(&id, &0);
    // Re-submitting would restart the review clock, letting the freelancer
    // postpone review indefinitely.
    s.env.ledger().set_timestamp(T0 + WINDOW);
    assert_eq!(
        s.escrow.try_submit_milestone(&id, &0),
        Err(Ok(EscrowError::InvalidStatus))
    );
    // The original submission time is preserved.
    let m = s.escrow.get_contract(&id).milestones.get(0).unwrap();
    assert_eq!(m.submitted_at, T0);
}

#[test]
fn submit_milestone_rejects_a_released_milestone() {
    let s = setup();
    let id = funded(&s, &[400, 600]);
    // One milestone outstanding, so the escrow is still Funded and the
    // milestone-level guard is the one that fires.
    s.escrow.release_milestone(&id, &0);
    assert_eq!(
        s.escrow.try_submit_milestone(&id, &0),
        Err(Ok(EscrowError::MilestoneAlreadyReleased))
    );
}

#[test]
fn submit_milestone_rejects_an_unknown_index() {
    let s = setup();
    let id = funded(&s, &[400]);
    assert_eq!(
        s.escrow.try_submit_milestone(&id, &7),
        Err(Ok(EscrowError::MilestoneNotFound))
    );
}

/// Submission must not be able to promise more than the contract holds.
///
/// Note on why there is no test that drives `uncommitted_funds` to refuse:
/// `submit_milestone` requires the escrow to be `Funded`, which means
/// `funded_amount == total_amount`. Under that condition `uncommitted_funds`
/// equals the sum of the still-`Pending` milestone amounts, and the milestone
/// being submitted is one of them, so it always fits. The guard is therefore
/// unreachable through the public API today — it is kept because it states the
/// bound explicitly instead of deriving it from the funding gate, so the check
/// survives any future change to that gate. What *is* testable is the resulting
/// invariant, which this asserts across every submission order.
#[test]
fn submitted_backing_never_exceeds_the_funded_balance() {
    let s = setup();
    let id = funded(&s, &[400, 600]);
    let holds = |s: &Setup| {
        let c = s.escrow.get_contract(&id);
        assert!(
            submitted_backing(&c) <= c.funded_amount - c.released_amount,
            "submitted backing {} exceeded funded - released {}",
            submitted_backing(&c),
            c.funded_amount - c.released_amount,
        );
    };

    holds(&s);
    s.escrow.submit_milestone(&id, &0);
    holds(&s);
    s.escrow.submit_milestone(&id, &1);
    holds(&s);
    assert_eq!(submitted_backing(&s.escrow.get_contract(&id)), 1_000);

    // Releasing frees backing rather than double-counting it.
    s.escrow.release_milestone(&id, &0);
    holds(&s);
    assert_eq!(submitted_backing(&s.escrow.get_contract(&id)), 600);
    assert_custody(&s, id);
}

/// A partially funded escrow cannot accept submissions at all, which is the
/// other half of "cannot lock more than is funded".
#[test]
fn submit_milestone_refuses_a_partially_funded_escrow() {
    let s = setup();
    let id = create(&s, &[400, 600]);
    s.escrow.deposit_funds(&id, &999);
    assert_eq!(
        s.escrow.try_submit_milestone(&id, &0),
        Err(Ok(EscrowError::InvalidStatus))
    );
    // Filling the last unit flips the escrow to Funded and submission opens.
    s.escrow.deposit_funds(&id, &1);
    assert!(s.escrow.try_submit_milestone(&id, &0).is_ok());
}

/// Once a milestone is submitted, later submissions see it as committed.
#[test]
fn submitted_backing_is_deducted_from_later_submissions() {
    let s = setup();
    let id = funded(&s, &[300, 300, 400]);
    s.escrow.submit_milestone(&id, &0);
    s.escrow.submit_milestone(&id, &1);
    // 600 is committed, so the 400 milestone fits exactly.
    s.escrow.submit_milestone(&id, &2);
    assert_eq!(submitted_backing(&s.escrow.get_contract(&id)), 1_000);
}

/// Released milestones free their backing for later submissions, and already
/// released money is not re-promised.
#[test]
fn released_milestones_do_not_count_as_committed() {
    let s = setup();
    let id = funded(&s, &[400, 600]);
    s.escrow.release_milestone(&id, &0);
    // 400 released, 600 still held, so the 600 milestone fits exactly.
    s.escrow.submit_milestone(&id, &1);
    assert_eq!(submitted_backing(&s.escrow.get_contract(&id)), 600);
    assert_custody(&s, id);
}

// ---------------------------------------------------------------------------
// claim_milestone and the review window
// ---------------------------------------------------------------------------

#[test]
fn claim_milestone_pays_after_the_window() {
    let s = setup();
    let id = funded(&s, &[400, 600]);
    s.escrow.submit_milestone(&id, &0);

    s.env.ledger().set_timestamp(T0 + WINDOW);
    s.escrow.claim_milestone(&id, &0);

    assert_eq!(s.token.balance(&s.freelancer), 400);
    assert_eq!(status_of(&s, id, 0), MilestoneStatus::Released);
    let m = s.escrow.get_contract(&id).milestones.get(0).unwrap();
    assert_eq!(m.submitted_at, 0, "submitted_at is cleared on release");
    assert_custody(&s, id);
}

/// The boundary, exactly: one second early fails, the deadline itself succeeds.
#[test]
fn claim_milestone_window_boundary_is_exact() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.escrow.submit_milestone(&id, &0);

    s.env.ledger().set_timestamp(T0 + WINDOW - 1);
    assert_eq!(
        s.escrow.try_claim_milestone(&id, &0),
        Err(Ok(EscrowError::ReviewWindowActive))
    );

    s.env.ledger().set_timestamp(T0 + WINDOW);
    assert!(s.escrow.try_claim_milestone(&id, &0).is_ok());
    assert_eq!(s.token.balance(&s.freelancer), 400);
}

#[test]
fn claim_milestone_before_the_window_is_refused() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.escrow.submit_milestone(&id, &0);
    assert_eq!(
        s.escrow.try_claim_milestone(&id, &0),
        Err(Ok(EscrowError::ReviewWindowActive))
    );
    assert_eq!(s.token.balance(&s.freelancer), 0);
}

#[test]
fn claim_milestone_rejects_a_pending_milestone() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.env.ledger().set_timestamp(T0 + WINDOW * 10);
    assert_eq!(
        s.escrow.try_claim_milestone(&id, &0),
        Err(Ok(EscrowError::MilestoneNotSubmitted))
    );
}

#[test]
fn claim_milestone_cannot_be_called_twice() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.escrow.submit_milestone(&id, &0);
    s.env.ledger().set_timestamp(T0 + WINDOW);
    s.escrow.claim_milestone(&id, &0);

    assert_eq!(
        s.escrow.try_claim_milestone(&id, &0),
        Err(Ok(EscrowError::MilestoneNotSubmitted))
    );
    assert_eq!(s.token.balance(&s.freelancer), 400, "paid exactly once");
    assert_custody(&s, id);
}

#[test]
fn claim_milestone_is_frozen_by_a_dispute() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.escrow.submit_milestone(&id, &0);
    s.escrow.open_dispute(&id, &s.client);

    // Even well past the window, the dispute owns the funds.
    s.env.ledger().set_timestamp(T0 + WINDOW * 100);
    assert_eq!(
        s.escrow.try_claim_milestone(&id, &0),
        Err(Ok(EscrowError::InvalidStatus))
    );
    assert_eq!(s.token.balance(&s.freelancer), 0);
}

/// A resolved dispute pays the whole held balance out, so a stale `Submitted`
/// milestone must not be claimable afterwards: that would pay twice.
#[test]
fn claim_milestone_is_refused_after_a_resolved_dispute() {
    let s = setup();
    let id = funded(&s, &[400, 600]);
    s.escrow.submit_milestone(&id, &0);
    s.escrow.open_dispute(&id, &s.client);
    s.escrow.resolve_dispute(&id, &5_000);

    let paid = s.token.balance(&s.freelancer);
    assert_eq!(paid, 500);
    assert_custody(&s, id);

    s.env.ledger().set_timestamp(T0 + WINDOW * 100);
    assert_eq!(
        s.escrow.try_claim_milestone(&id, &0),
        Err(Ok(EscrowError::InsufficientFunds))
    );
    assert_eq!(
        s.token.balance(&s.freelancer),
        paid,
        "a resolved dispute must not be paid a second time",
    );
}

#[test]
fn claim_milestone_rejects_an_unknown_index() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.env.ledger().set_timestamp(T0 + WINDOW);
    assert_eq!(
        s.escrow.try_claim_milestone(&id, &9),
        Err(Ok(EscrowError::MilestoneNotFound))
    );
}

#[test]
fn claim_milestone_rejects_the_client() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.escrow.submit_milestone(&id, &0);
    s.env.ledger().set_timestamp(T0 + WINDOW);

    let s2 = setup();
    let id2 = funded(&s2, &[400]);
    s2.escrow.submit_milestone(&id2, &0);
    s2.env.ledger().set_timestamp(T0 + WINDOW);
    s2.env.mock_auths(&[soroban_sdk::testutils::MockAuth {
        address: &s2.client,
        invoke: &soroban_sdk::testutils::MockAuthInvoke {
            contract: &s2.escrow_id,
            fn_name: "claim_milestone",
            args: (&id2, 0u32).into_val(&s2.env),
            sub_invokes: &[],
        },
    }]);
    assert!(s2.escrow.try_claim_milestone(&id2, &0).is_err());
}

/// A window at the top of the allowed range must not wrap the deadline.
#[test]
fn claim_milestone_handles_the_longest_window() {
    let s = setup();
    let id = create_with_window(&s, &[400], MAX_REVIEW_WINDOW_SECS);
    s.escrow.deposit_funds(&id, &400);
    s.escrow.submit_milestone(&id, &0);

    s.env
        .ledger()
        .set_timestamp(T0 + MAX_REVIEW_WINDOW_SECS - 1);
    assert_eq!(
        s.escrow.try_claim_milestone(&id, &0),
        Err(Ok(EscrowError::ReviewWindowActive))
    );

    s.env.ledger().set_timestamp(T0 + MAX_REVIEW_WINDOW_SECS);
    assert!(s.escrow.try_claim_milestone(&id, &0).is_ok());
}

// ---------------------------------------------------------------------------
// release_milestone still works for the client, on both states
// ---------------------------------------------------------------------------

#[test]
fn client_can_release_a_pending_milestone_early() {
    let s = setup();
    let id = funded(&s, &[400]);
    // No submission needed: the client may pay whenever it likes.
    s.escrow.release_milestone(&id, &0);
    assert_eq!(s.token.balance(&s.freelancer), 400);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Completed);
    assert_custody(&s, id);
}

#[test]
fn client_can_release_a_submitted_milestone_inside_the_window() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.escrow.submit_milestone(&id, &0);

    // Approve immediately, without waiting for the window.
    s.escrow.release_milestone(&id, &0);
    assert_eq!(s.token.balance(&s.freelancer), 400);
    assert_eq!(status_of(&s, id, 0), MilestoneStatus::Released);
    assert_custody(&s, id);

    // The freelancer cannot then also claim it.
    s.env.ledger().set_timestamp(T0 + WINDOW);
    assert_eq!(
        s.escrow.try_claim_milestone(&id, &0),
        Err(Ok(EscrowError::MilestoneNotSubmitted))
    );
    assert_eq!(s.token.balance(&s.freelancer), 400, "paid exactly once");
}

/// Completing the last milestone through either path marks the escrow completed.
#[test]
fn completing_via_claim_sets_the_escrow_completed() {
    let s = setup();
    let id = funded(&s, &[400, 600]);
    s.escrow.release_milestone(&id, &0);
    s.escrow.submit_milestone(&id, &1);
    s.env.ledger().set_timestamp(T0 + WINDOW);
    s.escrow.claim_milestone(&id, &1);

    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Completed);
    assert_eq!(s.token.balance(&s.freelancer), 1_000);
    assert_custody(&s, id);

    s.escrow.issue_reputation(&id, &5);
    assert_eq!(s.escrow.get_reputation(&s.freelancer).completed_contracts, 1);
}

// ---------------------------------------------------------------------------
// review_window_secs validation
// ---------------------------------------------------------------------------

#[test]
fn review_window_is_validated_at_creation() {
    let s = setup();
    assert_eq!(
        s.escrow.try_create_contract(
            &s.client,
            &s.freelancer,
            &s.token.address,
            &amounts(&s.env, &[100]),
            &0u64,
        ),
        Err(Ok(EscrowError::InvalidReviewWindow))
    );
    assert_eq!(
        s.escrow.try_create_contract(
            &s.client,
            &s.freelancer,
            &s.token.address,
            &amounts(&s.env, &[100]),
            &(MIN_REVIEW_WINDOW_SECS - 1),
        ),
        Err(Ok(EscrowError::InvalidReviewWindow))
    );
    assert_eq!(
        s.escrow.try_create_contract(
            &s.client,
            &s.freelancer,
            &s.token.address,
            &amounts(&s.env, &[100]),
            &(MAX_REVIEW_WINDOW_SECS + 1),
        ),
        Err(Ok(EscrowError::InvalidReviewWindow))
    );
    assert_eq!(
        s.escrow.try_create_contract(
            &s.client,
            &s.freelancer,
            &s.token.address,
            &amounts(&s.env, &[100]),
            &u64::MAX,
        ),
        Err(Ok(EscrowError::InvalidReviewWindow))
    );
}

#[test]
fn review_window_accepts_both_bounds() {
    let s = setup();
    let a = create_with_window(&s, &[100], MIN_REVIEW_WINDOW_SECS);
    assert_eq!(
        s.escrow.get_contract(&a).review_window_secs,
        MIN_REVIEW_WINDOW_SECS
    );
    let b = create_with_window(&s, &[100], MAX_REVIEW_WINDOW_SECS);
    assert_eq!(
        s.escrow.get_contract(&b).review_window_secs,
        MAX_REVIEW_WINDOW_SECS
    );
}

// ---------------------------------------------------------------------------
// Unit-level checks on the helpers
// ---------------------------------------------------------------------------

#[test]
fn uncommitted_funds_accounts_for_submitted_and_released() {
    let s = setup();
    let id = funded(&s, &[300, 300, 400]);
    let c = s.escrow.get_contract(&id);
    assert_eq!(uncommitted_funds(&c), 1_000);

    s.escrow.release_milestone(&id, &0);
    let c = s.escrow.get_contract(&id);
    assert_eq!(uncommitted_funds(&c), 700);

    s.escrow.submit_milestone(&id, &1);
    let c = s.escrow.get_contract(&id);
    assert_eq!(uncommitted_funds(&c), 400);

    s.escrow.submit_milestone(&id, &2);
    let c = s.escrow.get_contract(&id);
    assert_eq!(uncommitted_funds(&c), 0);
}
