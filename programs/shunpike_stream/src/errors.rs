//! Every failure mode, enumerated explicitly. The app's txerror map keys off
//! these names/codes — add here, add there (app repo `src/lib/txerror.ts`).

use anchor_lang::prelude::*;

#[error_code]
pub enum StreamError {
    #[msg("Signer is not authorized for this action")]
    Unauthorized, // 6000
    #[msg("Funder and recipient must be different wallets")]
    SelfStream, // 6001
    #[msg("Amount must be greater than zero")]
    ZeroAmount, // 6002
    #[msg("Duration must be greater than zero")]
    ZeroDuration, // 6003
    #[msg("Start time is more than 60 seconds in the past")]
    StartInPast, // 6004
    #[msg("Name must be at most 32 bytes of UTF-8")]
    NameTooLong, // 6005
    #[msg("Stream has not started yet")]
    NotStarted, // 6006
    #[msg("Stream has already ended")]
    AlreadyEnded, // 6007
    #[msg("Stream is already paused")]
    AlreadyPaused, // 6008
    #[msg("Stream is not paused")]
    NotPaused, // 6009
    #[msg("Nothing to withdraw yet")]
    NothingToWithdraw, // 6010
    #[msg("Stream is not settled: close requires the stream ended and fully withdrawn (use cancel otherwise)")]
    NotSettled, // 6011
    #[msg("Settlement legs do not equal the vault balance — refusing to move funds")]
    SettlementImbalance, // 6012
    #[msg("New admin cannot be the default (all-zeros) pubkey")]
    InvalidAdmin, // 6013
    #[msg("New recipient cannot be the default (all-zeros) pubkey")]
    InvalidRecipient, // 6014
}
