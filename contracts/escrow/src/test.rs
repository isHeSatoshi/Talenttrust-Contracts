extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events, Ledger},
    token::StellarAssetClient,
    token::TokenClient,
    vec, IntoVal, Symbol,
};

/// A non-zero starting ledger time, so `paused_at` is never confused with a
/// default of `0`.
const T0: u64 = 1_000_000;

struct Setup<'a> {
    env: Env,
    escrow: EscrowClient<'a>,
    escrow_id: Address,
    token: TokenClient<'a>,
    client: Address,
    freelancer: Address,
    admin: Address,
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
        admin,
    }
}

fn create(s: &Setup, xs: &[i128]) -> u64 {
    s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, xs[0]],
    )
}

/// Create an escrow for the given milestone amounts and fund it fully.
fn funded(s: &Setup, xs: &[i128]) -> u64 {
    let mut v = Vec::new(&s.env);
    for x in xs {
        v.push_back(*x);
    }
    let id = s
        .escrow
        .create_contract(&s.client, &s.freelancer, &s.token.address, &v);
    let total: i128 = xs.iter().sum();
    s.escrow.deposit_funds(&id, &total);
    id
}

/// The custody invariant: the tokens the contract holds must equal
/// `funded_amount - released_amount`.
fn assert_custody(s: &Setup, id: u64) {
    let c = s.escrow.get_contract(&id);
    assert_eq!(
        s.token.balance(&s.escrow_id),
        c.funded_amount - c.released_amount,
        "contract custody must equal funded_amount - released_amount",
    );
}

/// Advance the ledger clock.
fn at(s: &Setup, t: u64) {
    s.env.ledger().set_timestamp(t);
}

// ---------------------------------------------------------------------------
// Pre-existing behaviour, unmodified.
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
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 100i128, 200i128],
    );
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
    let id = create(&s, &[100]);
    assert_eq!(
        s.escrow.try_deposit_funds(&id, &101),
        Err(Ok(EscrowError::OverFunded))
    );
    s.escrow.deposit_funds(&id, &100);
    s.escrow.release_milestone(&id, &0);
    assert!(s.escrow.try_release_milestone(&id, &0).is_err());
}

#[test]
fn dispute_split_and_cancel() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.escrow.open_dispute(&id, &s.freelancer);
    s.escrow.resolve_dispute(&id, &2_500);
    assert_eq!(s.token.balance(&s.freelancer), 100);
    assert_eq!(s.token.balance(&s.client), 900);

    let id2 = funded(&s, &[50]);
    s.escrow.cancel_contract(&id2);
    assert_eq!(s.token.balance(&s.client), 900);
    assert_eq!(s.escrow.get_contract(&id2).status, ContractStatus::Cancelled);
}

#[test]
fn pause_blocks_mutations() {
    let s = setup();
    s.escrow.set_paused(&true);
    assert_eq!(
        s.escrow.try_create_contract(
            &s.client,
            &s.freelancer,
            &s.token.address,
            &vec![&s.env, 1i128]
        ),
        Err(Ok(EscrowError::Paused))
    );
}

// ---------------------------------------------------------------------------
// The pause clock
// ---------------------------------------------------------------------------

#[test]
fn pausing_records_the_start_time_and_unpausing_clears_it() {
    let s = setup();
    assert_eq!(s.escrow.paused_at(), None, "no clock while unpaused");

    at(&s, T0 + 500);
    s.escrow.set_paused(&true);
    assert_eq!(s.escrow.paused_at(), Some(T0 + 500));

    s.escrow.set_paused(&false);
    assert_eq!(s.escrow.paused_at(), None, "unpausing clears the clock");
}

/// Pausing again while already paused must not move the deadline: otherwise the
/// admin could extend the pause indefinitely with repeated calls, and the
/// emergency exit would never open.
#[test]
fn pausing_while_already_paused_does_not_reset_the_clock() {
    let s = setup();
    at(&s, T0);
    s.escrow.set_paused(&true);
    assert_eq!(s.escrow.paused_at(), Some(T0));

    at(&s, T0 + 10_000);
    s.escrow.set_paused(&true);
    assert_eq!(
        s.escrow.paused_at(),
        Some(T0),
        "a repeated pause must keep the original start time",
    );
}

/// Unpause then re-pause is a genuine new pause, so the clock restarts. This is
/// the griefing vector the issue asks to be documented.
#[test]
fn unpause_then_repause_resets_the_clock() {
    let s = setup();

    // Both escrows are created and funded before the pause, since
    // `create_contract` and `deposit_funds` are themselves pause-gated.
    let a = funded(&s, &[400]);
    let b = funded(&s, &[400]);

    at(&s, T0);
    s.escrow.set_paused(&true);

    // Long enough that the exit would have opened.
    let would_have_opened = T0 + EMERGENCY_EXIT_DELAY_SECS;
    at(&s, would_have_opened);
    assert!(s.escrow.try_emergency_withdraw(&s.client, &a).is_ok());

    // The admin bounces the pause: off, then on again.
    s.escrow.set_paused(&false);
    at(&s, would_have_opened + 1);
    s.escrow.set_paused(&true);

    assert_eq!(s.escrow.paused_at(), Some(would_have_opened + 1));

    // One second before the *new* deadline: refused.
    at(&s, would_have_opened + 1 + EMERGENCY_EXIT_DELAY_SECS - 1);
    assert_eq!(
        s.escrow.try_emergency_withdraw(&s.client, &b),
        Err(Ok(EscrowError::EmergencyExitNotAvailable))
    );

    // At the new deadline: allowed.
    at(&s, would_have_opened + 1 + EMERGENCY_EXIT_DELAY_SECS);
    assert!(s.escrow.try_emergency_withdraw(&s.client, &b).is_ok());
}

// ---------------------------------------------------------------------------
// The delay boundary
// ---------------------------------------------------------------------------

#[test]
fn emergency_withdraw_opens_exactly_at_the_delay_boundary() {
    let s = setup();
    let id = funded(&s, &[400]);

    at(&s, T0);
    s.escrow.set_paused(&true);

    // One second early.
    at(&s, T0 + EMERGENCY_EXIT_DELAY_SECS - 1);
    assert_eq!(
        s.escrow.try_emergency_withdraw(&s.client, &id),
        Err(Ok(EscrowError::EmergencyExitNotAvailable))
    );
    assert_eq!(s.token.balance(&s.client), 600, "nothing moved early");
    assert_custody(&s, id);

    // The deadline itself.
    at(&s, T0 + EMERGENCY_EXIT_DELAY_SECS);
    s.escrow.emergency_withdraw(&s.client, &id);

    assert_eq!(s.token.balance(&s.client), 1_000);
    assert_eq!(s.token.balance(&s.freelancer), 0);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Cancelled);
    assert_custody(&s, id);
}

#[test]
fn emergency_withdraw_is_refused_while_unpaused() {
    let s = setup();
    let id = funded(&s, &[400]);

    // Well past the delay in wall-clock terms, but the contract is not paused,
    // so there is no clock and nothing to escape from.
    at(&s, T0 + EMERGENCY_EXIT_DELAY_SECS * 10);
    assert_eq!(
        s.escrow.try_emergency_withdraw(&s.client, &id),
        Err(Ok(EscrowError::InvalidStatus))
    );
    assert_eq!(s.token.balance(&s.client), 600);
    assert_custody(&s, id);
}

// ---------------------------------------------------------------------------
// Payout rules
// ---------------------------------------------------------------------------

#[test]
fn funded_escrow_refunds_the_client_in_full() {
    let s = setup();
    let id = funded(&s, &[400, 600]);

    at(&s, T0);
    s.escrow.set_paused(&true);
    at(&s, T0 + EMERGENCY_EXIT_DELAY_SECS);
    s.escrow.emergency_withdraw(&s.client, &id);

    assert_eq!(s.token.balance(&s.client), 1_000, "all unreleased funds return");
    assert_eq!(s.token.balance(&s.freelancer), 0);
    let c = s.escrow.get_contract(&id);
    assert_eq!(c.status, ContractStatus::Cancelled);
    assert_eq!(c.funded_amount, c.released_amount);
    assert_custody(&s, id);
}

/// A partly released escrow only returns what is left.
#[test]
fn partly_released_escrow_returns_only_the_remainder() {
    let s = setup();
    let id = funded(&s, &[400, 600]);
    s.escrow.release_milestone(&id, &0);
    assert_eq!(s.token.balance(&s.freelancer), 400);
    assert_eq!(s.token.balance(&s.client), 0);

    at(&s, T0);
    s.escrow.set_paused(&true);
    at(&s, T0 + EMERGENCY_EXIT_DELAY_SECS);
    s.escrow.emergency_withdraw(&s.client, &id);

    assert_eq!(s.token.balance(&s.client), 600);
    assert_eq!(s.token.balance(&s.freelancer), 400, "released stays released");
    assert_custody(&s, id);
}

#[test]
fn disputed_escrow_splits_evenly() {
    let s = setup();
    let id = funded(&s, &[400]);

    // Open the dispute before pausing: `open_dispute` is pause-gated.
    s.escrow.open_dispute(&id, &s.freelancer);
    at(&s, T0);
    s.escrow.set_paused(&true);
    at(&s, T0 + EMERGENCY_EXIT_DELAY_SECS);
    s.escrow.emergency_withdraw(&s.client, &id);

    assert_eq!(s.token.balance(&s.freelancer), 200);
    assert_eq!(s.token.balance(&s.client), 800);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Cancelled);
    assert_custody(&s, id);
}

/// An odd held balance cannot split evenly; the extra unit goes to the client.
#[test]
fn disputed_escrow_rounds_the_odd_unit_to_the_client() {
    let s = setup();
    let id = funded(&s, &[101]);

    s.escrow.open_dispute(&id, &s.client);
    at(&s, T0);
    s.escrow.set_paused(&true);
    at(&s, T0 + EMERGENCY_EXIT_DELAY_SECS);
    s.escrow.emergency_withdraw(&s.freelancer, &id);

    // held = 101 -> freelancer 50, client 51.
    assert_eq!(s.token.balance(&s.freelancer), 50);
    assert_eq!(s.token.balance(&s.client), 1_000 - 101 + 51);
    assert_custody(&s, id);
}

#[test]
fn emergency_withdraw_emits_the_event() {
    let s = setup();
    let id = funded(&s, &[101]);
    s.escrow.open_dispute(&id, &s.client);

    at(&s, T0);
    s.escrow.set_paused(&true);
    at(&s, T0 + EMERGENCY_EXIT_DELAY_SECS);
    s.escrow.emergency_withdraw(&s.client, &id);

    let events = s.env.events().all();
    let (contract, topics, data) = events.get(events.len() - 1).unwrap();
    assert_eq!(contract, s.escrow_id);
    let name: Symbol = topics.get(0).unwrap().into_val(&s.env);
    let event_id: u64 = topics.get(1).unwrap().into_val(&s.env);
    assert_eq!(name, symbol_short!("emerg"));
    assert_eq!(event_id, id);

    let payload: Vec<soroban_sdk::Val> = data.into_val(&s.env);
    let to_client: i128 = payload.get(0).unwrap().into_val(&s.env);
    let to_freelancer: i128 = payload.get(1).unwrap().into_val(&s.env);
    assert_eq!(to_client, 51);
    assert_eq!(to_freelancer, 50);
}

// ---------------------------------------------------------------------------
// Authority and one-shot
// ---------------------------------------------------------------------------

#[test]
fn either_party_may_exit() {
    // The client.
    let a = setup();
    let id = funded(&a, &[400]);
    at(&a, T0);
    a.escrow.set_paused(&true);
    at(&a, T0 + EMERGENCY_EXIT_DELAY_SECS);
    assert!(a.escrow.try_emergency_withdraw(&a.client, &id).is_ok());

    // The freelancer, on the same escrow. Either party's exit unwinds it, which
    // is the point: it must not depend on the other side cooperating.
    let b = setup();
    let id2 = funded(&b, &[400]);
    at(&b, T0);
    b.escrow.set_paused(&true);
    at(&b, T0 + EMERGENCY_EXIT_DELAY_SECS);
    assert!(b.escrow.try_emergency_withdraw(&b.freelancer, &id2).is_ok());
    assert_eq!(b.token.balance(&b.client), 1_000);
}

#[test]
fn a_stranger_cannot_trigger_the_exit() {
    let s = setup();
    let stranger = Address::generate(&s.env);
    let id = funded(&s, &[400]);

    at(&s, T0);
    s.escrow.set_paused(&true);
    at(&s, T0 + EMERGENCY_EXIT_DELAY_SECS);

    assert_eq!(
        s.escrow.try_emergency_withdraw(&stranger, &id),
        Err(Ok(EscrowError::Unauthorized))
    );
    // Nor the admin, who is not a party to this escrow.
    assert_eq!(
        s.escrow.try_emergency_withdraw(&s.admin, &id),
        Err(Ok(EscrowError::Unauthorized))
    );
    assert_eq!(s.token.balance(&s.client), 600, "nothing moved");
    assert_custody(&s, id);
}

#[test]
fn emergency_withdraw_cannot_be_called_twice() {
    let s = setup();
    let id = funded(&s, &[400]);

    at(&s, T0);
    s.escrow.set_paused(&true);
    at(&s, T0 + EMERGENCY_EXIT_DELAY_SECS);
    s.escrow.emergency_withdraw(&s.client, &id);

    let client_after = s.token.balance(&s.client);
    let freelancer_after = s.token.balance(&s.freelancer);

    assert_eq!(
        s.escrow.try_emergency_withdraw(&s.client, &id),
        Err(Ok(EscrowError::InvalidStatus))
    );
    assert_eq!(
        s.escrow.try_emergency_withdraw(&s.freelancer, &id),
        Err(Ok(EscrowError::InvalidStatus))
    );
    assert_eq!(s.token.balance(&s.client), client_after, "paid exactly once");
    assert_eq!(s.token.balance(&s.freelancer), freelancer_after);
    assert_custody(&s, id);
}

#[test]
fn emergency_withdraw_is_refused_on_completed_and_resolved_escrows() {
    let s = setup();

    // Completed: everything was released, so there is nothing to unwind.
    let done = funded(&s, &[400]);
    s.escrow.release_milestone(&done, &0);
    assert_eq!(s.escrow.get_contract(&done).status, ContractStatus::Completed);

    // Resolved: the dispute already paid out.
    let resolved = funded(&s, &[400]);
    s.escrow.open_dispute(&resolved, &s.client);
    s.escrow.resolve_dispute(&resolved, &5_000);
    assert_eq!(
        s.escrow.get_contract(&resolved).status,
        ContractStatus::Resolved
    );

    at(&s, T0);
    s.escrow.set_paused(&true);
    at(&s, T0 + EMERGENCY_EXIT_DELAY_SECS);

    assert_eq!(
        s.escrow.try_emergency_withdraw(&s.client, &done),
        Err(Ok(EscrowError::InvalidStatus))
    );
    assert_eq!(
        s.escrow.try_emergency_withdraw(&s.client, &resolved),
        Err(Ok(EscrowError::InvalidStatus))
    );
}

/// A `Created` escrow that was never funded holds nothing, so the exit is a
/// no-op that still closes it out.
#[test]
fn an_unfunded_escrow_can_still_be_closed() {
    let s = setup();
    let id = create(&s, &[400]);

    at(&s, T0);
    s.escrow.set_paused(&true);
    at(&s, T0 + EMERGENCY_EXIT_DELAY_SECS);
    s.escrow.emergency_withdraw(&s.client, &id);

    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Cancelled);
    assert_eq!(s.token.balance(&s.client), 1_000, "nothing was ever funded");
    assert_custody(&s, id);
}

// ---------------------------------------------------------------------------
// The exit is available regardless of the pause's other effects
// ---------------------------------------------------------------------------

/// A `Funded` escrow is the genuinely frozen case, and the reason this feature
/// exists: while paused there is no path at all out of it.
///
/// - `release_milestone` (client pays) — pause-gated
/// - `cancel_contract` (client refunds itself) — pause-gated
/// - `open_dispute` (either party escalates) — pause-gated
/// - `resolve_dispute` — not pause-gated, but requires `Disputed`, which cannot
///   be reached from `Funded` while paused
///
/// So the client cannot get its money back and the freelancer cannot be paid.
/// Only the admin unpausing would change that, and if the admin key is gone
/// nobody can. The emergency exit is the way out.
#[test]
fn a_funded_escrow_is_fully_frozen_while_paused() {
    let s = setup();
    let id = funded(&s, &[400]);

    at(&s, T0);
    s.escrow.set_paused(&true);
    at(&s, T0 + EMERGENCY_EXIT_DELAY_SECS);

    // Every ordinary exit is refused.
    assert_eq!(
        s.escrow.try_release_milestone(&id, &0),
        Err(Ok(EscrowError::Paused))
    );
    assert_eq!(
        s.escrow.try_cancel_contract(&id),
        Err(Ok(EscrowError::Paused))
    );
    assert_eq!(
        s.escrow.try_open_dispute(&id, &s.client),
        Err(Ok(EscrowError::Paused))
    );
    // And the dispute path is unreachable anyway, since it needs `Disputed`.
    assert_eq!(
        s.escrow.try_resolve_dispute(&id, &5_000),
        Err(Ok(EscrowError::InvalidStatus))
    );

    // The emergency exit is not.
    s.escrow.emergency_withdraw(&s.client, &id);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Cancelled);
    assert_eq!(s.token.balance(&s.client), 1_000);
    assert_custody(&s, id);
}

/// `resolve_dispute` is deliberately *not* pause-gated, so a disputed escrow
/// stays resolvable by an available admin. That is worth pinning down: it means
/// the emergency exit is load-bearing only when the admin is unavailable, not
/// merely uncooperative.
#[test]
fn resolve_dispute_still_works_while_paused() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.escrow.open_dispute(&id, &s.client);

    at(&s, T0);
    s.escrow.set_paused(&true);

    // No waiting required: the admin can settle during the pause.
    s.escrow.resolve_dispute(&id, &5_000);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Resolved);
    assert_eq!(s.token.balance(&s.freelancer), 200);
    assert_custody(&s, id);
}

/// The contract's balance must track the sum of every escrow's remaining
/// `funded - released`, including after an exit.
#[test]
fn contract_balance_tracks_all_escrows_after_exits() {
    let s = setup();

    let a = funded(&s, &[400]);
    let b = funded(&s, &[600]);
    // A third escrow left unfunded.
    let c = create(&s, &[500]);

    let expected_held = |s: &Setup| {
        let mut total = 0i128;
        for id in [a, b, c] {
            let e = s.escrow.get_contract(&id);
            total += e.funded_amount - e.released_amount;
        }
        total
    };
    assert_eq!(s.token.balance(&s.escrow_id), expected_held(&s));

    at(&s, T0);
    s.escrow.set_paused(&true);
    at(&s, T0 + EMERGENCY_EXIT_DELAY_SECS);

    s.escrow.emergency_withdraw(&s.client, &a);
    assert_eq!(s.token.balance(&s.escrow_id), expected_held(&s));

    s.escrow.emergency_withdraw(&s.freelancer, &b);
    assert_eq!(s.token.balance(&s.escrow_id), expected_held(&s));

    s.escrow.emergency_withdraw(&s.client, &c);
    assert_eq!(s.token.balance(&s.escrow_id), expected_held(&s));
    assert_eq!(s.token.balance(&s.escrow_id), 0, "all escrows unwound");

    // Every party is whole: nothing was created or destroyed.
    assert_eq!(s.token.balance(&s.client) + s.token.balance(&s.freelancer), 1_000);
}
