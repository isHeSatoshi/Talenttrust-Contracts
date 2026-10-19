//! TalentTrust escrow contract for milestone-based freelancer payments on Stellar (Soroban).
//!
//! Lifecycle:
//! 1. `initialize(admin)` once after deployment.
//! 2. A client calls `create_contract(client, freelancer, token, milestone_amounts)`.
//! 3. The client funds the escrow with `deposit_funds` (real token transfer into the contract).
//! 4. The client approves work with `release_milestone`, paying the freelancer per milestone.
//! 5. Either party may `open_dispute`; the admin settles with `resolve_dispute`.
//! 6. The client can `cancel_contract` before completion to recover unreleased funds.
//! 7. After completion the client may `issue_reputation` for the freelancer.
#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

pub const MAX_MILESTONES: u32 = 20;
pub const BPS_DENOMINATOR: i128 = 10_000;
/// Shortest review window a client may offer: one hour.
pub const MIN_REVIEW_WINDOW_SECS: u64 = 3_600;
/// Longest review window a client may offer: thirty days.
pub const MAX_REVIEW_WINDOW_SECS: u64 = 2_592_000;
const PERSISTENT_TTL_THRESHOLD: u32 = 17_280; // ~1 day
const PERSISTENT_TTL_EXTEND: u32 = 518_400; // ~30 days

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum EscrowError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    Paused = 4,
    ContractNotFound = 5,
    InvalidParties = 6,
    InvalidMilestones = 7,
    InvalidAmount = 8,
    OverFunded = 9,
    InvalidStatus = 10,
    MilestoneNotFound = 11,
    MilestoneAlreadyReleased = 12,
    InsufficientFunds = 13,
    InvalidSplit = 14,
    InvalidRating = 15,
    ReputationAlreadyIssued = 16,
    /// `review_window_secs` is outside `MIN_REVIEW_WINDOW_SECS..=MAX_REVIEW_WINDOW_SECS`.
    InvalidReviewWindow = 17,
    /// `claim_milestone` on a milestone the freelancer has not submitted.
    MilestoneNotSubmitted = 18,
    /// `claim_milestone` before `submitted_at + review_window_secs`.
    ReviewWindowActive = 19,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ContractStatus {
    Created,
    Funded,
    Completed,
    Disputed,
    Resolved,
    Cancelled,
}

/// Where a milestone sits between "not started" and "paid".
///
/// The distinction between `Pending` and `Submitted` is what protects the
/// freelancer: once work is submitted, the money backing it can no longer be
/// refunded to the client by `cancel_contract`.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MilestoneStatus {
    /// Not delivered. The client may release early, and cancellation refunds it.
    Pending,
    /// Delivered and awaiting review. Only the freelancer can move it on, by
    /// `claim_milestone` once the window elapses; the client may release or
    /// dispute it sooner. Cancellation does not refund it.
    Submitted,
    /// Paid. Terminal.
    Released,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub amount: i128,
    pub status: MilestoneStatus,
    /// Ledger timestamp of submission; `0` while `Pending`.
    pub submitted_at: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EscrowContract {
    pub id: u64,
    pub client: Address,
    pub freelancer: Address,
    pub token: Address,
    pub milestones: Vec<Milestone>,
    pub total_amount: i128,
    pub funded_amount: i128,
    pub released_amount: i128,
    /// How long the client has to review a submitted milestone before the
    /// freelancer can claim it. Bounded at creation.
    pub review_window_secs: u64,
    pub status: ContractStatus,
    pub reputation_issued: bool,
}

#[contracttype]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Reputation {
    pub completed_contracts: u32,
    pub total_rating: u32,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    Paused,
    NextId,
    Contract(u64),
    Reputation(Address),
}

#[contract]
pub struct Escrow;

fn admin(env: &Env) -> Result<Address, EscrowError> {
    env.storage()
        .instance()
        .get(&DataKey::Admin)
        .ok_or(EscrowError::NotInitialized)
}

fn require_admin(env: &Env) -> Result<Address, EscrowError> {
    let a = admin(env)?;
    a.require_auth();
    Ok(a)
}

fn require_not_paused(env: &Env) -> Result<(), EscrowError> {
    if env
        .storage()
        .instance()
        .get::<_, bool>(&DataKey::Paused)
        .unwrap_or(false)
    {
        return Err(EscrowError::Paused);
    }
    Ok(())
}

fn load(env: &Env, id: u64) -> Result<EscrowContract, EscrowError> {
    env.storage()
        .persistent()
        .get(&DataKey::Contract(id))
        .ok_or(EscrowError::ContractNotFound)
}

fn save(env: &Env, c: &EscrowContract) {
    let key = DataKey::Contract(c.id);
    env.storage().persistent().set(&key, c);
    env.storage()
        .persistent()
        .extend_ttl(&key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND);
}

fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND);
}

fn transfer(env: &Env, token_addr: &Address, from: &Address, to: &Address, amount: i128) {
    if amount > 0 {
        token::Client::new(env, token_addr).transfer(from, to, &amount);
    }
}

/// Amount held behind milestones that are `Submitted` — delivered, not yet paid.
///
/// These funds are the freelancer's to claim, so `cancel_contract` must not
/// refund them and `submit_milestone` must not double-count them.
///
/// The accumulator cannot overflow: `create_contract` validates that the
/// milestone amounts sum to `total_amount` with `checked_add`, and this is a sum
/// over a subset of those same amounts, so it stays within `0..=total_amount`.
/// `saturating_add` states that bound rather than leaving a panic path open on a
/// value that is already known to be in range.
fn submitted_backing(c: &EscrowContract) -> i128 {
    c.milestones
        .iter()
        .filter(|m| m.status == MilestoneStatus::Submitted)
        .fold(0i128, |acc, m| acc.saturating_add(m.amount))
}

/// Funds the freelancer could still be paid: everything funded that has not
/// already been released, less what is already promised to a submitted
/// milestone. Never negative on a consistent contract.
fn uncommitted_funds(c: &EscrowContract) -> i128 {
    (c.funded_amount - c.released_amount).saturating_sub(submitted_backing(c))
}

/// Whether `submitted_at + review_window_secs` has passed.
///
/// `checked_add` because the sum is attacker-influenced in principle: a window
/// near `u64::MAX` would wrap. On overflow the deadline is unrepresentable, so
/// the window is treated as **not** elapsed — the conservative direction, since
/// releasing early would take the money out from under the client's review.
/// Real ledger timestamps are around `1.7e9` and the window is capped at 30
/// days, so this branch is unreachable in practice; it exists so the function is
/// total.
fn review_window_elapsed(env: &Env, m: &Milestone, window_secs: u64) -> bool {
    match m.submitted_at.checked_add(window_secs) {
        Some(deadline) => env.ledger().timestamp() >= deadline,
        None => false,
    }
}

/// Mark a milestone released, pay the freelancer, and advance the escrow status
/// if that was the last one. Shared by `release_milestone` and
/// `claim_milestone` so both pay identically.
fn pay_milestone(
    env: &Env,
    c: &mut EscrowContract,
    index: u32,
    m: &mut Milestone,
) -> Result<(), EscrowError> {
    let available = c
        .funded_amount
        .checked_sub(c.released_amount)
        .ok_or(EscrowError::InsufficientFunds)?;
    if available < m.amount {
        return Err(EscrowError::InsufficientFunds);
    }
    transfer(
        env,
        &c.token,
        &env.current_contract_address(),
        &c.freelancer,
        m.amount,
    );
    m.status = MilestoneStatus::Released;
    m.submitted_at = 0;
    c.released_amount = c
        .released_amount
        .checked_add(m.amount)
        .ok_or(EscrowError::InvalidAmount)?;
    c.milestones.set(index, m.clone());
    if c.milestones.iter().all(|x| x.status == MilestoneStatus::Released) {
        c.status = ContractStatus::Completed;
    }
    Ok(())
}

#[contractimpl]
impl Escrow {
    /// One-time setup. Sets the admin that controls pausing and dispute resolution.
    pub fn initialize(env: Env, admin: Address) -> Result<(), EscrowError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(EscrowError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().set(&DataKey::NextId, &1u64);
        bump_instance(&env);
        env.events().publish((symbol_short!("init"),), admin);
        Ok(())
    }

    pub fn version(_env: Env) -> u32 {
        1
    }

    pub fn get_admin(env: Env) -> Result<Address, EscrowError> {
        admin(&env)
    }

    pub fn set_paused(env: Env, paused: bool) -> Result<(), EscrowError> {
        require_admin(&env)?;
        env.storage().instance().set(&DataKey::Paused, &paused);
        bump_instance(&env);
        env.events().publish((symbol_short!("paused"),), paused);
        Ok(())
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Creates a new escrow agreement. Returns the escrow id.
    ///
    /// `review_window_secs` is how long the client has to review a milestone the
    /// freelancer submits before the freelancer can claim it without approval.
    /// It is bounded to `MIN_REVIEW_WINDOW_SECS..=MAX_REVIEW_WINDOW_SECS` so a
    /// client cannot set a window so short that review is impossible, or so long
    /// that it is effectively never.
    pub fn create_contract(
        env: Env,
        client: Address,
        freelancer: Address,
        token: Address,
        milestone_amounts: Vec<i128>,
        review_window_secs: u64,
    ) -> Result<u64, EscrowError> {
        admin(&env)?;
        require_not_paused(&env)?;
        client.require_auth();
        if client == freelancer {
            return Err(EscrowError::InvalidParties);
        }
        if !(MIN_REVIEW_WINDOW_SECS..=MAX_REVIEW_WINDOW_SECS).contains(&review_window_secs) {
            return Err(EscrowError::InvalidReviewWindow);
        }
        let n = milestone_amounts.len();
        if n == 0 || n > MAX_MILESTONES {
            return Err(EscrowError::InvalidMilestones);
        }
        let mut milestones = Vec::new(&env);
        let mut total: i128 = 0;
        for amount in milestone_amounts.iter() {
            if amount <= 0 {
                return Err(EscrowError::InvalidAmount);
            }
            total = total.checked_add(amount).ok_or(EscrowError::InvalidAmount)?;
            milestones.push_back(Milestone {
                amount,
                status: MilestoneStatus::Pending,
                submitted_at: 0,
            });
        }
        let id: u64 = env.storage().instance().get(&DataKey::NextId).unwrap_or(1);
        env.storage().instance().set(&DataKey::NextId, &(id + 1));
        bump_instance(&env);
        let c = EscrowContract {
            id,
            client: client.clone(),
            freelancer: freelancer.clone(),
            token,
            milestones,
            total_amount: total,
            funded_amount: 0,
            released_amount: 0,
            review_window_secs,
            status: ContractStatus::Created,
            reputation_issued: false,
        };
        save(&env, &c);
        env.events()
            .publish((symbol_short!("created"), id), (client, freelancer, total));
        Ok(id)
    }

    /// Client deposits `amount` of the escrow token into the contract.
    pub fn deposit_funds(env: Env, id: u64, amount: i128) -> Result<(), EscrowError> {
        require_not_paused(&env)?;
        let mut c = load(&env, id)?;
        c.client.require_auth();
        if !matches!(c.status, ContractStatus::Created | ContractStatus::Funded) {
            return Err(EscrowError::InvalidStatus);
        }
        if amount <= 0 {
            return Err(EscrowError::InvalidAmount);
        }
        let new_funded = c
            .funded_amount
            .checked_add(amount)
            .ok_or(EscrowError::InvalidAmount)?;
        if new_funded > c.total_amount {
            return Err(EscrowError::OverFunded);
        }
        transfer(
            &env,
            &c.token,
            &c.client,
            &env.current_contract_address(),
            amount,
        );
        c.funded_amount = new_funded;
        if c.funded_amount == c.total_amount {
            c.status = ContractStatus::Funded;
        }
        save(&env, &c);
        env.events().publish((symbol_short!("deposit"), id), amount);
        Ok(())
    }

    /// Client approves a milestone and the freelancer is paid.
    ///
    /// Accepts a `Pending` milestone (the client paying early) and a `Submitted`
    /// one (the client approving within the review window). Both pay the same
    /// amount by the same path.
    pub fn release_milestone(env: Env, id: u64, index: u32) -> Result<(), EscrowError> {
        require_not_paused(&env)?;
        let mut c = load(&env, id)?;
        c.client.require_auth();
        if !matches!(c.status, ContractStatus::Created | ContractStatus::Funded) {
            return Err(EscrowError::InvalidStatus);
        }
        let mut m = c
            .milestones
            .get(index)
            .ok_or(EscrowError::MilestoneNotFound)?;
        match m.status {
            MilestoneStatus::Pending | MilestoneStatus::Submitted => {}
            MilestoneStatus::Released => return Err(EscrowError::MilestoneAlreadyReleased),
        }
        pay_milestone(&env, &mut c, index, &mut m)?;
        save(&env, &c);
        env.events()
            .publish((symbol_short!("release"), id), (index, m.amount));
        Ok(())
    }

    /// Freelancer records that a milestone has been delivered, starting the
    /// client's review window.
    ///
    /// From here the funds backing this milestone can no longer be refunded by
    /// `cancel_contract`: the client must release, dispute, or let the window
    /// lapse. This is what closes the cancel-after-delivery hole, where a client
    /// could take delivered work and refund themselves in the same ledger.
    ///
    /// The milestone amount must be covered by funds that are not already
    /// released and not already promised to another submitted milestone, so
    /// submission can never promise more than is actually held.
    pub fn submit_milestone(env: Env, id: u64, index: u32) -> Result<(), EscrowError> {
        require_not_paused(&env)?;
        let mut c = load(&env, id)?;
        c.freelancer.require_auth();
        if c.status != ContractStatus::Funded {
            return Err(EscrowError::InvalidStatus);
        }
        let mut m = c
            .milestones
            .get(index)
            .ok_or(EscrowError::MilestoneNotFound)?;
        match m.status {
            MilestoneStatus::Pending => {}
            // Already delivered: re-submitting would restart the review clock and
            // let the freelancer postpone review indefinitely.
            MilestoneStatus::Submitted => return Err(EscrowError::InvalidStatus),
            MilestoneStatus::Released => return Err(EscrowError::MilestoneAlreadyReleased),
        }
        if uncommitted_funds(&c) < m.amount {
            return Err(EscrowError::InsufficientFunds);
        }
        let submitted_at = env.ledger().timestamp();
        m.status = MilestoneStatus::Submitted;
        m.submitted_at = submitted_at;
        c.milestones.set(index, m.clone());
        save(&env, &c);
        env.events()
            .publish((symbol_short!("submit"), id), (index, submitted_at));
        Ok(())
    }

    /// Freelancer claims a submitted milestone once the review window has
    /// elapsed without the client approving or disputing it.
    ///
    /// Deliberately does **not** require the escrow to still be `Funded`: a
    /// cancelled escrow leaves submitted milestones claimable, which is the
    /// point of the submission state. It refuses while the escrow is `Disputed`,
    /// because the dispute path owns the funds at that point, and it refuses if
    /// the funds are no longer held (a resolved dispute has already paid them
    /// out), which `pay_milestone` enforces.
    pub fn claim_milestone(env: Env, id: u64, index: u32) -> Result<(), EscrowError> {
        require_not_paused(&env)?;
        let mut c = load(&env, id)?;
        c.freelancer.require_auth();
        if c.status == ContractStatus::Disputed {
            return Err(EscrowError::InvalidStatus);
        }
        let mut m = c
            .milestones
            .get(index)
            .ok_or(EscrowError::MilestoneNotFound)?;
        if m.status != MilestoneStatus::Submitted {
            return Err(EscrowError::MilestoneNotSubmitted);
        }
        if !review_window_elapsed(&env, &m, c.review_window_secs) {
            return Err(EscrowError::ReviewWindowActive);
        }
        pay_milestone(&env, &mut c, index, &mut m)?;
        save(&env, &c);
        env.events()
            .publish((symbol_short!("claim"), id), (index, m.amount));
        Ok(())
    }

    /// Either party may raise a dispute, freezing releases until the admin resolves it.
    pub fn open_dispute(env: Env, id: u64, caller: Address) -> Result<(), EscrowError> {
        require_not_paused(&env)?;
        let mut c = load(&env, id)?;
        caller.require_auth();
        if caller != c.client && caller != c.freelancer {
            return Err(EscrowError::Unauthorized);
        }
        if !matches!(c.status, ContractStatus::Created | ContractStatus::Funded) {
            return Err(EscrowError::InvalidStatus);
        }
        c.status = ContractStatus::Disputed;
        save(&env, &c);
        env.events().publish((symbol_short!("dispute"), id), caller);
        Ok(())
    }

    /// Admin resolves a dispute, paying `freelancer_bps` (0..=10000) of the escrowed
    /// balance to the freelancer and refunding the rest to the client.
    pub fn resolve_dispute(env: Env, id: u64, freelancer_bps: u32) -> Result<(), EscrowError> {
        require_admin(&env)?;
        let mut c = load(&env, id)?;
        if c.status != ContractStatus::Disputed {
            return Err(EscrowError::InvalidStatus);
        }
        if freelancer_bps as i128 > BPS_DENOMINATOR {
            return Err(EscrowError::InvalidSplit);
        }
        let held = c.funded_amount - c.released_amount;
        let to_freelancer = held * freelancer_bps as i128 / BPS_DENOMINATOR;
        let to_client = held - to_freelancer;
        let this = env.current_contract_address();
        transfer(&env, &c.token, &this, &c.freelancer, to_freelancer);
        transfer(&env, &c.token, &this, &c.client, to_client);
        c.released_amount += to_freelancer;
        c.funded_amount = c.released_amount;
        c.status = ContractStatus::Resolved;
        save(&env, &c);
        env.events()
            .publish((symbol_short!("resolved"), id), (to_freelancer, to_client));
        Ok(())
    }

    /// Client cancels the agreement and recovers funds that are not already
    /// delivered.
    ///
    /// Refunds only what backs `Pending` milestones. Money behind a `Submitted`
    /// milestone stays in the contract: the freelancer has already delivered, so
    /// the client cannot take it back by cancelling. Those funds remain
    /// claimable by `claim_milestone` once the window elapses, or reachable by
    /// `open_dispute`. With no submitted milestones this is the previous
    /// behaviour exactly.
    pub fn cancel_contract(env: Env, id: u64) -> Result<(), EscrowError> {
        require_not_paused(&env)?;
        let mut c = load(&env, id)?;
        c.client.require_auth();
        if !matches!(c.status, ContractStatus::Created | ContractStatus::Funded) {
            return Err(EscrowError::InvalidStatus);
        }
        // `refund` is a subset of what is held, so both subtractions are in
        // range on a consistent contract; checked anyway so a corrupted state
        // reports rather than trapping.
        let uncommitted = c
            .funded_amount
            .checked_sub(c.released_amount)
            .ok_or(EscrowError::InsufficientFunds)?;
        let refund = uncommitted.saturating_sub(submitted_backing(&c));
        transfer(
            &env,
            &c.token,
            &env.current_contract_address(),
            &c.client,
            refund,
        );
        // Only the refunded part leaves the contract, so `funded_amount` drops by
        // exactly that much and the contract balance stays equal to
        // `funded_amount - released_amount`.
        c.funded_amount = c
            .funded_amount
            .checked_sub(refund)
            .ok_or(EscrowError::InsufficientFunds)?;
        c.status = ContractStatus::Cancelled;
        save(&env, &c);
        env.events().publish((symbol_short!("cancel"), id), refund);
        Ok(())
    }

    /// After completion the client may rate the freelancer (1..=5) once per contract.
    pub fn issue_reputation(env: Env, id: u64, rating: u32) -> Result<(), EscrowError> {
        let mut c = load(&env, id)?;
        c.client.require_auth();
        if c.status != ContractStatus::Completed {
            return Err(EscrowError::InvalidStatus);
        }
        if c.reputation_issued {
            return Err(EscrowError::ReputationAlreadyIssued);
        }
        if !(1..=5).contains(&rating) {
            return Err(EscrowError::InvalidRating);
        }
        let key = DataKey::Reputation(c.freelancer.clone());
        let mut rep: Reputation = env.storage().persistent().get(&key).unwrap_or_default();
        rep.completed_contracts += 1;
        rep.total_rating += rating;
        env.storage().persistent().set(&key, &rep);
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND);
        c.reputation_issued = true;
        save(&env, &c);
        env.events()
            .publish((symbol_short!("rep"), id), (c.freelancer, rating));
        Ok(())
    }

    pub fn get_contract(env: Env, id: u64) -> Result<EscrowContract, EscrowError> {
        load(&env, id)
    }

    pub fn contract_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get::<_, u64>(&DataKey::NextId)
            .unwrap_or(1)
            - 1
    }

    pub fn get_reputation(env: Env, freelancer: Address) -> Reputation {
        env.storage()
            .persistent()
            .get(&DataKey::Reputation(freelancer))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod test;
