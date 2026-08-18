//! Stream creation — the ONE instruction the funder needs (no per-sender
//! config account, no token whitelist, no fee quote: all Zebec-era concepts
//! that die here).

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount, TransferChecked};

use crate::errors::StreamError;
use crate::events::StreamCreated;
use crate::state::{Stream, MAX_NAME_LEN, START_LEAD_TOLERANCE_SEC, STREAM_SEED};

#[derive(Accounts)]
#[instruction(seed: u64)]
pub struct CreateStream<'info> {
    #[account(mut)]
    pub funder: Signer<'info>,

    /// CHECK: stored as the payee and used only as an address; any wallet is
    /// valid. Token accounts for it are created at withdraw/settle time.
    pub recipient: UncheckedAccount<'info>,

    pub mint: Account<'info, Mint>,

    /// Seeds deliberately exclude the recipient (it is mutable via
    /// change_recipient); `seed` (client convention: ms timestamp) keeps
    /// (funder, seed) unique.
    #[account(
        init,
        payer = funder,
        space = 8 + Stream::INIT_SPACE,
        seeds = [STREAM_SEED, funder.key().as_ref(), &seed.to_le_bytes()],
        bump,
    )]
    pub stream: Account<'info, Stream>,

    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = funder,
    )]
    pub funder_token: Account<'info, TokenAccount>,

    /// The deposit lives here — an ATA owned by the stream PDA. Only the
    /// withdraw/cancel/close/force_settle math can move it out; its rent (like
    /// the stream account's) returns to the funder at settlement.
    #[account(
        init,
        payer = funder,
        associated_token::mint = mint,
        associated_token::authority = stream,
    )]
    pub vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub fn create_stream(
    ctx: Context<CreateStream>,
    seed: u64,
    amount: u64,
    start_time: i64,
    duration: i64,
    name: String,
) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;

    // Gate order: cheap shape checks before any state is written. Each maps to
    // an explicit error the app's txerror layer translates for the user.
    require!(
        ctx.accounts.recipient.key() != ctx.accounts.funder.key(),
        StreamError::SelfStream
    );
    require!(amount > 0, StreamError::ZeroAmount);
    require!(duration > 0, StreamError::ZeroDuration);
    // Allow a small past tolerance for the wallet-approval race; see the
    // constant's doc comment. Future starts are fine (scheduled streams).
    require!(
        start_time >= now - START_LEAD_TOLERANCE_SEC,
        StreamError::StartInPast
    );
    let name_bytes = name.as_bytes();
    require!(name_bytes.len() <= MAX_NAME_LEN, StreamError::NameTooLong);

    // Deposit moves wallet -> vault up front; the program never holds an IOU.
    token::transfer_checked(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.funder_token.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.vault.to_account_info(),
                authority: ctx.accounts.funder.to_account_info(),
            },
        ),
        amount,
        ctx.accounts.mint.decimals,
    )?;

    let stream = &mut ctx.accounts.stream;
    stream.funder = ctx.accounts.funder.key();
    stream.recipient = ctx.accounts.recipient.key();
    stream.mint = ctx.accounts.mint.key();
    stream.deposited = amount;
    stream.withdrawn = 0;
    stream.start_time = start_time;
    // checked_add: a hostile duration near i64::MAX must fail loudly, not wrap.
    stream.end_time = start_time
        .checked_add(duration)
        .ok_or(StreamError::ZeroDuration)?;
    stream.paused_at = 0;
    stream.paused_total = 0;
    stream.seed = seed;
    stream.name = [0u8; MAX_NAME_LEN];
    stream.name[..name_bytes.len()].copy_from_slice(name_bytes);
    stream.bump = ctx.bumps.stream;

    msg!(
        "stream created: {} -> {} amount={} window=[{}, {}]",
        stream.funder,
        stream.recipient,
        amount,
        stream.start_time,
        stream.end_time
    );
    emit!(StreamCreated {
        stream: stream.key(),
        funder: stream.funder,
        recipient: stream.recipient,
        mint: stream.mint,
        seed,
        amount,
        start_time: stream.start_time,
        end_time: stream.end_time,
        name: stream.name,
    });
    Ok(())
}
