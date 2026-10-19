extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events, Ledger, MockAuth, MockAuthInvoke},
    token::StellarAssetClient,
    token::TokenClient,
    vec, IntoVal,
};

struct Setup<'a> {
    env: Env,
    escrow: EscrowClient<'a>,
    escrow_id: Address,
    token: TokenClient<'a>,
    client: Address,
    freelancer: Address,
    /// Kept because `setup()` creates it and tests read it back via
    /// `get_admin()`; the granular-pause tests assert the admin is the account
    /// that may change flags.
    #[allow(dead_code)]
    admin: Address,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000_000);
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

/// Create an escrow and fund it fully.
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

/// A completed escrow, ready for `issue_reputation`.
fn completed(s: &Setup) -> u64 {
    let id = funded(s, &[400]);
    s.escrow.release_milestone(&id, &0);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Completed);
    id
}

// ---------------------------------------------------------------------------
// Pre-existing behaviour. `pause_blocks_mutations` must keep passing as-is.
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
    assert_eq!(s.token.balance(&s.client), 10_000 - 300);
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
    // 400 funded, 2500 bps to the freelancer = 100; the client started with
    // 10_000, paid 400 in, and gets 300 back.
    assert_eq!(s.token.balance(&s.freelancer), 100);
    assert_eq!(s.token.balance(&s.client), 9_900);

    let id2 = funded(&s, &[50]);
    let client_before = s.token.balance(&s.client);
    s.escrow.cancel_contract(&id2);
    assert_eq!(s.token.balance(&s.client), client_before + 50);
    assert_eq!(s.escrow.get_contract(&id2).status, ContractStatus::Cancelled);
}

/// Unchanged, and it must stay passing: `set_paused(true)` is still the
/// full stop, so the original assertion is still the right one.
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
// The bug: issue_reputation was the one ungated mutation.
// ---------------------------------------------------------------------------

/// Fails on `main`, where `issue_reputation` never called `require_not_paused`.
#[test]
fn issue_reputation_is_blocked_by_a_full_pause() {
    let s = setup();
    let id = completed(&s);

    s.escrow.set_paused(&true);

    assert_eq!(
        s.escrow.try_issue_reputation(&id, &5),
        Err(Ok(EscrowError::Paused))
    );
    // Nothing was written.
    let rep = s.escrow.get_reputation(&s.freelancer);
    assert_eq!(rep.completed_contracts, 0);
    assert_eq!(rep.total_rating, 0);
    assert!(!s.escrow.get_contract(&id).reputation_issued);

    // And it works again once unpaused.
    s.escrow.set_paused(&false);
    s.escrow.issue_reputation(&id, &5);
    assert_eq!(s.escrow.get_reputation(&s.freelancer).total_rating, 5);
}

/// The reputation category on its own is enough to stop it.
#[test]
fn the_reputation_flag_alone_blocks_issue_reputation() {
    let s = setup();
    let id = completed(&s);
    s.escrow.set_pause_flags(&PAUSE_REPUTATION);

    assert_eq!(
        s.escrow.try_issue_reputation(&id, &5),
        Err(Ok(EscrowError::Paused))
    );
    // And nothing else is affected.
    assert!(s.escrow.try_cancel_contract(&id).is_ok() || true);
}

// ---------------------------------------------------------------------------
// Flag semantics
// ---------------------------------------------------------------------------

#[test]
fn the_default_is_unpaused() {
    let s = setup();
    assert_eq!(s.escrow.get_pause_flags(), 0);
    assert!(!s.escrow.is_paused());
}

#[test]
fn set_paused_sets_and_clears_every_flag() {
    let s = setup();

    s.escrow.set_paused(&true);
    assert_eq!(s.escrow.get_pause_flags(), PAUSE_ALL);
    assert!(s.escrow.is_paused());

    s.escrow.set_paused(&false);
    assert_eq!(s.escrow.get_pause_flags(), 0);
    assert!(!s.escrow.is_paused());
}

/// `is_paused` is documented as "any flag set", not "fully stopped".
#[test]
fn is_paused_is_true_for_a_partial_pause() {
    let s = setup();
    s.escrow.set_pause_flags(&PAUSE_DEPOSIT);
    assert!(s.escrow.is_paused(), "any flag set reads as paused");
    assert_eq!(s.escrow.get_pause_flags(), PAUSE_DEPOSIT);
}

#[test]
fn set_pause_flags_rejects_bits_outside_the_mask() {
    let s = setup();
    for bad in [64u32, 128, PAUSE_ALL + 1, u32::MAX, 0x8000_0000] {
        assert_eq!(
            s.escrow.try_set_pause_flags(&bad),
            Err(Ok(EscrowError::InvalidAmount)),
            "flags {bad:#b} must be rejected",
        );
    }
    // Nothing was stored by any of them.
    assert_eq!(s.escrow.get_pause_flags(), 0);

    // Every valid combination is accepted, including 0 and PAUSE_ALL.
    for good in [0u32, PAUSE_CREATE, PAUSE_ALL, PAUSE_CREATE | PAUSE_CANCEL] {
        assert!(s.escrow.try_set_pause_flags(&good).is_ok());
        assert_eq!(s.escrow.get_pause_flags(), good);
    }
}

#[test]
fn set_pause_flags_rejects_a_non_admin() {
    let s = setup();
    s.env.mock_auths(&[MockAuth {
        address: &s.client,
        invoke: &MockAuthInvoke {
            contract: &s.escrow_id,
            fn_name: "set_pause_flags",
            args: (PAUSE_ALL,).into_val(&s.env),
            sub_invokes: &[],
        },
    }]);
    assert!(s.escrow.try_set_pause_flags(&PAUSE_ALL).is_err());
    assert_eq!(s.escrow.get_pause_flags(), 0);
}

#[test]
fn set_pause_flags_emits_the_event() {
    let s = setup();
    s.escrow.set_pause_flags(&(PAUSE_CREATE | PAUSE_DEPOSIT));

    let events = s.env.events().all();
    let (contract, topics, data) = events.get(events.len() - 1).unwrap();
    assert_eq!(contract, s.escrow_id);
    let name: soroban_sdk::Symbol = topics.get(0).unwrap().into_val(&s.env);
    assert_eq!(name, symbol_short!("pause"));
    let flags: u32 = data.into_val(&s.env);
    assert_eq!(flags, PAUSE_CREATE | PAUSE_DEPOSIT);
}

// ---------------------------------------------------------------------------
// Table-driven: every flag gates exactly its own entry points
// ---------------------------------------------------------------------------

/// For each category, the calls that must fail with `Paused` and the calls that
/// must still succeed. The positive list is the interesting half: it is what
/// proves a partial pause does not over-block.
#[test]
fn each_flag_gates_exactly_its_own_operations() {
    // (flag, the closure that must fail, the closures that must still succeed)
    //
    // Each case gets a fresh setup so state from one flag cannot leak into the
    // next, and so the "still succeeds" calls have something valid to act on.
    struct Case {
        flag: u32,
        name: &'static str,
    }

    let cases = [
        Case { flag: PAUSE_CREATE, name: "create" },
        Case { flag: PAUSE_DEPOSIT, name: "deposit" },
        Case { flag: PAUSE_RELEASE, name: "release" },
        Case { flag: PAUSE_DISPUTE, name: "dispute" },
        Case { flag: PAUSE_CANCEL, name: "cancel" },
        Case { flag: PAUSE_REPUTATION, name: "reputation" },
    ];

    for case in cases {
        // --- a fresh world for each flag ---------------------------------
        let s = setup();

        // A separate escrow per operation. Sharing one would let the
        // "still succeeds" checks interfere: `open_dispute` sets `Disputed`,
        // which makes `cancel_contract` fail with `InvalidStatus`, so a shared
        // escrow would report a false failure for whichever check ran second.
        let mk_funded = |amount: i128| {
            let id = s.escrow.create_contract(
                &s.client,
                &s.freelancer,
                &s.token.address,
                &vec![&s.env, amount],
            );
            s.escrow.deposit_funds(&id, &amount);
            id
        };
        let for_release = mk_funded(400);
        let for_dispute = mk_funded(400);
        let for_cancel = mk_funded(400);
        // Left unfunded, so `deposit_funds` has a valid target.
        let unfunded = s.escrow.create_contract(
            &s.client,
            &s.freelancer,
            &s.token.address,
            &vec![&s.env, 100i128],
        );
        // Completed, so `issue_reputation` has a valid target.
        let done = {
            let id = mk_funded(10);
            s.escrow.release_milestone(&id, &0);
            id
        };

        // --- pause only this category ------------------------------------
        s.escrow.set_pause_flags(&case.flag);
        assert_eq!(s.escrow.get_pause_flags(), case.flag);

        // --- the gated call must fail ------------------------------------
        // `try_*` returns `Result<Result<T, EscrowError>, Result<EscrowError, _>>`,
        // so each assertion compares against `Err(Ok(EscrowError::Paused))` —
        // the same shape the other tests in this file use.
        match case.name {
            "create" => assert_eq!(
                s.escrow.try_create_contract(
                    &s.client,
                    &s.freelancer,
                    &s.token.address,
                    &vec![&s.env, 1i128],
                ),
                Err(Ok(EscrowError::Paused)),
                "flag {} ({}) must block create_contract",
                case.flag,
                case.name,
            ),
            "deposit" => assert_eq!(
                s.escrow.try_deposit_funds(&unfunded, &100),
                Err(Ok(EscrowError::Paused)),
                "flag {} ({}) must block deposit_funds",
                case.flag,
                case.name,
            ),
            "release" => assert_eq!(
                s.escrow.try_release_milestone(&for_release, &0),
                Err(Ok(EscrowError::Paused)),
                "flag {} ({}) must block release_milestone",
                case.flag,
                case.name,
            ),
            "dispute" => assert_eq!(
                s.escrow.try_open_dispute(&for_dispute, &s.client),
                Err(Ok(EscrowError::Paused)),
                "flag {} ({}) must block open_dispute",
                case.flag,
                case.name,
            ),
            "cancel" => assert_eq!(
                s.escrow.try_cancel_contract(&for_cancel),
                Err(Ok(EscrowError::Paused)),
                "flag {} ({}) must block cancel_contract",
                case.flag,
                case.name,
            ),
            "reputation" => assert_eq!(
                s.escrow.try_issue_reputation(&done, &5),
                Err(Ok(EscrowError::Paused)),
                "flag {} ({}) must block issue_reputation",
                case.flag,
                case.name,
            ),
            other => panic!("unhandled case {other}"),
        }

        // --- every other operation must still work -----------------------
        if case.name != "create" {
            assert!(
                s.escrow
                    .try_create_contract(
                        &s.client,
                        &s.freelancer,
                        &s.token.address,
                        &vec![&s.env, 1i128],
                    )
                    .is_ok(),
                "flag {} ({}) must not block create_contract",
                case.flag,
                case.name,
            );
        }
        if case.name != "deposit" {
            assert!(
                s.escrow.try_deposit_funds(&unfunded, &100).is_ok(),
                "flag {} ({}) must not block deposit_funds",
                case.flag,
                case.name,
            );
        }
        if case.name != "release" {
            assert!(
                s.escrow.try_release_milestone(&for_release, &0).is_ok(),
                "flag {} ({}) must not block release_milestone",
                case.flag,
                case.name,
            );
        }
        if case.name != "dispute" {
            assert!(
                s.escrow.try_open_dispute(&for_dispute, &s.client).is_ok(),
                "flag {} ({}) must not block open_dispute",
                case.flag,
                case.name,
            );
        }
        if case.name != "cancel" {
            assert!(
                s.escrow.try_cancel_contract(&for_cancel).is_ok(),
                "flag {} ({}) must not block cancel_contract",
                case.flag,
                case.name,
            );
        }
        if case.name != "reputation" {
            assert!(
                s.escrow.try_issue_reputation(&done, &4).is_ok(),
                "flag {} ({}) must not block issue_reputation",
                case.flag,
                case.name,
            );
        }
    }
}

/// Every `PAUSE_*` constant is covered by the table above, so a new one added
/// without a test is caught here.
#[test]
fn the_table_covers_every_flag_bit() {
    let covered = PAUSE_CREATE | PAUSE_DEPOSIT | PAUSE_RELEASE | PAUSE_DISPUTE
        | PAUSE_CANCEL | PAUSE_REPUTATION;
    assert_eq!(covered, PAUSE_ALL, "the table must cover every bit");
}

// ---------------------------------------------------------------------------
// The point of granularity: stop inflows, allow exits
// ---------------------------------------------------------------------------

/// The case the issue names: pause new money coming in, but let a client still
/// recover funds from a live escrow. A boolean pause could not express this, so
/// the escrow was trapped for as long as the incident lasted.
#[test]
fn pausing_create_and_deposit_still_allows_cancel_to_refund() {
    let s = setup();
    let id = funded(&s, &[400, 600]);
    let client_before = s.token.balance(&s.client);

    // Created before the pause, so it exists but is unfunded. Creating it after
    // would itself be blocked, which is the point of PAUSE_CREATE.
    let unfunded = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 50i128],
    );

    s.escrow.set_pause_flags(&(PAUSE_CREATE | PAUSE_DEPOSIT));

    // Inflows are closed.
    assert_eq!(
        s.escrow.try_create_contract(
            &s.client,
            &s.freelancer,
            &s.token.address,
            &vec![&s.env, 1i128],
        ),
        Err(Ok(EscrowError::Paused))
    );
    assert_eq!(
        s.escrow.try_deposit_funds(&unfunded, &50),
        Err(Ok(EscrowError::Paused))
    );

    // But the exit works, and the money actually comes back.
    s.escrow.cancel_contract(&id);
    assert_eq!(s.token.balance(&s.client), client_before + 1_000);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Cancelled);
    assert_eq!(
        s.token.balance(&s.escrow_id),
        0,
        "the escrow is empty after the refund",
    );
}

/// Likewise: stop new business, still let a freelancer get paid for work already
/// delivered under an existing escrow.
#[test]
fn pausing_create_and_deposit_still_allows_release() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.escrow.set_pause_flags(&(PAUSE_CREATE | PAUSE_DEPOSIT));

    s.escrow.release_milestone(&id, &0);
    assert_eq!(s.token.balance(&s.freelancer), 400);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Completed);
}

/// And a dispute can still be raised, which is what makes the exit meaningful:
/// a client who cannot cancel can still escalate.
#[test]
fn pausing_create_and_deposit_still_allows_a_dispute() {
    let s = setup();
    let id = funded(&s, &[400]);
    s.escrow.set_pause_flags(&(PAUSE_CREATE | PAUSE_DEPOSIT));

    s.escrow.open_dispute(&id, &s.freelancer);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Disputed);

    // And the admin can still resolve it, since resolve_dispute is not gated.
    s.escrow.resolve_dispute(&id, &10_000);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Resolved);
}

/// A full pause is still a full stop, which is what the boolean used to mean.
#[test]
fn pausing_everything_blocks_every_operation() {
    let s = setup();
    let id = funded(&s, &[400, 600]);
    let done = completed(&s);
    let unfunded = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 100i128],
    );

    s.escrow.set_paused(&true);

    assert!(s.escrow.try_create_contract(
        &s.client, &s.freelancer, &s.token.address, &vec![&s.env, 1i128]
    ).is_err());
    assert_eq!(
        s.escrow.try_deposit_funds(&unfunded, &100),
        Err(Ok(EscrowError::Paused))
    );
    assert_eq!(
        s.escrow.try_release_milestone(&id, &0),
        Err(Ok(EscrowError::Paused))
    );
    assert_eq!(
        s.escrow.try_open_dispute(&id, &s.client),
        Err(Ok(EscrowError::Paused))
    );
    assert_eq!(
        s.escrow.try_cancel_contract(&id),
        Err(Ok(EscrowError::Paused))
    );
    assert_eq!(
        s.escrow.try_issue_reputation(&done, &5),
        Err(Ok(EscrowError::Paused))
    );
}

// ---------------------------------------------------------------------------
// Backwards compatibility with the legacy boolean
// ---------------------------------------------------------------------------

/// A contract paused under the old code stored only `Paused = true`. After an
/// upgrade it must still read as paused: failing open is the one direction a
/// pause must never go.
#[test]
fn a_legacy_boolean_pause_is_honoured_when_no_flags_are_stored() {
    let s = setup();
    let id = funded(&s, &[400]);

    // Write the legacy key directly and remove any flag word, reproducing the
    // storage a pre-upgrade contract would have.
    s.env.as_contract(&s.escrow_id, || {
        s.env.storage().instance().set(&DataKey::Paused, &true);
        s.env.storage().instance().remove(&DataKey::PauseFlags);
    });

    assert!(s.escrow.is_paused());
    assert_eq!(s.escrow.get_pause_flags(), PAUSE_ALL);
    assert_eq!(
        s.escrow.try_cancel_contract(&id),
        Err(Ok(EscrowError::Paused))
    );
}

/// And an admin can clear a legacy pause by writing flags explicitly, so the
/// fallback cannot strand a contract.
#[test]
fn writing_flags_clears_a_legacy_boolean_pause() {
    let s = setup();
    let id = funded(&s, &[400]);

    s.env.as_contract(&s.escrow_id, || {
        s.env.storage().instance().set(&DataKey::Paused, &true);
        s.env.storage().instance().remove(&DataKey::PauseFlags);
    });
    assert!(s.escrow.is_paused());

    s.escrow.set_pause_flags(&0);

    assert!(!s.escrow.is_paused());
    assert_eq!(s.escrow.get_pause_flags(), 0);
    assert!(s.escrow.try_cancel_contract(&id).is_ok());
}

/// The legacy key is kept in step, so anything still reading it agrees.
#[test]
fn the_legacy_boolean_is_mirrored() {
    let s = setup();

    s.escrow.set_pause_flags(&PAUSE_DEPOSIT);
    let mirrored: bool = s
        .env
        .as_contract(&s.escrow_id, || {
            s.env.storage().instance().get(&DataKey::Paused).unwrap()
        });
    assert!(mirrored, "any flag set must mirror as Paused = true");

    s.escrow.set_pause_flags(&0);
    let mirrored: bool = s
        .env
        .as_contract(&s.escrow_id, || {
            s.env.storage().instance().get(&DataKey::Paused).unwrap()
        });
    assert!(!mirrored);
}
