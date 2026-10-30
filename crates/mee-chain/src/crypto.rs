//! Domain-separated messages and ed25519 helpers. Keys are plain 32-byte
//! ed25519 public keys, so Solana wallets sign MEE transactions directly.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

pub type Pubkey = [u8; 32];
pub type Sig = [u8; 64];
pub type Hash = [u8; 32];

pub fn sha256(parts: &[&[u8]]) -> Hash {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

pub fn sign(key: &SigningKey, msg: &[u8]) -> Sig {
    key.sign(msg).to_bytes()
}

pub fn verify(pubkey: &Pubkey, msg: &[u8], sig: &Sig) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(pubkey) else { return false };
    vk.verify(msg, &Signature::from_bytes(sig)).is_ok()
}

pub fn pubkey(key: &SigningKey) -> Pubkey {
    key.verifying_key().to_bytes()
}

/// Votes needed from `n` equal-weight validators: more than two thirds.
pub fn quorum(n: usize) -> usize {
    n * 2 / 3 + 1
}

/// What validators sign to attest a Solana bridge deposit.
pub fn deposit_message(chain_id: u64, deposit_id: u64, owner: &Pubkey, asset: u8, amount: u64) -> Hash {
    sha256(&[
        b"MEE/deposit",
        &chain_id.to_le_bytes(),
        &deposit_id.to_le_bytes(),
        owner,
        &[asset],
        &amount.to_le_bytes(),
    ])
}

/// What validators sign to release a withdrawal on Solana. The Solana bridge
/// program rebuilds exactly these bytes.
pub fn withdrawal_message(chain_id: u64, id: u64, asset: u8, destination: &Pubkey, amount: u64) -> Hash {
    sha256(&[b"MEE/withdraw", &chain_id.to_le_bytes(), &id.to_le_bytes(), &[asset], destination, &amount.to_le_bytes()])
}

/// What validators sign to list a graduated launchpad token as a perp.
pub fn list_message(
    chain_id: u64,
    launch_id: u64,
    mint: &Pubkey,
    creator: &Pubkey,
    symbol: &[u8; 16],
    twap_scaled: u128,
    pool_quote: u64,
) -> Hash {
    sha256(&[
        b"MEE/list",
        &chain_id.to_le_bytes(),
        &launch_id.to_le_bytes(),
        mint,
        creator,
        symbol,
        &twap_scaled.to_le_bytes(),
        &pool_quote.to_le_bytes(),
    ])
}

/// What validators sign for a batch of launchpad pool observations taken at
/// Solana slot `slot`.
pub fn launch_oracle_message(chain_id: u64, slot: u64, updates: &[u8]) -> Hash {
    sha256(&[b"MEE/launch-oracle", &chain_id.to_le_bytes(), &slot.to_le_bytes(), updates])
}

/// What validators sign so light clients can trust a state root.
pub fn state_root_message(chain_id: u64, height: u64, state_root: &Hash) -> Hash {
    sha256(&[b"MEE/state-root", &chain_id.to_le_bytes(), &height.to_le_bytes(), state_root])
}

/// What validators sign for a governance action (`body` is its encoding).
pub fn governance_message(chain_id: u64, nonce: u64, body: &[u8]) -> Hash {
    sha256(&[b"MEE/governance", &chain_id.to_le_bytes(), &nonce.to_le_bytes(), body])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quorum_sizes() {
        assert_eq!(quorum(1), 1);
        assert_eq!(quorum(3), 3);
        assert_eq!(quorum(4), 3);
        assert_eq!(quorum(7), 5);
        assert_eq!(quorum(10), 7);
    }

    #[test]
    fn sign_verify() {
        let k = SigningKey::from_bytes(&[7; 32]);
        let s = sign(&k, b"hello");
        assert!(verify(&pubkey(&k), b"hello", &s));
        assert!(!verify(&pubkey(&k), b"hellp", &s));
    }
}
