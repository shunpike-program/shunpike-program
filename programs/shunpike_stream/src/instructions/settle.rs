//! Terminal instructions: cancel, close, force_settle.
//!
//! `cancel` and `force_settle` run the IDENTICAL money path
//! ([`execute_settlement`]): accrued-minus-withdrawn to the recipient, the
//! remainder to the funder, vault closed, all rent back to the funder — one
//! transaction, terminal. force_settle differs ONLY in who may sign (the
//! config admin instead of the funder). No parameter exists through which
//! either could redirect a lamport anywhere else; that claim is the heart of
//! TRANSPARENCY.md and of the adversarial test suite.
//!
//! Unlike the program this replaces, cancel keeps working AFTER the stream
//! ends (it simply pays the recipient everything and refunds only rent) — no
//! "already ended" trap, no stranded accounts.

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, CloseAccount, Mint, Token, TokenAccount, TransferChecked};

use crate::errors::StreamError;
use crate::events::{ForceSettled, StreamCancelled, StreamClosed};
use crate::state::{settlement_split, GlobalConfig, SettlementSplit, Stream, CONFIG_SEED, STREAM_SEED};

/// The shared settle-and-close-vault path. The caller's account struct closes
/// the Stream account itself (Anchor `close = funder`), so after this returns
/// the whole stream — accounts, rent, balances — has been unwound.
#[allow(clippy::too_many_arguments)]
fn execute_settlement<'info>(
    stream: &Account<'info, Stream>,
    vault: &Account<'info, TokenAccount>,
    mint: &Account<'info, Mint>,
    recipient_token: &Account<'info, TokenAccount>,
    funder_token: &Account<'info, TokenAccount>,
    funder: &AccountInfo<'info>,
    token_program: &Program<'info, Token>,
    now: i64,
) -> Result<SettlementSplit> {
    let split = settlement_split(stream, now);

    // Belt-and-braces: the two legs must account for the vault to the atom
    // BEFORE anything moves. If this ever trips, state is corrupt and
    // refusing to settle is strictly safer than settling wrong.
    require!(
        split
            .to_recipient
            .checked_add(split.to_funder)
            .is_some_and(|total| total == vault.amount),
        StreamError::SettlementImbalance
    );

    let funder_key = stream.funder;
    let seed_bytes = stream.seed.to_le_bytes();
    let bump = [stream.bump];
    let signer_seeds: &[&[&[u8]]] =
        &[&[STREAM_SEED, funder_key.as_ref(), &seed_bytes, &bump]];

    if split.to_recipient > 0 {
        token::transfer_checked(
            CpiContext::new_with_signer(
                token_program.to_account_info(),
                TransferChecked {
                    from: vault.to_account_info(),
                    mint: mint.to_account_info(),
                    to: recipient_token.to_account_info(),
                    authority: stream.to_account_info(),
                },
                signer_seeds,
            ),
            split.to_recipient,
            mint.decimals,
        )?;
    }
    if split.to_funder > 0 {
        token::transfer_checked(
            CpiContext::new_with_signer(
                token_program.to_account_info(),
                TransferChecked {
                    from: vault.to_account_info(),
                    mint: mint.to_account_info(),
                    to: funder_token.to_account_info(),
                    authority: stream.to_account_info(),
                },
                signer_seeds,
            ),
            split.to_funder,
            mint.decimals,
        )?;
    }

    // Vault is now exactly zero (guaranteed by the imbalance check above) —
    // close it and hand its rent to the funder. The refundable-deposit story
    // is literal: every lamport of rent goes home.
    token::close_account(CpiContext::new_with_signer(
        token_program.to_account_info(),
        CloseAccount {
            account: vault.to_account_info(),
            destination: funder.clone(),
            authority: stream.to_account_info(),
        },
        signer_seeds,
    ))?;

    Ok(split)
}

// ---------------------------------------------------------------------------
// cancel — funder-signed, works at any point in the stream's life.
// ---------------------------------------------------------------------------

#[derive(Accounts)]
pub struct Cancel<'info> {
    #[account(mut)]
    pub funder: Signer<'info>,

    #[account(
        mut,
        seeds = [STREAM_SEED, stream.funder.as_ref(), &stream.seed.to_le_bytes()],
        bump = stream.bump,
        has_one = mint,
        constraint = stream.funder == funder.key() @ StreamError::Unauthorized,
        close = funder,
    )]
    pub stream: Account<'info, Stream>,

    pub mint: Account<'info, Mint>,

    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = stream,
    )]
    pub vault: Account<'info, TokenAccount>,

    /// CHECK: constrained to the stream's stored recipient; only used as the
    /// ATA authority for the payout leg.
    #[account(constraint = recipient.key() == stream.recipient @ StreamError::Unauthorized)]
    pub recipient: UncheckedAccount<'info>,

    /// The recipient may never have created an ATA for this mint — the funder
    /// (who chose to cancel) fronts its rent.
    #[account(
        init_if_needed,
        payer = funder,
        associated_token::mint = mint,
        associated_token::authority = recipient,
    )]
    pub recipient_token: Account<'info, TokenAccount>,

    /// The funder could have closed their own ATA since creating the stream.
    #[account(
        init_if_needed,
        payer = funder,
        associated_token::mint = mint,
        associated_token::authority = funder,
    )]
    pub funder_token: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub fn cancel(ctx: Context<Cancel>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;

    let split = execute_settlement(
        &ctx.accounts.stream,
        &ctx.accounts.vault,
        &ctx.accounts.mint,
        &ctx.accounts.recipient_token,
        &ctx.accounts.funder_token,
        &ctx.accounts.funder.to_account_info(),
        &ctx.accounts.token_program,
        now,
    )?;

    msg!(
        "cancel: stream={} paid_recipient={} refunded_funder={}",
        ctx.accounts.stream.key(),
        split.to_recipient,
        split.to_funder
    );
    emit!(StreamCancelled {
        stream: ctx.accounts.stream.key(),
        at: now,
        paid_recipient: split.to_recipient,
        refunded_funder: split.to_funder,
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// close — permissionless cleanup of a fully-played-out stream. Rent goes to
// the funder no matter who calls it, so opening it to anyone is safe and lets
// the app (or the recipient, or a bot) tidy up on the funder's behalf.
// ---------------------------------------------------------------------------

#[derive(Accounts)]
pub struct CloseStream<'info> {
    /// Anyone. See module note — the refund destination is fixed to the
    /// funder by constraint, so the caller's identity changes nothing.
    pub closer: Signer<'info>,

    /// CHECK: constrained to the stream's stored funder; receives all rent.
    #[account(mut, constraint = funder.key() == stream.funder @ StreamError::Unauthorized)]
    pub funder: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [STREAM_SEED, stream.funder.as_ref(), &stream.seed.to_le_bytes()],
        bump = stream.bump,
        has_one = mint,
        close = funder,
    )]
    pub stream: Account<'info, Stream>,

    pub mint: Account<'info, Mint>,

    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = stream,
    )]
    pub vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn close_stream(ctx: Context<CloseStream>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let stream = &ctx.accounts.stream;

    // Close is for streams that fully played out: ended AND fully withdrawn.
    // Anything short of that must go through cancel (which settles both
    // sides) — close never moves tokens, only reclaims rent.
    require!(
        now >= stream.end_time && stream.withdrawn == stream.deposited,
        StreamError::NotSettled
    );
    // Defensive restatement of the same condition from the vault's view.
    require!(ctx.accounts.vault.amount == 0, StreamError::NotSettled);

    let funder_key = stream.funder;
    let seed_bytes = stream.seed.to_le_bytes();
    let bump = [stream.bump];
    let signer_seeds: &[&[&[u8]]] =
        &[&[STREAM_SEED, funder_key.as_ref(), &seed_bytes, &bump]];

    token::close_account(CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        CloseAccount {
            account: ctx.accounts.vault.to_account_info(),
            destination: ctx.accounts.funder.to_account_info(),
            authority: stream.to_account_info(),
        },
        signer_seeds,
    ))?;

    msg!("close: stream={} rent -> {}", stream.key(), funder_key);
    emit!(StreamClosed {
        stream: stream.key(),
        funder: funder_key,
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// force_settle — the one admin power, permanent by design and loudly audited.
// ---------------------------------------------------------------------------

#[derive(Accounts)]
pub struct ForceSettle<'info> {
    /// Must match GlobalConfig.admin. Also fronts rent for any missing ATAs —
    /// the admin eats that cost, never the users.
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        seeds = [CONFIG_SEED],
        bump = config.bump,
        constraint = config.admin == admin.key() @ StreamError::Unauthorized,
    )]
    pub config: Account<'info, GlobalConfig>,

    #[account(
        mut,
        seeds = [STREAM_SEED, stream.funder.as_ref(), &stream.seed.to_le_bytes()],
        bump = stream.bump,
        has_one = mint,
        close = funder,
    )]
    pub stream: Account<'info, Stream>,

    pub mint: Account<'info, Mint>,

    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = stream,
    )]
    pub vault: Account<'info, TokenAccount>,

    /// CHECK: constrained to the stream's stored funder — the ONLY place the
    /// remainder + rent can go, no matter what the admin wants.
    #[account(mut, constraint = funder.key() == stream.funder @ StreamError::Unauthorized)]
    pub funder: UncheckedAccount<'info>,

    /// CHECK: constrained to the stream's stored recipient — the ONLY place
    /// the accrued leg can go.
    #[account(constraint = recipient.key() == stream.recipient @ StreamError::Unauthorized)]
    pub recipient: UncheckedAccount<'info>,

    #[account(
        init_if_needed,
        payer = admin,
        associated_token::mint = mint,
        associated_token::authority = recipient,
    )]
    pub recipient_token: Account<'info, TokenAccount>,

    #[account(
        init_if_needed,
        payer = admin,
        associated_token::mint = mint,
        associated_token::authority = funder,
    )]
    pub funder_token: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub fn force_settle(ctx: Context<ForceSettle>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;

    let split = execute_settlement(
        &ctx.accounts.stream,
        &ctx.accounts.vault,
        &ctx.accounts.mint,
        &ctx.accounts.recipient_token,
        &ctx.accounts.funder_token,
        &ctx.accounts.funder.to_account_info(),
        &ctx.accounts.token_program,
        now,
    )?;

    // Warning-level by intent: an admin settlement is always an unusual path
    // worth spotting in logs, even though it is a disclosed, legitimate power.
    msg!(
        "FORCE-SETTLE by admin {}: stream={} paid_recipient={} refunded_funder={}",
        ctx.accounts.admin.key(),
        ctx.accounts.stream.key(),
        split.to_recipient,
        split.to_funder
    );
    emit!(ForceSettled {
        stream: ctx.accounts.stream.key(),
        admin: ctx.accounts.admin.key(),
        at: now,
        paid_recipient: split.to_recipient,
        refunded_funder: split.to_funder,
    });
    Ok(())
}
