use solana_program::pubkey::Pubkey;

pub const EXCHANGE_SEED: &[u8] = b"exchange";
pub const QUOTE_VAULT_SEED: &[u8] = b"quote_vault";
pub const STAKE_VAULT_SEED: &[u8] = b"stake_vault";
pub const VAULT_AUTHORITY_SEED: &[u8] = b"vault_authority";

pub fn exchange(program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[EXCHANGE_SEED], program_id)
}

/// Collateral vault of one DEX (an SPL token account in the quote mint).
pub fn quote_vault(program_id: &Pubkey, dex: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[QUOTE_VAULT_SEED, dex.as_ref()], program_id)
}

/// Deployer stake escrow of one DEX (an SPL token account in the stake mint).
pub fn stake_vault(program_id: &Pubkey, dex: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[STAKE_VAULT_SEED, dex.as_ref()], program_id)
}

/// Token owner of both vaults of one DEX.
pub fn vault_authority(program_id: &Pubkey, dex: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[VAULT_AUTHORITY_SEED, dex.as_ref()], program_id)
}

pub const UPGRADEABLE_LOADER_ID: Pubkey = solana_program::pubkey!("BPFLoaderUpgradeab1e11111111111111111111111");

/// The upgradeable loader's ProgramData account of `program_id`.
pub fn program_data(program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[program_id.as_ref()], &UPGRADEABLE_LOADER_ID).0
}

/// Bytes of a ProgramData header (bincode `UpgradeableLoaderState::ProgramData`)
/// with `authority` as upgrade authority. For tests and tooling.
pub fn program_data_header(authority: &Pubkey) -> Vec<u8> {
    let mut d = 3u32.to_le_bytes().to_vec();
    d.extend_from_slice(&0u64.to_le_bytes());
    d.push(1);
    d.extend_from_slice(authority.as_ref());
    d
}

/// Singletons (bridge, launchpad config, exchange) may only be initialized
/// by the program's upgrade authority. Otherwise whoever sends the first init
/// after deployment picks the validators, mints and admin. Only the loader
/// can create the ProgramData account at this address, so its contents are
/// genuine.
pub fn check_upgrade_authority(
    program_id: &Pubkey,
    program_data_ai: &solana_program::account_info::AccountInfo,
    signer: &solana_program::account_info::AccountInfo,
) -> solana_program::entrypoint::ProgramResult {
    use crate::error::MeeError;
    if program_data_ai.key != &program_data(program_id) || program_data_ai.owner != &UPGRADEABLE_LOADER_ID {
        return Err(MeeError::InvalidPda.into());
    }
    let d = program_data_ai.try_borrow_data()?;
    let ok = signer.is_signer
        && d.len() >= 45
        && d[..4] == 3u32.to_le_bytes()
        && d[12] == 1
        && d[13..45] == signer.key.as_ref()[..];
    if ok { Ok(()) } else { Err(MeeError::Unauthorized.into()) }
}
