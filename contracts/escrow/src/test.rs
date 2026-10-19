extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, MockAuth, MockAuthInvoke},
    token::StellarAssetClient,
    token::TokenClient,
    vec,
    IntoVal, Val, Vec,
};

struct Setup<'a> {
    env: Env,
    escrow: EscrowClient<'a>,
    token: TokenClient<'a>,
    client: Address,
    freelancer: Address,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
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
        token,
        client,
        freelancer,
    }
}

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
    assert!(!s.escrow.is_accepted(&id));
    s.escrow.accept_contract(&id);
    assert!(s.escrow.is_accepted(&id));
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
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 100i128],
    );
    s.escrow.accept_contract(&id);
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
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 400i128],
    );
    s.escrow.accept_contract(&id);
    s.escrow.deposit_funds(&id, &400);
    s.escrow.open_dispute(&id, &s.freelancer);
    s.escrow.resolve_dispute(&id, &2_500);
    assert_eq!(s.token.balance(&s.freelancer), 100);
    assert_eq!(s.token.balance(&s.client), 900);

    let id2 = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 50i128],
    );
    s.escrow.accept_contract(&id2);
    s.escrow.deposit_funds(&id2, &50);
    s.escrow.cancel_contract(&id2);
    assert_eq!(s.token.balance(&s.client), 900);
    assert_eq!(
        s.escrow.get_contract(&id2).status,
        ContractStatus::Cancelled
    );
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
// Freelancer acceptance (#1694)
// ---------------------------------------------------------------------------

/// Creates a 100 + 200 escrow that the freelancer has not accepted yet.
fn created<'a>(s: &Setup<'a>) -> u64 {
    s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 100i128, 200i128],
    )
}

/// Authorises exactly one address for exactly one call.
///
/// `setup` calls `mock_all_auths`, which puts the host in recording mode. Both
/// `mock_auths` and `mock_all_auths` swap the whole authorization manager, so
/// this replaces recording with an enforcing manager holding only the entry
/// below -- a missing signature then fails instead of being waved through.
fn auth_only(s: &Setup, address: &Address, fn_name: &str, args: Vec<Val>) {
    s.env.mock_auths(&[MockAuth {
        address,
        invoke: &MockAuthInvoke {
            contract: &s.escrow.address,
            fn_name,
            args,
            sub_invokes: &[],
        },
    }]);
}

/// Args for a one-`u64` entry point.
fn id_arg(s: &Setup, id: u64) -> Vec<Val> {
    vec![&s.env, id.into_val(&s.env)]
}

#[test]
fn deposit_before_acceptance_is_rejected() {
    let s = setup();
    let id = created(&s);

    assert!(!s.escrow.is_accepted(&id));
    assert_eq!(
        s.escrow.try_deposit_funds(&id, &300),
        Err(Ok(EscrowError::NotAccepted))
    );
    // Nothing moved and the escrow is still inert.
    assert_eq!(s.token.balance(&s.client), 1_000);
    assert_eq!(s.token.balance(&s.escrow.address), 0);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Created);

    // Accepting is what unblocks it.
    s.escrow.accept_contract(&id);
    s.escrow.deposit_funds(&id, &300);
    assert_eq!(s.token.balance(&s.escrow.address), 300);
}

#[test]
fn accept_by_non_freelancer_is_rejected() {
    let s = setup();
    let id = created(&s);

    // The client signs, but the freelancer is the one whose auth is required.
    auth_only(&s, &s.client, "accept_contract", id_arg(&s, id));
    assert!(s.escrow.try_accept_contract(&id).is_err());
    assert!(!s.escrow.is_accepted(&id));

    // An unrelated address is no better.
    let outsider = Address::generate(&s.env);
    auth_only(&s, &outsider, "accept_contract", id_arg(&s, id));
    assert!(s.escrow.try_accept_contract(&id).is_err());
    assert!(!s.escrow.is_accepted(&id));

    // With the freelancer's own auth it succeeds.
    auth_only(&s, &s.freelancer, "accept_contract", id_arg(&s, id));
    s.escrow.accept_contract(&id);
    assert!(s.escrow.is_accepted(&id));
}

#[test]
fn decline_by_non_freelancer_is_rejected() {
    let s = setup();
    let id = created(&s);

    auth_only(&s, &s.client, "decline_contract", id_arg(&s, id));
    assert!(s.escrow.try_decline_contract(&id).is_err());
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Created);

    auth_only(&s, &s.freelancer, "decline_contract", id_arg(&s, id));
    s.escrow.decline_contract(&id);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Cancelled);
}

#[test]
fn decline_cancels_and_refunds_any_deposit() {
    let s = setup();
    let id = created(&s);

    // Nothing has been deposited, so the refund is a no-op here; the client's
    // balance is asserted so a future ordering change cannot silently keep it.
    let before = s.token.balance(&s.client);
    s.escrow.decline_contract(&id);

    assert_eq!(s.token.balance(&s.client), before);
    assert_eq!(s.token.balance(&s.escrow.address), 0);
    let c = s.escrow.get_contract(&id);
    assert_eq!(c.status, ContractStatus::Cancelled);
    assert_eq!(c.funded_amount, c.released_amount);
}

#[test]
fn accept_after_decline_is_rejected() {
    let s = setup();
    let id = created(&s);
    s.escrow.decline_contract(&id);

    assert_eq!(
        s.escrow.try_accept_contract(&id),
        Err(Ok(EscrowError::InvalidStatus))
    );
    assert!(!s.escrow.is_accepted(&id));
    // And the declined escrow cannot be funded either.
    assert_eq!(
        s.escrow.try_deposit_funds(&id, &100),
        Err(Ok(EscrowError::NotAccepted))
    );
}

#[test]
fn decline_after_accept_is_rejected() {
    let s = setup();
    let id = created(&s);
    s.escrow.accept_contract(&id);

    assert_eq!(
        s.escrow.try_decline_contract(&id),
        Err(Ok(EscrowError::InvalidStatus))
    );
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Created);
}

#[test]
fn acceptance_is_one_shot() {
    let s = setup();
    let id = created(&s);
    s.escrow.accept_contract(&id);
    s.escrow.deposit_funds(&id, &100); // status is still Created (partial)

    assert_eq!(
        s.escrow.try_accept_contract(&id),
        Err(Ok(EscrowError::InvalidStatus))
    );
    assert!(s.escrow.is_accepted(&id));
}

#[test]
fn acceptance_is_rejected_once_the_escrow_is_closed() {
    let s = setup();
    let id = created(&s);
    s.escrow.cancel_contract(&id);

    assert_eq!(
        s.escrow.try_accept_contract(&id),
        Err(Ok(EscrowError::InvalidStatus))
    );
    assert!(!s.escrow.is_accepted(&id));
}

#[test]
fn pause_gates_accept_and_decline() {
    let s = setup();
    let id = created(&s);
    s.escrow.set_paused(&true);

    assert_eq!(
        s.escrow.try_accept_contract(&id),
        Err(Ok(EscrowError::Paused))
    );
    assert_eq!(
        s.escrow.try_decline_contract(&id),
        Err(Ok(EscrowError::Paused))
    );
    // Reads stay available while paused.
    assert!(!s.escrow.is_accepted(&id));
}

#[test]
fn is_accepted_is_false_for_an_unknown_escrow() {
    let s = setup();
    assert!(!s.escrow.is_accepted(&999));
    assert_eq!(
        s.escrow.try_accept_contract(&999),
        Err(Ok(EscrowError::ContractNotFound))
    );
}

#[test]
fn acceptance_is_per_escrow() {
    let s = setup();
    let a = created(&s);
    let b = created(&s);
    s.escrow.accept_contract(&a);

    assert!(s.escrow.is_accepted(&a));
    assert!(!s.escrow.is_accepted(&b));
    assert_eq!(
        s.escrow.try_deposit_funds(&b, &300),
        Err(Ok(EscrowError::NotAccepted))
    );
    s.escrow.deposit_funds(&a, &300);
}

