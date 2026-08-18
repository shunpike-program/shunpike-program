//! # shunpike_stream
//!
//! A streaming-payroll program with three properties its predecessors lack,
//! each verifiable by reading this repo against the deployed bytecode
//! (solana-verify):
//!
//! 1. **No fee machinery exists.** Not a zero-fee configuration — there are
//!    no fee fields, no fee instructions, no fee accounts anywhere in this
//!    program. "There is no toll booth", not "the toll is set to zero".
//! 2. **Rent is a refundable deposit.** Every settlement path (cancel, close,
//!    force_settle) closes the stream + vault accounts and returns their rent
//!    to the funder. Nothing is permanently stranded on chain.
//! 3. **One narrow, disclosed, permanent admin power.** `force_settle` lets
//!    the config admin settle any stream early — with destinations and
//!    amounts fixed by the SAME math as a funder's own cancel. It exists so
//!    that if a redeploy is ever needed, every user is made whole from the
//!    old program without touching another client. See TRANSPARENCY.md for
//!    the full trust model, including what happens if the admin key is lost
//!    (answer: nothing, for users — every self-service exit works forever).
//!
//! Layout: `state.rs` (accounts + the accrual math and its reference-vector
//! tests) · `instructions/` (one module per concern) · `errors.rs` ·
//! `events.rs`. Integration tests live in `tests/` on LiteSVM.

use anchor_lang::prelude::*;

pub mod errors;
pub mod events;
pub mod instructions;
pub mod state;

use instructions::*;

// Placeholder until `anchor keys sync` after the deploy keypair is generated
// (kept in lock-step with Anchor.toml).
declare_id!("Fg6PaFpoGXkYsidMpWTK6W2BeZ7FEfcYkg476zPFsLnS");

#[program]
pub mod shunpike_stream {
    use super::*;

    /// One-time config genesis. Unprivileged on purpose — the admin written
    /// is always the `INITIAL_ADMIN` compiled into `state.rs`, so
    /// front-running this only donates our rent. See admin.rs.
    pub fn initialize_config(ctx: Context<InitializeConfig>) -> Result<()> {
        instructions::admin::initialize_config(ctx)
    }

    /// Funder deposits `amount` of `mint`, streaming linearly to `recipient`
    /// over `[start_time, start_time + duration]`. `seed` uniquifies the PDA
    /// (client convention: ms timestamp).
    pub fn create_stream(
        ctx: Context<CreateStream>,
        seed: u64,
        amount: u64,
        start_time: i64,
        duration: i64,
        name: String,
    ) -> Result<()> {
        instructions::create::create_stream(ctx, seed, amount, start_time, duration, name)
    }

    /// Recipient pulls accrued-minus-withdrawn. Fails with NothingToWithdraw
    /// when the difference is zero.
    pub fn withdraw(ctx: Context<Withdraw>) -> Result<()> {
        instructions::withdraw::withdraw(ctx)
    }

    /// Funder freezes accrual. Only legal between start and end of a running
    /// stream.
    pub fn pause(ctx: Context<FunderControl>) -> Result<()> {
        instructions::controls::pause(ctx)
    }

    /// Funder resumes accrual; `end_time` extends by the pause length so the
    /// rate never changes and a paused stream can never silently end.
    pub fn resume(ctx: Context<FunderControl>) -> Result<()> {
        instructions::controls::resume(ctx)
    }

    /// Funder points the stream at a new payee (employee wallet rotation
    /// without cancel/recreate).
    pub fn change_recipient(ctx: Context<FunderControl>, new_recipient: Pubkey) -> Result<()> {
        instructions::controls::change_recipient(ctx, new_recipient)
    }

    /// Funder settles both sides and unwinds everything in one transaction:
    /// accrued → recipient, remainder → funder, accounts closed, rent →
    /// funder. Legal at any point in the stream's life.
    pub fn cancel(ctx: Context<Cancel>) -> Result<()> {
        instructions::settle::cancel(ctx)
    }

    /// Permissionless cleanup of an ended, fully-withdrawn stream — reclaims
    /// rent to the funder, moves no tokens.
    pub fn close_stream(ctx: Context<CloseStream>) -> Result<()> {
        instructions::settle::close_stream(ctx)
    }

    /// Admin-only: settle any stream with cancel's exact math. The narrow
    /// disclosed power — see lib-level doc and TRANSPARENCY.md.
    pub fn force_settle(ctx: Context<ForceSettle>) -> Result<()> {
        instructions::settle::force_settle(ctx)
    }

    /// Current admin hands the force-settle power to a new key.
    pub fn rotate_admin(ctx: Context<RotateAdmin>, new_admin: Pubkey) -> Result<()> {
        instructions::admin::rotate_admin(ctx, new_admin)
    }
}
