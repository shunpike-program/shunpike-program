//! LiteSVM integration suite — every instruction, every error path, plus the
//! adversarial cases the design doc demands: wrong signer on every authority
//! check, double close, withdraw-after-cancel, force-settle by non-admin,
//! pause/resume timing abuse, and rounding dust at settlement (the vault must
//! always zero out exactly).
//!
//! Run `anchor build` first: these tests load target/deploy/shunpike_stream.so.
//!
//! Admin note: production's genesis admin is the INITIAL_ADMIN constant (we do
//! not hold its secret key), so admin-path tests install a throwaway admin by
//! writing the GlobalConfig account directly with set_account — byte-identical
//! to what initialize_config + rotate_admin would produce.

use anchor_lang::{Discriminator, InstructionData, ToAccountMetas};
use litesvm::LiteSVM;
use solana_sdk::{
    account::Account,
    clock::Clock,
    instruction::{Instruction, InstructionError},
    program_pack::Pack,
    pubkey::Pubkey,
    rent::Rent,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::{Transaction, TransactionError},
};
use spl_associated_token_account::get_associated_token_address;

use shunpike_stream::errors::StreamError;
use shunpike_stream::state::{GlobalConfig, Stream, CONFIG_SEED, INITIAL_ADMIN, STREAM_SEED};
use shunpike_stream::{accounts as accs, instruction as ixs, ID as PROGRAM_ID};

/// Deterministic test epoch — every test pins the clock explicitly.
const T0: i64 = 1_700_000_000;
/// 100 units of a 6-decimal token, mirroring the TS reference vectors.
const DEPOSIT: u64 = 100_000_000;
const DURATION: i64 = 1_000;
/// LiteSVM's per-signature fee (default fee structure).
const FEE_PER_SIG: u64 = 5_000;

struct Ctx {
    svm: LiteSVM,
    funder: Keypair,
    recipient: Keypair,
    outsider: Keypair,
    mint: Pubkey,
    funder_ata: Pubkey,
    recipient_ata: Pubkey,
}

fn setup() -> Ctx {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(
        PROGRAM_ID,
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/deploy/shunpike_stream.so"
        ),
    )
    .expect("run `anchor build` before `cargo test` — tests load the compiled .so");

    let funder = Keypair::new();
    let recipient = Keypair::new();
    let outsider = Keypair::new();
    for kp in [&funder, &recipient, &outsider] {
        svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
    }

    warp_to(&mut svm, T0);

    // A 6-decimal mint with the funder holding 1000 units.
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let mint_rent = Rent::default().minimum_balance(spl_token::state::Mint::LEN);
    let create_mint = system_instruction::create_account(
        &funder.pubkey(),
        &mint,
        mint_rent,
        spl_token::state::Mint::LEN as u64,
        &spl_token::ID,
    );
    let init_mint = spl_token::instruction::initialize_mint2(
        &spl_token::ID,
        &mint,
        &funder.pubkey(),
        None,
        6,
    )
    .unwrap();
    let create_funder_ata = spl_associated_token_account::instruction::create_associated_token_account(
        &funder.pubkey(),
        &funder.pubkey(),
        &mint,
        &spl_token::ID,
    );
    let funder_ata = get_associated_token_address(&funder.pubkey(), &mint);
    let mint_to = spl_token::instruction::mint_to(
        &spl_token::ID,
        &mint,
        &funder_ata,
        &funder.pubkey(),
        &[],
        1_000_000_000,
    )
    .unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[create_mint, init_mint, create_funder_ata, mint_to],
        Some(&funder.pubkey()),
        &[&funder, &mint_kp],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).unwrap();

    let recipient_ata = get_associated_token_address(&recipient.pubkey(), &mint);

    Ctx {
        svm,
        funder,
        recipient,
        outsider,
        mint,
        funder_ata,
        recipient_ata,
    }
}

fn warp_to(svm: &mut LiteSVM, unix_timestamp: i64) {
    let mut clock: Clock = svm.get_sysvar();
    clock.unix_timestamp = unix_timestamp;
    svm.set_sysvar::<Clock>(&clock);
}

fn stream_pda(funder: &Pubkey, seed: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[STREAM_SEED, funder.as_ref(), &seed.to_le_bytes()],
        &PROGRAM_ID,
    )
    .0
}

fn config_pda() -> Pubkey {
    Pubkey::find_program_address(&[CONFIG_SEED], &PROGRAM_ID).0
}

/// Install a GlobalConfig with an arbitrary admin, bypassing initialize_config
/// (see module note).
fn install_config(svm: &mut LiteSVM, admin: &Pubkey) {
    let (pda, bump) = Pubkey::find_program_address(&[CONFIG_SEED], &PROGRAM_ID);
    let mut data = Vec::with_capacity(8 + 32 + 1);
    data.extend_from_slice(GlobalConfig::DISCRIMINATOR);
    data.extend_from_slice(admin.as_ref());
    data.push(bump);
    let lamports = Rent::default().minimum_balance(data.len());
    svm.set_account(
        pda,
        Account {
            lamports,
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

fn send(
    svm: &mut LiteSVM,
    ixs: &[Instruction],
    payer: &Keypair,
    extra_signers: &[&Keypair],
) -> Result<(), TransactionError> {
    // Rotate the blockhash on every send: repeated identical instructions
    // (withdraw-again, double-pause probes…) would otherwise produce the same
    // signature and bounce off the dedup cache as AlreadyProcessed.
    svm.expire_blockhash();
    let mut signers: Vec<&Keypair> = vec![payer];
    signers.extend_from_slice(extra_signers);
    let tx = Transaction::new_signed_with_payer(
        ixs,
        Some(&payer.pubkey()),
        &signers,
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).map(|_| ()).map_err(|f| f.err)
}

fn assert_stream_err(result: Result<(), TransactionError>, expected: StreamError) {
    let expected_code = 6000 + expected as u32;
    match result {
        Err(TransactionError::InstructionError(_, InstructionError::Custom(code))) => {
            assert_eq!(code, expected_code, "wrong custom error code");
        }
        other => panic!("expected custom error {expected_code}, got {other:?}"),
    }
}

fn token_balance(svm: &LiteSVM, ata: &Pubkey) -> u64 {
    match svm.get_account(ata) {
        Some(acc) if !acc.data.is_empty() => {
            spl_token::state::Account::unpack(&acc.data).unwrap().amount
        }
        _ => 0,
    }
}

fn lamports(svm: &LiteSVM, pk: &Pubkey) -> u64 {
    svm.get_account(pk).map(|a| a.lamports).unwrap_or(0)
}

fn read_stream(svm: &LiteSVM, pk: &Pubkey) -> Stream {
    let acc = svm.get_account(pk).expect("stream account exists");
    let mut slice: &[u8] = &acc.data;
    <Stream as anchor_lang::AccountDeserialize>::try_deserialize(&mut slice).unwrap()
}

// ---------------------------------------------------------------------------
// Instruction builders
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn create_stream_ix(
    ctx: &Ctx,
    seed: u64,
    amount: u64,
    start_time: i64,
    duration: i64,
    name: &str,
    recipient: Pubkey,
) -> Instruction {
    let stream = stream_pda(&ctx.funder.pubkey(), seed);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accs::CreateStream {
            funder: ctx.funder.pubkey(),
            recipient,
            mint: ctx.mint,
            stream,
            funder_token: ctx.funder_ata,
            vault: get_associated_token_address(&stream, &ctx.mint),
            token_program: spl_token::ID,
            associated_token_program: spl_associated_token_account::ID,
            system_program: solana_sdk::system_program::ID,
        }
        .to_account_metas(None),
        data: ixs::CreateStream {
            seed,
            amount,
            start_time,
            duration,
            name: name.to_string(),
        }
        .data(),
    }
}

fn withdraw_ix(ctx: &Ctx, seed: u64, recipient: &Pubkey) -> Instruction {
    let stream = stream_pda(&ctx.funder.pubkey(), seed);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accs::Withdraw {
            recipient: *recipient,
            stream,
            mint: ctx.mint,
            vault: get_associated_token_address(&stream, &ctx.mint),
            recipient_token: get_associated_token_address(recipient, &ctx.mint),
            token_program: spl_token::ID,
            associated_token_program: spl_associated_token_account::ID,
            system_program: solana_sdk::system_program::ID,
        }
        .to_account_metas(None),
        data: ixs::Withdraw {}.data(),
    }
}

fn control_ix(ctx: &Ctx, seed: u64, signer: &Pubkey, data: Vec<u8>) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accs::FunderControl {
            funder: *signer,
            stream: stream_pda(&ctx.funder.pubkey(), seed),
        }
        .to_account_metas(None),
        data,
    }
}

fn cancel_ix(ctx: &Ctx, seed: u64) -> Instruction {
    let stream = stream_pda(&ctx.funder.pubkey(), seed);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accs::Cancel {
            funder: ctx.funder.pubkey(),
            stream,
            mint: ctx.mint,
            vault: get_associated_token_address(&stream, &ctx.mint),
            recipient: ctx.recipient.pubkey(),
            recipient_token: ctx.recipient_ata,
            funder_token: ctx.funder_ata,
            token_program: spl_token::ID,
            associated_token_program: spl_associated_token_account::ID,
            system_program: solana_sdk::system_program::ID,
        }
        .to_account_metas(None),
        data: ixs::Cancel {}.data(),
    }
}

fn close_ix(ctx: &Ctx, seed: u64, closer: &Pubkey) -> Instruction {
    let stream = stream_pda(&ctx.funder.pubkey(), seed);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accs::CloseStream {
            closer: *closer,
            funder: ctx.funder.pubkey(),
            stream,
            mint: ctx.mint,
            vault: get_associated_token_address(&stream, &ctx.mint),
            token_program: spl_token::ID,
        }
        .to_account_metas(None),
        data: ixs::CloseStream {}.data(),
    }
}

fn force_settle_ix(ctx: &Ctx, seed: u64, admin: &Pubkey) -> Instruction {
    let stream = stream_pda(&ctx.funder.pubkey(), seed);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accs::ForceSettle {
            admin: *admin,
            config: config_pda(),
            stream,
            mint: ctx.mint,
            vault: get_associated_token_address(&stream, &ctx.mint),
            funder: ctx.funder.pubkey(),
            recipient: ctx.recipient.pubkey(),
            recipient_token: ctx.recipient_ata,
            funder_token: ctx.funder_ata,
            token_program: spl_token::ID,
            associated_token_program: spl_associated_token_account::ID,
            system_program: solana_sdk::system_program::ID,
        }
        .to_account_metas(None),
        data: ixs::ForceSettle {}.data(),
    }
}

/// Create the standard test stream: 100 units over 1000s starting at T0.
fn create_default_stream(ctx: &mut Ctx, seed: u64) {
    let ix = create_stream_ix(ctx, seed, DEPOSIT, T0, DURATION, "payroll", ctx.recipient.pubkey());
    let funder = ctx.funder.insecure_clone();
    send(&mut ctx.svm, &[ix], &funder, &[]).unwrap();
}

// ---------------------------------------------------------------------------
// Config genesis + admin rotation
// ---------------------------------------------------------------------------

#[test]
fn initialize_config_writes_hardcoded_admin_and_runs_once() {
    let mut ctx = setup();
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accs::InitializeConfig {
            payer: ctx.outsider.pubkey(),
            config: config_pda(),
            system_program: solana_sdk::system_program::ID,
        }
        .to_account_metas(None),
        data: ixs::InitializeConfig {}.data(),
    };
    let outsider = ctx.outsider.insecure_clone();
    send(&mut ctx.svm, &[ix.clone()], &outsider, &[]).unwrap();

    // Whoever paid, the admin is the compiled-in constant — front-running is
    // rent donation, nothing more.
    let acc = ctx.svm.get_account(&config_pda()).unwrap();
    let mut slice: &[u8] = &acc.data;
    let config =
        <GlobalConfig as anchor_lang::AccountDeserialize>::try_deserialize(&mut slice).unwrap();
    assert_eq!(config.admin, INITIAL_ADMIN);

    // Singleton: a second init must fail (account already in use).
    ctx.svm.expire_blockhash();
    assert!(send(&mut ctx.svm, &[ix], &outsider, &[]).is_err());
}

#[test]
fn rotate_admin_hands_over_power_and_rejects_default_pubkey() {
    let mut ctx = setup();
    let admin_a = Keypair::new();
    let admin_b = Keypair::new();
    ctx.svm.airdrop(&admin_a.pubkey(), 1_000_000_000).unwrap();
    ctx.svm.airdrop(&admin_b.pubkey(), 1_000_000_000).unwrap();
    install_config(&mut ctx.svm, &admin_a.pubkey());
    create_default_stream(&mut ctx, 1);

    let rotate = |to: Pubkey, signer: Pubkey| Instruction {
        program_id: PROGRAM_ID,
        accounts: accs::RotateAdmin {
            admin: signer,
            config: config_pda(),
        }
        .to_account_metas(None),
        data: ixs::RotateAdmin { new_admin: to }.data(),
    };

    // Accidental renounce is refused.
    assert_stream_err(
        send(&mut ctx.svm, &[rotate(Pubkey::default(), admin_a.pubkey())], &admin_a, &[]),
        StreamError::InvalidAdmin,
    );
    // A non-admin cannot rotate.
    assert_stream_err(
        send(&mut ctx.svm, &[rotate(admin_b.pubkey(), admin_b.pubkey())], &admin_b, &[]),
        StreamError::Unauthorized,
    );

    // Real rotation: A -> B.
    send(&mut ctx.svm, &[rotate(admin_b.pubkey(), admin_a.pubkey())], &admin_a, &[]).unwrap();

    // Old admin's force_settle is dead; new admin's works.
    warp_to(&mut ctx.svm, T0 + 500);
    assert_stream_err(
        { let __ix = force_settle_ix(&ctx, 1, &admin_a.pubkey()); send(&mut ctx.svm, &[__ix], &admin_a, &[]) },
        StreamError::Unauthorized,
    );
    { let __ix = force_settle_ix(&ctx, 1, &admin_b.pubkey()); send(&mut ctx.svm, &[__ix], &admin_b, &[]) }.unwrap();
}

// ---------------------------------------------------------------------------
// create_stream
// ---------------------------------------------------------------------------

#[test]
fn create_stream_moves_deposit_and_records_terms() {
    let mut ctx = setup();
    create_default_stream(&mut ctx, 1);

    let stream_pk = stream_pda(&ctx.funder.pubkey(), 1);
    let vault = get_associated_token_address(&stream_pk, &ctx.mint);
    assert_eq!(token_balance(&ctx.svm, &vault), DEPOSIT);
    assert_eq!(token_balance(&ctx.svm, &ctx.funder_ata), 1_000_000_000 - DEPOSIT);

    let s = read_stream(&ctx.svm, &stream_pk);
    assert_eq!(s.funder, ctx.funder.pubkey());
    assert_eq!(s.recipient, ctx.recipient.pubkey());
    assert_eq!(s.mint, ctx.mint);
    assert_eq!(s.deposited, DEPOSIT);
    assert_eq!(s.withdrawn, 0);
    assert_eq!(s.start_time, T0);
    assert_eq!(s.end_time, T0 + DURATION);
    assert_eq!(s.paused_at, 0);
    assert_eq!(s.paused_total, 0);
    assert_eq!(&s.name[..7], b"payroll");
}

#[test]
fn create_stream_rejects_bad_shapes() {
    let mut ctx = setup();
    let funder = ctx.funder.insecure_clone();
    let recipient = ctx.recipient.pubkey();
    let funder_pk = ctx.funder.pubkey();

    // Self-stream.
    let ix = create_stream_ix(&ctx, 10, DEPOSIT, T0, DURATION, "x", funder_pk);
    assert_stream_err(send(&mut ctx.svm, &[ix], &funder, &[]), StreamError::SelfStream);
    // Zero amount.
    let ix = create_stream_ix(&ctx, 11, 0, T0, DURATION, "x", recipient);
    assert_stream_err(send(&mut ctx.svm, &[ix], &funder, &[]), StreamError::ZeroAmount);
    // Zero duration.
    let ix = create_stream_ix(&ctx, 12, DEPOSIT, T0, 0, "x", recipient);
    assert_stream_err(send(&mut ctx.svm, &[ix], &funder, &[]), StreamError::ZeroDuration);
    // Start further than the 60s approval-race tolerance in the past.
    let ix = create_stream_ix(&ctx, 13, DEPOSIT, T0 - 61, DURATION, "x", recipient);
    assert_stream_err(send(&mut ctx.svm, &[ix], &funder, &[]), StreamError::StartInPast);
    // ...but 60s in the past is inside the tolerance.
    let ix = create_stream_ix(&ctx, 14, DEPOSIT, T0 - 60, DURATION, "x", recipient);
    send(&mut ctx.svm, &[ix], &funder, &[]).unwrap();
    // Name over 32 bytes.
    let ix = create_stream_ix(&ctx, 15, DEPOSIT, T0, DURATION, &"n".repeat(33), recipient);
    assert_stream_err(send(&mut ctx.svm, &[ix], &funder, &[]), StreamError::NameTooLong);
}

// ---------------------------------------------------------------------------
// withdraw
// ---------------------------------------------------------------------------

#[test]
fn withdraw_pays_linear_accrual_and_creates_recipient_ata() {
    let mut ctx = setup();
    create_default_stream(&mut ctx, 1);
    let recipient = ctx.recipient.insecure_clone();

    // TS vector: at 50% elapsed, half the deposit is withdrawable.
    warp_to(&mut ctx.svm, T0 + 500);
    { let __ix = withdraw_ix(&ctx, 1, &recipient.pubkey()); send(&mut ctx.svm, &[__ix], &recipient, &[]) }.unwrap();
    assert_eq!(token_balance(&ctx.svm, &ctx.recipient_ata), DEPOSIT / 2);

    let s = read_stream(&ctx.svm, &stream_pda(&ctx.funder.pubkey(), 1));
    assert_eq!(s.withdrawn, DEPOSIT / 2);

    // Immediately again with no time passed: nothing left to withdraw.
    ctx.svm.expire_blockhash();
    assert_stream_err(
        { let __ix = withdraw_ix(&ctx, 1, &recipient.pubkey()); send(&mut ctx.svm, &[__ix], &recipient, &[]) },
        StreamError::NothingToWithdraw,
    );

    // After end: the remainder lands and totals are exact.
    warp_to(&mut ctx.svm, T0 + DURATION + 5);
    { let __ix = withdraw_ix(&ctx, 1, &recipient.pubkey()); send(&mut ctx.svm, &[__ix], &recipient, &[]) }.unwrap();
    assert_eq!(token_balance(&ctx.svm, &ctx.recipient_ata), DEPOSIT);
}

#[test]
fn withdraw_rejects_everyone_but_the_current_recipient() {
    let mut ctx = setup();
    create_default_stream(&mut ctx, 1);
    warp_to(&mut ctx.svm, T0 + 500);

    // The funder cannot pull the recipient's side.
    let funder = ctx.funder.insecure_clone();
    assert_stream_err(
        { let __ix = withdraw_ix(&ctx, 1, &funder.pubkey()); send(&mut ctx.svm, &[__ix], &funder, &[]) },
        StreamError::Unauthorized,
    );
    // Nor can a stranger.
    let outsider = ctx.outsider.insecure_clone();
    assert_stream_err(
        { let __ix = withdraw_ix(&ctx, 1, &outsider.pubkey()); send(&mut ctx.svm, &[__ix], &outsider, &[]) },
        StreamError::Unauthorized,
    );
}

#[test]
fn scheduled_stream_pays_nothing_before_start() {
    let mut ctx = setup();
    let ix = create_stream_ix(&ctx, 1, DEPOSIT, T0 + 5_000, DURATION, "future", ctx.recipient.pubkey());
    let funder = ctx.funder.insecure_clone();
    send(&mut ctx.svm, &[ix], &funder, &[]).unwrap();

    let recipient = ctx.recipient.insecure_clone();
    assert_stream_err(
        { let __ix = withdraw_ix(&ctx, 1, &recipient.pubkey()); send(&mut ctx.svm, &[__ix], &recipient, &[]) },
        StreamError::NothingToWithdraw,
    );

    // Cancelling a scheduled stream refunds the entire deposit to the funder.
    let before = token_balance(&ctx.svm, &ctx.funder_ata);
    { let __ix = cancel_ix(&ctx, 1); send(&mut ctx.svm, &[__ix], &funder, &[]) }.unwrap();
    assert_eq!(token_balance(&ctx.svm, &ctx.funder_ata), before + DEPOSIT);
    assert_eq!(token_balance(&ctx.svm, &ctx.recipient_ata), 0);
}

// ---------------------------------------------------------------------------
// pause / resume
// ---------------------------------------------------------------------------

#[test]
fn pause_freezes_and_resume_extends_end_time() {
    let mut ctx = setup();
    create_default_stream(&mut ctx, 1);
    let funder = ctx.funder.insecure_clone();
    let recipient = ctx.recipient.insecure_clone();

    // Pause at 40%.
    warp_to(&mut ctx.svm, T0 + 400);
    { let __ix = control_ix(&ctx, 1, &funder.pubkey(), ixs::Pause {}.data()); send(&mut ctx.svm, &[__ix], &funder, &[]) }.unwrap();

    // 300s later, accrual is still frozen at 40%.
    warp_to(&mut ctx.svm, T0 + 700);
    { let __ix = withdraw_ix(&ctx, 1, &recipient.pubkey()); send(&mut ctx.svm, &[__ix], &recipient, &[]) }.unwrap();
    assert_eq!(token_balance(&ctx.svm, &ctx.recipient_ata), 40_000_000);

    // Resume: end_time extends by exactly the 300s pause — the stream can
    // never have "ended while paused".
    { let __ix = control_ix(&ctx, 1, &funder.pubkey(), ixs::Resume {}.data()); send(&mut ctx.svm, &[__ix], &funder, &[]) }.unwrap();
    let s = read_stream(&ctx.svm, &stream_pda(&funder.pubkey(), 1));
    assert_eq!(s.end_time, T0 + DURATION + 300);
    assert_eq!(s.paused_total, 300);
    assert_eq!(s.paused_at, 0);

    // At the ORIGINAL end time the stream is still running (70% accrued).
    warp_to(&mut ctx.svm, T0 + 1_000);
    { let __ix = withdraw_ix(&ctx, 1, &recipient.pubkey()); send(&mut ctx.svm, &[__ix], &recipient, &[]) }.unwrap();
    assert_eq!(token_balance(&ctx.svm, &ctx.recipient_ata), 70_000_000);

    // At the extended end everything has streamed, to the atom.
    warp_to(&mut ctx.svm, T0 + 1_300);
    { let __ix = withdraw_ix(&ctx, 1, &recipient.pubkey()); send(&mut ctx.svm, &[__ix], &recipient, &[]) }.unwrap();
    assert_eq!(token_balance(&ctx.svm, &ctx.recipient_ata), DEPOSIT);
}

#[test]
fn pause_resume_timing_abuse_is_rejected() {
    let mut ctx = setup();
    let funder = ctx.funder.insecure_clone();
    let pause = |ctx: &Ctx| control_ix(ctx, 1, &funder.pubkey(), ixs::Pause {}.data());
    let resume = |ctx: &Ctx| control_ix(ctx, 1, &funder.pubkey(), ixs::Resume {}.data());

    // Scheduled stream: pausing before start is meaningless and rejected.
    let ix = create_stream_ix(&ctx, 1, DEPOSIT, T0 + 5_000, DURATION, "x", ctx.recipient.pubkey());
    send(&mut ctx.svm, &[ix], &funder, &[]).unwrap();
    assert_stream_err({ let __ix = pause(&ctx); send(&mut ctx.svm, &[__ix], &funder, &[]) }, StreamError::NotStarted);

    // Resume while running is rejected.
    warp_to(&mut ctx.svm, T0 + 5_100);
    assert_stream_err({ let __ix = resume(&ctx); send(&mut ctx.svm, &[__ix], &funder, &[]) }, StreamError::NotPaused);

    // Double pause is rejected.
    { let __ix = pause(&ctx); send(&mut ctx.svm, &[__ix], &funder, &[]) }.unwrap();
    ctx.svm.expire_blockhash();
    assert_stream_err({ let __ix = pause(&ctx); send(&mut ctx.svm, &[__ix], &funder, &[]) }, StreamError::AlreadyPaused);
    { let __ix = resume(&ctx); send(&mut ctx.svm, &[__ix], &funder, &[]) }.unwrap();

    // Pause after the (extended) end is rejected: the stream is complete.
    warp_to(&mut ctx.svm, T0 + 5_000 + DURATION + 200);
    assert_stream_err({ let __ix = pause(&ctx); send(&mut ctx.svm, &[__ix], &funder, &[]) }, StreamError::AlreadyEnded);
}

#[test]
fn controls_reject_non_funder_signers() {
    let mut ctx = setup();
    create_default_stream(&mut ctx, 1);
    warp_to(&mut ctx.svm, T0 + 100);

    let recipient = ctx.recipient.insecure_clone();
    for data in [
        ixs::Pause {}.data(),
        ixs::Resume {}.data(),
        ixs::ChangeRecipient { new_recipient: recipient.pubkey() }.data(),
    ] {
        assert_stream_err(
            { let __ix = control_ix(&ctx, 1, &recipient.pubkey(), data); send(&mut ctx.svm, &[__ix], &recipient, &[]) },
            StreamError::Unauthorized,
        );
    }
}

// ---------------------------------------------------------------------------
// change_recipient
// ---------------------------------------------------------------------------

#[test]
fn change_recipient_locks_out_old_and_pays_new() {
    let mut ctx = setup();
    create_default_stream(&mut ctx, 1);
    let funder = ctx.funder.insecure_clone();
    let old = ctx.recipient.insecure_clone();
    let new = ctx.outsider.insecure_clone();

    warp_to(&mut ctx.svm, T0 + 500);
    let ix = control_ix(&ctx, 1, &funder.pubkey(), ixs::ChangeRecipient { new_recipient: new.pubkey() }.data());
    send(&mut ctx.svm, &[ix], &funder, &[]).unwrap();

    // The old wallet is locked out...
    assert_stream_err(
        { let __ix = withdraw_ix(&ctx, 1, &old.pubkey()); send(&mut ctx.svm, &[__ix], &old, &[]) },
        StreamError::Unauthorized,
    );
    // ...and the new wallet withdraws the accrued half.
    { let __ix = withdraw_ix(&ctx, 1, &new.pubkey()); send(&mut ctx.svm, &[__ix], &new, &[]) }.unwrap();
    let new_ata = get_associated_token_address(&new.pubkey(), &ctx.mint);
    assert_eq!(token_balance(&ctx.svm, &new_ata), DEPOSIT / 2);

    // Changing to the funder itself stays banned.
    let ix = control_ix(&ctx, 1, &funder.pubkey(), ixs::ChangeRecipient { new_recipient: funder.pubkey() }.data());
    assert_stream_err(send(&mut ctx.svm, &[ix], &funder, &[]), StreamError::SelfStream);
}

// ---------------------------------------------------------------------------
// cancel
// ---------------------------------------------------------------------------

#[test]
fn cancel_settles_both_sides_and_refunds_all_rent() {
    let mut ctx = setup();
    // Pre-create the recipient ATA so the cancel tx's lamport math is purely
    // rent-refund + fee (no init_if_needed spend muddying the assertion).
    let recipient = ctx.recipient.insecure_clone();
    let create_ata = spl_associated_token_account::instruction::create_associated_token_account(
        &recipient.pubkey(),
        &recipient.pubkey(),
        &ctx.mint,
        &spl_token::ID,
    );
    send(&mut ctx.svm, &[create_ata], &recipient, &[]).unwrap();

    create_default_stream(&mut ctx, 1);
    let funder = ctx.funder.insecure_clone();
    let stream_pk = stream_pda(&funder.pubkey(), 1);
    let vault = get_associated_token_address(&stream_pk, &ctx.mint);

    // 30% in, with 10% already withdrawn.
    warp_to(&mut ctx.svm, T0 + 100);
    { let __ix = withdraw_ix(&ctx, 1, &recipient.pubkey()); send(&mut ctx.svm, &[__ix], &recipient, &[]) }.unwrap();
    assert_eq!(token_balance(&ctx.svm, &ctx.recipient_ata), 10_000_000);
    warp_to(&mut ctx.svm, T0 + 300);

    let stream_rent = lamports(&ctx.svm, &stream_pk);
    let vault_rent = lamports(&ctx.svm, &vault);
    let funder_sol_before = lamports(&ctx.svm, &funder.pubkey());
    let funder_tokens_before = token_balance(&ctx.svm, &ctx.funder_ata);

    { let __ix = cancel_ix(&ctx, 1); send(&mut ctx.svm, &[__ix], &funder, &[]) }.unwrap();

    // Token legs: recipient tops up to the 30% accrued; funder gets the 70%.
    assert_eq!(token_balance(&ctx.svm, &ctx.recipient_ata), 30_000_000);
    assert_eq!(token_balance(&ctx.svm, &ctx.funder_ata), funder_tokens_before + 70_000_000);

    // Rent legs: stream + vault rent land on the funder, minus the tx fee the
    // funder paid as sole signer. Rent is a refundable deposit — literally.
    assert_eq!(
        lamports(&ctx.svm, &funder.pubkey()),
        funder_sol_before + stream_rent + vault_rent - FEE_PER_SIG
    );

    // The accounts are gone from the chain.
    assert!(ctx.svm.get_account(&stream_pk).is_none_or(|a| a.lamports == 0));
    assert!(ctx.svm.get_account(&vault).is_none_or(|a| a.lamports == 0));

    // Terminal: a second cancel and a post-cancel withdraw both fail.
    ctx.svm.expire_blockhash();
    assert!({ let __ix = cancel_ix(&ctx, 1); send(&mut ctx.svm, &[__ix], &funder, &[]) }.is_err());
    assert!({ let __ix = withdraw_ix(&ctx, 1, &recipient.pubkey()); send(&mut ctx.svm, &[__ix], &recipient, &[]) }.is_err());
}

#[test]
fn cancel_after_end_pays_recipient_everything() {
    let mut ctx = setup();
    create_default_stream(&mut ctx, 1);
    let funder = ctx.funder.insecure_clone();
    let funder_tokens_before = token_balance(&ctx.svm, &ctx.funder_ata);

    // No "already ended" trap here — cancel doubles as final settlement.
    warp_to(&mut ctx.svm, T0 + DURATION + 999);
    { let __ix = cancel_ix(&ctx, 1); send(&mut ctx.svm, &[__ix], &funder, &[]) }.unwrap();
    assert_eq!(token_balance(&ctx.svm, &ctx.recipient_ata), DEPOSIT);
    assert_eq!(token_balance(&ctx.svm, &ctx.funder_ata), funder_tokens_before);
}

#[test]
fn cancel_rejects_non_funder() {
    let mut ctx = setup();
    create_default_stream(&mut ctx, 1);
    warp_to(&mut ctx.svm, T0 + 500);

    // A recipient (or anyone) trying to cancel hits the funder constraint —
    // they cannot even sign as the funder account, so build the ix with the
    // recipient in the funder seat.
    let recipient = ctx.recipient.insecure_clone();
    let stream = stream_pda(&ctx.funder.pubkey(), 1);
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: accs::Cancel {
            funder: recipient.pubkey(),
            stream,
            mint: ctx.mint,
            vault: get_associated_token_address(&stream, &ctx.mint),
            recipient: ctx.recipient.pubkey(),
            recipient_token: ctx.recipient_ata,
            funder_token: get_associated_token_address(&recipient.pubkey(), &ctx.mint),
            token_program: spl_token::ID,
            associated_token_program: spl_associated_token_account::ID,
            system_program: solana_sdk::system_program::ID,
        }
        .to_account_metas(None),
        data: ixs::Cancel {}.data(),
    };
    assert_stream_err(send(&mut ctx.svm, &[ix], &recipient, &[]), StreamError::Unauthorized);
}

// ---------------------------------------------------------------------------
// close
// ---------------------------------------------------------------------------

#[test]
fn close_reclaims_rent_after_full_playout_and_rejects_early_calls() {
    let mut ctx = setup();
    create_default_stream(&mut ctx, 1);
    let funder = ctx.funder.insecure_clone();
    let recipient = ctx.recipient.insecure_clone();
    let outsider = ctx.outsider.insecure_clone();
    let stream_pk = stream_pda(&funder.pubkey(), 1);
    let vault = get_associated_token_address(&stream_pk, &ctx.mint);

    // Mid-stream: close is refused (cancel is the mid-stream exit).
    warp_to(&mut ctx.svm, T0 + 500);
    assert_stream_err(
        { let __ix = close_ix(&ctx, 1, &outsider.pubkey()); send(&mut ctx.svm, &[__ix], &outsider, &[]) },
        StreamError::NotSettled,
    );

    // Ended but not fully withdrawn: still refused.
    warp_to(&mut ctx.svm, T0 + DURATION + 1);
    assert_stream_err(
        { let __ix = close_ix(&ctx, 1, &outsider.pubkey()); send(&mut ctx.svm, &[__ix], &outsider, &[]) },
        StreamError::NotSettled,
    );

    // Recipient drains, then ANYONE may close — rent still goes to the funder,
    // who did not sign, so the refund is exact (no fee deduction).
    { let __ix = withdraw_ix(&ctx, 1, &recipient.pubkey()); send(&mut ctx.svm, &[__ix], &recipient, &[]) }.unwrap();
    let stream_rent = lamports(&ctx.svm, &stream_pk);
    let vault_rent = lamports(&ctx.svm, &vault);
    let funder_before = lamports(&ctx.svm, &funder.pubkey());

    { let __ix = close_ix(&ctx, 1, &outsider.pubkey()); send(&mut ctx.svm, &[__ix], &outsider, &[]) }.unwrap();
    assert_eq!(
        lamports(&ctx.svm, &funder.pubkey()),
        funder_before + stream_rent + vault_rent
    );

    // Double close fails: the accounts no longer exist.
    ctx.svm.expire_blockhash();
    assert!({ let __ix = close_ix(&ctx, 1, &outsider.pubkey()); send(&mut ctx.svm, &[__ix], &outsider, &[]) }.is_err());
}

// ---------------------------------------------------------------------------
// force_settle
// ---------------------------------------------------------------------------

#[test]
fn force_settle_matches_cancel_math_and_pays_only_the_two_parties() {
    let mut ctx = setup();
    let admin = Keypair::new();
    ctx.svm.airdrop(&admin.pubkey(), 1_000_000_000).unwrap();
    install_config(&mut ctx.svm, &admin.pubkey());
    create_default_stream(&mut ctx, 1);

    let funder = ctx.funder.insecure_clone();
    let stream_pk = stream_pda(&funder.pubkey(), 1);
    let vault = get_associated_token_address(&stream_pk, &ctx.mint);

    warp_to(&mut ctx.svm, T0 + 250);
    let stream_rent = lamports(&ctx.svm, &stream_pk);
    let vault_rent = lamports(&ctx.svm, &vault);
    let funder_sol_before = lamports(&ctx.svm, &funder.pubkey());
    let funder_tokens_before = token_balance(&ctx.svm, &ctx.funder_ata);
    let recipient_ata_rent =
        Rent::default().minimum_balance(spl_token::state::Account::LEN);

    { let __ix = force_settle_ix(&ctx, 1, &admin.pubkey()); send(&mut ctx.svm, &[__ix], &admin, &[]) }.unwrap();

    // Exactly cancel's split at 25%: recipient 25, funder 75.
    assert_eq!(token_balance(&ctx.svm, &ctx.recipient_ata), 25_000_000);
    assert_eq!(token_balance(&ctx.svm, &ctx.funder_ata), funder_tokens_before + 75_000_000);

    // All rent to the funder — who did not sign, so the credit is exact.
    assert_eq!(
        lamports(&ctx.svm, &funder.pubkey()),
        funder_sol_before + stream_rent + vault_rent
    );

    // The admin PAID (fee + fronted the recipient's missing ATA rent) and
    // received nothing — the power moves money only to the two parties.
    assert_eq!(
        lamports(&ctx.svm, &admin.pubkey()),
        1_000_000_000 - FEE_PER_SIG - recipient_ata_rent
    );
}

#[test]
fn force_settle_rejects_non_admin_and_missing_config() {
    let mut ctx = setup();
    create_default_stream(&mut ctx, 1);
    warp_to(&mut ctx.svm, T0 + 500);
    let outsider = ctx.outsider.insecure_clone();

    // No config account at all: the config account check fails.
    assert!({ let __ix = force_settle_ix(&ctx, 1, &outsider.pubkey()); send(&mut ctx.svm, &[__ix], &outsider, &[]) }.is_err());

    // Config exists but the signer isn't the admin.
    let admin = Keypair::new();
    install_config(&mut ctx.svm, &admin.pubkey());
    ctx.svm.expire_blockhash();
    assert_stream_err(
        { let __ix = force_settle_ix(&ctx, 1, &outsider.pubkey()); send(&mut ctx.svm, &[__ix], &outsider, &[]) },
        StreamError::Unauthorized,
    );
}

// ---------------------------------------------------------------------------
// Rounding dust — the vault must zero exactly, always
// ---------------------------------------------------------------------------

#[test]
fn awkward_numbers_settle_with_zero_dust() {
    let mut ctx = setup();
    // 33_333_337 base units over 997 seconds: nothing divides evenly.
    let odd_deposit: u64 = 33_333_337;
    let odd_duration: i64 = 997;
    let ix = create_stream_ix(&ctx, 1, odd_deposit, T0, odd_duration, "odd", ctx.recipient.pubkey());
    let funder = ctx.funder.insecure_clone();
    let recipient = ctx.recipient.insecure_clone();
    send(&mut ctx.svm, &[ix], &funder, &[]).unwrap();

    let funder_tokens_before = token_balance(&ctx.svm, &ctx.funder_ata);

    // Withdraw at an awkward instant (floor division leaves dust in the vault).
    warp_to(&mut ctx.svm, T0 + 313);
    { let __ix = withdraw_ix(&ctx, 1, &recipient.pubkey()); send(&mut ctx.svm, &[__ix], &recipient, &[]) }.unwrap();
    let withdrawn_mid = token_balance(&ctx.svm, &ctx.recipient_ata);
    assert_eq!(withdrawn_mid, ((odd_deposit as u128) * 313 / 997) as u64);

    // Cancel at another awkward instant: recipient tops up to accrued, funder
    // gets the remainder, and the two legs account for EVERY atom.
    warp_to(&mut ctx.svm, T0 + 641);
    { let __ix = cancel_ix(&ctx, 1); send(&mut ctx.svm, &[__ix], &funder, &[]) }.unwrap();

    let accrued_at_cancel = ((odd_deposit as u128) * 641 / 997) as u64;
    assert_eq!(token_balance(&ctx.svm, &ctx.recipient_ata), accrued_at_cancel);
    assert_eq!(
        token_balance(&ctx.svm, &ctx.funder_ata),
        funder_tokens_before + (odd_deposit - accrued_at_cancel)
    );

    // Conservation across the whole lifecycle: recipient + funder holdings
    // equal the original mint supply; the vault is gone.
    let stream_pk = stream_pda(&funder.pubkey(), 1);
    let vault = get_associated_token_address(&stream_pk, &ctx.mint);
    assert!(ctx.svm.get_account(&vault).is_none_or(|a| a.lamports == 0));
    assert_eq!(
        token_balance(&ctx.svm, &ctx.recipient_ata) + token_balance(&ctx.svm, &ctx.funder_ata),
        1_000_000_000
    );
}

#[test]
fn full_playout_with_pause_pays_every_atom() {
    let mut ctx = setup();
    let odd_deposit: u64 = 999_999_999; // does not divide by anything nice
    let ix = create_stream_ix(&ctx, 1, odd_deposit, T0, 997, "odd2", ctx.recipient.pubkey());
    let funder = ctx.funder.insecure_clone();
    let recipient = ctx.recipient.insecure_clone();
    send(&mut ctx.svm, &[ix], &funder, &[]).unwrap();

    // Pause 111s in, resume 250s later — end extends to T0 + 997 + 250.
    warp_to(&mut ctx.svm, T0 + 111);
    { let __ix = control_ix(&ctx, 1, &funder.pubkey(), ixs::Pause {}.data()); send(&mut ctx.svm, &[__ix], &funder, &[]) }.unwrap();
    warp_to(&mut ctx.svm, T0 + 361);
    { let __ix = control_ix(&ctx, 1, &funder.pubkey(), ixs::Resume {}.data()); send(&mut ctx.svm, &[__ix], &funder, &[]) }.unwrap();

    // Past the extended end: a single withdraw pays the full deposit exactly.
    warp_to(&mut ctx.svm, T0 + 997 + 250 + 1);
    { let __ix = withdraw_ix(&ctx, 1, &recipient.pubkey()); send(&mut ctx.svm, &[__ix], &recipient, &[]) }.unwrap();
    assert_eq!(token_balance(&ctx.svm, &ctx.recipient_ata), odd_deposit);

    // And close reclaims the rent — the stream leaves zero residue on chain.
    let outsider = ctx.outsider.insecure_clone();
    { let __ix = close_ix(&ctx, 1, &outsider.pubkey()); send(&mut ctx.svm, &[__ix], &outsider, &[]) }.unwrap();
    let stream_pk = stream_pda(&funder.pubkey(), 1);
    assert!(ctx.svm.get_account(&stream_pk).is_none_or(|a| a.lamports == 0));
}
