# TalentTrust Contracts

## Live

- Website: https://talenttrust-frontend.vercel.app
- Docs: https://talenttrust-frontend.vercel.app/docs
- API: https://talenttrust-backend.vercel.app (docs at [/docs](https://talenttrust-backend.vercel.app/docs))
- Contract (testnet): [`CCZMIVUBMG5SBC47SDTC32RWLBJ2HW2DCFVWJEOKCH2IFJBZGBKTXK5W`](https://stellar.expert/explorer/testnet/contract/CCZMIVUBMG5SBC47SDTC32RWLBJ2HW2DCFVWJEOKCH2IFJBZGBKTXK5W)

Soroban smart contract for the TalentTrust freelancer escrow protocol on Stellar.
Clients lock payment for a job into escrow, release it milestone by milestone as
work is approved, and either party can raise a dispute that the protocol admin settles.

## Deployments

| Network | Contract | ID | Explorer |
| --- | --- | --- | --- |
| Stellar testnet | escrow | `CCZMIVUBMG5SBC47SDTC32RWLBJ2HW2DCFVWJEOKCH2IFJBZGBKTXK5W` | [stellar.expert](https://stellar.expert/explorer/testnet/contract/CCZMIVUBMG5SBC47SDTC32RWLBJ2HW2DCFVWJEOKCH2IFJBZGBKTXK5W) |

The contract is initialized with admin `GA7TXEBMEPQCLHIYCWDN7RKAO5IMRD2UV7O7TZXMEW6VZUMEXNR4XBB5`.
Machine-readable details are in [deployments/testnet.json](deployments/testnet.json).

## Contract interface (`contracts/escrow`)

| Function | Auth | Description |
| --- | --- | --- |
| `initialize(admin)` | admin | One-time setup |
| `create_contract(client, freelancer, token, milestone_amounts) -> u64` | client | Create an escrow with up to 20 milestones |
| `deposit_funds(id, amount)` | client | Transfer tokens into escrow (cannot exceed total) |
| `release_milestone(id, index)` | client | Pay the freelancer for a milestone |
| `open_dispute(id, caller)` | client or freelancer | Freeze the escrow pending resolution |
| `resolve_dispute(id, freelancer_bps)` | admin | Split the held balance (basis points to freelancer) |
| `cancel_contract(id)` | client | Refund unreleased funds and close |
| `schedule_upgrade(wasm_hash)` | admin | Announce a wasm upgrade, executable after 48 hours |
| `cancel_upgrade()` | admin | Withdraw an announced upgrade |
| `execute_upgrade()` | admin | Apply an announced upgrade once its delay has elapsed |
| `issue_reputation(id, rating)` | client | Rate a completed job (1-5), once |
| `set_paused(bool)` / `is_paused()` | admin / - | Circuit breaker for all mutations |
| `get_contract(id)`, `contract_count()`, `get_reputation(addr)`, `get_admin()`, `version()`, `get_pending_upgrade()` | - | Reads |

Status flow: `Created -> Funded -> Completed`, with `Disputed -> Resolved` and `Cancelled` side exits.

### Upgrades

The contract can be upgraded in place, so a bug fix does not strand live escrows
on the old code. An upgrade is **announced** first and only takes effect after
`UPGRADE_DELAY_SECS` (48 hours), which gives clients and freelancers a window to
release, cancel or dispute before new code runs.

```bash
# 1. upload the new wasm and note the hash it prints
stellar contract upload --wasm target/wasm32v1-none/release/escrow.wasm \
  --source <key> --network testnet
# 2. announce it (starts the 48h clock)
stellar contract invoke --id <CONTRACT_ID> --source <key> --network testnet \
  -- schedule_upgrade --wasm_hash <HASH>
# 3. after 48 hours, apply it
stellar contract invoke --id <CONTRACT_ID> --source <key> --network testnet \
  -- execute_upgrade
```

`execute_upgrade` requires the wasm to have been uploaded already, otherwise the
call fails. Re-announcing replaces the pending upgrade and **restarts** the
clock, so an admin cannot announce something harmless, wait out the delay, and
then swap in a different hash. `version()` reads the stored counter, which starts
at 1 and increments on each executed upgrade.

An upgrade is allowed while paused: a pause is a response to a live incident, and
an upgrade is how a code-level incident gets fixed. The delay already provides the
exit window, and `emergency_withdraw` works during a pause.

### Errors

`EscrowError` is part of the public interface: clients map on the numeric code, so
**codes are append-only**. A new variant takes the next free number and is never
inserted in the middle.

| Code | Name | Raised when |
| --- | --- | --- |
| 1 | `AlreadyInitialized` | `initialize` called twice |
| 2 | `NotInitialized` | admin not set |
| 3 | `Unauthorized` | caller is not the admin or a party |
| 4 | `Paused` | a mutation while the circuit breaker is on |
| 5 | `ContractNotFound` | unknown escrow id |
| 6 | `InvalidParties` | client equals freelancer |
| 7 | `InvalidMilestones` | zero milestones, or more than 20 |
| 8 | `InvalidAmount` | non-positive amount, or the milestones sum past `i128::MAX` |
| 9 | `OverFunded` | deposit would exceed the contract total |
| 10 | `InvalidStatus` | the call is not valid in the current status |
| 11 | `MilestoneNotFound` | index out of range |
| 12 | `MilestoneAlreadyReleased` | releasing a released milestone |
| 13 | `InsufficientFunds` | unreleased balance is below the milestone |
| 14 | `InvalidSplit` | `freelancer_bps` above 10 000 |
| 15 | `InvalidRating` | rating outside 1..=5 |
| 16 | `ReputationAlreadyIssued` | rating a job twice |
| 17 | `NoPendingUpgrade` | cancelling or executing with nothing announced |
| 18 | `UpgradeNotReady` | executing before the announced time |

### Events

| Topics | Data |
| --- | --- |
| `init` | admin |
| `paused` | `bool` |
| `created`, `id` | (client, freelancer, total) |
| `deposit`, `id` | amount |
| `release`, `id` | (milestone index, amount) |
| `dispute`, `id` | caller |
| `resolved`, `id` | (to_freelancer, to_client) |
| `cancel`, `id` | refund |
| `upg_sched` | (wasm_hash, eta) |
| `upg_cncl` | wasm_hash |
| `upgraded` | (wasm_hash, new_version) |
| `rep`, `id` | (freelancer, rating) |

## Build and test

```bash
rustup target add wasm32v1-none      # toolchain pinned in rust-toolchain.toml
cargo build --target wasm32v1-none --release
cargo test
```

The wasm is written to `target/wasm32v1-none/release/escrow.wasm`.

**Build before you test.** The upgrade integration test swaps real wasm bytes, so
it reads `escrow.wasm` and `escrow_fixture.wasm` from that directory at runtime.
Running `cargo test` on a clean checkout fails with an explicit message telling
you to build first; CI already does the build in the step above. `escrow_fixture`
is a tiny test-only contract in this workspace — it is never deployed.

## Deploy

```bash
stellar contract deploy --wasm target/wasm32v1-none/release/escrow.wasm \
  --source <key> --network testnet
stellar contract invoke --id <CONTRACT_ID> --source <key> --network testnet \
  -- initialize --admin <ADMIN_ADDRESS>
```

## License

MIT
