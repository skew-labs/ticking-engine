//! Solana side of the MEE chain bridge.
//!
//! - Deposits lock tokens in a bridge vault and write a sequenced
//!   `DepositRecord`; MEE validators attest records in order and the chain
//!   credits them.
//! - Withdrawals (Hyperliquid-style): `RequestWithdrawal` records a chain
//!   receipt; `ApproveWithdrawal` adds validator signatures, checked by
//!   Solana's Ed25519 precompile in the same transaction and read back from
//!   the instructions sysvar, and can be repeated across transactions (so the
//!   validator set is not limited by transaction size); `FinalizeWithdrawal`
//!   pays out once a quorum approved and the dispute window passed. The
//!   record PDA per receipt id makes each payout happen once.
//! - Any single validator can `LockBridge` (halting finalization) when it
//!   sees fraud; a quorum can `UnlockBridge` or `CancelWithdrawal`.
//! - The validator set rotates only with a quorum of the current set; old
//!   approvals stop counting after a rotation.

use bytemuck::{Pod, Zeroable};
use mee_core::Key;
use solana_program::account_info::AccountInfo;
use solana_program::clock::Clock;
use solana_program::entrypoint::ProgramResult;
use solana_program::hash::hashv;
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::program::invoke;
use solana_program::program_error::ProgramError;
use solana_program::pubkey::Pubkey;
use solana_program::rent::Rent;
use solana_program::sysvar::SysvarSerialize;

use crate::error::MeeError;
use crate::token;

pub const MAX_VALIDATORS: usize = 16;
pub const BRIDGE_DISCRIMINATOR: [u8; 8] = *b"MEE:BRDG";
pub const DEPOSIT_DISCRIMINATOR: [u8; 8] = *b"MEE:DEPO";
pub const WITHDRAWAL_DISCRIMINATOR: [u8; 8] = *b"MEE:WDRW";
pub const ASSET_QUOTE: u8 = 0;
pub const ASSET_STAKE: u8 = 1;

pub const BRIDGE_SEED: &[u8] = b"bridge";
pub const BRIDGE_AUTHORITY_SEED: &[u8] = b"bridge_authority";
pub const BRIDGE_VAULT_SEED: &[u8] = b"bridge_vault";
pub const DEPOSIT_SEED: &[u8] = b"deposit";
pub const WITHDRAWAL_SEED: &[u8] = b"withdrawal";

pub const ED25519_PROGRAM_ID: Pubkey = solana_program::pubkey!("Ed25519SigVerify111111111111111111111111111");
const INSTRUCTIONS_SYSVAR_ID: Pubkey = solana_program::pubkey!("Sysvar1nstructions1111111111111111111111111");

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct Bridge {
    pub discriminator: [u8; 8],
    pub admin: Key,
    pub quote_mint: Key,
    pub stake_mint: Key,
    pub chain_id: u64,
    pub next_deposit_id: u64,
    pub valset_nonce: u64,
    pub validators: [Key; MAX_VALIDATORS],
    pub validator_count: u8,
    /// Signatures needed: more than two thirds, same as the chain.
    pub threshold: u8,
    pub bump: u8,
    pub authority_bump: u8,
    pub locked: u8,
    pub _pad: [u8; 3],
    /// Seconds between reaching quorum and being able to finalize.
    pub dispute_secs: i64,
    /// Bumped on every unlock so old lock signatures can't be replayed.
    pub lock_nonce: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct DepositRecord {
    pub discriminator: [u8; 8],
    pub id: u64,
    pub owner: Key,
    pub amount: u64,
    pub slot: u64,
    pub asset: u8,
    pub _pad: [u8; 7],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct WithdrawalRecord {
    pub discriminator: [u8; 8],
    pub id: u64,
    pub destination: Key,
    pub amount: u64,
    /// When approvals first reached the threshold (0 = not yet).
    pub quorum_ts: i64,
    /// Validator set the approvals belong to.
    pub valset_nonce: u64,
    /// Bitmap of approving validator indices.
    pub approvals: u32,
    pub asset: u8,
    pub status: u8,
    pub _pad: [u8; 2],
}

pub const WITHDRAWAL_PENDING: u8 = 1;
pub const WITHDRAWAL_PAID: u8 = 2;
pub const WITHDRAWAL_CANCELLED: u8 = 3;

pub fn quorum(n: usize) -> usize {
    n * 2 / 3 + 1
}

// --- messages (must match mee-chain's crypto module byte for byte) -------------

pub fn withdrawal_message(chain_id: u64, id: u64, asset: u8, destination: &[u8; 32], amount: u64) -> [u8; 32] {
    hashv(&[b"MEE/withdraw", &chain_id.to_le_bytes(), &id.to_le_bytes(), &[asset], destination, &amount.to_le_bytes()])
        .to_bytes()
}

pub fn lock_message(chain_id: u64, lock_nonce: u64) -> [u8; 32] {
    hashv(&[b"MEE/lock", &chain_id.to_le_bytes(), &lock_nonce.to_le_bytes()]).to_bytes()
}

pub fn unlock_message(chain_id: u64, lock_nonce: u64) -> [u8; 32] {
    hashv(&[b"MEE/unlock", &chain_id.to_le_bytes(), &lock_nonce.to_le_bytes()]).to_bytes()
}

pub fn cancel_message(chain_id: u64, id: u64) -> [u8; 32] {
    hashv(&[b"MEE/cancel", &chain_id.to_le_bytes(), &id.to_le_bytes()]).to_bytes()
}

pub fn valset_message(chain_id: u64, nonce: u64, validators: &[[u8; 32]]) -> [u8; 32] {
    let mut parts: Vec<&[u8]> = vec![b"MEE/valset"];
    let c = chain_id.to_le_bytes();
    let n = nonce.to_le_bytes();
    parts.push(&c);
    parts.push(&n);
    for v in validators {
        parts.push(v);
    }
    hashv(&parts).to_bytes()
}

// --- PDAs ---------------------------------------------------------------------

pub fn bridge_pda(program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[BRIDGE_SEED], program_id)
}

pub fn authority_pda(program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[BRIDGE_AUTHORITY_SEED], program_id)
}

pub fn vault_pda(program_id: &Pubkey, mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[BRIDGE_VAULT_SEED, mint.as_ref()], program_id)
}

pub fn deposit_pda(program_id: &Pubkey, id: u64) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[DEPOSIT_SEED, &id.to_le_bytes()], program_id)
}

pub fn withdrawal_pda(program_id: &Pubkey, id: u64) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[WITHDRAWAL_SEED, &id.to_le_bytes()], program_id)
}

// --- Ed25519 precompile -------------------------------------------------------

/// Builds one Ed25519 precompile instruction carrying several signatures over
/// the same 32-byte message (the message is stored once).
pub fn ed25519_instruction(message: &[u8; 32], signatures: &[([u8; 32], [u8; 64])]) -> Instruction {
    let n = signatures.len();
    let header = 2 + 14 * n;
    let msg_offset = header;
    let mut data = vec![n as u8, 0];
    let mut body = message.to_vec();
    let mut entries = Vec::new();
    for (i, _) in signatures.iter().enumerate() {
        let base = header + 32 + i * 96;
        let (sig_off, pk_off) = (base, base + 64);
        for v in [sig_off as u16, u16::MAX, pk_off as u16, u16::MAX, msg_offset as u16, 32, u16::MAX] {
            entries.extend_from_slice(&v.to_le_bytes());
        }
    }
    for (pk, sig) in signatures {
        body.extend_from_slice(sig);
        body.extend_from_slice(pk);
    }
    data.extend_from_slice(&entries);
    data.extend_from_slice(&body);
    Instruction { program_id: ED25519_PROGRAM_ID, accounts: vec![], data }
}

/// Bitmap of current validators whose signature over `message` was verified
/// by an Ed25519 precompile instruction in this transaction. Only entries
/// whose offsets all point into the precompile instruction itself count, so
/// what we read is exactly what was verified.
fn verified_signers(instructions: &AccountInfo, bridge: &Bridge, message: &[u8; 32]) -> Result<u32, ProgramError> {
    if instructions.key != &INSTRUCTIONS_SYSVAR_ID {
        return Err(MeeError::InvalidAccountData.into());
    }
    #[allow(deprecated)]
    use solana_program::sysvar::instructions::load_instruction_at_checked;
    let mut seen = 0u32;
    let mut idx = 0usize;
    while let Ok(ix) = load_instruction_at_checked(idx, instructions) {
        idx += 1;
        if ix.program_id != ED25519_PROGRAM_ID {
            continue;
        }
        let d = &ix.data;
        let Some(&n) = d.first() else { continue };
        for i in 0..n as usize {
            let at = 2 + 14 * i;
            let Some(e) = d.get(at..at + 14) else { break };
            let u = |k: usize| u16::from_le_bytes([e[k], e[k + 1]]);
            let (pk_off, msg_off, msg_len) = (u(4) as usize, u(8) as usize, u(10) as usize);
            if u(2) != u16::MAX || u(6) != u16::MAX || u(12) != u16::MAX || msg_len != 32 {
                continue;
            }
            let (Some(pk), Some(msg)) = (d.get(pk_off..pk_off + 32), d.get(msg_off..msg_off + 32)) else { continue };
            if msg != message {
                continue;
            }
            let validators = &bridge.validators[..bridge.validator_count as usize];
            if let Some(v) = validators.iter().position(|k| k[..] == pk[..]) {
                seen |= 1 << v;
            }
        }
    }
    Ok(seen)
}

fn signer_count(instructions: &AccountInfo, bridge: &Bridge, message: &[u8; 32]) -> Result<usize, ProgramError> {
    Ok(verified_signers(instructions, bridge, message)?.count_ones() as usize)
}

// --- processing -----------------------------------------------------------------

type Accounts<'a, 'b> = core::slice::Iter<'b, AccountInfo<'a>>;

fn next<'a, 'b>(it: &mut Accounts<'a, 'b>) -> Result<&'b AccountInfo<'a>, ProgramError> {
    it.next().ok_or(ProgramError::NotEnoughAccountKeys)
}

fn load_bridge(ai: &AccountInfo, program_id: &Pubkey) -> Result<Bridge, ProgramError> {
    if ai.owner != program_id {
        return Err(MeeError::InvalidAccountOwner.into());
    }
    let data = ai.try_borrow_data()?;
    let b: &Bridge = bytemuck::try_from_bytes(&data[..]).map_err(|_| MeeError::InvalidAccountData)?;
    if b.discriminator != BRIDGE_DISCRIMINATOR {
        return Err(MeeError::InvalidAccountData.into());
    }
    let expected =
        Pubkey::create_program_address(&[BRIDGE_SEED, &[b.bump]], program_id).map_err(|_| MeeError::InvalidPda)?;
    if ai.key != &expected {
        return Err(MeeError::InvalidPda.into());
    }
    Ok(*b)
}

fn store<T: Pod>(ai: &AccountInfo, v: &T) -> ProgramResult {
    ai.try_borrow_mut_data()?.copy_from_slice(bytemuck::bytes_of(v));
    Ok(())
}

fn set_validators(b: &mut Bridge, validators: &[[u8; 32]]) -> ProgramResult {
    if validators.is_empty() || validators.len() > MAX_VALIDATORS {
        return Err(MeeError::TooManyGuardians.into());
    }
    for (i, v) in validators.iter().enumerate() {
        if validators[..i].contains(v) {
            return Err(MeeError::InvalidInstruction.into());
        }
    }
    b.validators = [[0; 32]; MAX_VALIDATORS];
    b.validators[..validators.len()].copy_from_slice(validators);
    b.validator_count = validators.len() as u8;
    b.threshold = quorum(validators.len()) as u8;
    Ok(())
}

fn mint_of(b: &Bridge, asset: u8) -> Result<Pubkey, ProgramError> {
    match asset {
        ASSET_QUOTE => Ok(Pubkey::new_from_array(b.quote_mint)),
        ASSET_STAKE => Ok(Pubkey::new_from_array(b.stake_mint)),
        _ => Err(MeeError::InvalidInstruction.into()),
    }
}

pub fn init(
    program_id: &Pubkey,
    it: &mut Accounts,
    chain_id: u64,
    validators: &[[u8; 32]],
    dispute_secs: i64,
) -> ProgramResult {
    let admin = next(it)?;
    let bridge_ai = next(it)?;
    let quote_mint = next(it)?;
    let stake_mint = next(it)?;
    let quote_vault = next(it)?;
    let stake_vault = next(it)?;
    let authority = next(it)?;
    let token_program = next(it)?;
    let system_program = next(it)?;
    let rent = Rent::from_account_info(next(it)?)?;
    if !admin.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    token::check_token_program(token_program)?;
    let (bp, bump) = bridge_pda(program_id);
    let (auth, authority_bump) = authority_pda(program_id);
    if bridge_ai.key != &bp || authority.key != &auth {
        return Err(MeeError::InvalidPda.into());
    }
    let mut b = Bridge {
        discriminator: BRIDGE_DISCRIMINATOR,
        admin: admin.key.to_bytes(),
        quote_mint: quote_mint.key.to_bytes(),
        stake_mint: stake_mint.key.to_bytes(),
        chain_id,
        next_deposit_id: 0,
        valset_nonce: 0,
        validators: [[0; 32]; MAX_VALIDATORS],
        validator_count: 0,
        threshold: 0,
        bump,
        authority_bump,
        locked: 0,
        _pad: [0; 3],
        dispute_secs: dispute_secs.max(0),
        lock_nonce: 0,
    };
    set_validators(&mut b, validators)?;
    token::create_pda_account(
        admin,
        bridge_ai,
        system_program,
        &rent,
        size_of::<Bridge>(),
        program_id,
        &[BRIDGE_SEED, &[bump]],
    )?;
    for (vault, mint) in [(quote_vault, quote_mint), (stake_vault, stake_mint)] {
        let (vp, vbump) = vault_pda(program_id, mint.key);
        if vault.key != &vp {
            return Err(MeeError::InvalidPda.into());
        }
        token::create_pda_account(
            admin,
            vault,
            system_program,
            &rent,
            token::TOKEN_ACCOUNT_LEN,
            &token::TOKEN_PROGRAM_ID,
            &[BRIDGE_VAULT_SEED, mint.key.as_ref(), &[vbump]],
        )?;
        invoke(
            &token::initialize_account3_ix(vault.key, mint.key, &auth),
            &[vault.clone(), mint.clone(), token_program.clone()],
        )?;
    }
    store(bridge_ai, &b)
}

pub fn deposit(program_id: &Pubkey, it: &mut Accounts, asset: u8, amount: u64) -> ProgramResult {
    let owner = next(it)?;
    let bridge_ai = next(it)?;
    let record = next(it)?;
    let source = next(it)?;
    let vault = next(it)?;
    let token_program = next(it)?;
    let system_program = next(it)?;
    let rent = Rent::from_account_info(next(it)?)?;
    let clock = Clock::from_account_info(next(it)?)?;
    if !owner.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if amount == 0 {
        return Err(MeeError::InvalidInstruction.into());
    }
    let mut b = load_bridge(bridge_ai, program_id)?;
    let mint = mint_of(&b, asset)?;
    if vault.key != &vault_pda(program_id, &mint).0 {
        return Err(MeeError::InvalidPda.into());
    }
    let id = b.next_deposit_id;
    let (rp, rbump) = deposit_pda(program_id, id);
    if record.key != &rp {
        return Err(MeeError::InvalidPda.into());
    }
    token::check_token_program(token_program)?;
    token::transfer(token_program, source, vault, owner, amount, &[])?;
    token::create_pda_account(
        owner,
        record,
        system_program,
        &rent,
        size_of::<DepositRecord>(),
        program_id,
        &[DEPOSIT_SEED, &id.to_le_bytes(), &[rbump]],
    )?;
    store(
        record,
        &DepositRecord {
            discriminator: DEPOSIT_DISCRIMINATOR,
            id,
            owner: owner.key.to_bytes(),
            amount,
            slot: clock.slot,
            asset,
            _pad: [0; 7],
        },
    )?;
    b.next_deposit_id += 1;
    store(bridge_ai, &b)
}

fn load_withdrawal(ai: &AccountInfo, program_id: &Pubkey, id: u64) -> Result<WithdrawalRecord, ProgramError> {
    if ai.owner != program_id || ai.key != &withdrawal_pda(program_id, id).0 {
        return Err(MeeError::InvalidPda.into());
    }
    let data = ai.try_borrow_data()?;
    let r: &WithdrawalRecord = bytemuck::try_from_bytes(&data[..]).map_err(|_| MeeError::InvalidAccountData)?;
    if r.discriminator != WITHDRAWAL_DISCRIMINATOR {
        return Err(MeeError::InvalidAccountData.into());
    }
    Ok(*r)
}

/// [payer (s,w), bridge, record (w), system_program, rent]
pub fn request_withdrawal(
    program_id: &Pubkey,
    it: &mut Accounts,
    id: u64,
    asset: u8,
    destination: [u8; 32],
    amount: u64,
) -> ProgramResult {
    let payer = next(it)?;
    let bridge_ai = next(it)?;
    let record = next(it)?;
    let system_program = next(it)?;
    let rent = Rent::from_account_info(next(it)?)?;
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let b = load_bridge(bridge_ai, program_id)?;
    mint_of(&b, asset)?;
    let (rp, rbump) = withdrawal_pda(program_id, id);
    if record.key != &rp {
        return Err(MeeError::InvalidPda.into());
    }
    // Fails if the record exists: each receipt id is requested once, ever.
    token::create_pda_account(
        payer,
        record,
        system_program,
        &rent,
        size_of::<WithdrawalRecord>(),
        program_id,
        &[WITHDRAWAL_SEED, &id.to_le_bytes(), &[rbump]],
    )?;
    store(
        record,
        &WithdrawalRecord {
            discriminator: WITHDRAWAL_DISCRIMINATOR,
            id,
            destination,
            amount,
            quorum_ts: 0,
            valset_nonce: b.valset_nonce,
            approvals: 0,
            asset,
            status: WITHDRAWAL_PENDING,
            _pad: [0; 2],
        },
    )
}

/// [bridge, record (w), instructions_sysvar, clock]. Adds the validator
/// signatures verified in this transaction; repeatable.
pub fn approve_withdrawal(program_id: &Pubkey, it: &mut Accounts, id: u64) -> ProgramResult {
    let bridge_ai = next(it)?;
    let record = next(it)?;
    let instructions = next(it)?;
    let clock = Clock::from_account_info(next(it)?)?;
    let b = load_bridge(bridge_ai, program_id)?;
    let mut r = load_withdrawal(record, program_id, id)?;
    if r.status != WITHDRAWAL_PENDING {
        return Err(MeeError::InvalidInstruction.into());
    }
    if r.valset_nonce != b.valset_nonce {
        // The set rotated: earlier approvals were by the old set.
        r.valset_nonce = b.valset_nonce;
        r.approvals = 0;
        r.quorum_ts = 0;
    }
    let msg = withdrawal_message(b.chain_id, r.id, r.asset, &r.destination, r.amount);
    let signers = verified_signers(instructions, &b, &msg)?;
    if signers == 0 {
        return Err(MeeError::Unauthorized.into());
    }
    r.approvals |= signers;
    if r.quorum_ts == 0 && r.approvals.count_ones() >= b.threshold as u32 {
        r.quorum_ts = clock.unix_timestamp;
    }
    store(record, &r)
}

/// [bridge, record (w), vault (w), destination_token (w), authority,
///  token_program, clock]. Permissionless.
pub fn finalize_withdrawal(program_id: &Pubkey, it: &mut Accounts, id: u64) -> ProgramResult {
    let bridge_ai = next(it)?;
    let record = next(it)?;
    let vault = next(it)?;
    let dest_token = next(it)?;
    let authority = next(it)?;
    let token_program = next(it)?;
    let clock = Clock::from_account_info(next(it)?)?;
    let b = load_bridge(bridge_ai, program_id)?;
    let mut r = load_withdrawal(record, program_id, id)?;
    if b.locked != 0 {
        return Err(MeeError::BridgeLocked.into());
    }
    if r.status != WITHDRAWAL_PENDING
        || r.valset_nonce != b.valset_nonce
        || r.quorum_ts == 0
        || r.approvals.count_ones() < b.threshold as u32
    {
        return Err(MeeError::Unauthorized.into());
    }
    if clock.unix_timestamp < r.quorum_ts.saturating_add(b.dispute_secs) {
        return Err(MeeError::DisputeWindow.into());
    }
    let mint = mint_of(&b, r.asset)?;
    if vault.key != &vault_pda(program_id, &mint).0 {
        return Err(MeeError::InvalidPda.into());
    }
    let dest = token::read_token_account(dest_token)?;
    if dest.owner.to_bytes() != r.destination || dest.mint != mint {
        return Err(MeeError::InvalidTokenAccount.into());
    }
    r.status = WITHDRAWAL_PAID;
    store(record, &r)?;
    token::check_token_program(token_program)?;
    token::transfer(
        token_program,
        vault,
        dest_token,
        authority,
        r.amount,
        &[&[BRIDGE_AUTHORITY_SEED, &[b.authority_bump]]],
    )
}

/// [bridge (w), instructions_sysvar]. One validator is enough to lock.
pub fn lock(program_id: &Pubkey, it: &mut Accounts) -> ProgramResult {
    let bridge_ai = next(it)?;
    let instructions = next(it)?;
    let mut b = load_bridge(bridge_ai, program_id)?;
    if signer_count(instructions, &b, &lock_message(b.chain_id, b.lock_nonce))? == 0 {
        return Err(MeeError::Unauthorized.into());
    }
    b.locked = 1;
    store(bridge_ai, &b)
}

/// [bridge (w), instructions_sysvar]. Needs a quorum.
pub fn unlock(program_id: &Pubkey, it: &mut Accounts) -> ProgramResult {
    let bridge_ai = next(it)?;
    let instructions = next(it)?;
    let mut b = load_bridge(bridge_ai, program_id)?;
    if signer_count(instructions, &b, &unlock_message(b.chain_id, b.lock_nonce))? < b.threshold as usize {
        return Err(MeeError::Unauthorized.into());
    }
    b.locked = 0;
    b.lock_nonce += 1;
    store(bridge_ai, &b)
}

/// [bridge, record (w), instructions_sysvar]. A quorum voids a pending
/// withdrawal during its dispute window.
pub fn cancel_withdrawal(program_id: &Pubkey, it: &mut Accounts, id: u64) -> ProgramResult {
    let bridge_ai = next(it)?;
    let record = next(it)?;
    let instructions = next(it)?;
    let b = load_bridge(bridge_ai, program_id)?;
    let mut r = load_withdrawal(record, program_id, id)?;
    if r.status != WITHDRAWAL_PENDING {
        return Err(MeeError::InvalidInstruction.into());
    }
    if signer_count(instructions, &b, &cancel_message(b.chain_id, id))? < b.threshold as usize {
        return Err(MeeError::Unauthorized.into());
    }
    r.status = WITHDRAWAL_CANCELLED;
    store(record, &r)
}

pub fn rotate(program_id: &Pubkey, it: &mut Accounts, validators: &[[u8; 32]]) -> ProgramResult {
    let bridge_ai = next(it)?;
    let instructions = next(it)?;
    let mut b = load_bridge(bridge_ai, program_id)?;
    let msg = valset_message(b.chain_id, b.valset_nonce, validators);
    if signer_count(instructions, &b, &msg)? < b.threshold as usize {
        return Err(MeeError::Unauthorized.into());
    }
    set_validators(&mut b, validators)?;
    b.valset_nonce += 1;
    store(bridge_ai, &b)
}

// --- client builders ------------------------------------------------------------

use crate::instruction::MeeInstruction;

fn ix(program_id: &Pubkey, data: MeeInstruction, accounts: Vec<AccountMeta>) -> Instruction {
    Instruction { program_id: *program_id, accounts, data: borsh::to_vec(&data).expect("borsh") }
}

pub fn init_bridge_ix(
    program_id: &Pubkey,
    admin: &Pubkey,
    quote_mint: &Pubkey,
    stake_mint: &Pubkey,
    chain_id: u64,
    validators: Vec<[u8; 32]>,
    dispute_secs: i64,
) -> Instruction {
    ix(
        program_id,
        MeeInstruction::InitBridge { chain_id, validators, dispute_secs },
        vec![
            AccountMeta::new(*admin, true),
            AccountMeta::new(bridge_pda(program_id).0, false),
            AccountMeta::new_readonly(*quote_mint, false),
            AccountMeta::new_readonly(*stake_mint, false),
            AccountMeta::new(vault_pda(program_id, quote_mint).0, false),
            AccountMeta::new(vault_pda(program_id, stake_mint).0, false),
            AccountMeta::new_readonly(authority_pda(program_id).0, false),
            AccountMeta::new_readonly(token::TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(token::SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(solana_program::sysvar::rent::ID, false),
        ],
    )
}

/// `next_deposit_id` is read from the bridge account first.
pub fn deposit_ix(
    program_id: &Pubkey,
    owner: &Pubkey,
    source: &Pubkey,
    mint: &Pubkey,
    next_deposit_id: u64,
    asset: u8,
    amount: u64,
) -> Instruction {
    ix(
        program_id,
        MeeInstruction::BridgeDeposit { asset, amount },
        vec![
            AccountMeta::new(*owner, true),
            AccountMeta::new(bridge_pda(program_id).0, false),
            AccountMeta::new(deposit_pda(program_id, next_deposit_id).0, false),
            AccountMeta::new(*source, false),
            AccountMeta::new(vault_pda(program_id, mint).0, false),
            AccountMeta::new_readonly(token::TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(token::SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(solana_program::sysvar::rent::ID, false),
            AccountMeta::new_readonly(solana_program::sysvar::clock::ID, false),
        ],
    )
}

pub fn request_withdrawal_ix(
    program_id: &Pubkey,
    payer: &Pubkey,
    id: u64,
    asset: u8,
    destination: [u8; 32],
    amount: u64,
) -> Instruction {
    ix(
        program_id,
        MeeInstruction::RequestWithdrawal { id, asset, destination, amount },
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(bridge_pda(program_id).0, false),
            AccountMeta::new(withdrawal_pda(program_id, id).0, false),
            AccountMeta::new_readonly(token::SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(solana_program::sysvar::rent::ID, false),
        ],
    )
}

/// Pair with `ed25519_instruction(withdrawal_message(..), signatures)`
/// earlier in the same transaction.
pub fn approve_withdrawal_ix(program_id: &Pubkey, id: u64) -> Instruction {
    ix(
        program_id,
        MeeInstruction::ApproveWithdrawal { id },
        vec![
            AccountMeta::new_readonly(bridge_pda(program_id).0, false),
            AccountMeta::new(withdrawal_pda(program_id, id).0, false),
            AccountMeta::new_readonly(INSTRUCTIONS_SYSVAR_ID, false),
            AccountMeta::new_readonly(solana_program::sysvar::clock::ID, false),
        ],
    )
}

pub fn finalize_withdrawal_ix(program_id: &Pubkey, mint: &Pubkey, destination_token: &Pubkey, id: u64) -> Instruction {
    ix(
        program_id,
        MeeInstruction::FinalizeWithdrawal { id },
        vec![
            AccountMeta::new_readonly(bridge_pda(program_id).0, false),
            AccountMeta::new(withdrawal_pda(program_id, id).0, false),
            AccountMeta::new(vault_pda(program_id, mint).0, false),
            AccountMeta::new(*destination_token, false),
            AccountMeta::new_readonly(authority_pda(program_id).0, false),
            AccountMeta::new_readonly(token::TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(solana_program::sysvar::clock::ID, false),
        ],
    )
}

pub fn lock_ix(program_id: &Pubkey) -> Instruction {
    ix(
        program_id,
        MeeInstruction::LockBridge,
        vec![
            AccountMeta::new(bridge_pda(program_id).0, false),
            AccountMeta::new_readonly(INSTRUCTIONS_SYSVAR_ID, false),
        ],
    )
}

pub fn unlock_ix(program_id: &Pubkey) -> Instruction {
    ix(
        program_id,
        MeeInstruction::UnlockBridge,
        vec![
            AccountMeta::new(bridge_pda(program_id).0, false),
            AccountMeta::new_readonly(INSTRUCTIONS_SYSVAR_ID, false),
        ],
    )
}

pub fn cancel_withdrawal_ix(program_id: &Pubkey, id: u64) -> Instruction {
    ix(
        program_id,
        MeeInstruction::CancelWithdrawal { id },
        vec![
            AccountMeta::new_readonly(bridge_pda(program_id).0, false),
            AccountMeta::new(withdrawal_pda(program_id, id).0, false),
            AccountMeta::new_readonly(INSTRUCTIONS_SYSVAR_ID, false),
        ],
    )
}

pub fn rotate_ix(program_id: &Pubkey, validators: Vec<[u8; 32]>) -> Instruction {
    ix(
        program_id,
        MeeInstruction::RotateValidators { validators },
        vec![
            AccountMeta::new(bridge_pda(program_id).0, false),
            AccountMeta::new_readonly(INSTRUCTIONS_SYSVAR_ID, false),
        ],
    )
}
