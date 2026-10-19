extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events, MockAuth, MockAuthInvoke},
    token::StellarAssetClient,
    token::TokenClient,
    vec, IntoVal, Symbol,
};

struct Setup<'a> {
    env: Env,
    escrow: EscrowClient<'a>,
    escrow_id: Address,
    token: TokenClient<'a>,
    client: Address,
    freelancer: Address,
    /// Created by `setup()` and read back through `get_admin()`. Kept on the
    /// struct so a test that needs it does not have to re-derive it.
    #[allow(dead_code)]
    admin: Address,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let client = Address::generate(&env);
    let freelancer = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    StellarAssetClient::new(&env, &sac.address()).mint(&client, &10_000);
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

fn amounts(env: &Env, xs: &[i128]) -> Vec<i128> {
    let mut v = Vec::new(env);
    for x in xs {
        v.push_back(*x);
    }
    v
}

/// Create and fund an escrow for the given milestone amounts.
fn funded(s: &Setup, xs: &[i128]) -> u64 {
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &amounts(&s.env, xs),
    );
    let total: i128 = xs.iter().sum();
    s.escrow.deposit_funds(&id, &total);
    id
}

/// The custody invariant: the contract's token balance must equal
/// `funded_amount - released_amount`. Asserted after every money-moving step.
fn assert_custody(s: &Setup, id: u64) {
    let c = s.escrow.get_contract(&id);
    assert_eq!(
        s.token.balance(&s.escrow_id),
        c.funded_amount - c.released_amount,
        "contract custody must equal funded_amount - released_amount",
    );
}

/// The other half of the invariant: nothing was created or destroyed.
fn assert_conserved(s: &Setup) {
    assert_eq!(
        s.token.balance(&s.client) + s.token.balance(&s.freelancer) + s.token.balance(&s.escrow_id),
        10_000,
        "tokens must be conserved across client, freelancer and the escrow",
    );
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
    assert_eq!(s.token.balance(&s.client), 9_700);
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
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 100i128],
    );
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
    assert_eq!(s.token.balance(&s.client), 9_900);

    let id2 = funded(&s, &[50]);
    s.escrow.cancel_contract(&id2);
    assert_eq!(s.token.balance(&s.client), 9_900);
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
// The core scenario from the issue: dispute one of three, keep the others moving
// ---------------------------------------------------------------------------

#[test]
fn disputing_one_milestone_leaves_the_others_releasable() {
    let s = setup();
    let id = funded(&s, &[100, 200, 300]);
    assert_custody(&s, id);

    // Dispute the middle milestone.
    s.escrow.open_milestone_dispute(&id, &s.client, &1);

    // The escrow-level status is untouched: a milestone dispute is not a freeze.
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);

    // Milestones 0 and 2 release normally.
    s.escrow.release_milestone(&id, &0);
    assert_eq!(s.token.balance(&s.freelancer), 100);
    assert_custody(&s, id);
    s.escrow.release_milestone(&id, &2);
    assert_eq!(s.token.balance(&s.freelancer), 400);
    assert_custody(&s, id);

    // The disputed one does not.
    assert_eq!(
        s.escrow.try_release_milestone(&id, &1),
        Err(Ok(EscrowError::MilestoneDisputed))
    );
    assert_eq!(s.token.balance(&s.freelancer), 400, "nothing extra moved");

    // 50/50 of 200 is 100 each.
    let client_before = s.token.balance(&s.client);
    s.escrow.resolve_milestone_dispute(&id, &1, &5_000);

    assert_eq!(s.token.balance(&s.freelancer), 500);
    assert_eq!(s.token.balance(&s.client), client_before + 100);
    assert_custody(&s, id);
    assert_conserved(&s);

    // Every milestone is now released, so the escrow is complete.
    let c = s.escrow.get_contract(&id);
    assert_eq!(c.status, ContractStatus::Completed);
    assert!(c.milestones.iter().all(|m| m.released));
    assert_eq!(c.released_amount, 500);
    assert_eq!(c.funded_amount, 500);
}

/// The resolver can only move the disputed milestone's amount, never the whole
/// held balance. This is the property that makes scoping worth having.
#[test]
fn resolution_moves_only_the_disputed_milestone_amount() {
    let s = setup();
    let id = funded(&s, &[100, 200, 300]);

    // Release the first two so a large unreleased balance is not what is at
    // stake: the disputed milestone is 300, and only 300 may move.
    s.escrow.release_milestone(&id, &0);
    s.escrow.release_milestone(&id, &1);
    let freelancer_before = s.token.balance(&s.freelancer);
    let client_before = s.token.balance(&s.client);
    assert_custody(&s, id);

    s.escrow.open_milestone_dispute(&id, &s.freelancer, &2);
    s.escrow.resolve_milestone_dispute(&id, &2, &10_000);

    // All of the 300 goes to the freelancer; the client gets nothing back.
    assert_eq!(s.token.balance(&s.freelancer), freelancer_before + 300);
    assert_eq!(s.token.balance(&s.client), client_before);
    assert_eq!(s.token.balance(&s.escrow_id), 0);
    assert_custody(&s, id);
    assert_conserved(&s);
}

/// A milestone whose amount exceeds the held balance cannot be disputed.
///
/// The rule is per milestone: `funded - released >= m.amount`. Note this does
/// *not* prevent two individually-covered milestones from together exceeding the
/// held balance; that is caught at resolution instead, by
/// `resolving_a_second_dispute_beyond_the_held_balance_is_refused`.
#[test]
fn a_milestone_dispute_needs_the_amount_to_be_held() {
    let s = setup();
    // Milestones total 1_000, but only 500 is funded.
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &amounts(&s.env, &[200, 800]),
    );
    s.escrow.deposit_funds(&id, &500);

    // 200 is covered by the 500 held.
    assert!(s.escrow.try_open_milestone_dispute(&id, &s.client, &0).is_ok());
    // 800 is not.
    assert_eq!(
        s.escrow.try_open_milestone_dispute(&id, &s.client, &1),
        Err(Ok(EscrowError::InsufficientFunds))
    );
}

#[test]
fn disputing_a_released_milestone_is_refused() {
    let s = setup();
    let id = funded(&s, &[100, 200]);
    s.escrow.release_milestone(&id, &0);
    assert_eq!(
        s.escrow.try_open_milestone_dispute(&id, &s.client, &0),
        Err(Ok(EscrowError::MilestoneAlreadyReleased))
    );
}

#[test]
fn disputing_an_unknown_milestone_is_refused() {
    let s = setup();
    let id = funded(&s, &[100]);
    assert_eq!(
        s.escrow.try_open_milestone_dispute(&id, &s.client, &9),
        Err(Ok(EscrowError::MilestoneNotFound))
    );
}

#[test]
fn a_milestone_can_only_be_disputed_once() {
    let s = setup();
    let id = funded(&s, &[100, 200]);
    s.escrow.open_milestone_dispute(&id, &s.client, &1);

    // Either party, and it is still refused: a second dispute would let the
    // opener restart a process that is already running.
    assert_eq!(
        s.escrow.try_open_milestone_dispute(&id, &s.client, &1),
        Err(Ok(EscrowError::MilestoneDisputed))
    );
    assert_eq!(
        s.escrow.try_open_milestone_dispute(&id, &s.freelancer, &1),
        Err(Ok(EscrowError::MilestoneDisputed))
    );

    // A different milestone is still fine.
    assert!(s.escrow.try_open_milestone_dispute(&id, &s.freelancer, &0).is_ok());
}

#[test]
fn either_party_may_open_a_milestone_dispute() {
    let a = setup();
    let id = funded(&a, &[100]);
    assert!(a.escrow.try_open_milestone_dispute(&id, &a.client, &0).is_ok());

    let b = setup();
    let id2 = funded(&b, &[100]);
    assert!(b.escrow.try_open_milestone_dispute(&id2, &b.freelancer, &0).is_ok());
}

#[test]
fn a_stranger_cannot_open_a_milestone_dispute() {
    let s = setup();
    let stranger = Address::generate(&s.env);
    let id = funded(&s, &[100]);
    assert_eq!(
        s.escrow.try_open_milestone_dispute(&id, &stranger, &0),
        Err(Ok(EscrowError::Unauthorized))
    );
}

#[test]
fn only_the_admin_can_resolve_a_milestone_dispute() {
    let s = setup();
    let id = funded(&s, &[100]);
    s.escrow.open_milestone_dispute(&id, &s.client, &0);

    // `resolve_milestone_dispute` takes no caller argument: it authenticates the
    // stored admin directly. So the negative case is a call where the admin's
    // signature is not available, which `mock_auths` with an empty list
    // reproduces exactly.
    s.env.mock_auths(&[]);
    assert!(
        s.escrow.try_resolve_milestone_dispute(&id, &0, &5_000).is_err(),
        "without the admin's signature the call must fail",
    );
    // Nothing moved, and the dispute is still open.
    assert_eq!(s.token.balance(&s.freelancer), 0);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);

    // Signing as a party is not enough either.
    s.env.mock_auths(&[MockAuth {
        address: &s.client,
        invoke: &MockAuthInvoke {
            contract: &s.escrow_id,
            fn_name: "resolve_milestone_dispute",
            args: (id, 0u32, 5_000u32).into_val(&s.env),
            sub_invokes: &[],
        },
    }]);
    assert!(s.escrow.try_resolve_milestone_dispute(&id, &0, &5_000).is_err());

    // With the admin's signature restored it succeeds.
    s.env.mock_all_auths();
    s.escrow.resolve_milestone_dispute(&id, &0, &5_000);
    assert_eq!(s.token.balance(&s.freelancer), 50);
}

#[test]
fn resolving_a_milestone_that_is_not_disputed_is_refused() {
    let s = setup();
    let id = funded(&s, &[100, 200]);
    assert_eq!(
        s.escrow.try_resolve_milestone_dispute(&id, &0, &5_000),
        Err(Ok(EscrowError::InvalidStatus))
    );
}

#[test]
fn resolving_an_unknown_milestone_is_refused() {
    let s = setup();
    let id = funded(&s, &[100]);
    assert_eq!(
        s.escrow.try_resolve_milestone_dispute(&id, &9, &5_000),
        Err(Ok(EscrowError::InvalidStatus))
    );
}

#[test]
fn an_invalid_split_is_refused() {
    let s = setup();
    let id = funded(&s, &[100]);
    s.escrow.open_milestone_dispute(&id, &s.client, &0);
    assert_eq!(
        s.escrow.try_resolve_milestone_dispute(&id, &0, &10_001),
        Err(Ok(EscrowError::InvalidSplit))
    );
    // Still open, so it can be resolved correctly afterwards.
    s.escrow.resolve_milestone_dispute(&id, &0, &10_000);
    assert_eq!(s.token.balance(&s.freelancer), 100);
}

/// A resolved milestone cannot be resolved again, which is the double-pay guard.
#[test]
fn a_resolved_milestone_dispute_cannot_be_resolved_again() {
    let s = setup();
    // Two milestones, so resolving the disputed one leaves the escrow `Funded`
    // and the milestone-level guards are the ones that fire. With a single
    // milestone the escrow would become `Completed` and the status gate would
    // answer first, which tests less.
    let id = funded(&s, &[100, 200]);
    s.escrow.open_milestone_dispute(&id, &s.client, &0);
    s.escrow.resolve_milestone_dispute(&id, &0, &5_000);

    let freelancer_after = s.token.balance(&s.freelancer);
    let client_after = s.token.balance(&s.client);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);

    // The dispute record is gone, so this reads as "no dispute open".
    assert_eq!(
        s.escrow.try_resolve_milestone_dispute(&id, &0, &5_000),
        Err(Ok(EscrowError::InvalidStatus))
    );
    // And the milestone itself is released.
    assert_eq!(
        s.escrow.try_release_milestone(&id, &0),
        Err(Ok(EscrowError::MilestoneAlreadyReleased))
    );
    // A dispute cannot be reopened over it either.
    assert_eq!(
        s.escrow.try_open_milestone_dispute(&id, &s.client, &0),
        Err(Ok(EscrowError::MilestoneAlreadyReleased))
    );
    assert_eq!(s.token.balance(&s.freelancer), freelancer_after);
    assert_eq!(s.token.balance(&s.client), client_after);
    assert_custody(&s, id);
}

/// Two milestones can each be individually covered when their disputes open,
/// yet together exceed what the escrow holds. Resolving the first reduces the
/// balance, so resolving the second must report `InsufficientFunds` rather than
/// attempt a transfer the token contract would reject.
#[test]
fn resolving_a_second_dispute_beyond_the_held_balance_is_refused() {
    let s = setup();
    // Total 1_000, but only 500 is funded.
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &amounts(&s.env, &[500, 500]),
    );
    s.escrow.deposit_funds(&id, &500);

    // Each milestone is covered by the 500 held, so both disputes open.
    s.escrow.open_milestone_dispute(&id, &s.client, &0);
    s.escrow.open_milestone_dispute(&id, &s.client, &1);

    // The first resolution pays out the whole 500, leaving nothing held.
    s.escrow.resolve_milestone_dispute(&id, &0, &5_000);
    assert_eq!(s.token.balance(&s.escrow_id), 0);
    assert_custody(&s, id);

    // The second has nothing left to pay from, and says so.
    let freelancer_before = s.token.balance(&s.freelancer);
    let client_before = s.token.balance(&s.client);
    assert_eq!(
        s.escrow.try_resolve_milestone_dispute(&id, &1, &5_000),
        Err(Ok(EscrowError::InsufficientFunds))
    );
    assert_eq!(s.token.balance(&s.freelancer), freelancer_before);
    assert_eq!(s.token.balance(&s.client), client_before);
    assert_custody(&s, id);
    assert_conserved(&s);
}

// ---------------------------------------------------------------------------
// Cancel interaction
// ---------------------------------------------------------------------------

#[test]
fn cancel_is_blocked_while_a_milestone_dispute_is_open() {
    let s = setup();
    let id = funded(&s, &[100, 200]);
    s.escrow.open_milestone_dispute(&id, &s.freelancer, &1);

    assert_eq!(
        s.escrow.try_cancel_contract(&id),
        Err(Ok(EscrowError::MilestoneDisputed))
    );
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);
    assert_custody(&s, id);
}

/// After resolution the cancel is allowed again, and refunds only what is left.
#[test]
fn cancel_works_after_the_dispute_resolves_and_refunds_the_remainder() {
    let s = setup();
    let id = funded(&s, &[100, 200, 300]);

    s.escrow.open_milestone_dispute(&id, &s.freelancer, &1);
    s.escrow.resolve_milestone_dispute(&id, &1, &5_000);

    // 200 resolved 50/50: freelancer 100, client 100. Milestones 0 and 2 are
    // still pending, so 400 remains.
    assert_eq!(s.token.balance(&s.freelancer), 100);
    assert_custody(&s, id);

    let client_before = s.token.balance(&s.client);
    s.escrow.cancel_contract(&id);

    assert_eq!(s.token.balance(&s.client), client_before + 400);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Cancelled);
    assert_eq!(s.token.balance(&s.escrow_id), 0);
    assert_custody(&s, id);
    assert_conserved(&s);
}

/// Cancelling with one dispute already resolved and another still open is
/// refused, and the resolved one is not disturbed.
#[test]
fn cancel_is_refused_while_any_milestone_dispute_remains_open() {
    let s = setup();
    let id = funded(&s, &[100, 200, 300]);

    s.escrow.open_milestone_dispute(&id, &s.client, &0);
    s.escrow.open_milestone_dispute(&id, &s.client, &2);
    s.escrow.resolve_milestone_dispute(&id, &0, &10_000);

    // One remains open, so the whole cancel is blocked.
    assert_eq!(
        s.escrow.try_cancel_contract(&id),
        Err(Ok(EscrowError::MilestoneDisputed))
    );

    s.escrow.resolve_milestone_dispute(&id, &2, &10_000);
    // Now it is allowed, and only the untouched milestone 1 is refunded.
    let client_before = s.token.balance(&s.client);
    s.escrow.cancel_contract(&id);
    assert_eq!(s.token.balance(&s.client), client_before + 200);
    assert_custody(&s, id);
    assert_conserved(&s);
}

/// The whole-escrow dispute still behaves exactly as before, and does not
/// interfere with the milestone-scoped state.
#[test]
fn the_whole_escrow_dispute_is_unchanged() {
    let s = setup();
    let id = funded(&s, &[100, 200]);

    s.escrow.open_dispute(&id, &s.client);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Disputed);

    // Every release is blocked by the escrow-level status, as before.
    assert_eq!(
        s.escrow.try_release_milestone(&id, &0),
        Err(Ok(EscrowError::InvalidStatus))
    );

    s.escrow.resolve_dispute(&id, &5_000);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Resolved);
    assert_eq!(s.token.balance(&s.freelancer), 150);
    assert_custody(&s, id);
    assert_conserved(&s);
}

/// A milestone dispute cannot be opened once the escrow is under a whole-escrow
/// dispute or has been resolved.
#[test]
fn a_milestone_dispute_needs_an_active_escrow() {
    let s = setup();
    let id = funded(&s, &[100, 200]);
    s.escrow.open_dispute(&id, &s.client);

    assert_eq!(
        s.escrow.try_open_milestone_dispute(&id, &s.client, &0),
        Err(Ok(EscrowError::InvalidStatus))
    );
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[test]
fn the_milestone_dispute_events_are_emitted() {
    let s = setup();
    let id = funded(&s, &[100, 200]);
    s.escrow.open_milestone_dispute(&id, &s.freelancer, &1);

    let events = s.env.events().all();
    let (contract, topics, data) = events.get(events.len() - 1).unwrap();
    assert_eq!(contract, s.escrow_id);
    let name: Symbol = topics.get(0).unwrap().into_val(&s.env);
    let event_id: u64 = topics.get(1).unwrap().into_val(&s.env);
    assert_eq!(name, symbol_short!("m_disp"));
    assert_eq!(event_id, id);
    let payload: Vec<soroban_sdk::Val> = data.into_val(&s.env);
    let index: u32 = payload.get(0).unwrap().into_val(&s.env);
    let caller: Address = payload.get(1).unwrap().into_val(&s.env);
    assert_eq!(index, 1);
    assert_eq!(caller, s.freelancer);

    // Read the resolution event before any other call, since `events().all()`
    // reports the most recent invocation only.
    s.escrow.resolve_milestone_dispute(&id, &1, &5_000);
    let events = s.env.events().all();
    let (_, topics, data) = events.get(events.len() - 1).unwrap();
    let name: Symbol = topics.get(0).unwrap().into_val(&s.env);
    let event_id: u64 = topics.get(1).unwrap().into_val(&s.env);
    assert_eq!(name, symbol_short!("m_resolv"));
    assert_eq!(event_id, id);
    let payload: Vec<soroban_sdk::Val> = data.into_val(&s.env);
    let index: u32 = payload.get(0).unwrap().into_val(&s.env);
    let to_freelancer: i128 = payload.get(1).unwrap().into_val(&s.env);
    let to_client: i128 = payload.get(2).unwrap().into_val(&s.env);
    assert_eq!(index, 1);
    assert_eq!(to_freelancer, 100);
    assert_eq!(to_client, 100);
}

// ---------------------------------------------------------------------------
// Splits, including the rounding and the extremes
// ---------------------------------------------------------------------------

/// An odd amount cannot split evenly; the remainder goes to the client, matching
/// `resolve_dispute`'s rounding direction.
#[test]
fn an_odd_amount_rounds_the_remainder_to_the_client() {
    let s = setup();
    let id = funded(&s, &[101]);
    let client_before = s.token.balance(&s.client);

    s.escrow.open_milestone_dispute(&id, &s.client, &0);
    s.escrow.resolve_milestone_dispute(&id, &0, &5_000);

    assert_eq!(s.token.balance(&s.freelancer), 50);
    assert_eq!(s.token.balance(&s.client), client_before + 51);
    assert_custody(&s, id);
}

#[test]
fn the_split_endpoints_pay_one_side_only() {
    // All to the freelancer.
    let a = setup();
    let id = funded(&a, &[100]);
    a.escrow.open_milestone_dispute(&id, &a.client, &0);
    a.escrow.resolve_milestone_dispute(&id, &0, &10_000);
    assert_eq!(a.token.balance(&a.freelancer), 100);
    assert_eq!(a.token.balance(&a.escrow_id), 0);
    assert_custody(&a, id);

    // All to the client.
    let b = setup();
    let id2 = funded(&b, &[100]);
    let client_before = b.token.balance(&b.client);
    b.escrow.open_milestone_dispute(&id2, &b.freelancer, &0);
    b.escrow.resolve_milestone_dispute(&id2, &0, &0);
    assert_eq!(b.token.balance(&b.freelancer), 0);
    assert_eq!(b.token.balance(&b.client), client_before + 100);
    assert_eq!(b.token.balance(&b.escrow_id), 0);
    assert_custody(&b, id2);
}

/// A large disputed amount must not overflow the split, which is why this uses
/// the same `split_by_bps` as the escrow-level resolver.
#[test]
fn a_large_milestone_amount_splits_without_overflow() {
    let s = setup();
    let big: i128 = i128::MAX / 2;
    StellarAssetClient::new(&s.env, &s.token.address).mint(&s.client, &big);

    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &amounts(&s.env, &[big]),
    );
    s.escrow.deposit_funds(&id, &big);

    s.escrow.open_milestone_dispute(&id, &s.client, &0);
    s.escrow.resolve_milestone_dispute(&id, &0, &2_500);

    assert_eq!(s.token.balance(&s.freelancer), big / 4);
    assert_custody(&s, id);
}

/// The invariant holds at every step of a longer, mixed sequence.
#[test]
fn custody_holds_through_a_mixed_sequence() {
    let s = setup();
    let id = funded(&s, &[100, 200, 300, 400]);
    assert_custody(&s, id);

    s.escrow.release_milestone(&id, &0);
    assert_custody(&s, id);

    s.escrow.open_milestone_dispute(&id, &s.client, &2);
    assert_custody(&s, id);

    s.escrow.release_milestone(&id, &1);
    assert_custody(&s, id);

    s.escrow.resolve_milestone_dispute(&id, &2, &3_333);
    assert_custody(&s, id);

    s.escrow.release_milestone(&id, &3);
    assert_custody(&s, id);

    // All four released, so the escrow is complete and empty.
    let c = s.escrow.get_contract(&id);
    assert_eq!(c.status, ContractStatus::Completed);
    assert_eq!(s.token.balance(&s.escrow_id), 0);
    assert_conserved(&s);
}
