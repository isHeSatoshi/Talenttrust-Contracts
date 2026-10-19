//! TalentTrust escrow contract for milestone-based freelancer payments on Stellar (Soroban).
//!
//! Lifecycle:
//! 1. `initialize(admin)` once after deployment.
//! 2. A client calls `create_contract(client, freelancer, token, milestone_amounts)`.
//! 3. The client funds the escrow with `deposit_funds` (real token transfer into the contract).
//! 4. The client approves work with `release_milestone`, paying the freelancer per milestone.
//! 5. Either party may `open_dispute`; the admin settles with `resolve_dispute`.
//! 6. Or the two parties agree a split themselves: `propose_settlement` then
//!    `accept_settlement`, which closes the escrow without the admin.
//! 7. The client can `cancel_contract` before completion to recover unreleased funds.
//! 8. After completion the client may `issue_reputation` for the freelancer.
#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

pub const MAX_MILESTONES: u32 = 20;
pub const BPS_DENOMINATOR: i128 = 10_000;
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
    /// The acceptor is not the other party, or did not echo the proposed amount.
    SettlementMismatch = 17,
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

/// A pending, two-signature agreement to close an escrow at a fixed split.
///
/// `freelancer_amount` is an absolute amount, not a ratio, and is only ever
/// honoured against the balance that existed when it was proposed: `deposit_funds`
/// and `release_milestone` both delete the proposal, so it can never be accepted
/// against a balance it was not quoted for.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Settlement {
    pub proposer: Address,
    pub freelancer_amount: i128,
    pub created_at: u64,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    Paused,
    NextId,
    Contract(u64),
    Reputation(Address),
    Settlement(u64),
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

/// Drops any pending settlement proposal.
///
/// Called from every path that moves `funded_amount` or `released_amount`, so a
/// proposal can only ever be accepted against the balance it was quoted for.
fn clear_settlement(env: &Env, id: u64) {
    let key = DataKey::Settlement(id);
    if env.storage().persistent().has(&key) {
        env.storage().persistent().remove(&key);
    }
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
        // The balance changed, so any pending split no longer refers to it.
        clear_settlement(&env, id);
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
        transfer(
            &env,
            &c.token,
            &env.current_contract_address(),
            &c.freelancer,
            m.amount,
        );
        m.released = true;
        c.released_amount += m.amount;
        c.milestones.set(index, m.clone());
        if c.milestones.iter().all(|x| x.released) {
            c.status = ContractStatus::Completed;
        }
        // The balance changed, so any pending split no longer refers to it.
        clear_settlement(&env, id);
        save(&env, &c);
        env.events()
            .publish((symbol_short!("release"), id), (index, m.amount));
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

    /// Either party proposes closing the escrow at an agreed split, without the admin.
    ///
    /// `freelancer_amount` is an absolute amount of the held balance
    /// (`funded_amount - released_amount`); the rest goes to the client. Any
    /// earlier proposal is replaced, so only one can be pending at a time.
    pub fn propose_settlement(
        env: Env,
        id: u64,
        proposer: Address,
        freelancer_amount: i128,
    ) -> Result<(), EscrowError> {
        require_not_paused(&env)?;
        let c = load(&env, id)?;
        proposer.require_auth();
        if proposer != c.client && proposer != c.freelancer {
            return Err(EscrowError::Unauthorized);
        }
        if !matches!(
            c.status,
            ContractStatus::Created | ContractStatus::Funded | ContractStatus::Disputed
        ) {
            return Err(EscrowError::InvalidStatus);
        }
        let held = c.funded_amount - c.released_amount;
        if freelancer_amount < 0 || freelancer_amount > held {
            return Err(EscrowError::InvalidAmount);
        }
        let settlement = Settlement {
            proposer: proposer.clone(),
            freelancer_amount,
            created_at: env.ledger().timestamp(),
        };
        let key = DataKey::Settlement(id);
        env.storage().persistent().set(&key, &settlement);
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );
        env.events().publish(
            (symbol_short!("settle_p"), id),
            (proposer, freelancer_amount),
        );
        Ok(())
    }

    /// The other party accepts, echoing the proposed amount exactly.
    ///
    /// Echoing the amount is what makes this safe to sign: the proposer cannot
    /// swap the split between the counterparty reading it and the counterparty
    /// signing it. A stale proposal (replaced by a newer one) fails here rather
    /// than settling at a split the acceptor never saw. Both sides are paid and
    /// the escrow closes as `Resolved`, so the admin can no longer intervene.
    pub fn accept_settlement(
        env: Env,
        id: u64,
        acceptor: Address,
        freelancer_amount: i128,
    ) -> Result<(), EscrowError> {
        require_not_paused(&env)?;
        let mut c = load(&env, id)?;
        acceptor.require_auth();
        if acceptor != c.client && acceptor != c.freelancer {
            return Err(EscrowError::Unauthorized);
        }
        if !matches!(
            c.status,
            ContractStatus::Created | ContractStatus::Funded | ContractStatus::Disputed
        ) {
            return Err(EscrowError::InvalidStatus);
        }
        let key = DataKey::Settlement(id);
        let settlement: Settlement = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(EscrowError::InvalidStatus)?;
        if acceptor == settlement.proposer || freelancer_amount != settlement.freelancer_amount {
            return Err(EscrowError::SettlementMismatch);
        }
        let held = c.funded_amount - c.released_amount;
        // Redundant today: deposit_funds and release_milestone both delete the
        // proposal, so `held` cannot have moved since it was written. Kept as a
        // fail-closed guard in case a future path adjusts the balance.
        if settlement.freelancer_amount < 0 || settlement.freelancer_amount > held {
            return Err(EscrowError::SettlementMismatch);
        }
        let to_freelancer = settlement.freelancer_amount;
        let to_client = held - to_freelancer;
        let this = env.current_contract_address();
        transfer(&env, &c.token, &this, &c.freelancer, to_freelancer);
        transfer(&env, &c.token, &this, &c.client, to_client);
        env.storage().persistent().remove(&key);
        c.released_amount += to_freelancer;
        c.funded_amount = c.released_amount;
        c.status = ContractStatus::Resolved;
        save(&env, &c);
        env.events()
            .publish((symbol_short!("settled"), id), (to_freelancer, to_client));
        Ok(())
    }

    /// The proposer retracts their own pending proposal.
    ///
    /// Deliberately not pause-gated: it only ever removes state, so a party must
    /// always be able to pull an offer they no longer stand behind. Only the
    /// proposer may withdraw, so a counterparty cannot erase an offer it simply
    /// does not want (it can decline by not accepting, and the next deposit or
    /// release invalidates the proposal anyway).
    pub fn withdraw_settlement(env: Env, id: u64, proposer: Address) -> Result<(), EscrowError> {
        load(&env, id)?;
        proposer.require_auth();
        let key = DataKey::Settlement(id);
        let settlement: Settlement = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(EscrowError::InvalidStatus)?;
        if proposer != settlement.proposer {
            return Err(EscrowError::Unauthorized);
        }
        env.storage().persistent().remove(&key);
        env.events().publish((symbol_short!("settle_w"), id), proposer);
        Ok(())
    }

    /// Reads the pending settlement proposal, if any.
    ///
    /// Returns `ContractNotFound` for an unknown escrow, so a caller can tell
    /// "no such escrow" apart from "no proposal pending".
    pub fn get_settlement(env: Env, id: u64) -> Result<Option<Settlement>, EscrowError> {
        load(&env, id)?;
        Ok(env.storage().persistent().get(&DataKey::Settlement(id)))
    }

    /// Client cancels the agreement and recovers any unreleased funds.
    pub fn cancel_contract(env: Env, id: u64) -> Result<(), EscrowError> {
        require_not_paused(&env)?;
        let mut c = load(&env, id)?;
        c.client.require_auth();
        if !matches!(c.status, ContractStatus::Created | ContractStatus::Funded) {
            return Err(EscrowError::InvalidStatus);
        }
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
