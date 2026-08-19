# Transparency

This document is the full trust model for Shunpike's streaming-payroll
program. It says what we can do, what we cannot do, and what happens in every
failure case. Nothing here asks you to trust us. Every claim points at
something you can check yourself: the source in this repository, the deployed
bytecode, or an account on chain.

> **Status note:** live on mainnet (2026-08-18) with a verified build — the
> on-chain bytecode hash-matches this repository (see the explorer's
> verified-build tab). The addresses below are identical on devnet and
> mainnet because we deploy with the same keypair.

## The addresses

| What | Address |
| --- | --- |
| Program id | `GooHiyMEYnVAUkajiAc33fA9rQjF5K8ZvFojHNzhrShg` |
| Global config PDA | `Bke4WmWnJwmHMJCBA2iDDCfnB77LQBGrdXjG97gr9Kt8` (derived: seed `"config"`) |
| Current admin | `4Fb83Vku87dadiqdCED1We8tdzk61oLx6yoTfMhYm8cW` (genesis; see below) |
| Deployer | `DXjgtYT3mYBRQedKV7YN7hN3WLHqwadEFKWbJRNMcKzX` |

Tools to check with: [Solscan](https://solscan.io) and
[Solana Explorer](https://explorer.solana.com) (both show the verified-build
tab), and the [`solana-verify` docs](https://solana.com/developers/guides/advanced/verified-builds).

**Check the upgrade authority yourself:** run
`solana program show <program id>`. The `Authority` line is the key that can
change this program's code. While we are in the testing window it shows our
deployer. After the burn (below) it shows `none` — at that point the code can
never change again, by anyone, including us.

## What we can do

Two things. Both are in the source, both leave a public audit trail.

1. **Force-settle any stream.** The admin key can end any active stream
   early. The money goes where a normal cancel would send it: everything
   accrued so far to the recipient, the remainder plus all account rent back
   to the funder. The program computes both amounts itself
   (`instructions/settle.rs` — `cancel` and `force_settle` call the same
   function). There is no parameter through which the admin could redirect a
   single lamport anywhere else. Every use emits a `ForceSettled` event,
   distinct from a normal cancel, so you can see exactly how often we have
   used this power and on which streams.
2. **Rotate the admin key.** The current admin can hand the force-settle
   power to a new key (`rotate_admin`). That is the whole list.

Why force-settle exists: if this program ever needs replacing, we deploy the
new one at our own cost and use force-settle to return every user's balance
from the old program automatically. Users do nothing and touch no new
software to be made whole. We consider this a feature worth advertising, not
a back door to hide — which is why it is documented here rather than
discovered by a reader of the bytecode.

Why it is permanent: there is no renounce instruction. We keep this power for
the life of the program, deliberately, because the redeploy pledge below
depends on it.

## What we cannot do

- We cannot touch, freeze, redirect, or fee anyone's funds. No instruction
  exists that moves tokens anywhere except: recipient withdrawals, and the
  fixed two-party settlement math above.
- We cannot charge fees. This is not a zero-fee configuration that could be
  raised later. No fee fields, fee instructions, or fee accounts exist in the
  program. Adding one would require changing the code — which is exactly what
  the authority burn makes impossible.
- After the burn, we cannot change the rules at all. Neither can anyone else.

## The genesis admin is in the source

`initialize_config` takes no admin argument. Whoever calls it first only pays
the rent; the admin it writes is always the `INITIAL_ADMIN` constant compiled
into `state.rs`. Two consequences: front-running our config initialization is
harmless, and you can audit the genesis admin by reading one line of source
instead of trusting a deploy-time transaction.

## The immutability commitment

The program launches with its upgrade authority intact and this is disclosed
plainly: **during the test phase we technically can change the code.** That
phase exists so real production use can shake out defects while a fix is
still cheap, and it has no fixed end date — it runs as long as testing
honestly requires. When it ends, we burn the upgrade authority in a
deliberate, manual, publicly announced step. From then on the program is
immutable. We do not promise a schedule, so you never have to trust one —
`solana program show` tells you the current state any time.

## The redeploy pledge

If a critical problem ever surfaces after the burn:

1. We deploy a corrected program at our own cost (~2–4 SOL of program rent —
   we eat it).
2. We force-settle every active stream on the old program. The math above
   guarantees everyone receives exactly their side of the ledger, plus rent.
3. The app points at the new program.

The old immutable program stays on chain forever. Anyone who prefers it can
keep using it through any client — we cannot remove it, and would not.

## What happens if the admin key is lost

Force-settle dies. Nothing else changes. Users are never trapped: `withdraw`,
`cancel`, and `close_stream` need only the user's own wallet and keep working
for as long as Solana exists. The only casualty would be the redeploy
pledge's automation — users would exit self-service instead.

## The contrast that explains why this program exists

Shunpike began as a client for Zebec's streaming program
(`zSTRMmYcFF8SPdHmsAmAUjBnx4zDHvnqqGz2mPcc5QC`). Running real payroll on it
taught us exactly what we wanted to do differently. Point by point:

| | Zebec's program | This program |
| --- | --- | --- |
| **Upgradeability** | Upgradeable by a single key, indefinitely | Authority burned when the disclosed test phase ends; immutable after |
| **Source** | Closed (only the IDL is public) | Public, MIT-licensed, verified build hash-matched to the deployed bytecode |
| **Fees** | ZBCN fee choreography stapled on by the vendor SDK at create time | No fee code exists |
| **Rent** | Stream accounts are permanent; creation rent (~0.006 SOL/stream) is stranded forever | Every terminal path closes the accounts and refunds all rent to the funder |
| **Pause semantics** | A stream ends at wall-clock end even while paused; pause/cancel then fail on-chain | Resume extends the end time by the pause length; ending-while-paused cannot happen |
| **Custody shape** | The vendor's app deposits to an app-managed flow first | Wallet → per-stream vault → wallet; the program never holds funds outside a stream |

The general point is what we call the worst-quadrant argument. A service
whose operator retains god-mode over an opaque program gives users neither of
the two good deals: not a bank's accountability (no regulator, no charter, no
recourse), and not code's guarantees (the code can change under you). We
picked a lane — code — and made every remaining human power narrow,
disclosed, and provable on chain. This table is the only place Zebec is named
in this repository; the program stands on its own.

## Verify, don't trust

1. Read the source (start at `programs/shunpike_stream/src/lib.rs` — it is
   short).
2. Confirm the deployed bytecode matches this source: the explorer's
   "verified build" tab, or run `solana-verify` yourself against this repo.
3. Check the upgrade authority: `solana program show <program id>`.
4. Check the admin's history: filter the program's transactions for
   `ForceSettled` events.

If any of those checks ever fails to match what this document claims, the
document is wrong and the chain is right — and we would expect you to say so
loudly.
