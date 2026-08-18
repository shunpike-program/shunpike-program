//! Anchor events — the indexing/history surface. The app's StreamEvent archive
//! and "past streams" dashboard are fed from these (closed accounts vanish from
//! getProgramAccounts, so events + DB are the durable history).

use anchor_lang::prelude::*;

#[event]
pub struct ConfigInitialized {
    pub admin: Pubkey,
    pub payer: Pubkey,
}

#[event]
pub struct StreamCreated {
    pub stream: Pubkey,
    pub funder: Pubkey,
    pub recipient: Pubkey,
    pub mint: Pubkey,
    pub seed: u64,
    pub amount: u64,
    pub start_time: i64,
    pub end_time: i64,
    pub name: [u8; 32],
}

#[event]
pub struct Withdrawn {
    pub stream: Pubkey,
    pub recipient: Pubkey,
    pub amount: u64,
    /// Cumulative after this withdrawal — lets an indexer rebuild state
    /// without replaying every event.
    pub withdrawn_total: u64,
}

#[event]
pub struct StreamPaused {
    pub stream: Pubkey,
    pub at: i64,
}

#[event]
pub struct StreamResumed {
    pub stream: Pubkey,
    pub at: i64,
    pub pause_len: i64,
    /// end_time after the pause-extension — the resume invariant made visible.
    pub new_end_time: i64,
}

#[event]
pub struct RecipientChanged {
    pub stream: Pubkey,
    pub old_recipient: Pubkey,
    pub new_recipient: Pubkey,
}

#[event]
pub struct StreamCancelled {
    pub stream: Pubkey,
    pub at: i64,
    pub paid_recipient: u64,
    pub refunded_funder: u64,
}

#[event]
pub struct StreamClosed {
    pub stream: Pubkey,
    pub funder: Pubkey,
}

/// Deliberately distinct from StreamCancelled so the admin power leaves its own
/// audit trail — transparency doc: every use of force_settle is publicly
/// distinguishable from a funder's own cancel.
#[event]
pub struct ForceSettled {
    pub stream: Pubkey,
    pub admin: Pubkey,
    pub at: i64,
    pub paid_recipient: u64,
    pub refunded_funder: u64,
}

#[event]
pub struct AdminRotated {
    pub old_admin: Pubkey,
    pub new_admin: Pubkey,
}
