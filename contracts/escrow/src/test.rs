extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events, Ledger},
    token::StellarAssetClient,
    token::TokenClient,
    vec, Bytes, IntoVal, Symbol, TryFromVal, Val,
};
use std::path::PathBuf;

struct Setup<'a> {
    env: Env,
    escrow: EscrowClient<'a>,
    escrow_id: Address,
    token: TokenClient<'a>,
    client: Address,
    freelancer: Address,
    admin: Address,
    /// A real uploaded wasm hash, so `execute_upgrade` has something to swap in.
    /// `update_current_contract_wasm` rejects a hash that was never uploaded, so
    /// a made-up 32-byte value would only fail at execution time.
    fixture_hash: BytesN<32>,
    /// The escrow's own wasm, uploaded. Upgrading to it keeps the interface, so
    /// it is what the repeated-upgrade tests use.
    escrow_hash: BytesN<32>,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000_000);
    let admin = Address::generate(&env);
    let client = Address::generate(&env);
    let freelancer = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    StellarAssetClient::new(&env, &sac.address()).mint(&client, &1_000);
    let id = env.register(Escrow, ());
    let escrow = EscrowClient::new(&env, &id);
    escrow.initialize(&admin);
    let token = TokenClient::new(&env, &sac.address());
    let fixture_hash = env
        .deployer()
        .upload_contract_wasm(Bytes::from_slice(&env, &release_wasm("escrow_fixture.wasm")));
    let escrow_hash = env
        .deployer()
        .upload_contract_wasm(Bytes::from_slice(&env, &release_wasm("escrow.wasm")));
    Setup {
        env,
        escrow,
        escrow_id: id,
        token,
        client,
        freelancer,
        admin,
        fixture_hash,
        escrow_hash,
    }
}

fn at(s: &Setup, t: u64) {
    s.env.ledger().set_timestamp(t);
}

/// Read a wasm the workspace's release build produced.
///
/// Resolved from `CARGO_MANIFEST_DIR` rather than the process CWD, so it works
/// whether the test is run from the workspace root or the crate directory.
/// Missing files are a hard failure with the command that fixes it, not a silent
/// skip: an upgrade test that quietly does nothing would be worse than none.
fn release_wasm(name: &str) -> std::vec::Vec<u8> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let path = root.join("target/wasm32v1-none/release").join(name);
    std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "could not read {}\n  {e}\n\
             build it first:  cargo build --target wasm32v1-none --release",
            path.display()
        )
    })
}

/// The `(topics, data)` of the most recent event.
fn last_event(s: &Setup) -> (Vec<Val>, Val) {
    let events = s.env.events().all();
    let (_, topics, data) = events.get(events.len() - 1).unwrap();
    (topics, data)
}

fn topic<T: IntoVal<Env, Val> + TryFromVal<Env, Val>>(s: &Setup, topics: &Vec<Val>, i: u32) -> T {
    topics.get(i).unwrap().into_val(&s.env)
}

fn payload<T: IntoVal<Env, Val> + TryFromVal<Env, Val>>(s: &Setup, data: &Val, i: u32) -> T {
    let v: Vec<Val> = data.clone().into_val(&s.env);
    v.get(i).unwrap().into_val(&s.env)
}

// ---------------------------------------------------------------------------
// Pre-existing behaviour. No signature changed, so these are untouched.
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
// version
// ---------------------------------------------------------------------------

#[test]
fn version_starts_at_one_and_is_stored_not_hard_coded() {
    let s = setup();
    assert_eq!(s.escrow.version(), INITIAL_VERSION);
    assert_eq!(s.escrow.version(), 1);

    // Seeding storage directly changes what `version` reports, which is how we
    // know it reads rather than returning a literal.
    s.env.as_contract(&s.escrow_id, || {
        s.env.storage().instance().set(&DataKey::Version, &7u32);
    });
    assert_eq!(s.escrow.version(), 7);
}

// ---------------------------------------------------------------------------
// schedule_upgrade
// ---------------------------------------------------------------------------

#[test]
fn schedule_records_the_hash_and_the_eta() {
    let s = setup();
    let hash = BytesN::from_array(&s.env, &[7u8; 32]);
    at(&s, 5_000);

    s.escrow.schedule_upgrade(&hash);

    let pending = s.escrow.get_pending_upgrade().unwrap();
    assert_eq!(pending.wasm_hash, hash);
    assert_eq!(pending.eta, 5_000 + UPGRADE_DELAY_SECS);
}

#[test]
fn schedule_emits_the_event() {
    let s = setup();
    let hash = BytesN::from_array(&s.env, &[7u8; 32]);
    at(&s, 5_000);
    s.escrow.schedule_upgrade(&hash);

    let (topics, data) = last_event(&s);
    assert_eq!(topic::<Symbol>(&s, &topics, 0), symbol_short!("upg_sched"));
    assert_eq!(payload::<BytesN<32>>(&s, &data, 0), hash);
    assert_eq!(payload::<u64>(&s, &data, 1), 5_000 + UPGRADE_DELAY_SECS);
}

#[test]
fn schedule_replaces_a_pending_upgrade_and_restarts_the_clock() {
    let s = setup();
    let first = BytesN::from_array(&s.env, &[1u8; 32]);
    let second = BytesN::from_array(&s.env, &[2u8; 32]);

    at(&s, 1_000);
    s.escrow.schedule_upgrade(&first);
    let first_eta = s.escrow.get_pending_upgrade().unwrap().eta;

    // Re-announcing with a different hash must not inherit the earlier eta:
    // otherwise an admin could announce something harmless, wait out the delay,
    // and swap in a hash users never had time to review.
    at(&s, 1_000 + UPGRADE_DELAY_SECS - 1);
    s.escrow.schedule_upgrade(&second);

    let pending = s.escrow.get_pending_upgrade().unwrap();
    assert_eq!(pending.wasm_hash, second);
    assert_eq!(pending.eta, 1_000 + UPGRADE_DELAY_SECS - 1 + UPGRADE_DELAY_SECS);
    assert!(pending.eta > first_eta);

    // The original eta has passed, but the new one has not.
    at(&s, first_eta);
    assert_eq!(
        s.escrow.try_execute_upgrade(),
        Err(Ok(EscrowError::UpgradeNotReady))
    );
}

#[test]
fn schedule_rejects_a_non_admin() {
    let s = setup();
    let hash = BytesN::from_array(&s.env, &[7u8; 32]);

    // Sign as the client instead of the admin.
    s.env.mock_auths(&[soroban_sdk::testutils::MockAuth {
        address: &s.client,
        invoke: &soroban_sdk::testutils::MockAuthInvoke {
            contract: &s.escrow_id,
            fn_name: "schedule_upgrade",
            args: (&hash,).into_val(&s.env),
            sub_invokes: &[],
        },
    }]);
    assert!(s.escrow.try_schedule_upgrade(&hash).is_err());
    assert_eq!(s.escrow.get_pending_upgrade(), None);
}

// ---------------------------------------------------------------------------
// cancel_upgrade
// ---------------------------------------------------------------------------

#[test]
fn cancel_clears_the_pending_upgrade_and_emits_the_hash() {
    let s = setup();
    s.escrow.schedule_upgrade(&s.fixture_hash);

    s.escrow.cancel_upgrade();

    // Read the event before any other call: `events().all()` reports the most
    // recent invocation only, so an intervening read clears it.
    let (topics, data) = last_event(&s);
    assert_eq!(topic::<Symbol>(&s, &topics, 0), symbol_short!("upg_cncl"));
    // This event's payload is the hash itself, not a tuple.
    let cancelled: BytesN<32> = data.into_val(&s.env);
    assert_eq!(cancelled, s.fixture_hash);

    assert_eq!(s.escrow.get_pending_upgrade(), None);
}

#[test]
fn cancel_without_a_pending_upgrade_is_refused() {
    let s = setup();
    assert_eq!(
        s.escrow.try_cancel_upgrade(),
        Err(Ok(EscrowError::NoPendingUpgrade))
    );
}

#[test]
fn cancel_rejects_a_non_admin() {
    let s = setup();
    let hash = BytesN::from_array(&s.env, &[9u8; 32]);
    s.escrow.schedule_upgrade(&hash);

    s.env.mock_auths(&[soroban_sdk::testutils::MockAuth {
        address: &s.client,
        invoke: &soroban_sdk::testutils::MockAuthInvoke {
            contract: &s.escrow_id,
            fn_name: "cancel_upgrade",
            args: ().into_val(&s.env),
            sub_invokes: &[],
        },
    }]);
    assert!(s.escrow.try_cancel_upgrade().is_err());
    assert!(s.escrow.get_pending_upgrade().is_some(), "still pending");
}

// ---------------------------------------------------------------------------
// execute_upgrade: the delay
// ---------------------------------------------------------------------------

#[test]
fn execute_without_a_pending_upgrade_is_refused() {
    let s = setup();
    assert_eq!(
        s.escrow.try_execute_upgrade(),
        Err(Ok(EscrowError::NoPendingUpgrade))
    );
}

/// The boundary, exactly: one second early fails, the eta itself succeeds.
///
/// Upgrades to the escrow's own wasm, so `version()` keeps meaning "the stored
/// version". Upgrading to the fixture would leave the fixture answering, which
/// is a different test.
#[test]
fn execute_upgrade_opens_exactly_at_the_eta() {
    let s = setup();
    at(&s, 1_000);
    s.escrow.schedule_upgrade(&s.escrow_hash);
    let eta = s.escrow.get_pending_upgrade().unwrap().eta;

    at(&s, eta - 1);
    assert_eq!(
        s.escrow.try_execute_upgrade(),
        Err(Ok(EscrowError::UpgradeNotReady))
    );
    // Still pending, and the version is untouched.
    assert!(s.escrow.get_pending_upgrade().is_some());
    assert_eq!(s.escrow.version(), INITIAL_VERSION);

    at(&s, eta);
    s.escrow.execute_upgrade();
    assert_eq!(s.escrow.get_pending_upgrade(), None);
    assert_eq!(s.escrow.version(), INITIAL_VERSION + 1);
}

#[test]
fn execute_upgrade_is_refused_after_a_cancel() {
    let s = setup();
    let hash = BytesN::from_array(&s.env, &[3u8; 32]);
    at(&s, 1_000);
    s.escrow.schedule_upgrade(&hash);
    let eta = s.escrow.get_pending_upgrade().unwrap().eta;

    s.escrow.cancel_upgrade();

    at(&s, eta + 1);
    assert_eq!(
        s.escrow.try_execute_upgrade(),
        Err(Ok(EscrowError::NoPendingUpgrade))
    );
    assert_eq!(s.escrow.version(), INITIAL_VERSION);
}

#[test]
fn execute_upgrade_rejects_a_non_admin() {
    let s = setup();
    let hash = BytesN::from_array(&s.env, &[3u8; 32]);
    at(&s, 1_000);
    s.escrow.schedule_upgrade(&hash);
    at(&s, 1_000 + UPGRADE_DELAY_SECS);

    s.env.mock_auths(&[soroban_sdk::testutils::MockAuth {
        address: &s.client,
        invoke: &soroban_sdk::testutils::MockAuthInvoke {
            contract: &s.escrow_id,
            fn_name: "execute_upgrade",
            args: ().into_val(&s.env),
            sub_invokes: &[],
        },
    }]);
    assert!(s.escrow.try_execute_upgrade().is_err());
    assert!(s.escrow.get_pending_upgrade().is_some(), "still pending");
    assert_eq!(s.escrow.version(), INITIAL_VERSION);
}

#[test]
fn execute_upgrade_emits_the_event() {
    let s = setup();
    at(&s, 1_000);
    s.escrow.schedule_upgrade(&s.escrow_hash);
    at(&s, 1_000 + UPGRADE_DELAY_SECS);
    s.escrow.execute_upgrade();

    let (topics, data) = last_event(&s);
    assert_eq!(topic::<Symbol>(&s, &topics, 0), symbol_short!("upgraded"));
    assert_eq!(payload::<BytesN<32>>(&s, &data, 0), s.escrow_hash);
    assert_eq!(payload::<u32>(&s, &data, 1), INITIAL_VERSION + 1);
}

/// Successive upgrades keep counting.
///
/// Each upgrade targets the escrow's own wasm, so the contract keeps the same
/// interface and can be upgraded again. Upgrading to the fixture would end the
/// sequence after one step, since the fixture has no `schedule_upgrade`.
#[test]
fn version_increments_on_each_executed_upgrade() {
    let s = setup();
    assert_eq!(s.escrow.version(), INITIAL_VERSION);

    for expected in 2..=4u32 {
        let t = 1_000 * expected as u64;
        at(&s, t);
        s.escrow.schedule_upgrade(&s.escrow_hash);
        at(&s, t + UPGRADE_DELAY_SECS);
        s.escrow.execute_upgrade();
        assert_eq!(s.escrow.version(), expected);
        assert_eq!(s.escrow.get_pending_upgrade(), None);
    }
}

// ---------------------------------------------------------------------------
// execute_upgrade and the pause
// ---------------------------------------------------------------------------

/// Upgrades are deliberately allowed while paused, and this pins that down.
///
/// A pause is a response to a live incident; an upgrade is how a code-level
/// incident gets fixed. Requiring an unpause first would mean unpausing a
/// contract that is paused for a reason.
#[test]
fn execute_upgrade_works_while_paused() {
    let s = setup();
    at(&s, 1_000);
    s.escrow.schedule_upgrade(&s.escrow_hash);

    s.escrow.set_paused(&true);
    at(&s, 1_000 + UPGRADE_DELAY_SECS);

    s.escrow.execute_upgrade();
    assert_eq!(s.escrow.version(), INITIAL_VERSION + 1);
    assert!(s.escrow.is_paused(), "the pause is not lifted by an upgrade");
}

#[test]
fn schedule_and_cancel_work_while_paused() {
    let s = setup();
    let hash = BytesN::from_array(&s.env, &[5u8; 32]);
    s.escrow.set_paused(&true);

    s.escrow.schedule_upgrade(&hash);
    assert!(s.escrow.get_pending_upgrade().is_some());

    s.escrow.cancel_upgrade();
    assert_eq!(s.escrow.get_pending_upgrade(), None);
}

// ---------------------------------------------------------------------------
// Integration: the wasm is actually swapped, and storage survives it
// ---------------------------------------------------------------------------

/// Proves the swap actually replaces the running code.
///
/// Upgrades to the fixture, whose `fixture_marker` exists nowhere else, so the
/// call can only succeed if the code really changed. This is what separates
/// "`execute_upgrade` returned Ok" from "the contract now runs different code".
#[test]
fn upgrade_replaces_the_running_code() {
    let s = setup();
    at(&s, 1_000);
    s.escrow.schedule_upgrade(&s.fixture_hash);
    at(&s, 1_000 + UPGRADE_DELAY_SECS);
    s.escrow.execute_upgrade();

    let fixture = escrow_fixture::EscrowFixtureClient::new(&s.env, &s.escrow_id);
    assert_eq!(fixture.fixture_marker(), escrow_fixture::MARKER);

    // The escrow's own `version` is gone: this address now runs the fixture, and
    // the fixture's `version` answers with its own marker instead.
    assert_eq!(fixture.version(), escrow_fixture::MARKER);
    assert_ne!(fixture.version(), INITIAL_VERSION + 1);
}

/// Proves escrow storage survives a wasm swap, and the escrow stays usable.
///
/// Upgrades to the escrow's own wasm, so the interface is unchanged and the
/// second half of the property can be checked: a live funded escrow, upgraded
/// mid-flight, still reads back its exact state and still pays out.
#[test]
fn upgrade_preserves_escrow_storage_and_leaves_it_usable() {
    let s = setup();

    // A live escrow with money in it, one milestone already paid.
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 400i128, 600i128],
    );
    s.escrow.deposit_funds(&id, &1_000);
    s.escrow.release_milestone(&id, &0);
    let before = s.escrow.get_contract(&id);
    assert_eq!(before.status, ContractStatus::Funded);
    assert_eq!(before.funded_amount, 1_000);
    assert_eq!(before.released_amount, 400);
    assert_eq!(s.token.balance(&s.freelancer), 400);

    // Upgrade to the escrow wasm itself.
    at(&s, 1_000);
    s.escrow.schedule_upgrade(&s.escrow_hash);
    at(&s, 1_000 + UPGRADE_DELAY_SECS);
    s.escrow.execute_upgrade();
    assert_eq!(s.escrow.version(), INITIAL_VERSION + 1);

    // Every field written before the swap is still there, and still correct.
    let after = s.escrow.get_contract(&id);
    assert_eq!(after, before, "escrow storage must survive the upgrade");
    assert_eq!(after.funded_amount, 1_000);
    assert_eq!(after.released_amount, 400);
    assert_eq!(s.token.balance(&s.freelancer), 400);

    // And it is still live: the second milestone releases and pays normally.
    s.escrow.release_milestone(&id, &1);
    assert_eq!(s.token.balance(&s.freelancer), 1_000);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Completed);
}

/// `schedule_upgrade` does not validate the hash, so an admin can announce a
/// wasm that was never uploaded. The failure then surfaces at
/// `execute_upgrade`, after the state writes — and this proves that is safe.
///
/// `execute_upgrade` clears the pending record, bumps the version and emits the
/// event **before** calling `update_current_contract_wasm`, because that call
/// ends the invocation. If the swap then fails, the whole invocation reverts:
/// the pending upgrade is still there, the version is unchanged, and no event
/// was emitted. Nothing is half-applied.
#[test]
fn a_failed_swap_reverts_cleanly() {
    let s = setup();
    // A syntactically valid hash that no wasm was ever uploaded under.
    let bogus = BytesN::from_array(&s.env, &[0xAB; 32]);

    at(&s, 1_000);
    s.escrow.schedule_upgrade(&bogus);
    let eta = s.escrow.get_pending_upgrade().unwrap().eta;
    at(&s, eta);

    // The swap fails. `try_execute_upgrade` reports it as an `Err` rather than
    // panicking, so no `catch_unwind` is needed here.
    let result = s.escrow.try_execute_upgrade();
    assert!(
        result.is_err(),
        "executing an unuploaded wasm must fail, got {result:?}",
    );

    // State is exactly as it was: still pending, still version 1.
    let pending = s.escrow.get_pending_upgrade().unwrap();
    assert_eq!(pending.wasm_hash, bogus);
    assert_eq!(pending.eta, eta);
    assert_eq!(s.escrow.version(), INITIAL_VERSION);

    // And the escrow still works, so the failure did not corrupt anything.
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 400i128],
    );
    s.escrow.deposit_funds(&id, &400);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);
}

/// A pending upgrade does not stop the admin cancelling and re-announcing,
/// which is the recovery path when a scheduled hash turns out to be wrong.
#[test]
fn a_bad_pending_upgrade_can_be_cancelled_and_replaced() {
    let s = setup();
    let bogus = BytesN::from_array(&s.env, &[0xCD; 32]);

    at(&s, 1_000);
    s.escrow.schedule_upgrade(&bogus);
    at(&s, 1_500);
    s.escrow.cancel_upgrade();
    s.escrow.schedule_upgrade(&s.fixture_hash);

    at(&s, 1_500 + UPGRADE_DELAY_SECS);
    s.escrow.execute_upgrade();

    // The replacement took effect.
    let fixture = escrow_fixture::EscrowFixtureClient::new(&s.env, &s.escrow_id);
    assert_eq!(fixture.fixture_marker(), escrow_fixture::MARKER);
}

/// The admin is the account that can do all three, which is the positive half of
/// the negative auth tests above.
#[test]
fn the_admin_is_the_account_that_can_upgrade() {
    let s = setup();
    assert_eq!(s.escrow.get_admin(), s.admin);

    at(&s, 1_000);
    s.escrow.schedule_upgrade(&s.escrow_hash);
    s.escrow.cancel_upgrade();
    s.escrow.schedule_upgrade(&s.escrow_hash);
    at(&s, 1_000 + UPGRADE_DELAY_SECS);
    s.escrow.execute_upgrade();

    assert_eq!(s.escrow.version(), INITIAL_VERSION + 1);
}

/// An upgrade that is scheduled but never executed leaves the contract working.
#[test]
fn a_pending_upgrade_does_not_affect_normal_operation() {
    let s = setup();
    let id = s.escrow.create_contract(
        &s.client,
        &s.freelancer,
        &s.token.address,
        &vec![&s.env, 400i128],
    );
    s.escrow.schedule_upgrade(&s.fixture_hash);

    // Everything still works while the delay runs down.
    s.escrow.deposit_funds(&id, &400);
    s.escrow.release_milestone(&id, &0);
    assert_eq!(s.token.balance(&s.freelancer), 400);
    assert_eq!(s.escrow.version(), INITIAL_VERSION);
}
