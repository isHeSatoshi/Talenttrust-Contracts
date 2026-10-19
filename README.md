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
| `set_paused(bool)` / `is_paused()` | admin / - | Circuit breaker for all mutations |
| `set_fee(fee_bps, treasury)` | admin | Set the protocol fee rate (capped at 5%) and its destination |
| `get_contract(id)`, `contract_count()`, `get_reputation(addr)`, `get_admin()`, `version()`, `get_fee_config()`, `get_escrow_fee(id)` | - | Reads |

Status flow: `Created -> Funded -> Completed`, with `Disputed -> Resolved` and `Cancelled` side exits.

### Protocol fee

`set_fee(fee_bps, treasury)` sets a protocol fee in basis points, capped at
`MAX_FEE_BPS` (500, i.e. 5%), and the address that receives it. Both are written
together, so a non-zero rate always has a destination. The default is 0, which is
the behaviour the contract had before fees existed.

**The rate is snapshotted per escrow at creation.** `create_contract` copies the
current rate into `DataKey::EscrowFee(id)`, and every payout reads that snapshot.
A later `set_fee` therefore applies only to escrows created afterwards. Without
this, an admin could raise the fee on an escrow that was agreed and funded at a
lower rate, which is the whole reason the snapshot exists.

The fee is charged on:

- **`release_milestone`** — `floor(m.amount * fee_bps / 10_000)` to the treasury,
  the rest to the freelancer.
- **`resolve_dispute`** — on the freelancer's share only. The client's refund is
  its own money coming back, not payment for work, so charging a fee on it would
  take from the wrong party.

It is **not** charged on `cancel_contract`: nothing was delivered, so there is no
release to charge against.

Rounding is always down, so a fee that rounds to zero charges nothing and no
treasury transfer is made at all. A milestone amount is only bounded by
`i128::MAX`, so the fee is computed by dividing before multiplying — the direct
`amount * bps` form would overflow and trap for large amounts.

### Errors

`EscrowError` is part of the public interface: clients map on the numeric code, so
**codes are append-only**. A new variant takes the next free number and is never
inserted in the middle.

| Code | Name | Raised when |
| --- | --- | --- |
| 1 | `AlreadyInitialized` | `initialize` called twice |
| 2 | `NotInitialized` | admin not set, or a fee is owed with no treasury configured |
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
| 17 | `FeeTooHigh` | `set_fee` above `MAX_FEE_BPS` |

### Events

| Topics | Data |
| --- | --- |
| `init` | admin |
| `paused` | `bool` |
| `fee_set` | (fee_bps, treasury) |
| `created`, `id` | (client, freelancer, total) |
| `deposit`, `id` | amount |
| `release`, `id` | (milestone index, amount, fee) |
| `dispute`, `id` | caller |
| `resolved`, `id` | (to_freelancer, to_client, fee) |
| `cancel`, `id` | refund |
| `rep`, `id` | (freelancer, rating) |

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
