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

/// Mints extra escrow tokens to an address. `setup` funds only the client, so a
/// test that needs the freelancer to stake a dispute bond mints it here rather
/// than changing `setup` (which would move the balances other tests assert).
fn mint(s: &Setup, to: &Address, amount: i128) {
    StellarAssetClient::new(&s.env, &s.token.address).mint(to, &amount);
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
    // The freelancer opens the dispute, so they must stake the bond: 400 * 200 /
    // 10000 = 8. `setup` funds only the client, so mint it here.
    mint(&s, &s.freelancer, 8);
    s.escrow.open_dispute(&id, &s.freelancer);
    assert_eq!(s.token.balance(&s.freelancer), 0);
    assert_eq!(s.escrow.get_dispute_bond(&id), Some((s.freelancer.clone(), 8)));
    // The bond sits on top of the escrow, not inside it.
    assert_eq!(s.token.balance(&s.escrow.address), 408);

    // 25% of the held 400 to the freelancer, and the bond returned because the
    // dispute was upheld. The split is unchanged by the bond: still exactly 100.
    s.escrow.resolve_dispute(&id, &2_500, &true);
    assert_eq!(s.token.balance(&s.freelancer), 108);
    assert_eq!(s.token.balance(&s.client), 900);
    assert_eq!(s.token.balance(&s.escrow.address), 0);

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
// Dispute bond (#1683)
// ---------------------------------------------------------------------------

/// Creates and fully funds an escrow whose total is `total`.
fn funded<'a>(s: &Setup<'a>, total: i128) -> u64 {
    let id = s
        .escrow
        .create_contract(&s.client, &s.freelancer, &s.token.address, &vec![&s.env, total]);
    s.escrow.deposit_funds(&id, &total);
    id
}

#[test]
fn bond_is_two_percent_of_the_total() {
    let s = setup();
    for (total, expected) in [(10_000i128, 200i128), (400, 8), (50, 1), (49, 0), (1, 0)] {
        // Top the client up so the deposit can cover this escrow's total, and
        // stake exactly the expected bond so the transfer cannot fail.
        mint(&s, &s.client, total);
        let id = funded(&s, total);
        if expected > 0 {
            mint(&s, &s.freelancer, expected);
        }
        s.escrow.open_dispute(&id, &s.freelancer);
        assert_eq!(
            s.escrow.get_dispute_bond(&id),
            Some((s.freelancer.clone(), expected)),
            "total {total} should stake {expected}"
        );
        s.escrow.resolve_dispute(&id, &10_000, &true);
    }
}

#[test]
fn bond_rounds_to_zero_for_a_tiny_total() {
    let s = setup();
    let id = funded(&s, 49);
    // 49 * 200 / 10_000 == 0, so no tokens are needed at all.
    assert_eq!(s.token.balance(&s.freelancer), 0);
    s.escrow.open_dispute(&id, &s.freelancer);

    assert_eq!(s.escrow.get_dispute_bond(&id), Some((s.freelancer.clone(), 0)));
    assert_eq!(s.token.balance(&s.escrow.address), 49);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Disputed);

    s.escrow.resolve_dispute(&id, &5_000, &false);
    assert_eq!(s.token.balance(&s.escrow.address), 0);
}

#[test]
fn opener_without_the_bond_cannot_open_a_dispute() {
    let s = setup();
    let id = funded(&s, 400); // bond is 8
    assert_eq!(s.token.balance(&s.freelancer), 0);

    // The transfer fails, so the status must not have moved and the escrow must
    // still be releasable.
    assert!(s.escrow.try_open_dispute(&id, &s.freelancer).is_err());
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);
    assert_eq!(s.escrow.get_dispute_bond(&id), None);
    assert_eq!(s.token.balance(&s.escrow.address), 400);

    s.escrow.release_milestone(&id, &0);
    assert_eq!(s.token.balance(&s.freelancer), 400);
}

#[test]
fn bond_returns_to_the_opener_when_upheld() {
    let s = setup();
    let id = funded(&s, 400);
    mint(&s, &s.freelancer, 8);
    s.escrow.open_dispute(&id, &s.freelancer);
    s.escrow.resolve_dispute(&id, &2_500, &true);

    assert_eq!(s.token.balance(&s.freelancer), 108); // 100 split + 8 bond back
    assert_eq!(s.token.balance(&s.client), 900); // 300 refund, bond not theirs
    assert_eq!(s.token.balance(&s.escrow.address), 0);
    assert_eq!(s.escrow.get_dispute_bond(&id), None);
}

#[test]
fn bond_is_forfeited_to_the_counterparty_when_rejected() {
    let s = setup();
    let id = funded(&s, 400);
    mint(&s, &s.freelancer, 8);
    s.escrow.open_dispute(&id, &s.freelancer);
    s.escrow.resolve_dispute(&id, &2_500, &false);

    // The freelancer's split is still exactly 100; only the bond moved.
    assert_eq!(s.token.balance(&s.freelancer), 100);
    assert_eq!(s.token.balance(&s.client), 908); // 300 refund + 8 bond
    assert_eq!(s.token.balance(&s.escrow.address), 0);
    assert_eq!(s.escrow.get_dispute_bond(&id), None);
}

#[test]
fn bond_is_forfeited_to_the_freelancer_when_the_client_loses_it() {
    let s = setup();
    let id = funded(&s, 400);
    // The client already holds 600 after funding 400 of 1000.
    s.escrow.open_dispute(&id, &s.client);
    assert_eq!(s.token.balance(&s.client), 592);

    s.escrow.resolve_dispute(&id, &10_000, &false);
    // 400 split to the freelancer, plus the client's 8 bond.
    assert_eq!(s.token.balance(&s.freelancer), 408);
    assert_eq!(s.token.balance(&s.client), 592);
    assert_eq!(s.token.balance(&s.escrow.address), 0);
}

#[test]
fn the_bond_never_enters_the_split() {
    let s = setup();
    let id = funded(&s, 400);
    mint(&s, &s.freelancer, 8);
    s.escrow.open_dispute(&id, &s.freelancer);

    // Held is 400 whether or not a bond was staked: 2500 bps of 400 is 100, not
    // 2500 bps of 408.
    s.escrow.resolve_dispute(&id, &2_500, &true);
    let c = s.escrow.get_contract(&id);
    // `resolve_dispute` zeroes the outstanding balance, so funded == released.
    assert_eq!(c.funded_amount, c.released_amount);
    assert_eq!(c.released_amount, 100);
    assert_eq!(s.token.balance(&s.freelancer), 108);
    assert_eq!(s.token.balance(&s.client), 900);
}

#[test]
fn no_path_leaves_the_bond_stranded() {
    let s = setup();
    // Opener keeps the bond.
    let a = funded(&s, 400);
    mint(&s, &s.freelancer, 8);
    s.escrow.open_dispute(&a, &s.freelancer);
    s.escrow.resolve_dispute(&a, &0, &true);
    assert_eq!(s.token.balance(&s.escrow.address), 0);
    assert_eq!(s.escrow.get_dispute_bond(&a), None);

    // Opener forfeits it.
    let b = funded(&s, 400);
    mint(&s, &s.freelancer, 8);
    s.escrow.open_dispute(&b, &s.freelancer);
    s.escrow.resolve_dispute(&b, &10_000, &false);
    assert_eq!(s.token.balance(&s.escrow.address), 0);
    assert_eq!(s.escrow.get_dispute_bond(&b), None);

    // Zero bond.
    let d = funded(&s, 49);
    s.escrow.open_dispute(&d, &s.freelancer);
    s.escrow.resolve_dispute(&d, &5_000, &true);
    assert_eq!(s.token.balance(&s.escrow.address), 0);
    assert_eq!(s.escrow.get_dispute_bond(&d), None);
}

#[test]
fn bond_is_per_escrow() {
    let s = setup();
    let a = funded(&s, 400);
    let b = funded(&s, 500);
    mint(&s, &s.freelancer, 8 + 10);

    s.escrow.open_dispute(&a, &s.freelancer);
    s.escrow.open_dispute(&b, &s.client);

    assert_eq!(s.escrow.get_dispute_bond(&a), Some((s.freelancer.clone(), 8)));
    assert_eq!(s.escrow.get_dispute_bond(&b), Some((s.client.clone(), 10)));
    // 400 + 8 + 500 + 10.
    assert_eq!(s.token.balance(&s.escrow.address), 918);
}

#[test]
fn dispute_still_requires_a_party_and_a_live_escrow() {
    let s = setup();
    let id = funded(&s, 400);
    let outsider = Address::generate(&s.env);
    mint(&s, &outsider, 8);
    mint(&s, &s.freelancer, 8);

    assert_eq!(
        s.escrow.try_open_dispute(&id, &outsider),
        Err(Ok(EscrowError::Unauthorized))
    );
    assert_eq!(s.escrow.get_dispute_bond(&id), None);

    s.escrow.open_dispute(&id, &s.freelancer);
    assert_eq!(
        s.escrow.try_open_dispute(&id, &s.freelancer),
        Err(Ok(EscrowError::InvalidStatus))
    );
}

#[test]
fn get_dispute_bond_is_none_before_a_dispute() {
    let s = setup();
    let id = funded(&s, 400);
    assert_eq!(s.escrow.get_dispute_bond(&id), None);
    assert_eq!(s.escrow.get_dispute_bond(&999), None);
}

#[test]
fn bond_is_charged_on_the_total_not_the_funded_amount() {
    let s = setup();
    // 1000 total, only 400 funded: the bond follows the total (20, not 8).
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 1_000i128],
    );
    s.escrow.deposit_funds(&id, &400);
    mint(&s, &s.freelancer, 20);

    s.escrow.open_dispute(&id, &s.freelancer);
    assert_eq!(s.escrow.get_dispute_bond(&id), Some((s.freelancer.clone(), 20)));
    assert_eq!(s.token.balance(&s.escrow.address), 420);

    // Only the funded 400 is splittable.
    s.escrow.resolve_dispute(&id, &5_000, &true);
    assert_eq!(s.token.balance(&s.freelancer), 220);
    assert_eq!(s.token.balance(&s.client), 800);
    assert_eq!(s.token.balance(&s.escrow.address), 0);
}

