//! Config genesis + admin rotation.
//!
//! Design note (see TRANSPARENCY.md): `initialize_config` takes NO admin
//! argument and enforces no privileged signer. Whoever calls it first only
//! pays the config rent — the admin written is ALWAYS the `INITIAL_ADMIN`
//! constant compiled into the program. Front-running the initializer is
//! therefore harmless, and the genesis admin is auditable in source rather
//! than being whatever pubkey a deploy-time transaction happened to carry.

use anchor_lang::prelude::*;

use crate::errors::StreamError;
use crate::events::{AdminRotated, ConfigInitialized};
use crate::state::{GlobalConfig, CONFIG_SEED, INITIAL_ADMIN};

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    /// Pays the config rent. Deliberately unprivileged — see module note.
    #[account(mut)]
    pub payer: Signer<'info>,

    /// Singleton: `init` fails if it already exists, so this runs once ever.
    #[account(
        init,
        payer = payer,
        space = 8 + GlobalConfig::INIT_SPACE,
        seeds = [CONFIG_SEED],
        bump,
    )]
    pub config: Account<'info, GlobalConfig>,

    pub system_program: Program<'info, System>,
}

pub fn initialize_config(ctx: Context<InitializeConfig>) -> Result<()> {
    let config = &mut ctx.accounts.config;
    config.admin = INITIAL_ADMIN;
    config.bump = ctx.bumps.config;

    msg!("config initialized: admin={}", config.admin);
    emit!(ConfigInitialized {
        admin: config.admin,
        payer: ctx.accounts.payer.key(),
    });
    Ok(())
}

#[derive(Accounts)]
pub struct RotateAdmin<'info> {
    /// Must be the CURRENT admin — the one signature that can hand the
    /// force-settle power to a new key. No renounce path exists on purpose
    /// (the power is permanent by design; losing the key only kills
    /// force_settle — every self-service exit keeps working forever).
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [CONFIG_SEED],
        bump = config.bump,
        constraint = config.admin == admin.key() @ StreamError::Unauthorized,
    )]
    pub config: Account<'info, GlobalConfig>,
}

pub fn rotate_admin(ctx: Context<RotateAdmin>, new_admin: Pubkey) -> Result<()> {
    // The default pubkey would be an accidental renounce — the one rotation
    // this instruction refuses.
    require!(new_admin != Pubkey::default(), StreamError::InvalidAdmin);

    let config = &mut ctx.accounts.config;
    let old_admin = config.admin;
    config.admin = new_admin;

    msg!("admin rotated: {} -> {}", old_admin, new_admin);
    emit!(AdminRotated {
        old_admin,
        new_admin,
    });
    Ok(())
}
