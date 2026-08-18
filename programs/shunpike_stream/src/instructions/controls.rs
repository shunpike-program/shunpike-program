//! Funder controls: pause / resume / change_recipient.
//!
//! Pause semantics are THE deliberate improvement over the program this one
//! replaces: `resume` extends `end_time` by exactly the pause length, so the
//! accrual rate is constant for the stream's whole life and a stream can never
//! "end while paused" (the upstream bug class where pause/cancel start failing
//! once the wall clock passes the original end).

use anchor_lang::prelude::*;

use crate::errors::StreamError;
use crate::events::{RecipientChanged, StreamPaused, StreamResumed};
use crate::state::{Stream, STREAM_SEED};

/// Shared account shape for the three funder-signed controls.
#[derive(Accounts)]
pub struct FunderControl<'info> {
    pub funder: Signer<'info>,

    #[account(
        mut,
        seeds = [STREAM_SEED, stream.funder.as_ref(), &stream.seed.to_le_bytes()],
        bump = stream.bump,
        constraint = stream.funder == funder.key() @ StreamError::Unauthorized,
    )]
    pub stream: Account<'info, Stream>,
}

pub fn pause(ctx: Context<FunderControl>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let stream = &mut ctx.accounts.stream;

    require!(stream.paused_at == 0, StreamError::AlreadyPaused);
    // Pausing a scheduled (not-yet-started) stream is rejected rather than
    // given bespoke math: nothing is accruing yet, so there is nothing to
    // pause — and allowing it would put paused_at BEFORE start_time, which
    // the accrual/resume arithmetic is not defined over. Cancel remains the
    // way out of a scheduled stream.
    require!(now >= stream.start_time, StreamError::NotStarted);
    // With the resume-extension invariant, a running stream past end_time is
    // simply complete — pausing it would be meaningless.
    require!(now < stream.end_time, StreamError::AlreadyEnded);

    stream.paused_at = now;

    msg!("pause: stream={} at={}", stream.key(), now);
    emit!(StreamPaused {
        stream: stream.key(),
        at: now,
    });
    Ok(())
}

pub fn resume(ctx: Context<FunderControl>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let stream = &mut ctx.accounts.stream;

    require!(stream.paused_at != 0, StreamError::NotPaused);

    // The invariant move: end_time and paused_total shift together by the
    // pause length, so duration() (= end - start - paused_total) is unchanged
    // and the accrual rate never varies. Because paused_at < end_time always
    // held at pause time, the new end_time is strictly in the future — a
    // resumed stream always has streaming time left.
    let pause_len = now.saturating_sub(stream.paused_at).max(0);
    stream.end_time = stream
        .end_time
        .checked_add(pause_len)
        .ok_or(StreamError::AlreadyEnded)?;
    stream.paused_total = stream
        .paused_total
        .checked_add(pause_len)
        .ok_or(StreamError::AlreadyEnded)?;
    stream.paused_at = 0;

    msg!(
        "resume: stream={} pause_len={} new_end={}",
        stream.key(),
        pause_len,
        stream.end_time
    );
    emit!(StreamResumed {
        stream: stream.key(),
        at: now,
        pause_len,
        new_end_time: stream.end_time,
    });
    Ok(())
}

pub fn change_recipient(ctx: Context<FunderControl>, new_recipient: Pubkey) -> Result<()> {
    let stream = &mut ctx.accounts.stream;

    // Same bans as create: no self-streams, and a default pubkey would burn
    // the accrued funds into an unspendable address.
    require!(new_recipient != stream.funder, StreamError::SelfStream);
    require!(new_recipient != Pubkey::default(), StreamError::InvalidRecipient);

    // Allowed at ANY point in the stream's life (including after end, before
    // final withdrawal) — the whole purpose is surviving employee wallet
    // rotation without cancel/recreate. Un-withdrawn accrued funds follow the
    // stream to the new recipient; that is the funder's call to make.
    let old_recipient = stream.recipient;
    stream.recipient = new_recipient;

    msg!(
        "recipient changed: stream={} {} -> {}",
        stream.key(),
        old_recipient,
        new_recipient
    );
    emit!(RecipientChanged {
        stream: stream.key(),
        old_recipient,
        new_recipient,
    });
    Ok(())
}
