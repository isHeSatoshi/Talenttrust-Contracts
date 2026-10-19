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
/// Ceiling on the protocol fee: 500 bps, i.e. 5%.
pub const MAX_FEE_BPS: u32 = 500;
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
    /// `set_fee` was given a rate above `MAX_FEE_BPS`.
    FeeTooHigh = 17,
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

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub amount: i128,
    pub released: bool,
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
    /// The global fee rate, in basis points.
    FeeBps,
    /// Where the fee is paid. Set together with `FeeBps`, never separately.
    Treasury,
    /// The fee rate this escrow was created under. Snapshotted so an admin
    /// cannot raise the fee on an escrow that was agreed and funded at a lower
    /// one.
    EscrowFee(u64),
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

/// The global fee rate in basis points. Absent reads as 0, which is the
/// behaviour the contract had before fees existed.
fn fee_bps(env: &Env) -> u32 {
    env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0)
}

/// Where the fee is paid. Only meaningful when `fee_bps` is non-zero.
fn treasury(env: &Env) -> Option<Address> {
    env.storage().instance().get(&DataKey::Treasury)
}

/// The rate this escrow was created under, snapshotted in `create_contract`.
fn escrow_fee(env: &Env, id: u64) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::EscrowFee(id))
        .unwrap_or(0)
}

/// Take the fee on `amount` and pay it to the treasury, returning what the
/// freelancer keeps.
///
/// A zero fee, or a fee that rounds to zero, transfers nothing to the treasury:
/// `transfer` already skips zero amounts, and paying a zero-value token transfer
/// would be a wasted host call.
fn pay_fee(env: &Env, token_addr: &Address, fee: i128) -> Result<(), EscrowError> {
    if fee <= 0 {
        return Ok(());
    }
    // `set_fee` writes both keys together, so a non-zero fee always has a
    // treasury. Treating a missing one as an error rather than skipping the
    // payment means a corrupted config cannot silently give the fee to nobody.
    let dest = treasury(env).ok_or(EscrowError::NotInitialized)?;
    transfer(env, token_addr, &env.current_contract_address(), &dest, fee);
    Ok(())
}

/// `floor(amount * bps / BPS_DENOMINATOR)`, computed so it cannot overflow.
///
/// The direct form multiplies first, so it is only valid while
/// `amount <= i128::MAX / BPS_DENOMINATOR`. Past that it traps, and the release
/// profile sets `overflow-checks = true`, so the trap is a panic the caller
/// cannot handle. A milestone amount is only bounded by `i128::MAX`, so that
/// range is reachable. Dividing first removes the wide product.
///
/// # Correctness
///
/// Write `amount = q * D + r` with `D = BPS_DENOMINATOR`, `q = amount / D` and
/// `r = amount % D`, so `0 <= r < D` by the division algorithm. Then
///
/// ```text
/// amount * bps = (q * D + r) * bps = q * D * bps + r * bps
/// ```
///
/// Dividing by `D` and taking the floor:
///
/// ```text
/// floor(amount * bps / D) = q * bps + floor(r * bps / D)
/// ```
///
/// because `q * D * bps / D = q * bps` is an integer and `floor` distributes
/// over an integer summand. The right-hand side is exactly what this function
/// evaluates.
///
/// # Bounds
///
/// For all `amount >= 0` and `0 <= bps <= D`:
///
/// - `q = amount / D <= amount`, so `q * bps <= q * D <= amount <= i128::MAX`.
/// - `r < D = 10_000` and `bps <= D = 10_000`, so `r * bps < 10^8`.
/// - The result is `floor(amount * bps / D) <= floor(amount * D / D) = amount`,
///   since `bps <= D`. So the fee can never exceed the amount it is taken from.
fn fee_on(amount: i128, bps: u32) -> i128 {
    let bps = bps as i128;
    (amount / BPS_DENOMINATOR) * bps + ((amount % BPS_DENOMINATOR) * bps) / BPS_DENOMINATOR
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

    /// Sets the protocol fee rate and the treasury that receives it. Admin only.
    ///
    /// Both are written together, so a non-zero rate always has a destination
    /// and a rate can never be raised without also naming where it goes. The
    /// rate is capped at `MAX_FEE_BPS`.
    ///
    /// The new rate applies only to escrows created afterwards: existing escrows
    /// keep the rate they were created under, stored per escrow.
    pub fn set_fee(env: Env, fee_bps: u32, treasury: Address) -> Result<(), EscrowError> {
        require_admin(&env)?;
        if fee_bps > MAX_FEE_BPS {
            return Err(EscrowError::FeeTooHigh);
        }
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        bump_instance(&env);
        env.events()
            .publish((symbol_short!("fee_set"),), (fee_bps, treasury));
        Ok(())
    }

    /// The current fee rate and treasury. A rate of 0 means fees are off.
    pub fn get_fee_config(env: Env) -> (u32, Option<Address>) {
        (fee_bps(&env), treasury(&env))
    }

    /// The fee rate this escrow was created under.
    pub fn get_escrow_fee(env: Env, id: u64) -> u32 {
        escrow_fee(&env, id)
    }

    /// Creates a new escrow agreement. Returns the escrow id.
    pub fn create_contract(
        env: Env,
        client: Address,
        freelancer: Address,
        token: Address,
        milestone_amounts: Vec<i128>,
    ) -> Result<u64, EscrowError> {
        admin(&env)?;
        require_not_paused(&env)?;
        client.require_auth();
        if client == freelancer {
            return Err(EscrowError::InvalidParties);
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
                released: false,
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
            status: ContractStatus::Created,
            reputation_issued: false,
        };
        save(&env, &c);
        // Snapshot the fee rate for this escrow. A later `set_fee` must not
        // change what an already-agreed escrow pays.
        let rate = fee_bps(&env);
        if rate > 0 {
            let key = DataKey::EscrowFee(id);
            env.storage().persistent().set(&key, &rate);
            env.storage()
                .persistent()
                .extend_ttl(&key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND);
        }
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
        if m.released {
            return Err(EscrowError::MilestoneAlreadyReleased);
        }
        if c.funded_amount - c.released_amount < m.amount {
            return Err(EscrowError::InsufficientFunds);
        }
        // The rate snapshotted at creation, not the current global one.
        let fee = fee_on(m.amount, escrow_fee(&env, id));
        let to_freelancer = m
            .amount
            .checked_sub(fee)
            .ok_or(EscrowError::InvalidAmount)?;
        transfer(
            &env,
            &c.token,
            &env.current_contract_address(),
            &c.freelancer,
            to_freelancer,
        );
        pay_fee(&env, &c.token, fee)?;
        m.released = true;
        c.released_amount += m.amount;
        c.milestones.set(index, m.clone());
        if c.milestones.iter().all(|x| x.released) {
            c.status = ContractStatus::Completed;
        }
        save(&env, &c);
        env.events()
            .publish((symbol_short!("release"), id), (index, m.amount, fee));
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
    ///
    /// The protocol fee applies only to the freelancer's share. The client's
    /// refund is its own money coming back, not a payment for work, so charging
    /// a fee on it would take from the wrong party. `cancel_contract` likewise
    /// charges nothing.
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
        // Charged against the freelancer's gross share, at the escrow's
        // snapshotted rate.
        let fee = fee_on(to_freelancer, escrow_fee(&env, id));
        let freelancer_net = to_freelancer
            .checked_sub(fee)
            .ok_or(EscrowError::InvalidAmount)?;
        let this = env.current_contract_address();
        transfer(&env, &c.token, &this, &c.freelancer, freelancer_net);
        transfer(&env, &c.token, &this, &c.client, to_client);
        pay_fee(&env, &c.token, fee)?;
        // The gross share is what leaves the escrow, so the accounting is the
        // same whether or not a fee was taken.
        c.released_amount += to_freelancer;
        c.funded_amount = c.released_amount;
        c.status = ContractStatus::Resolved;
        save(&env, &c);
        env.events().publish(
            (symbol_short!("resolved"), id),
            (to_freelancer, to_client, fee),
        );
        Ok(())
    }

    /// Client cancels the agreement and recovers any unreleased funds.
    pub fn cancel_contract(env: Env, id: u64) -> Result<(), EscrowError> {
        require_not_paused(&env)?;
        let mut c = load(&env, id)?;
        c.client.require_auth();
        if !matches!(c.status, ContractStatus::Created | ContractStatus::Funded) {
            return Err(EscrowError::InvalidStatus);
        }
        // No fee on a cancellation: nothing was delivered, so there is no
        // release to charge against, and the client is recovering its own money.
        let refund = c.funded_amount - c.released_amount;
        transfer(
            &env,
            &c.token,
            &env.current_contract_address(),
            &c.client,
            refund,
        );
        c.funded_amount = c.released_amount;
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
