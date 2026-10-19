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
| `issue_reputation(id, rating)` | client | Rate a completed job (1-5), once |
| `set_paused(bool)` | admin | Full stop, or clear every pause flag |
| `set_pause_flags(flags)` | admin | Pause specific operation categories |
| `is_paused()` / `get_pause_flags()` | - / - | Any category paused / the raw bitmask |
| `get_contract(id)`, `contract_count()`, `get_reputation(addr)`, `get_admin()`, `version()` | - | Reads |

### Pausing

The pause is a bitmask, one bit per operation category, so an incident can stop
new money coming in while still letting users get out. A single boolean could not
express that: pausing for a deposit bug also blocked `cancel_contract`, trapping
every escrow for as long as the investigation lasted.

| Flag | Value | Blocks |
| --- | --- | --- |
| `PAUSE_CREATE` | 1 | `create_contract` |
| `PAUSE_DEPOSIT` | 2 | `deposit_funds` |
| `PAUSE_RELEASE` | 4 | `release_milestone` |
| `PAUSE_DISPUTE` | 8 | `open_dispute` |
| `PAUSE_CANCEL` | 16 | `cancel_contract` |
| `PAUSE_REPUTATION` | 32 | `issue_reputation` |
| `PAUSE_ALL` | 63 | everything above |

`set_paused(true)` sets `PAUSE_ALL` and `set_paused(false)` clears everything, so
the existing ABI and the `paused` event are unchanged. `set_pause_flags` takes any
combination; bits outside `PAUSE_ALL` are rejected rather than stored, since a
stray bit would never be checked and would silently do nothing.

`is_paused()` returns `true` if **any** flag is set, which is not the same as
"fully stopped" — a partial pause reads as `true` while most entry points still
work. Use `get_pause_flags()` to see which categories are actually closed.

`resolve_dispute` is deliberately **not** gated, so an available admin can always
settle a dispute during a pause. That is the case that matters: a partial pause
that keeps exits open does not need the admin, and a full pause does.

A contract paused under the old boolean-only code is still honoured: when no flag
word has ever been written, a stored `Paused == true` is read as `PAUSE_ALL`.
Failing open is the one direction a pause must never go. Writing flags explicitly
clears the legacy key, so the fallback cannot strand a contract.

### Errors

`EscrowError` is part of the public interface: clients map on the numeric code, so
**codes are append-only**. A new variant takes the next free number and is never
inserted in the middle.

| Code | Name | Raised when |
| --- | --- | --- |
| 1 | `AlreadyInitialized` | `initialize` called twice |
| 2 | `NotInitialized` | admin not set |
| 3 | `Unauthorized` | caller is not the admin or a party |
| 4 | `Paused` | the operation's pause category is set |
| 5 | `ContractNotFound` | unknown escrow id |
| 6 | `InvalidParties` | client equals freelancer |
| 7 | `InvalidMilestones` | zero milestones, or more than 20 |
| 8 | `InvalidAmount` | non-positive amount, milestone sum past `i128::MAX`, or pause flags outside `PAUSE_ALL` |
| 9 | `OverFunded` | deposit would exceed the contract total |
| 10 | `InvalidStatus` | the call is not valid in the current status |
| 11 | `MilestoneNotFound` | index out of range |
| 12 | `MilestoneAlreadyReleased` | releasing a released milestone |
| 13 | `InsufficientFunds` | unreleased balance is below the milestone |
| 14 | `InvalidSplit` | `freelancer_bps` above 10 000 |
| 15 | `InvalidRating` | rating outside 1..=5 |
| 16 | `ReputationAlreadyIssued` | rating a job twice |

### Events

| Topics | Data |
| --- | --- |
| `init` | admin |
| `paused` | `bool` |
| `pause` | flags |
| `created`, `id` | (client, freelancer, total) |
| `deposit`, `id` | amount |
| `release`, `id` | (milestone index, amount) |
| `dispute`, `id` | caller |
| `resolved`, `id` | (to_freelancer, to_client) |
| `cancel`, `id` | refund |
| `rep`, `id` | (freelancer, rating) |

Status flow: `Created -> Funded -> Completed`, with `Disputed -> Resolved` and `Cancelled` side exits.

## Build and test

```bash
rustup target add wasm32v1-none      # toolchain pinned in rust-toolchain.toml
cargo build --target wasm32v1-none --release
cargo test
```

The wasm is written to `target/wasm32v1-none/release/escrow.wasm`.

## Deploy

```bash
stellar contract deploy --wasm target/wasm32v1-none/release/escrow.wasm \
  --source <key> --network testnet
stellar contract invoke --id <CONTRACT_ID> --source <key> --network testnet \
  -- initialize --admin <ADMIN_ADDRESS>
```

## License

MIT
