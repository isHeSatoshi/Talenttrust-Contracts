extern crate std;

use super::*;
use soroban_sdk::{testutils::Address as _, token::StellarAssetClient, token::TokenClient, vec};

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
// Mutual settlement (#1689)
// ---------------------------------------------------------------------------

/// Creates and fully funds an escrow of 100 + 200, returning its id.
fn funded<'a>(s: &Setup<'a>) -> u64 {
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 100i128, 200i128],
    );
    s.escrow.deposit_funds(&id, &300);
    id
}

#[test]
fn client_proposes_freelancer_accepts() {
    let s = setup();
    let id = funded(&s);

    s.escrow.propose_settlement(&id, &s.client, &150);
    let pending = s.escrow.get_settlement(&id).unwrap();
    assert_eq!(pending.proposer, s.client);
    assert_eq!(pending.freelancer_amount, 150);
    assert_eq!(pending.created_at, s.env.ledger().timestamp());

    s.escrow.accept_settlement(&id, &s.freelancer, &150);

    assert_eq!(s.token.balance(&s.freelancer), 150);
    assert_eq!(s.token.balance(&s.client), 850);
    assert_eq!(s.token.balance(&s.escrow.address), 0);
    let c = s.escrow.get_contract(&id);
    assert_eq!(c.status, ContractStatus::Resolved);
    assert_eq!(c.funded_amount, c.released_amount);
    assert_eq!(s.escrow.get_settlement(&id), None);
}

#[test]
fn freelancer_proposes_client_accepts() {
    let s = setup();
    let id = funded(&s);

    s.escrow.propose_settlement(&id, &s.freelancer, &200);
    s.escrow.accept_settlement(&id, &s.client, &200);

    assert_eq!(s.token.balance(&s.freelancer), 200);
    assert_eq!(s.token.balance(&s.client), 800);
    assert_eq!(s.token.balance(&s.escrow.address), 0);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Resolved);
}

#[test]
fn proposer_cannot_self_accept() {
    let s = setup();
    let id = funded(&s);

    s.escrow.propose_settlement(&id, &s.client, &150);
    assert_eq!(
        s.escrow.try_accept_settlement(&id, &s.client, &150),
        Err(Ok(EscrowError::SettlementMismatch))
    );
    // Nothing moved and the proposal is still pending.
    assert_eq!(s.token.balance(&s.escrow.address), 300);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);
    assert!(s.escrow.get_settlement(&id).is_some());
}

#[test]
fn mismatched_amount_rejected_then_correct_amount_accepted() {
    let s = setup();
    let id = funded(&s);

    s.escrow.propose_settlement(&id, &s.client, &150);
    assert_eq!(
        s.escrow.try_accept_settlement(&id, &s.freelancer, &151),
        Err(Ok(EscrowError::SettlementMismatch))
    );
    assert_eq!(s.token.balance(&s.escrow.address), 300);

    s.escrow.accept_settlement(&id, &s.freelancer, &150);
    assert_eq!(s.token.balance(&s.freelancer), 150);
}

#[test]
fn stale_proposal_is_not_settleable_after_a_newer_one() {
    let s = setup();
    let id = funded(&s);

    // The front-running case: the counterparty signs the split it read (150)
    // while the proposer has already replaced the proposal with 250.
    s.escrow.propose_settlement(&id, &s.client, &150);
    s.escrow.propose_settlement(&id, &s.client, &250);
    assert_eq!(
        s.escrow.try_accept_settlement(&id, &s.freelancer, &150),
        Err(Ok(EscrowError::SettlementMismatch))
    );
    assert_eq!(s.token.balance(&s.escrow.address), 300);

    s.escrow.accept_settlement(&id, &s.freelancer, &250);
    assert_eq!(s.token.balance(&s.freelancer), 250);
    assert_eq!(s.token.balance(&s.client), 750);
}

#[test]
fn proposal_is_invalidated_by_a_release() {
    let s = setup();
    let id = funded(&s);

    s.escrow.propose_settlement(&id, &s.client, &150);
    s.escrow.release_milestone(&id, &0); // 100 paid out, balance is now 200

    assert_eq!(s.escrow.get_settlement(&id), None);
    assert_eq!(
        s.escrow.try_accept_settlement(&id, &s.freelancer, &150),
        Err(Ok(EscrowError::InvalidStatus))
    );
    // The freelancer has the released 100 and nothing more.
    assert_eq!(s.token.balance(&s.freelancer), 100);
}

#[test]
fn proposal_is_invalidated_by_a_deposit() {
    let s = setup();
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 100i128, 200i128],
    );
    s.escrow.deposit_funds(&id, &100);
    s.escrow.propose_settlement(&id, &s.client, &50);
    s.escrow.deposit_funds(&id, &200); // balance 100 -> 300

    assert_eq!(s.escrow.get_settlement(&id), None);
    assert_eq!(
        s.escrow.try_accept_settlement(&id, &s.freelancer, &50),
        Err(Ok(EscrowError::InvalidStatus))
    );
}

#[test]
fn settlement_from_disputed_closes_and_blocks_the_admin() {
    let s = setup();
    let id = funded(&s);
    s.escrow.open_dispute(&id, &s.freelancer);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Disputed);

    // Settlement is reachable from Disputed, so the parties can still walk away
    // together instead of waiting for the admin.
    s.escrow.propose_settlement(&id, &s.freelancer, &120);
    s.escrow.accept_settlement(&id, &s.client, &120);

    assert_eq!(s.token.balance(&s.freelancer), 120);
    assert_eq!(s.token.balance(&s.client), 880);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Resolved);

    // Once resolved, the admin has no path back in.
    assert_eq!(
        s.escrow.try_resolve_dispute(&id, &5_000),
        Err(Ok(EscrowError::InvalidStatus))
    );
}

#[test]
fn admin_cannot_resolve_after_settlement() {
    let s = setup();
    let id = funded(&s);
    s.escrow.propose_settlement(&id, &s.client, &300);
    s.escrow.accept_settlement(&id, &s.freelancer, &300);

    assert_eq!(
        s.escrow.try_resolve_dispute(&id, &0),
        Err(Ok(EscrowError::InvalidStatus))
    );
    assert_eq!(
        s.escrow.try_open_dispute(&id, &s.freelancer),
        Err(Ok(EscrowError::InvalidStatus))
    );
}

#[test]
fn settlement_amounts_must_be_within_the_held_balance() {
    let s = setup();
    let id = funded(&s);

    assert_eq!(
        s.escrow.try_propose_settlement(&id, &s.client, &301),
        Err(Ok(EscrowError::InvalidAmount))
    );
    assert_eq!(
        s.escrow.try_propose_settlement(&id, &s.client, &-1),
        Err(Ok(EscrowError::InvalidAmount))
    );

    // Both edges are legal: everything to the freelancer, and everything back
    // to the client.
    s.escrow.propose_settlement(&id, &s.client, &300);
    s.escrow.accept_settlement(&id, &s.freelancer, &300);
    assert_eq!(s.token.balance(&s.freelancer), 300);
    assert_eq!(s.token.balance(&s.client), 700);
    assert_eq!(s.token.balance(&s.escrow.address), 0);
}

#[test]
fn settlement_can_refund_everything_to_the_client() {
    let s = setup();
    let id = funded(&s);

    s.escrow.propose_settlement(&id, &s.freelancer, &0);
    s.escrow.accept_settlement(&id, &s.client, &0);

    assert_eq!(s.token.balance(&s.freelancer), 0);
    assert_eq!(s.token.balance(&s.client), 1_000);
    assert_eq!(s.token.balance(&s.escrow.address), 0);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Resolved);
}

#[test]
fn only_parties_may_propose_or_accept() {
    let s = setup();
    let id = funded(&s);
    let outsider = Address::generate(&s.env);

    assert_eq!(
        s.escrow.try_propose_settlement(&id, &outsider, &100),
        Err(Ok(EscrowError::Unauthorized))
    );
    s.escrow.propose_settlement(&id, &s.client, &100);
    assert_eq!(
        s.escrow.try_accept_settlement(&id, &outsider, &100),
        Err(Ok(EscrowError::Unauthorized))
    );
    assert_eq!(s.token.balance(&s.escrow.address), 300);
}

#[test]
fn only_the_proposer_may_withdraw() {
    let s = setup();
    let id = funded(&s);

    s.escrow.propose_settlement(&id, &s.client, &150);
    assert_eq!(
        s.escrow.try_withdraw_settlement(&id, &s.freelancer),
        Err(Ok(EscrowError::Unauthorized))
    );
    assert!(s.escrow.get_settlement(&id).is_some());

    s.escrow.withdraw_settlement(&id, &s.client);
    assert_eq!(s.escrow.get_settlement(&id), None);
    assert_eq!(
        s.escrow.try_accept_settlement(&id, &s.freelancer, &150),
        Err(Ok(EscrowError::InvalidStatus))
    );
    assert_eq!(
        s.escrow.try_withdraw_settlement(&id, &s.client),
        Err(Ok(EscrowError::InvalidStatus))
    );
}

#[test]
fn get_settlement_distinguishes_missing_escrow_from_missing_proposal() {
    let s = setup();
    let id = funded(&s);

    assert_eq!(s.escrow.get_settlement(&id), None);
    assert_eq!(
        s.escrow.try_get_settlement(&999),
        Err(Ok(EscrowError::ContractNotFound))
    );
}

#[test]
fn pause_gates_propose_and_accept_but_not_withdraw() {
    let s = setup();
    let id = funded(&s);
    s.escrow.propose_settlement(&id, &s.client, &150);

    s.escrow.set_paused(&true);
    assert_eq!(
        s.escrow.try_propose_settlement(&id, &s.freelancer, &200),
        Err(Ok(EscrowError::Paused))
    );
    assert_eq!(
        s.escrow.try_accept_settlement(&id, &s.freelancer, &150),
        Err(Ok(EscrowError::Paused))
    );
    // Withdrawing only removes state, so it stays available during a pause.
    s.escrow.withdraw_settlement(&id, &s.client);
    assert_eq!(s.escrow.get_settlement(&id), None);
}

#[test]
fn settlement_is_rejected_once_the_escrow_is_closed() {
    let s = setup();
    let id = funded(&s);
    s.escrow.cancel_contract(&id);

    assert_eq!(
        s.escrow.try_propose_settlement(&id, &s.client, &100),
        Err(Ok(EscrowError::InvalidStatus))
    );
}

#[test]
fn settlement_after_partial_release_splits_only_the_remainder() {
    let s = setup();
    let id = funded(&s);
    s.escrow.release_milestone(&id, &0); // 100 to the freelancer, 200 held

    s.escrow.propose_settlement(&id, &s.freelancer, &200);
    s.escrow.accept_settlement(&id, &s.client, &200);

    assert_eq!(s.token.balance(&s.freelancer), 300);
    assert_eq!(s.token.balance(&s.client), 700);
    assert_eq!(s.token.balance(&s.escrow.address), 0);
    let c = s.escrow.get_contract(&id);
    assert_eq!(c.released_amount, 300);
    assert_eq!(c.funded_amount, 300);
}

