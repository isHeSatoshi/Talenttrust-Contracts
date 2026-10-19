//! A second contract, used only to prove that `execute_upgrade` really swaps the
//! running code.
//!
//! It is deliberately tiny and has no storage of its own: the escrow's upgrade
//! test deploys the escrow, upgrades to this, calls `fixture_marker` (which only
//! exists here), then upgrades back and checks the escrow's data is untouched.
//! That covers both halves of "the upgrade took effect" and "existing data
//! survives it", without this crate duplicating any of the escrow's storage
//! layout.
#![no_std]

use soroban_sdk::{contract, contractimpl, Env};

/// The value `fixture_marker` returns. Chosen to be recognisable in a failure
/// message, so a mismatched call is obvious at a glance.
pub const MARKER: u32 = 0xF1_57_00;

#[contract]
pub struct EscrowFixture;

#[contractimpl]
impl EscrowFixture {
    /// Present only in this contract, so a successful call proves the escrow's
    /// code has been replaced.
    pub fn fixture_marker(_env: Env) -> u32 {
        MARKER
    }

    /// Named the same as the escrow's function so the swap is visible in the
    /// return value rather than as a missing-function error.
    pub fn version(_env: Env) -> u32 {
        MARKER
    }
}
