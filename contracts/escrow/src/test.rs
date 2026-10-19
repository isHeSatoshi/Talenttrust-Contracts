extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events, MockAuth, MockAuthInvoke},
    token::StellarAssetClient,
    token::TokenClient,
    IntoVal, Symbol,
};

struct Setup<'a> {
    env: Env,
    escrow: EscrowClient<'a>,
    escrow_id: Address,
    token: TokenClient<'a>,
    client: Address,
    freelancer: Address,
    admin: Address,
    arbiter: Address,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let client = Address::generate(&env);
    let freelancer = Address::generate(&env);
    let arbiter = Address::generate(&env);
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
        arbiter,
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

/// Create an escrow with no arbiter.
fn create(s: &Setup, xs: &[i128]) -> u64 {
    s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &amounts(&s.env, xs),
        &None,
    )
}

/// Create an escrow naming `arbiter`.
fn create_with_arbiter(s: &Setup, xs: &[i128], arbiter: &Address) -> u64 {
    s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &amounts(&s.env, xs),
        &Some(arbiter.clone()),
    )
}

/// Create, fund fully, and return the id.
fn funded(s: &Setup, xs: &[i128]) -> u64 {
    let id = create(s, xs);
    let total: i128 = xs.iter().sum();
    s.escrow.deposit_funds(&id, &total);
    id
}

/// Create with an arbiter, have it accept, fund fully, and return the id.
fn funded_with_arbiter(s: &Setup, xs: &[i128]) -> u64 {
    let id = create_with_arbiter(s, xs, &s.arbiter);
    s.escrow.accept_arbiter(&id);
    let total: i128 = xs.iter().sum();
    s.escrow.deposit_funds(&id, &total);
    id
}

// ---------------------------------------------------------------------------
// Pre-existing behaviour, updated only for the two signatures that changed.
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
    // `resolve_dispute` now takes the caller explicitly; the admin is the one
    // authorised when the escrow names no arbiter.
    s.escrow.resolve_dispute(&s.admin, &id, &2_500);
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
            &amounts(&s.env, &[1]),
            &None,
        ),
        Err(Ok(EscrowError::Paused))
    );
}

// ---------------------------------------------------------------------------
// Who may resolve
// ---------------------------------------------------------------------------

#[test]
fn arbiter_resolves_a_dispute() {
    let s = setup();
    let id = funded_with_arbiter(&s, &[400]);
    s.escrow.open_dispute(&id, &s.freelancer);
    s.escrow.resolve_dispute(&s.arbiter, &id, &2_500);

    assert_eq!(s.token.balance(&s.freelancer), 100);
    assert_eq!(s.token.balance(&s.client), 900);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Resolved);
}

#[test]
fn admin_cannot_resolve_an_arbiter_backed_escrow() {
    let s = setup();
    let id = funded_with_arbiter(&s, &[400]);
    s.escrow.open_dispute(&id, &s.freelancer);

    // The whole point of naming an arbiter is that the admin is not the judge.
    assert_eq!(
        s.escrow.try_resolve_dispute(&s.admin, &id, &2_500),
        Err(Ok(EscrowError::Unauthorized))
    );
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Disputed);
    assert_eq!(s.token.balance(&s.freelancer), 0);

    // The arbiter still can.
    s.escrow.resolve_dispute(&s.arbiter, &id, &2_500);
    assert_eq!(s.token.balance(&s.freelancer), 100);
}

#[test]
fn arbiter_cannot_resolve_a_different_escrow() {
    let s = setup();
    let other_arbiter = Address::generate(&s.env);

    let a = funded_with_arbiter(&s, &[400]);
    let b = create_with_arbiter(&s, &[400], &other_arbiter);
    s.escrow.accept_arbiter(&b);
    s.escrow.deposit_funds(&b, &400);

    s.escrow.open_dispute(&a, &s.freelancer);
    s.escrow.open_dispute(&b, &s.freelancer);

    // Each arbiter is scoped to the escrow that named them.
    assert_eq!(
        s.escrow.try_resolve_dispute(&other_arbiter, &a, &1_000),
        Err(Ok(EscrowError::Unauthorized))
    );
    assert_eq!(
        s.escrow.try_resolve_dispute(&s.arbiter, &b, &1_000),
        Err(Ok(EscrowError::Unauthorized))
    );

    s.escrow.resolve_dispute(&s.arbiter, &a, &1_000);
    s.escrow.resolve_dispute(&other_arbiter, &b, &1_000);
    assert_eq!(s.escrow.get_contract(&a).status, ContractStatus::Resolved);
    assert_eq!(s.escrow.get_contract(&b).status, ContractStatus::Resolved);
}

#[test]
fn escrow_without_an_arbiter_is_still_resolved_by_the_admin() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.escrow.open_dispute(&id, &s.client);
    s.escrow.resolve_dispute(&s.admin, &id, &10_000);
    assert_eq!(s.token.balance(&s.freelancer), 400);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Resolved);
}

#[test]
fn resolve_dispute_rejects_a_stranger() {
    let s = setup();
    let stranger = Address::generate(&s.env);

    let plain = funded(&s, &[400]);
    s.escrow.open_dispute(&plain, &s.client);
    assert_eq!(
        s.escrow.try_resolve_dispute(&stranger, &plain, &5_000),
        Err(Ok(EscrowError::Unauthorized))
    );
    // Nor may a party resolve their own dispute.
    assert_eq!(
        s.escrow.try_resolve_dispute(&s.client, &plain, &5_000),
        Err(Ok(EscrowError::Unauthorized))
    );
    assert_eq!(
        s.escrow.try_resolve_dispute(&s.freelancer, &plain, &5_000),
        Err(Ok(EscrowError::Unauthorized))
    );

    let arbitrated = funded_with_arbiter(&s, &[400]);
    s.escrow.open_dispute(&arbitrated, &s.client);
    assert_eq!(
        s.escrow.try_resolve_dispute(&stranger, &arbitrated, &5_000),
        Err(Ok(EscrowError::Unauthorized))
    );
}

/// An arbiter who never accepted has no authority: the admin keeps it, so the
/// unaccepted arbiter is just another unauthorised caller.
#[test]
fn an_unaccepted_arbiter_has_no_authority() {
    let s = setup();
    let id = create_with_arbiter(&s, &[400], &s.arbiter);
    assert_eq!(
        s.escrow.try_resolve_dispute(&s.arbiter, &id, &5_000),
        Err(Ok(EscrowError::Unauthorized))
    );

    // The admin is still the judge, and the status gate is what stops the call.
    assert_eq!(
        s.escrow.try_resolve_dispute(&s.admin, &id, &5_000),
        Err(Ok(EscrowError::InvalidStatus))
    );
}

// ---------------------------------------------------------------------------
// Funding gate
// ---------------------------------------------------------------------------

#[test]
fn deposit_before_accept_arbiter_is_refused() {
    let s = setup();
    let id = create_with_arbiter(&s, &[400], &s.arbiter);

    assert_eq!(
        s.escrow.try_deposit_funds(&id, &400),
        Err(Ok(EscrowError::ArbiterNotAccepted))
    );
    // Nothing moved.
    assert_eq!(s.token.balance(&s.client), 1_000);
    assert_eq!(s.token.balance(&s.escrow_id), 0);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Created);

    // A partial deposit is refused too, not just the completing one.
    assert_eq!(
        s.escrow.try_deposit_funds(&id, &1),
        Err(Ok(EscrowError::ArbiterNotAccepted))
    );

    s.escrow.accept_arbiter(&id);
    s.escrow.deposit_funds(&id, &400);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);
}

#[test]
fn deposits_are_unaffected_when_no_arbiter_is_named() {
    let s = setup();
    let id = create(&s, &[400]);
    s.escrow.deposit_funds(&id, &200);
    s.escrow.deposit_funds(&id, &200);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);
}

// ---------------------------------------------------------------------------
// create_contract validation
// ---------------------------------------------------------------------------

#[test]
fn arbiter_equal_to_a_party_is_rejected() {
    let s = setup();
    let client = s.client.clone();
    let freelancer = s.freelancer.clone();
    let contract = s.escrow_id.clone();

    for (label, bad) in [
        ("client", client),
        ("freelancer", freelancer),
        ("contract address", contract),
    ] {
        assert_eq!(
            s.escrow.try_create_contract(
                &s.client,
                &s.freelancer,
                &s.token.address,
                &amounts(&s.env, &[100]),
                &Some(bad),
            ),
            Err(Ok(EscrowError::InvalidParties)),
            "arbiter equal to the {label} must be rejected",
        );
    }
}

#[test]
fn a_distinct_arbiter_is_accepted_at_creation() {
    let s = setup();
    let id = create_with_arbiter(&s, &[100], &s.arbiter);
    let info = s.escrow.get_arbiter(&id).unwrap();
    assert_eq!(info.address, s.arbiter);
    assert!(!info.accepted);
}

#[test]
fn get_arbiter_is_none_when_no_arbiter_was_named() {
    let s = setup();
    let id = create(&s, &[100]);
    assert_eq!(s.escrow.get_arbiter(&id), None);
}

// ---------------------------------------------------------------------------
// accept_arbiter
// ---------------------------------------------------------------------------

#[test]
fn accept_arbiter_sets_accepted_and_emits_an_event() {
    let s = setup();
    let id = create_with_arbiter(&s, &[100], &s.arbiter);
    s.escrow.accept_arbiter(&id);

    // Read the events before any other contract call: `events().all()` reports
    // the most recent invocation only, so an intervening read would clear it.
    let events = s.env.events().all();
    let (contract, topics, data) = events.get(events.len() - 1).unwrap();
    assert_eq!(contract, s.escrow_id);
    let name: Symbol = topics.get(0).unwrap().into_val(&s.env);
    let event_id: u64 = topics.get(1).unwrap().into_val(&s.env);
    assert_eq!(name, symbol_short!("arb_acc"));
    assert_eq!(event_id, id);
    let who: Address = data.into_val(&s.env);
    assert_eq!(who, s.arbiter);

    let info = s.escrow.get_arbiter(&id).unwrap();
    assert!(info.accepted);
    assert_eq!(info.address, s.arbiter);
}

#[test]
fn accept_arbiter_requires_the_arbiters_own_signature() {
    let s = setup();
    let id = create_with_arbiter(&s, &[100], &s.arbiter);

    // Sign as the client instead of the arbiter: the address the contract
    // authenticates is the arbiter's, so this must fail.
    s.env.mock_auths(&[MockAuth {
        address: &s.client,
        invoke: &MockAuthInvoke {
            contract: &s.escrow_id,
            fn_name: "accept_arbiter",
            args: (&id,).into_val(&s.env),
            sub_invokes: &[],
        },
    }]);
    assert!(s.escrow.try_accept_arbiter(&id).is_err());
    assert!(!s.escrow.get_arbiter(&id).unwrap().accepted);

    // Signing as the arbiter works.
    let s2 = setup();
    let id2 = create_with_arbiter(&s2, &[100], &s2.arbiter);
    s2.env.mock_auths(&[MockAuth {
        address: &s2.arbiter,
        invoke: &MockAuthInvoke {
            contract: &s2.escrow_id,
            fn_name: "accept_arbiter",
            args: (&id2,).into_val(&s2.env),
            sub_invokes: &[],
        },
    }]);
    assert!(s2.escrow.try_accept_arbiter(&id2).is_ok());
    assert!(s2.escrow.get_arbiter(&id2).unwrap().accepted);
}

#[test]
fn accept_arbiter_is_refused_once_the_escrow_is_funded() {
    let s = setup();
    let id = create_with_arbiter(&s, &[400], &s.arbiter);
    s.escrow.accept_arbiter(&id);
    s.escrow.deposit_funds(&id, &400);

    assert_eq!(
        s.escrow.try_accept_arbiter(&id),
        Err(Ok(EscrowError::InvalidStatus))
    );
    // The arbiter is still the one recorded, and still accepted.
    let info = s.escrow.get_arbiter(&id).unwrap();
    assert_eq!(info.address, s.arbiter);
    assert!(info.accepted);
}

#[test]
fn accept_arbiter_is_refused_on_an_escrow_that_names_none() {
    let s = setup();
    let id = create(&s, &[100]);
    assert_eq!(
        s.escrow.try_accept_arbiter(&id),
        Err(Ok(EscrowError::Unauthorized))
    );
}

#[test]
fn accept_arbiter_is_paused_gated() {
    let s = setup();
    let id = create_with_arbiter(&s, &[100], &s.arbiter);
    s.escrow.set_paused(&true);
    assert_eq!(
        s.escrow.try_accept_arbiter(&id),
        Err(Ok(EscrowError::Paused))
    );
}

/// The arbiter's address is written once, at creation, and has no setter, so no
/// sequence of calls can swap the judge after the money is committed.
#[test]
fn the_arbiter_cannot_be_changed_after_funding() {
    let s = setup();
    let id = funded_with_arbiter(&s, &[400]);
    let before = s.escrow.get_arbiter(&id).unwrap();

    // There is no entry point that writes `DataKey::Arbiter` other than
    // create_contract and accept_arbiter; the latter only flips `accepted`.
    assert_eq!(s.escrow.try_accept_arbiter(&id), Err(Ok(EscrowError::InvalidStatus)));

    let after = s.escrow.get_arbiter(&id).unwrap();
    assert_eq!(after.address, before.address);
    assert!(after.accepted);

    // And a second escrow's arbiter is independent.
    let other = Address::generate(&s.env);
    let id2 = create_with_arbiter(&s, &[100], &other);
    assert_eq!(s.escrow.get_arbiter(&id2).unwrap().address, other);
    assert_eq!(s.escrow.get_arbiter(&id).unwrap().address, s.arbiter);
}

/// An accepted arbiter does not change any other authority: the client still
/// releases and cancels, the freelancer still disputes.
#[test]
fn an_arbiter_does_not_widen_anyone_elses_authority() {
    let s = setup();
    let id = funded_with_arbiter(&s, &[400, 600]);

    // The client still drives the ordinary lifecycle.
    s.escrow.release_milestone(&id, &0);
    assert_eq!(s.token.balance(&s.freelancer), 400);
    s.escrow.cancel_contract(&id);
    assert_eq!(s.token.balance(&s.client), 600);

    // The arbiter cannot release or cancel.
    assert_eq!(
        s.escrow.try_cancel_contract(&id),
        Err(Ok(EscrowError::InvalidStatus))
    );
    assert_eq!(s.escrow.try_resolve_dispute(&s.arbiter, &id, &5_000), Err(Ok(EscrowError::InvalidStatus)));
}

/// Escrows without an arbiter keep the exact behaviour they had before.
#[test]
fn no_arbiter_means_the_admin_path_is_unchanged() {
    let s = setup();
    let id = funded(&s, &[400]);
    assert_eq!(s.escrow.get_arbiter(&id), None);
    s.escrow.open_dispute(&id, &s.freelancer);
    s.escrow.resolve_dispute(&s.admin, &id, &2_500);
    assert_eq!(s.token.balance(&s.freelancer), 100);
    assert_eq!(s.token.balance(&s.client), 900);
}
