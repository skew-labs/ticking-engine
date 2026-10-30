//! Wire format: length-prefixed borsh frames over TCP, opened by a mutual
//! handshake and then encrypted.
//!
//! Handshake: each side sends its key, a fresh nonce and an ephemeral X25519
//! key, then signs the transcript (both nonces, both ephemeral keys, its own
//! key) with its ed25519 key. So each side proves it holds the key it claims
//! and the Diffie-Hellman exchange is bound to those identities (a
//! man in the middle can't substitute its own ephemeral key). Each direction
//! then gets its own ChaCha20-Poly1305 key derived from the shared secret and
//! the transcript; frames carry a per-direction counter as nonce, so frames
//! can't be replayed, reordered, dropped silently or forged.

use std::io;
use std::time::Duration;

use borsh::{BorshDeserialize, BorshSerialize};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use ed25519_dalek::SigningKey;
use mee_chain::Tx;
use mee_chain::crypto::{self, Hash, Pubkey, Sig};
use mee_node::{Envelope, SyncRequest, SyncResponse};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame accepted (sync responses are the big ones).
pub const MAX_FRAME: usize = 64 << 20;
const MAGIC: [u8; 4] = *b"MEE2";
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
    ephemeral: [u8; 32],
}

/// Encrypts outgoing frames of one connection.
pub struct Sealer {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

/// Decrypts incoming frames of one connection.
pub struct Opener {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&counter.to_le_bytes());
    Nonce::from(n)
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sealer").field("counter", &self.counter).finish_non_exhaustive()
    }
}

impl Sealer {
    /// Turns a plaintext frame (as built by `encode`) into an encrypted one.
    pub fn seal(&mut self, frame: &[u8]) -> Vec<u8> {
        let ct = self.cipher.encrypt(&nonce(self.counter), &frame[4..]).expect("encrypt");
        self.counter += 1;
        let mut out = (ct.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(&ct);
        out
    }
}

impl std::fmt::Debug for Opener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opener").field("counter", &self.counter).finish_non_exhaustive()
    }
}

impl Opener {
    pub fn open(&mut self, ciphertext: &[u8]) -> io::Result<Vec<u8>> {
        let pt = self
            .cipher
            .decrypt(&nonce(self.counter), ciphertext)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame failed authentication"))?;
        self.counter += 1;
        Ok(pt)
    }

    /// Reads, authenticates and decrypts one frame.
    pub async fn read<R: AsyncRead + Unpin>(&mut self, r: &mut R) -> io::Result<Vec<u8>> {
        let ct = read_frame(r).await?;
        self.open(&ct)
    }
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

/// Reads one small handshake message (before the peer is authenticated, so
/// it gets a tiny size limit instead of `MAX_FRAME`).
pub async fn read_msg<R: AsyncRead + Unpin, T: BorshDeserialize>(r: &mut R) -> io::Result<T> {
    let len = r.read_u32_le().await? as usize;
    if len > HANDSHAKE_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "handshake frame too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    T::try_from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

const HANDSHAKE_FRAME: usize = 1_024;

/// What each side signs: the peer's nonce and ephemeral key, then its own,
/// then its own identity.
fn auth_message(chain_id: u64, theirs: &Hello, mine: &Hello) -> Hash {
    crypto::sha256(&[
        b"MEE/p2p-handshake-v2",
        &chain_id.to_le_bytes(),
        &theirs.nonce,
        &theirs.ephemeral,
        &mine.nonce,
        &mine.ephemeral,
        &mine.key,
    ])
}

/// Session keys for both directions, named by which side's identity key
/// sorts first, so both ends derive the same pair.
fn session_keys(shared: &[u8; 32], a: &Hello, b: &Hello) -> ([u8; 32], [u8; 32]) {
    let (lo, hi) = if a.key < b.key { (a, b) } else { (b, a) };
    let k = |dir: &[u8]| {
        crypto::sha256(&[
            b"MEE/p2p-key",
            dir,
            shared,
            &lo.key,
            &lo.nonce,
            &lo.ephemeral,
            &hi.key,
            &hi.nonce,
            &hi.ephemeral,
        ])
    };
    (k(b"lo->hi"), k(b"hi->lo"))
}

/// Runs the handshake on a fresh connection and returns the peer's
/// authenticated key and the session ciphers. Both sides run the same steps,
/// so it works the same for dialer and listener.
pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    key: &SigningKey,
    chain_id: u64,
) -> io::Result<(Pubkey, Sealer, Opener)> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let me = crypto::pubkey(key);
        let mut nonce = [0u8; 32];
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut nonce);
        let eph = x25519_dalek::EphemeralSecret::random_from_rng(rand_core::OsRng);
        let ephemeral = x25519_dalek::PublicKey::from(&eph).to_bytes();
        let mine = Hello { magic: MAGIC, chain_id, key: me, nonce, ephemeral };
        stream.write_all(&encode(&mine)).await?;
        let hello: Hello = read_msg(stream).await?;
        let bad = |why: &str| io::Error::new(io::ErrorKind::PermissionDenied, why.to_string());
        if hello.magic != MAGIC || hello.chain_id != chain_id {
            return Err(bad("wrong network"));
        }
        if hello.key == me {
            return Err(bad("connected to self"));
        }
        let sig: Sig = crypto::sign(key, &auth_message(chain_id, &hello, &mine));
        stream.write_all(&encode(&sig)).await?;
        let theirs: Sig = read_msg(stream).await?;
        if !crypto::verify(&hello.key, &auth_message(chain_id, &mine, &hello), &theirs) {
            return Err(bad("bad handshake signature"));
        }
        let shared = eph.diffie_hellman(&x25519_dalek::PublicKey::from(hello.ephemeral));
        if !shared.was_contributory() {
            return Err(bad("degenerate key exchange"));
        }
        let (lo_hi, hi_lo) = session_keys(shared.as_bytes(), &mine, &hello);
        let (send, recv) = if me < hello.key { (lo_hi, hi_lo) } else { (hi_lo, lo_hi) };
        let cipher = |k: [u8; 32]| ChaCha20Poly1305::new(&Key::from(k));
        Ok((hello.key, Sealer { cipher: cipher(send), counter: 0 }, Opener { cipher: cipher(recv), counter: 0 }))
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
        let ((ka, mut sa, mut oa), (kb, mut sb, mut ob)) = (ra.unwrap(), rb.unwrap());
        assert_eq!((ka, kb), (crypto::pubkey(&b), crypto::pubkey(&a)));

        // Encrypted both ways; tampering, replay and reordering are caught.
        let f1 = sa.seal(&encode(&Wire::Status { epoch: 1, height: 2 }));
        let f2 = sa.seal(&encode(&Wire::Status { epoch: 1, height: 3 }));
        assert!(!f1.windows(8).any(|w| w == 2u64.to_le_bytes()), "no plaintext on the wire");
        assert_eq!(Wire::try_from_slice(&ob.open(&f1[4..]).unwrap()).unwrap(), Wire::Status { epoch: 1, height: 2 });
        assert!(ob.open(&f1[4..]).is_err(), "replay");
        let mut bad = f2.clone();
        bad[10] ^= 1;
        assert!(ob.open(&bad[4..]).is_err(), "tampered");
        let back = sb.seal(&encode(&Wire::Status { epoch: 7, height: 7 }));
        assert_eq!(Wire::try_from_slice(&oa.open(&back[4..]).unwrap()).unwrap(), Wire::Status { epoch: 7, height: 7 });
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
            let hello = Hello { magic: MAGIC, chain_id: 9, key: crypto::pubkey(&b), nonce, ephemeral: [9; 32] };
            y.write_all(&encode(&hello)).await?;
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
