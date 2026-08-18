# shunpike-program

The on-chain program behind [Shunpike](https://shunpike.azurewebsites.net), a
streaming-payroll service on Solana. This repository is public on purpose:
every guarantee the service makes is checkable by reading this source and
comparing it (via [verified builds](https://solana.com/developers/guides/advanced/verified-builds))
to the deployed bytecode.

**Read [TRANSPARENCY.md](TRANSPARENCY.md) first** — it is the full trust
model: what we can do, what we cannot do, and what happens in every failure
case.

## What the program does

Linear streaming payments of any classic SPL token from a funder to a
recipient over a fixed window:

- `create_stream` — deposit moves wallet → per-stream vault in one
  instruction. No setup accounts, no whitelists, no quotes.
- `withdraw` — the recipient pulls whatever has accrued, any time.
- `pause` / `resume` — pausing freezes accrual; resuming extends the end time
  by exactly the pause length, so the streaming rate never changes and a
  paused stream can never silently end.
- `change_recipient` — payee wallet rotation without tearing the stream down.
- `cancel` — settles both sides (accrued → recipient, remainder → funder),
  closes every account, and refunds all rent to the funder. One transaction,
  at any point in the stream's life.
- `close_stream` — permissionless rent reclamation for a fully-played-out
  stream.
- `force_settle` — the one admin power: settle any stream with *exactly*
  `cancel`'s math. Destinations and amounts are fixed by the program; no
  parameter exists that could redirect funds. See TRANSPARENCY.md for why
  this exists and why it is permanent.
- `rotate_admin` / `initialize_config` — admin key management. The genesis
  admin is a constant in [`state.rs`](programs/shunpike_stream/src/state.rs),
  not a deploy-time argument.

Three properties, verifiable by reading the code:

1. **No fee machinery exists.** No fee fields, no fee instructions, no fee
   accounts. Not configured to zero — absent.
2. **Rent is a refundable deposit.** Every terminal path returns all account
   rent to the funder.
3. **The retained power is narrow and audited.** `force_settle` emits its own
   event, distinct from a funder's cancel, so every use is publicly visible.

## Layout

```
programs/shunpike_stream/
  src/state.rs          accounts + accrual math (+ reference-vector unit tests)
  src/instructions/     one module per concern
  src/errors.rs         every failure mode, enumerated
  src/events.rs         the indexing surface
  tests/integration.rs  LiteSVM suite: every instruction, every error path,
                        adversarial signers, rounding-dust settlement checks
```

## Building and testing

Toolchain: Rust (stable), Solana CLI (Agave), Anchor 0.31.1. On Windows, use
WSL2 — `scripts/wsl-setup.sh` installs everything inside Ubuntu.

```bash
anchor build   # compiles the program + emits the IDL
cargo test     # unit tests + LiteSVM integration suite (needs the build first)
```

## License

MIT — see [LICENSE](LICENSE).
