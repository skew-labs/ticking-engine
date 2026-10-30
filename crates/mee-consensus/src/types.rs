use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

pub type Hash = [u8; 32];
pub type Pubkey = [u8; 32];
pub type Sig = [u8; 64];
pub type Round = u64;
pub type NodeId = u16;

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

pub fn verify(pk: &Pubkey, msg: &[u8], sig: &Sig) -> bool {
    VerifyingKey::from_bytes(pk).is_ok_and(|vk| vk.verify(msg, &Signature::from_bytes(sig)).is_ok())
}

pub fn quorum(n: usize) -> usize {
    n * 2 / 3 + 1
}

pub fn vote_message(block: &Hash, round: Round) -> Hash {
    sha256(&[b"MEE/vote", block, &round.to_le_bytes()])
}

pub fn timeout_message(round: Round, high_qc_round: Round) -> Hash {
    sha256(&[b"MEE/timeout", &round.to_le_bytes(), &high_qc_round.to_le_bytes()])
}

pub fn proposal_message(block: &Hash) -> Hash {
    sha256(&[b"MEE/propose", block])
}

/// Quorum certificate: 2f+1 votes for `block` in `round`. The genesis QC
/// (round 0) carries no votes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Qc {
    pub block: Hash,
    pub round: Round,
    pub votes: Vec<(NodeId, Sig)>,
}

/// Timeout certificate: 2f+1 replicas gave up on `round`. Each entry signs
/// (round, its high-QC round); `high_qc` is the highest of those.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tc {
    pub round: Round,
    pub timeouts: Vec<(NodeId, Round, Sig)>,
    pub high_qc: Qc,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub round: Round,
    pub author: NodeId,
    /// Certifies the parent; `parent == qc.block`.
    pub qc: Qc,
    pub payload: Vec<u8>,
    pub signature: Sig,
}

impl Block {
    pub fn hash(&self) -> Hash {
        sha256(&[
            b"MEE/cblock",
            &self.round.to_le_bytes(),
            &self.author.to_le_bytes(),
            &self.qc.block,
            &self.qc.round.to_le_bytes(),
            &sha256(&[&self.payload]),
        ])
    }

    pub fn parent(&self) -> Hash {
        self.qc.block
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vote {
    pub block: Hash,
    pub round: Round,
    pub voter: NodeId,
    pub signature: Sig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timeout {
    pub round: Round,
    pub high_qc: Qc,
    pub voter: NodeId,
    pub signature: Sig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Propose {
        block: Block,
        tc: Option<Tc>,
    },
    Vote(Vote),
    Timeout(Timeout),
    /// Block sync: ask a peer for a block we are missing.
    Fetch(Hash),
    Deliver(Block),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    Broadcast(Message),
    Send(NodeId, Message),
}
