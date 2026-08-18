//! Recipient-side payout of whatever has accrued so far.

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount, TransferChecked};

use crate::errors::StreamError;
use crate::events::Withdrawn;
use crate::state::{Stream, STREAM_SEED};

#[derive(Accounts)]
pub struct Withdraw<'info> {
    /// Must be the stream's CURRENT recipient (post change_recipient, the new
    /// wallet withdraws; the old one is locked out by the constraint below).
    #[account(mut)]
    pub recipient: Signer<'info>,

    #[account(
        mut,
        seeds = [STREAM_SEED, stream.funder.as_ref(), &stream.seed.to_le_bytes()],
        bump = stream.bump,
        has_one = mint,
        constraint = stream.recipient == recipient.key() @ StreamError::Unauthorized,
    )]
    pub stream: Account<'info, Stream>,

    pub mint: Account<'info, Mint>,

    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = stream,
    )]
    pub vault: Account<'info, TokenAccount>,

    /// init_if_needed: a recipient may never have held this mint before their
    /// first withdrawal. ATA derivation makes re-init attacks a non-issue.
    #[account(
        init_if_needed,
        payer = recipient,
        associated_token::mint = mint,
        associated_token::authority = recipient,
    )]
    pub recipient_token: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub fn withdraw(ctx: Context<Withdraw>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let stream = &mut ctx.accounts.stream;

    let accrued = stream.accrued_at(now);
    let payable = accrued.saturating_sub(stream.withdrawn);
    require!(payable > 0, StreamError::NothingToWithdraw);

    // The stream PDA signs the vault transfer with its own seeds.
    let funder_key = stream.funder;
    let seed_bytes = stream.seed.to_le_bytes();
    let bump = [stream.bump];
    let signer_seeds: &[&[&[u8]]] =
        &[&[STREAM_SEED, funder_key.as_ref(), &seed_bytes, &bump]];

    token::transfer_checked(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.vault.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.recipient_token.to_account_info(),
                authority: stream.to_account_info(),
            },
            signer_seeds,
        ),
        payable,
        ctx.accounts.mint.decimals,
    )?;

    stream.withdrawn = stream
        .withdrawn
        .checked_add(payable)
        .expect("withdrawn <= deposited fits u64");

    msg!(
        "withdraw: stream={} paid={} total_withdrawn={}",
        stream.key(),
        payable,
        stream.withdrawn
    );
    emit!(Withdrawn {
        stream: stream.key(),
        recipient: ctx.accounts.recipient.key(),
        amount: payable,
        withdrawn_total: stream.withdrawn,
    });
    Ok(())
}
