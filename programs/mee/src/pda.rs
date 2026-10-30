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
