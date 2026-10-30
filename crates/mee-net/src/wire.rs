//! Wire format: length-prefixed borsh frames over TCP, opened by a mutual
//! handshake in which each side proves it holds the validator key it claims
//! by signing the other side's fresh nonce.

use std::io;
use std::time::Duration;

use borsh::{BorshDeserialize, BorshSerialize};
use ed25519_dalek::SigningKey;
use mee_chain::Tx;
use mee_chain::crypto::{self, Hash, Pubkey, Sig};
use mee_node::{Envelope, SyncRequest, SyncResponse};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame accepted (sync responses are the big ones).
pub const MAX_FRAME: usize = 64 << 20;
const MAGIC: [u8; 4] = *b"MEE1";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Wire {
    Consensus(Envelope),
    /// Transaction gossip.
    Tx(Tx),
    /// Periodic announcement of the sender's committed tip.
    Status {
        epoch: u64,
        height: u64,
    },
    SyncRequest(SyncRequest),
    SyncResponse(SyncResponse),
    /// A validator's signed attestation of a Solana fact.
    Attest(crate::observer::SignedFact),
}

#[derive(BorshSerialize, BorshDeserialize)]
struct Hello {
    magic: [u8; 4],
    chain_id: u64,
    key: Pubkey,
    nonce: [u8; 32],
}

/// Encodes one frame: u32 little-endian length, then the borsh body.
pub fn encode<T: BorshSerialize>(msg: &T) -> Vec<u8> {
    let mut buf = vec![0u8; 4];
    msg.serialize(&mut buf).expect("borsh to vec");
    let len = (buf.len() - 4) as u32;
    buf[..4].copy_from_slice(&len.to_le_bytes());
    buf
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Vec<u8>> {
    let len = r.read_u32_le().await? as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(body)
}

pub async fn read_msg<R: AsyncRead + Unpin, T: BorshDeserialize>(r: &mut R) -> io::Result<T> {
    let body = read_frame(r).await?;
    T::try_from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn auth_message(chain_id: u64, their_nonce: &[u8; 32], my_nonce: &[u8; 32], my_key: &Pubkey) -> Hash {
    crypto::sha256(&[b"MEE/p2p-handshake", &chain_id.to_le_bytes(), their_nonce, my_nonce, my_key])
}

/// Runs the handshake on a fresh connection and returns the peer's
/// authenticated key. Both sides run the same steps, so it works the same
/// for dialer and listener.
pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    key: &SigningKey,
    chain_id: u64,
) -> io::Result<Pubkey> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let me = crypto::pubkey(key);
        let mut nonce = [0u8; 32];
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut nonce);
        stream.write_all(&encode(&Hello { magic: MAGIC, chain_id, key: me, nonce })).await?;
        let hello: Hello = read_msg(stream).await?;
        let bad = |why: &str| io::Error::new(io::ErrorKind::PermissionDenied, why.to_string());
        if hello.magic != MAGIC || hello.chain_id != chain_id {
            return Err(bad("wrong network"));
        }
        if hello.key == me {
            return Err(bad("connected to self"));
        }
        let sig: Sig = crypto::sign(key, &auth_message(chain_id, &hello.nonce, &nonce, &me));
        stream.write_all(&encode(&sig)).await?;
        let theirs: Sig = read_msg(stream).await?;
        if !crypto::verify(&hello.key, &auth_message(chain_id, &nonce, &hello.nonce, &hello.key), &theirs) {
            return Err(bad("bad handshake signature"));
        }
        Ok(hello.key)
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn handshake_authenticates_both_sides() {
        let (a, b) = (SigningKey::from_bytes(&[1; 32]), SigningKey::from_bytes(&[2; 32]));
        let (mut x, mut y) = tokio::io::duplex(1 << 16);
        let (ra, rb) = tokio::join!(handshake(&mut x, &a, 9), handshake(&mut y, &b, 9));
        assert_eq!(ra.unwrap(), crypto::pubkey(&b));
        assert_eq!(rb.unwrap(), crypto::pubkey(&a));
    }

    #[tokio::test]
    async fn handshake_rejects_other_chain_and_forged_key() {
        let (a, b) = (SigningKey::from_bytes(&[1; 32]), SigningKey::from_bytes(&[2; 32]));
        let (mut x, mut y) = tokio::io::duplex(1 << 16);
        let (ra, rb) = tokio::join!(handshake(&mut x, &a, 9), handshake(&mut y, &b, 10));
        assert!(ra.is_err() && rb.is_err());

        // A peer claiming `b`'s key without holding it fails the signature.
        let (mut x, mut y) = tokio::io::duplex(1 << 16);
        let honest = handshake(&mut x, &a, 9);
        let forger = async {
            let mut nonce = [7u8; 32];
            nonce[0] = 0;
            y.write_all(&encode(&Hello { magic: MAGIC, chain_id: 9, key: crypto::pubkey(&b), nonce })).await?;
            let _hello: Hello = read_msg(&mut y).await?;
            y.write_all(&encode(&[0u8; 64])).await?;
            let _sig: Sig = read_msg(&mut y).await?;
            io::Result::Ok(())
        };
        let (ra, _) = tokio::join!(honest, forger);
        assert_eq!(ra.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn frames_round_trip() {
        let w = Wire::Status { epoch: 3, height: 99 };
        let f = encode(&w);
        assert_eq!(u32::from_le_bytes(f[..4].try_into().unwrap()) as usize, f.len() - 4);
        assert_eq!(Wire::try_from_slice(&f[4..]).unwrap(), w);
    }
}
