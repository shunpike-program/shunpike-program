//! Account state + the accrual math.
//!
//! The accrual function here is a Rust port of the app's battle-tested TS
//! derivation (`src/lib/streamState.ts` in the app repo). The TS unit-test
//! vectors are replayed against this implementation in the tests below — the
//! same numbers must fall out of both, forever. If you change one side, you
//! change the other.

use anchor_lang::prelude::*;

/// PDA seed for the singleton [`GlobalConfig`].
pub const CONFIG_SEED: &[u8] = b"config";
/// PDA seed prefix for [`Stream`] accounts.
pub const STREAM_SEED: &[u8] = b"stream";

/// A stream's start may sit at most this many seconds in the past at create
/// time. Mirrors the app's MIN_START_LEAD_SEC approval-race tolerance: the
/// wallet-approval popup can eat most of a minute between building and landing
/// the transaction, and rejecting that race would fail honest "start now"
/// streams. Anything older is a real client bug and is rejected.
pub const START_LEAD_TOLERANCE_SEC: i64 = 60;

/// Stream names are fixed-width on chain (no heap, cheap deserialization).
pub const MAX_NAME_LEN: usize = 32;

/// The initial admin, hard-coded in source on purpose (see TRANSPARENCY.md):
/// `initialize_config` takes NO admin argument — whoever calls it first merely
/// pays the rent; the admin it writes is always this pubkey. That makes the
/// classic "front-run the config initializer" attack harmless by construction,
/// and it makes the genesis admin auditable by reading this file. Rotatable
/// afterwards via `rotate_admin`.
pub const INITIAL_ADMIN: Pubkey = anchor_lang::solana_program::pubkey!("4Fb83Vku87dadiqdCED1We8tdzk61oLx6yoTfMhYm8cW");

/// Singleton program config. Holds the one narrow power this program retains:
/// the admin pubkey allowed to call `force_settle` (and to rotate itself).
/// No fee fields exist here — or anywhere. That absence is the product.
#[account]
#[derive(InitSpace)]
pub struct GlobalConfig {
    /// The only key that may call `force_settle` / `rotate_admin`. Checked by
    /// wallet signature — a program cannot check a secret passphrase
    /// (transactions are public; a first use would leak it), so the app-layer
    /// passphrase stays app-layer and THIS is the on-chain gate.
    pub admin: Pubkey,
    pub bump: u8,
}

/// One payment stream. PDA seeds are `["stream", funder, seed_le_bytes]` —
/// deliberately WITHOUT the recipient (the design doc's original sketch
/// included it): `change_recipient` mutates the recipient, and an address
/// derived from the original recipient could never be re-derived after a
/// change. The client-chosen `seed` (millisecond timestamp by convention)
/// keeps (funder, seed) unique across a funder's streams.
#[account]
#[derive(InitSpace)]
pub struct Stream {
    /// Who funded the deposit. Receives the unstreamed remainder + ALL rent
    /// back at cancel/close/force_settle — rent is a refundable deposit here,
    /// not a burnt cost.
    pub funder: Pubkey,
    /// Current payee. Mutable via `change_recipient` (funder-signed).
    pub recipient: Pubkey,
    /// The streamed SPL token mint (classic SPL Token only in v1 — Token-2022
    /// extensions like transfer fees/hooks could break the exact-zero vault
    /// invariant and are out of scope until deliberately designed for).
    pub mint: Pubkey,
    /// Total deposited at create, base units. Fixed for the stream's life —
    /// no top-up instruction exists, by design (clean per-stream history;
    /// cancel+recreate is ~free once rent refunds).
    pub deposited: u64,
    /// Cumulative amount already paid to the recipient, base units.
    pub withdrawn: u64,
    /// Unix seconds. May be in the future (scheduled streams).
    pub start_time: i64,
    /// Unix seconds. INVARIANT: `end_time - start_time - paused_total` equals
    /// the original duration forever — `resume` pushes `end_time` right by
    /// exactly the pause length, so the accrual rate never changes and a
    /// stream can never "end while paused" (the Zebec bug class this program
    /// exists to eliminate).
    pub end_time: i64,
    /// When the current pause began; 0 = running. Pausing is only legal
    /// between start and end, so a nonzero value always sits inside the
    /// stream's active window.
    pub paused_at: i64,
    /// Total seconds spent in COMPLETED pauses (the current pause, if any, is
    /// not included until `resume` folds it in).
    pub paused_total: i64,
    /// Client-chosen uniquifier baked into the PDA seeds. Stored so the
    /// program can re-derive its own signer seeds for vault transfers.
    pub seed: u64,
    /// UTF-8, zero-padded. Display-only.
    pub name: [u8; MAX_NAME_LEN],
    pub bump: u8,
}

impl Stream {
    /// The stream's constant total streaming duration (see `end_time` invariant).
    pub fn duration(&self) -> i64 {
        self.end_time - self.start_time - self.paused_total
    }

    /// Total accrued to the recipient as of `now`, base units.
    pub fn accrued_at(&self, now: i64) -> u64 {
        accrued(
            self.deposited,
            self.start_time,
            self.end_time,
            self.paused_at,
            self.paused_total,
            now,
        )
    }
}

/// Linear accrual with pause-freeze — the money function.
///
/// Pivot priority mirrors the TS reference (cancel has no pivot here because a
/// cancelled stream is settled and CLOSED in the same transaction — there is no
/// post-cancel state to derive): paused → frozen at `paused_at`; running →
/// `now`, clamped to `end_time`.
///
/// Integer math notes:
/// - u128 intermediate: max deposit (u64::MAX) × max elapsed can't overflow.
/// - Floor division means mid-stream reads can under-report by at most one
///   base unit (dust) — but at `elapsed >= duration` the full `deposited` is
///   returned EXACTLY, so final settlements zero the vault to the atom.
pub fn accrued(
    deposited: u64,
    start_time: i64,
    end_time: i64,
    paused_at: i64,
    paused_total: i64,
    now: i64,
) -> u64 {
    let pivot = if paused_at != 0 {
        // paused_at is always < end_time (pause rejects ended streams), but
        // clamp defensively — over-paying the recipient is the one direction
        // this function must never round.
        paused_at.min(end_time)
    } else {
        now.min(end_time)
    };
    let duration = end_time - start_time - paused_total;
    let elapsed = (pivot - start_time - paused_total).max(0);
    // duration <= 0 cannot exist on chain (create rejects zero duration and
    // the resume invariant preserves it) — treat defensively as fully accrued
    // rather than dividing by zero.
    if duration <= 0 || elapsed >= duration {
        return deposited;
    }
    ((deposited as u128) * (elapsed as u128) / (duration as u128)) as u64
}

/// The two legs of a settlement (cancel / force_settle share this math — the
/// admin path is DEFINED as "exactly what cancel would pay", so there is no
/// parameter through which force_settle could redirect a lamport).
pub struct SettlementSplit {
    /// accrued − already-withdrawn → recipient.
    pub to_recipient: u64,
    /// deposited − accrued → funder.
    pub to_funder: u64,
}

/// Compute the settlement split at `now`. INVARIANT (asserted by callers
/// against the live vault balance): `to_recipient + to_funder ==
/// deposited − withdrawn == vault balance` — the vault always zeroes exactly.
pub fn settlement_split(stream: &Stream, now: i64) -> SettlementSplit {
    let accrued = stream.accrued_at(now);
    SettlementSplit {
        // withdrawn can never exceed accrued (withdraw pays accrued-withdrawn
        // and accrued is monotonic), but saturate rather than panic in the
        // money path.
        to_recipient: accrued.saturating_sub(stream.withdrawn),
        to_funder: stream.deposited.saturating_sub(accrued),
    }
}

// ---------------------------------------------------------------------------
// Unit tests — the TS reference vectors from the app's streamState.test.ts,
// replayed in base units (the TS suite uses 100 human units of a 6-decimal
// token → 100_000_000 base units here). Same numbers, both sides, forever.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// TS base vector: 100-unit stream over 1000s starting at t=1000.
    const DEP: u64 = 100_000_000;
    const START: i64 = 1_000;
    const END: i64 = 2_000;

    fn base_stream() -> Stream {
        Stream {
            funder: Pubkey::new_unique(),
            recipient: Pubkey::new_unique(),
            mint: Pubkey::new_unique(),
            deposited: DEP,
            withdrawn: 0,
            start_time: START,
            end_time: END,
            paused_at: 0,
            paused_total: 0,
            seed: 1,
            name: [0; MAX_NAME_LEN],
            bump: 255,
        }
    }

    #[test]
    fn accrues_linearly_between_start_and_end() {
        // TS: fraction 0.5, streamed 50 at now=1500.
        assert_eq!(accrued(DEP, START, END, 0, 0, 1_500), 50_000_000);
    }

    #[test]
    fn zero_before_start_full_after_end() {
        // TS: fraction 0 at 900, clamped to 1 at 9000.
        assert_eq!(accrued(DEP, START, END, 0, 0, 900), 0);
        assert_eq!(accrued(DEP, START, END, 0, 0, 9_000), DEP);
    }

    #[test]
    fn subtracts_previously_paused_seconds() {
        // TS: 600s wall clock, 200 spent paused → 400/1000 accrued = 40.
        // On-chain the resume invariant also pushed end_time right by 200.
        assert_eq!(accrued(DEP, START, END + 200, 0, 200, 1_600), 40_000_000);
    }

    #[test]
    fn freezes_at_paused_at_not_now() {
        // TS: paused at 1250, read at 1900 → fraction 0.25.
        assert_eq!(accrued(DEP, START, END, 1_250, 0, 1_900), 25_000_000);
    }

    #[test]
    fn exact_full_payout_at_end_no_dust() {
        // elapsed == duration must return deposited EXACTLY, including awkward
        // amounts that don't divide evenly.
        let odd = 33_333_337;
        assert_eq!(accrued(odd, START, END, 0, 0, END), odd);
        assert_eq!(accrued(odd, START, END, 0, 0, END + 1), odd);
    }

    #[test]
    fn floor_division_rounds_toward_zero_mid_stream() {
        // 1 base unit over 1000s: floor keeps it at 0 until the very end.
        assert_eq!(accrued(1, START, END, 0, 0, 1_999), 0);
        assert_eq!(accrued(1, START, END, 0, 0, 2_000), 1);
    }

    #[test]
    fn duration_zero_defends_without_division() {
        // Cannot exist on chain; defensive branch returns fully-accrued.
        assert_eq!(accrued(DEP, START, START, 0, 0, 1_500), DEP);
    }

    #[test]
    fn settlement_split_nets_out_withdrawn() {
        // TS: withdrawn 30 at now=1500 → withdrawable 20 (of streamed 50).
        let mut s = base_stream();
        s.withdrawn = 30_000_000;
        let split = settlement_split(&s, 1_500);
        assert_eq!(split.to_recipient, 20_000_000);
        assert_eq!(split.to_funder, 50_000_000);
    }

    #[test]
    fn settlement_split_never_negative() {
        // TS: withdrawn 80 > streamed 50 (possible transiently around a
        // settlement) → withdrawable clamps to 0, never negative.
        let mut s = base_stream();
        s.withdrawn = 80_000_000;
        let split = settlement_split(&s, 1_500);
        assert_eq!(split.to_recipient, 0);
    }

    #[test]
    fn settlement_always_zeroes_the_vault_exactly() {
        // The load-bearing invariant: to_recipient + to_funder must equal
        // deposited - withdrawn at EVERY instant, for awkward numbers too.
        for (dep, wd, now) in [
            (33_333_337u64, 0u64, 1_337i64),
            (999_999_999, 123_456, 1_001),
            (7, 0, 1_999),
            (1_000_000, 999_999, 1_500),
            (DEP, 49_999_999, 1_500),
        ] {
            let mut s = base_stream();
            s.deposited = dep;
            s.withdrawn = wd;
            let split = settlement_split(&s, now);
            assert_eq!(
                split.to_recipient + split.to_funder,
                dep - wd.min(s.accrued_at(now)),
                "vault must zero for dep={dep} wd={wd} now={now}"
            );
        }
    }

    #[test]
    fn resume_invariant_preserves_rate() {
        // Pause 300s at t=1400, resume at 1700: end 2000→2300, paused_total
        // 0→300. duration() stays 1000; accrual at resume instant equals the
        // frozen value; accrual continues at the original rate afterwards.
        let mut s = base_stream();
        s.paused_at = 1_400;
        let frozen = s.accrued_at(1_700);
        assert_eq!(frozen, 40_000_000);
        // resume:
        s.end_time += 300;
        s.paused_total += 300;
        s.paused_at = 0;
        assert_eq!(s.duration(), 1_000);
        assert_eq!(s.accrued_at(1_700), frozen);
        assert_eq!(s.accrued_at(1_800), 50_000_000);
        assert_eq!(s.accrued_at(2_300), DEP);
    }
}
