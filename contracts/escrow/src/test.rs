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
// Versioned storage envelope (#1673)
// ---------------------------------------------------------------------------

fn has_legacy(s: &Setup, id: u64) -> bool {
    s.env.as_contract(&s.escrow.address, || {
        s.env.storage().persistent().has(&DataKey::Contract(id))
    })
}

fn has_envelope(s: &Setup, id: u64) -> bool {
    s.env.as_contract(&s.escrow.address, || {
        s.env.storage().persistent().has(&DataKey::ContractV(id))
    })
}

/// Rewrites an escrow into the pre-envelope layout: value under the old
/// `Contract(id)` key, nothing under `ContractV(id)`. This is exactly the shape
/// an escrow written by an older release has on chain.
fn demote_to_legacy(s: &Setup, id: u64) {
    let c = s.escrow.get_contract(&id);
    s.env.as_contract(&s.escrow.address, || {
        s.env.storage().persistent().set(&DataKey::Contract(id), &c);
        s.env.storage().persistent().remove(&DataKey::ContractV(id));
    });
}

fn created<'a>(s: &Setup<'a>, total: i128) -> u64 {
    s.escrow
        .create_contract(&s.client, &s.freelancer, &s.token.address, &vec![&s.env, total])
}

#[test]
fn legacy_entry_is_read_then_migrated_by_the_first_write() {
    let s = setup();
    let id = created(&s, 300);
    s.escrow.deposit_funds(&id, &300);
    demote_to_legacy(&s, id);
    assert!(has_legacy(&s, id));
    assert!(!has_envelope(&s, id));

    // Reads decode the legacy layout.
    let c = s.escrow.get_contract(&id);
    assert_eq!(c.total_amount, 300);
    assert_eq!(c.status, ContractStatus::Funded);
    assert_eq!(c.milestones.len(), 1);

    // The first mutation writes the envelope and drops the legacy key.
    s.escrow.release_milestone(&id, &0);
    assert!(!has_legacy(&s, id));
    assert!(has_envelope(&s, id));

    // The migrated entry is the same escrow, not a reset one.
    let c = s.escrow.get_contract(&id);
    assert_eq!(c.released_amount, 300);
    assert!(c.milestones.get(0).unwrap().released);
    assert_eq!(s.token.balance(&s.freelancer), 300);
}

#[test]
fn deposit_works_off_a_legacy_entry_and_migrates_it() {
    let s = setup();
    let id = created(&s, 300);
    demote_to_legacy(&s, id);

    s.escrow.deposit_funds(&id, &300);
    assert!(!has_legacy(&s, id));
    assert!(has_envelope(&s, id));
    assert_eq!(s.token.balance(&s.escrow.address), 300);
    assert_eq!(s.escrow.get_contract(&id).status, ContractStatus::Funded);
}

#[test]
fn get_contract_on_a_legacy_entry_does_not_write() {
    let s = setup();
    let id = created(&s, 300);
    demote_to_legacy(&s, id);

    // Repeated reads must not migrate.
    for _ in 0..5 {
        assert_eq!(s.escrow.get_contract(&id).total_amount, 300);
    }
    assert_eq!(s.escrow.contract_count(), 1);
    assert!(has_legacy(&s, id));
    assert!(!has_envelope(&s, id));

    // An unknown id still reports not-found rather than being created.
    assert_eq!(
        s.escrow.try_get_contract(&999),
        Err(Ok(EscrowError::ContractNotFound))
    );
    assert!(!has_envelope(&s, 999));
}

#[test]
fn migrate_escrows_is_idempotent() {
    let s = setup();
    let a = created(&s, 300);
    let b = created(&s, 400);
    demote_to_legacy(&s, a);
    demote_to_legacy(&s, b);

    assert_eq!(s.escrow.migrate_escrows(&vec![&s.env, a, b]), 2);
    assert!(!has_legacy(&s, a) && has_envelope(&s, a));
    assert!(!has_legacy(&s, b) && has_envelope(&s, b));

    // A second pass finds nothing left to do.
    assert_eq!(s.escrow.migrate_escrows(&vec![&s.env, a, b]), 0);
    assert_eq!(s.escrow.get_contract(&a).total_amount, 300);
    assert_eq!(s.escrow.get_contract(&b).total_amount, 400);
}

#[test]
fn migrate_escrows_skips_unknown_and_already_current_ids() {
    let s = setup();
    let current = created(&s, 300); // never demoted: already in the envelope
    let legacy = created(&s, 400);
    demote_to_legacy(&s, legacy);

    assert_eq!(
        s.escrow.migrate_escrows(&vec![&s.env, 0, 999, current, legacy]),
        1
    );
    assert!(has_envelope(&s, legacy));
    assert!(has_envelope(&s, current));
    assert_eq!(s.escrow.migrate_escrows(&vec![&s.env, 0, 999]), 0);
}

#[test]
fn migrate_escrows_rejects_more_than_the_batch_cap() {
    let s = setup();
    let mut over = Vec::new(&s.env);
    for i in 0..(MIGRATION_BATCH_MAX + 1) {
        over.push_back(i as u64);
    }
    assert_eq!(
        s.escrow.try_migrate_escrows(&over),
        Err(Ok(EscrowError::InvalidAmount))
    );

    // Exactly the cap is accepted.
    let mut at_cap = Vec::new(&s.env);
    for i in 0..MIGRATION_BATCH_MAX {
        at_cap.push_back(i as u64);
    }
    assert_eq!(s.escrow.migrate_escrows(&at_cap), 0);

    // The empty batch is a no-op rather than an error.
    assert_eq!(s.escrow.migrate_escrows(&Vec::new(&s.env)), 0);
}

#[test]
fn migration_never_changes_an_escrows_contents() {
    let s = setup();
    let id = created(&s, 300);
    s.escrow.deposit_funds(&id, &300);
    s.escrow.release_milestone(&id, &0); // mid-flight state, worth preserving
    let before = s.escrow.get_contract(&id);

    demote_to_legacy(&s, id);
    assert_eq!(s.escrow.migrate_escrows(&vec![&s.env, id]), 1);

    assert_eq!(s.escrow.get_contract(&id), before);
}

#[test]
fn report_cost_of_a_full_migration_batch() {
    let s = setup();
    let mut ids = Vec::new(&s.env);
    for _ in 0..MIGRATION_BATCH_MAX {
        let id = created(&s, 1);
        demote_to_legacy(&s, id);
        ids.push_back(id);
    }

    // `env.budget()` is deprecated in favour of `cost_estimate().budget()`.
    let budget = s.env.cost_estimate().budget();
    let cpu_before = budget.cpu_instruction_cost();
    let mem_before = budget.memory_bytes_cost();
    let migrated = s.escrow.migrate_escrows(&ids);
    let cpu = budget.cpu_instruction_cost() - cpu_before;
    let mem = budget.memory_bytes_cost() - mem_before;

    std::println!(
        "MIGRATION COST: {migrated} escrows in one call -> cpu={cpu} instructions, mem={mem} bytes"
    );
    assert_eq!(migrated, MIGRATION_BATCH_MAX);
    assert!(cpu > 0 && mem > 0, "budget tracking reported nothing");
}

