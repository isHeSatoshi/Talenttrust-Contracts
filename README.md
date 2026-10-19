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
| `open_dispute(id, caller)` | client or freelancer | Stake the dispute bond and freeze the escrow |
| `resolve_dispute(id, freelancer_bps, bond_to_opener)` | admin | Split the held balance and settle the bond |
| `cancel_contract(id)` | client | Refund unreleased funds and close |
| `issue_reputation(id, rating)` | client | Rate a completed job (1-5), once |
| `set_paused(bool)` / `is_paused()` | admin / - | Circuit breaker for all mutations |
| `get_contract(id)`, `contract_count()`, `get_reputation(addr)`, `get_admin()`, `get_dispute_bond(id)`, `version()` | - | Reads |

Status flow: `Created -> Funded -> Completed`, with `Disputed -> Resolved` and `Cancelled` side exits.

### Dispute bond

A `Disputed` escrow blocks both `release_milestone` and `cancel_contract` until the
admin acts, so without a cost either party could freeze funds for free. Opening a
dispute therefore stakes a bond in the escrow token:

```
bond = max(DISPUTE_BOND_MIN, total_amount * DISPUTE_BOND_BPS / BPS_DENOMINATOR)
DISPUTE_BOND_BPS = 200   (2%)
DISPUTE_BOND_MIN = 0     (purely proportional; raise to price every dispute)
```

- `open_dispute` transfers the bond from the caller **before** changing the
  status, so a caller who cannot pay cannot freeze the escrow.
- The bond is stored under `DataKey::DisputeBond(id) -> (opener, amount)` and is
  held **outside** the escrow balance. It is never part of the split.
- `resolve_dispute` takes `bond_to_opener`: `true` returns the bond to whoever
  opened the dispute, `false` forfeits it to the other party.
- `get_dispute_bond(id)` returns `Option<(Address, i128)>`.
- A total small enough to round the bond to `0` makes the dispute free again;
  that is the documented consequence of `DISPUTE_BOND_MIN = 0`.

Events: `dispute(id) -> (caller, bond)`, `resolved(id) -> (to_freelancer, to_client, bond_recipient, bond)`.

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
