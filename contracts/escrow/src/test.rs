extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events, MockAuth, MockAuthInvoke},
    token::StellarAssetClient,
    token::TokenClient,
    vec, IntoVal, Symbol,
};

const MINT: i128 = 10_000;

struct Setup<'a> {
    env: Env,
    escrow: EscrowClient<'a>,
    escrow_id: Address,
    token: TokenClient<'a>,
    client: Address,
    freelancer: Address,
    /// Created by `setup()` and used for `initialize`. Kept on the struct so a
    /// test that needs it does not have to re-derive it.
    #[allow(dead_code)]
    admin: Address,
    treasury: Address,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let client = Address::generate(&env);
    let freelancer = Address::generate(&env);
    let treasury = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    StellarAssetClient::new(&env, &sac.address()).mint(&client, &MINT);
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
        treasury,
    }
}

fn amounts(env: &Env, xs: &[i128]) -> Vec<i128> {
    let mut v = Vec::new(env);
    for x in xs {
        v.push_back(*x);
    }
    v
}

/// Create an escrow and fund it fully.
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

/// The custody invariant: the contract's balance equals `funded - released`.
fn assert_custody(s: &Setup, id: u64) {
    let c = s.escrow.get_contract(&id);
    assert_eq!(
        s.token.balance(&s.escrow_id),
        c.funded_amount - c.released_amount,
        "contract custody must equal funded_amount - released_amount",
    );
}

/// Nothing was created or destroyed: the four parties always sum to the mint.
fn assert_conserved(s: &Setup) {
    let sum = s.token.balance(&s.client)
        + s.token.balance(&s.freelancer)
        + s.token.balance(&s.treasury)
        + s.token.balance(&s.escrow_id);
    assert_eq!(sum, MINT, "tokens must be conserved across all four parties");
}

// ---------------------------------------------------------------------------
// Pre-existing behaviour. Only `full_happy_path` needed updating, for the
// `release` event payload.
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
    assert_eq!(s.token.balance(&s.client), MINT - 300);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);
    s.escrow.release_milestone(&id, &0);
    s.escrow.release_milestone(&id, &1);
    // No fee configured, so the freelancer receives the full amount.
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
    assert_eq!(s.token.balance(&s.client), MINT - 400 + 300);

    let id2 = funded(&s, &[50]);
    let before = s.token.balance(&s.client);
    s.escrow.cancel_contract(&id2);
    assert_eq!(s.token.balance(&s.client), before + 50);
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
// Defaults
// ---------------------------------------------------------------------------

#[test]
fn the_default_is_no_fee() {
    let s = setup();
    assert_eq!(s.escrow.get_fee_config(), (0, None));

    let id = funded(&s, &[100]);
    assert_eq!(s.escrow.get_escrow_fee(&id), 0);

    s.escrow.release_milestone(&id, &0);
    assert_eq!(s.token.balance(&s.freelancer), 100, "full amount, no fee");
    assert_eq!(s.token.balance(&s.treasury), 0);
    assert_conserved(&s);
}

#[test]
fn an_escrow_created_before_a_fee_is_set_keeps_paying_none() {
    let s = setup();
    let id = funded(&s, &[100]);

    // Turn fees on afterwards.
    s.escrow.set_fee(&100, &s.treasury);
    assert_eq!(s.escrow.get_escrow_fee(&id), 0, "snapshot stays at 0");

    s.escrow.release_milestone(&id, &0);
    assert_eq!(s.token.balance(&s.freelancer), 100);
    assert_eq!(s.token.balance(&s.treasury), 0);
    assert_conserved(&s);
}

// ---------------------------------------------------------------------------
// set_fee
// ---------------------------------------------------------------------------

#[test]
fn set_fee_stores_the_rate_and_treasury() {
    let s = setup();
    s.escrow.set_fee(&250, &s.treasury);
    assert_eq!(
        s.escrow.get_fee_config(),
        (250, Some(s.treasury.clone()))
    );
}

#[test]
fn set_fee_emits_the_event() {
    let s = setup();
    s.escrow.set_fee(&250, &s.treasury);

    let events = s.env.events().all();
    let (contract, topics, data) = events.get(events.len() - 1).unwrap();
    assert_eq!(contract, s.escrow_id);
    let name: Symbol = topics.get(0).unwrap().into_val(&s.env);
    assert_eq!(name, symbol_short!("fee_set"));
    let payload: Vec<soroban_sdk::Val> = data.into_val(&s.env);
    let bps: u32 = payload.get(0).unwrap().into_val(&s.env);
    let dest: Address = payload.get(1).unwrap().into_val(&s.env);
    assert_eq!(bps, 250);
    assert_eq!(dest, s.treasury);
}

#[test]
fn set_fee_accepts_the_cap_and_rejects_anything_above() {
    let s = setup();
    assert!(s.escrow.try_set_fee(&MAX_FEE_BPS, &s.treasury).is_ok());
    assert_eq!(s.escrow.get_fee_config().0, MAX_FEE_BPS);

    for bad in [MAX_FEE_BPS + 1, 1_000, BPS_DENOMINATOR as u32, u32::MAX] {
        assert_eq!(
            s.escrow.try_set_fee(&bad, &s.treasury),
            Err(Ok(EscrowError::FeeTooHigh)),
            "fee {bad} must be rejected",
        );
    }
    // The rejected calls did not change the stored rate.
    assert_eq!(s.escrow.get_fee_config().0, MAX_FEE_BPS);
}

#[test]
fn set_fee_rejects_a_non_admin() {
    let s = setup();
    s.env.mock_auths(&[MockAuth {
        address: &s.client,
        invoke: &MockAuthInvoke {
            contract: &s.escrow_id,
            fn_name: "set_fee",
            args: (100u32, s.treasury.clone()).into_val(&s.env),
            sub_invokes: &[],
        },
    }]);
    assert!(s.escrow.try_set_fee(&100, &s.treasury).is_err());
    assert_eq!(s.escrow.get_fee_config(), (0, None));
}

/// `set_fee` writes both keys together, so a non-zero rate always has a
/// destination.
#[test]
fn a_nonzero_fee_always_has_a_treasury() {
    let s = setup();
    s.escrow.set_fee(&100, &s.treasury);
    let (bps, dest) = s.escrow.get_fee_config();
    assert_eq!(bps, 100);
    assert!(dest.is_some());

    // Turning fees off is allowed, and leaves the treasury recorded.
    s.escrow.set_fee(&0, &s.treasury);
    assert_eq!(s.escrow.get_fee_config().0, 0);
}

// ---------------------------------------------------------------------------
// The snapshot: the point of the whole feature
// ---------------------------------------------------------------------------

/// The headline case from the issue. An escrow agreed at 100 bps keeps paying
/// 100 bps after the admin raises the global rate to 300 bps.
#[test]
fn raising_the_fee_does_not_change_an_existing_escrow() {
    let s = setup();
    s.escrow.set_fee(&100, &s.treasury);

    // Agreed and funded at 100 bps.
    let id = funded(&s, &[1_000]);
    assert_eq!(s.escrow.get_escrow_fee(&id), 100);

    // The admin raises the global rate.
    s.escrow.set_fee(&300, &s.treasury);
    assert_eq!(s.escrow.get_fee_config().0, 300);
    assert_eq!(
        s.escrow.get_escrow_fee(&id),
        100,
        "the escrow keeps its snapshotted rate",
    );

    s.escrow.release_milestone(&id, &0);

    // 1_000 at 100 bps = 10 to the treasury, 990 to the freelancer. Not 30/970.
    assert_eq!(s.token.balance(&s.treasury), 10);
    assert_eq!(s.token.balance(&s.freelancer), 990);
    assert_custody(&s, id);
    assert_conserved(&s);

    // A new escrow picks up the new rate.
    let id2 = funded(&s, &[1_000]);
    assert_eq!(s.escrow.get_escrow_fee(&id2), 300);
    s.escrow.release_milestone(&id2, &0);
    assert_eq!(s.token.balance(&s.treasury), 10 + 30);
    assert_eq!(s.token.balance(&s.freelancer), 990 + 970);
    assert_conserved(&s);
}

/// Lowering the rate is likewise not retroactive.
#[test]
fn lowering_the_fee_does_not_change_an_existing_escrow() {
    let s = setup();
    s.escrow.set_fee(&500, &s.treasury);
    let id = funded(&s, &[1_000]);

    s.escrow.set_fee(&0, &s.treasury);
    s.escrow.release_milestone(&id, &0);

    // Still charged at the rate it was created under.
    assert_eq!(s.token.balance(&s.treasury), 50);
    assert_eq!(s.token.balance(&s.freelancer), 950);
    assert_conserved(&s);
}

// ---------------------------------------------------------------------------
// Fee arithmetic
// ---------------------------------------------------------------------------

/// Rounding is down, and a fee that rounds to zero charges nothing: the
/// freelancer receives the whole unit.
#[test]
fn a_fee_that_rounds_to_zero_charges_nothing() {
    let s = setup();
    s.escrow.set_fee(&100, &s.treasury); // 1%

    // 1 unit at 1% is 0.01, which floors to 0.
    let id = funded(&s, &[1]);
    s.escrow.release_milestone(&id, &0);

    assert_eq!(s.token.balance(&s.freelancer), 1);
    assert_eq!(s.token.balance(&s.treasury), 0);
    assert_custody(&s, id);
    assert_conserved(&s);
}

/// A release below the rounding threshold pays no fee, and one at it does.
#[test]
fn the_rounding_boundary_is_exact() {
    let s = setup();
    s.escrow.set_fee(&100, &s.treasury); // 1% -> 1 unit of fee per 100

    // 99 at 1% = 0.99, floors to 0, so the freelancer keeps all 99.
    let a = funded(&s, &[99]);
    s.escrow.release_milestone(&a, &0);
    assert_eq!(s.token.balance(&s.treasury), 0);
    assert_eq!(s.token.balance(&s.freelancer), 99);

    // 100 at 1% = 1 exactly, so 1 goes to the treasury and 99 to the freelancer.
    let b = funded(&s, &[100]);
    s.escrow.release_milestone(&b, &0);
    assert_eq!(s.token.balance(&s.treasury), 1);
    assert_eq!(s.token.balance(&s.freelancer), 99 + 99);
    assert_conserved(&s);
}

/// The fee is taken per release, so a multi-milestone escrow charges on each.
#[test]
fn the_fee_is_charged_on_every_release() {
    let s = setup();
    s.escrow.set_fee(&200, &s.treasury); // 2%
    let id = funded(&s, &[1_000, 500]);

    s.escrow.release_milestone(&id, &0);
    assert_eq!(s.token.balance(&s.treasury), 20);
    s.escrow.release_milestone(&id, &1);
    // 500 at 2% = 10.
    assert_eq!(s.token.balance(&s.treasury), 30);
    assert_eq!(s.token.balance(&s.freelancer), 980 + 490);
    assert_custody(&s, id);
    assert_conserved(&s);
}

/// A large milestone amount must not overflow the fee calculation. The direct
/// `amount * bps` form would trap here, since `i128::MAX / 2 * 500` is far past
/// `i128::MAX`.
#[test]
fn a_large_amount_does_not_overflow_the_fee() {
    let s = setup();
    s.escrow.set_fee(&MAX_FEE_BPS, &s.treasury);

    let big: i128 = i128::MAX / 2;
    StellarAssetClient::new(&s.env, &s.token.address).mint(&s.client, &big);
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &amounts(&s.env, &[big]),
    );
    s.escrow.deposit_funds(&id, &big);
    s.escrow.release_milestone(&id, &0);

    // 500 bps of i128::MAX/2, computed exactly.
    let expected_fee = big / 20;
    assert_eq!(s.token.balance(&s.treasury), expected_fee);
    assert_eq!(s.token.balance(&s.freelancer), big - expected_fee);
    assert_custody(&s, id);
}

/// The fee can never exceed the amount it is taken from, at any rate.
#[test]
fn the_fee_never_exceeds_the_amount() {
    let s = setup();
    s.escrow.set_fee(&MAX_FEE_BPS, &s.treasury);

    // A spread of amounts, including values where the divide-first form and the
    // naive form could disagree.
    let cases = [1i128, 2, 19, 20, 21, 99, 100, 10_000, 10_001, 999_999];
    let total: i128 = cases.iter().sum();
    // The default mint cannot cover all of these, so top it up.
    StellarAssetClient::new(&s.env, &s.token.address).mint(&s.client, &total);

    for amount in cases {
        let id = funded(&s, &[amount]);
        s.escrow.release_milestone(&id, &0);
        let c = s.escrow.get_contract(&id);
        assert_eq!(c.released_amount, amount);
        // Whatever happened, the escrow is empty and nothing is negative.
        assert_eq!(s.token.balance(&s.escrow_id), 0);
        assert!(s.token.balance(&s.freelancer) >= 0);
        assert!(s.token.balance(&s.treasury) >= 0);
        assert!(s.token.balance(&s.treasury) <= amount);
    }
    // Everything the client put in came back out to someone.
    assert_eq!(
        s.token.balance(&s.freelancer) + s.token.balance(&s.treasury),
        total,
    );
}

// ---------------------------------------------------------------------------
// Dispute and cancel interactions
// ---------------------------------------------------------------------------

/// The fee applies to the freelancer's share only. The client's refund is its
/// own money coming back, so it is not charged.
#[test]
fn a_dispute_charges_the_fee_only_on_the_freelancer_share() {
    let s = setup();
    s.escrow.set_fee(&500, &s.treasury); // 5%
    let id = funded(&s, &[1_000]);
    let client_before = s.token.balance(&s.client);

    s.escrow.open_dispute(&id, &s.freelancer);
    s.escrow.resolve_dispute(&id, &5_000);

    // 1_000 split 50/50 is 500 each. The fee is 5% of the freelancer's 500 = 25.
    assert_eq!(s.token.balance(&s.freelancer), 475);
    assert_eq!(s.token.balance(&s.client), client_before + 500);
    assert_eq!(s.token.balance(&s.treasury), 25);
    assert_custody(&s, id);
    assert_conserved(&s);
}

/// A split that pays the freelancer nothing charges no fee.
#[test]
fn a_dispute_paying_the_freelancer_nothing_charges_no_fee() {
    let s = setup();
    s.escrow.set_fee(&500, &s.treasury);
    let id = funded(&s, &[1_000]);
    let client_before = s.token.balance(&s.client);

    s.escrow.open_dispute(&id, &s.client);
    s.escrow.resolve_dispute(&id, &0);

    assert_eq!(s.token.balance(&s.freelancer), 0);
    assert_eq!(s.token.balance(&s.client), client_before + 1_000);
    assert_eq!(s.token.balance(&s.treasury), 0, "nothing to charge a fee on");
    assert_conserved(&s);
}

/// A cancellation is not a release, so it charges nothing.
#[test]
fn a_cancellation_charges_no_fee() {
    let s = setup();
    s.escrow.set_fee(&500, &s.treasury);
    let id = funded(&s, &[1_000]);
    let client_before = s.token.balance(&s.client);

    s.escrow.cancel_contract(&id);

    assert_eq!(s.token.balance(&s.client), client_before + 1_000);
    assert_eq!(s.token.balance(&s.treasury), 0);
    assert_eq!(s.token.balance(&s.freelancer), 0);
    assert_custody(&s, id);
    assert_conserved(&s);
}

/// The whole-escrow dispute and the milestone release both charge the escrow's
/// snapshotted rate, not the current global one.
#[test]
fn both_payout_paths_use_the_snapshotted_rate() {
    let s = setup();
    s.escrow.set_fee(&100, &s.treasury);

    let released = funded(&s, &[1_000]);
    let disputed = funded(&s, &[1_000]);

    // Raise the rate after both escrows exist.
    s.escrow.set_fee(&500, &s.treasury);

    s.escrow.release_milestone(&released, &0);
    s.escrow.open_dispute(&disputed, &s.client);
    s.escrow.resolve_dispute(&disputed, &10_000);

    // Both charged 100 bps: 10 from the release, 10 from the disputed share.
    assert_eq!(s.token.balance(&s.treasury), 20);
    assert_eq!(s.token.balance(&s.freelancer), 990 + 990);
    assert_conserved(&s);
}

// ---------------------------------------------------------------------------
// Event payloads
// ---------------------------------------------------------------------------

#[test]
fn the_release_event_reports_the_fee() {
    let s = setup();
    s.escrow.set_fee(&100, &s.treasury);
    let id = funded(&s, &[1_000]);
    s.escrow.release_milestone(&id, &0);

    let events = s.env.events().all();
    let (contract, topics, data) = events.get(events.len() - 1).unwrap();
    assert_eq!(contract, s.escrow_id);
    let name: Symbol = topics.get(0).unwrap().into_val(&s.env);
    let event_id: u64 = topics.get(1).unwrap().into_val(&s.env);
    assert_eq!(name, symbol_short!("release"));
    assert_eq!(event_id, id);
    let payload: Vec<soroban_sdk::Val> = data.into_val(&s.env);
    let index: u32 = payload.get(0).unwrap().into_val(&s.env);
    let amount: i128 = payload.get(1).unwrap().into_val(&s.env);
    let fee: i128 = payload.get(2).unwrap().into_val(&s.env);
    assert_eq!(index, 0);
    assert_eq!(amount, 1_000);
    assert_eq!(fee, 10);
}

#[test]
fn the_resolved_event_reports_the_fee() {
    let s = setup();
    s.escrow.set_fee(&500, &s.treasury);
    let id = funded(&s, &[1_000]);
    s.escrow.open_dispute(&id, &s.client);
    s.escrow.resolve_dispute(&id, &5_000);

    let events = s.env.events().all();
    let (_, topics, data) = events.get(events.len() - 1).unwrap();
    let name: Symbol = topics.get(0).unwrap().into_val(&s.env);
    assert_eq!(name, symbol_short!("resolved"));
    let payload: Vec<soroban_sdk::Val> = data.into_val(&s.env);
    let to_freelancer: i128 = payload.get(0).unwrap().into_val(&s.env);
    let to_client: i128 = payload.get(1).unwrap().into_val(&s.env);
    let fee: i128 = payload.get(2).unwrap().into_val(&s.env);
    assert_eq!(to_freelancer, 500);
    assert_eq!(to_client, 500);
    assert_eq!(fee, 25);
}

// ---------------------------------------------------------------------------
// A fee that rounds to zero pays no treasury transfer at all
// ---------------------------------------------------------------------------

/// When the fee is zero there is no treasury transfer, which matters because a
/// zero-value token transfer would be a wasted host call and some token
/// contracts reject it.
#[test]
fn a_zero_fee_makes_no_treasury_transfer() {
    let s = setup();
    // Fee configured but rounding to zero on this amount.
    s.escrow.set_fee(&100, &s.treasury);
    let id = funded(&s, &[1]);

    s.escrow.release_milestone(&id, &0);

    assert_eq!(s.token.balance(&s.treasury), 0);
    assert_eq!(s.token.balance(&s.freelancer), 1);
    // The escrow is empty: the full amount left, all to the freelancer.
    assert_eq!(s.token.balance(&s.escrow_id), 0);
}
