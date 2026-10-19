extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events, MockAuth, MockAuthInvoke},
    token::StellarAssetClient,
    token::TokenClient,
    vec,
    IntoVal, Symbol, TryIntoVal, Val, Vec,
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
// Freelancer resignation (#1695)
// ---------------------------------------------------------------------------

fn created<'a>(s: &Setup<'a>, amounts: &[i128]) -> u64 {
    let mut milestones = Vec::new(&s.env);
    for a in amounts {
        milestones.push_back(*a);
    }
    s.escrow
        .create_contract(&s.client, &s.freelancer, &s.token.address, &milestones)
}

/// Authorises exactly one address for exactly one call.
///
/// `setup` calls `mock_all_auths`, which puts the host in recording mode where
/// every `require_auth` passes. Both `mock_auths` and `mock_all_auths` swap the
/// whole authorization manager, so this replaces recording with an enforcing
/// manager holding only the entry below.
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

fn id_arg(s: &Setup, id: u64) -> Vec<Val> {
    vec![&s.env, id.into_val(&s.env)]
}

/// The refund amount carried by the `resign` event for `id`, if it was published.
fn resign_event_refund(s: &Setup, id: u64) -> Option<i128> {
    let want = symbol_short!("resign");
    for (contract, topics, data) in s.env.events().all().iter() {
        if contract != s.escrow.address || topics.len() != 2 {
            continue;
        }
        let name: Symbol = topics.get(0).unwrap().try_into_val(&s.env).unwrap();
        let topic_id: u64 = topics.get(1).unwrap().try_into_val(&s.env).unwrap();
        if name == want && topic_id == id {
            return Some(data.try_into_val(&s.env).unwrap());
        }
    }
    None
}

#[test]
fn resign_in_created_refunds_a_partial_deposit() {
    let s = setup();
    let id = created(&s, &[100, 200]);
    s.escrow.deposit_funds(&id, &100); // partial, so the status is still Created
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Created);

    s.escrow.resign(&id);

    assert_eq!(s.token.balance(&s.client), 1_000); // 1000 - 100 + 100
    assert_eq!(s.token.balance(&s.freelancer), 0);
    assert_eq!(s.token.balance(&s.escrow.address), 0);
    let c = s.escrow.get_contract(&id);
    assert_eq!(c.status, ContractStatus::Cancelled);
    assert_eq!(c.funded_amount, c.released_amount);
    assert!(s.escrow.was_resigned(&id));
}

#[test]
fn resign_in_funded_after_one_release_refunds_only_the_remainder() {
    let s = setup();
    let id = created(&s, &[100, 200]);
    s.escrow.deposit_funds(&id, &300);
    s.escrow.release_milestone(&id, &0); // 100 is paid and stays paid
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);

    s.escrow.resign(&id);

    // No clawback: the freelancer keeps the released milestone.
    assert_eq!(s.token.balance(&s.freelancer), 100);
    assert_eq!(s.token.balance(&s.client), 900); // 1000 - 300 + 200
    assert_eq!(s.token.balance(&s.escrow.address), 0);
    let c = s.escrow.get_contract(&id);
    assert_eq!(c.status, ContractStatus::Cancelled);
    assert_eq!(c.released_amount, 100);
    assert_eq!(c.funded_amount, 100);
    assert!(s.escrow.was_resigned(&id));
}

#[test]
fn resign_while_disputed_is_rejected() {
    let s = setup();
    let id = created(&s, &[400]);
    s.escrow.deposit_funds(&id, &400);
    s.escrow.open_dispute(&id, &s.client);

    assert_eq!(
        s.escrow.try_resign(&id),
        Err(Ok(EscrowError::InvalidStatus))
    );
    // The escrow is untouched and the admin still owns the outcome.
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Disputed);
    assert_eq!(s.token.balance(&s.escrow.address), 400);
    assert!(!s.escrow.was_resigned(&id));
}

#[test]
fn client_cannot_resign() {
    let s = setup();
    let id = created(&s, &[300]);
    s.escrow.deposit_funds(&id, &300);

    // The client signs, but the freelancer is the one whose auth is required.
    auth_only(&s, &s.client, "resign", id_arg(&s, id));
    assert!(s.escrow.try_resign(&id).is_err());
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);
    assert!(!s.escrow.was_resigned(&id));

    // An unrelated address is no better.
    let outsider = Address::generate(&s.env);
    auth_only(&s, &outsider, "resign", id_arg(&s, id));
    assert!(s.escrow.try_resign(&id).is_err());

    // With the freelancer's own auth it succeeds.
    auth_only(&s, &s.freelancer, "resign", id_arg(&s, id));
    s.escrow.resign(&id);
    assert!(s.escrow.was_resigned(&id));
}

#[test]
fn resign_after_completion_is_rejected() {
    let s = setup();
    let id = created(&s, &[100, 200]);
    s.escrow.deposit_funds(&id, &300);
    s.escrow.release_milestone(&id, &0);
    s.escrow.release_milestone(&id, &1);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Completed);

    assert_eq!(
        s.escrow.try_resign(&id),
        Err(Ok(EscrowError::InvalidStatus))
    );
    assert_eq!(s.token.balance(&s.freelancer), 300);
}

#[test]
fn resign_is_one_shot() {
    let s = setup();
    let id = created(&s, &[300]);
    s.escrow.deposit_funds(&id, &300);
    s.escrow.resign(&id);

    assert_eq!(
        s.escrow.try_resign(&id),
        Err(Ok(EscrowError::InvalidStatus))
    );
    assert_eq!(s.token.balance(&s.client), 1_000);
}

#[test]
fn was_resigned_distinguishes_resignation_from_cancellation() {
    let s = setup();
    let resigned = created(&s, &[300]);
    let cancelled = created(&s, &[300]);
    s.escrow.deposit_funds(&resigned, &300);
    s.escrow.deposit_funds(&cancelled, &300);

    s.escrow.resign(&resigned);
    s.escrow.cancel_contract(&cancelled);

    assert!(s.escrow.was_resigned(&resigned));
    assert!(!s.escrow.was_resigned(&cancelled));
    // Both are Cancelled, which is exactly why the flag is needed.
    assert_eq!(
        s.escrow.get_contract(&resigned).status,
        ContractStatus::Cancelled
    );
    assert_eq!(
        s.escrow.get_contract(&cancelled).status,
        ContractStatus::Cancelled
    );
}

#[test]
fn was_resigned_is_false_for_an_unknown_escrow() {
    let s = setup();
    let id = created(&s, &[300]);
    assert!(!s.escrow.was_resigned(&id));
    assert!(!s.escrow.was_resigned(&999));
    assert_eq!(s.escrow.try_resign(&999), Err(Ok(EscrowError::ContractNotFound)));
}

#[test]
fn pause_gates_resign() {
    let s = setup();
    let id = created(&s, &[300]);
    s.escrow.deposit_funds(&id, &300);
    s.escrow.set_paused(&true);

    assert_eq!(s.escrow.try_resign(&id), Err(Ok(EscrowError::Paused)));
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);
    // The read stays available while paused.
    assert!(!s.escrow.was_resigned(&id));
}

#[test]
fn resign_publishes_the_refund_it_paid() {
    let s = setup();
    let id = created(&s, &[100, 200]);
    s.escrow.deposit_funds(&id, &300);
    s.escrow.release_milestone(&id, &0);

    assert_eq!(resign_event_refund(&s, id), None);
    s.escrow.resign(&id);
    assert_eq!(resign_event_refund(&s, id), Some(200));
}

#[test]
fn resignation_moves_funds_only_to_the_client() {
    let s = setup();
    let id = created(&s, &[100, 200]);
    s.escrow.deposit_funds(&id, &300);
    s.escrow.release_milestone(&id, &0);

    let client_before = s.token.balance(&s.client);
    let freelancer_before = s.token.balance(&s.freelancer);
    s.escrow.resign(&id);

    // The freelancer's balance is unchanged by the resignation itself, and the
    // client receives exactly what the contract was holding.
    assert_eq!(s.token.balance(&s.freelancer), freelancer_before);
    assert_eq!(s.token.balance(&s.client), client_before + 200);
    assert_eq!(s.token.balance(&s.escrow.address), 0);
}

