use borsh::{BorshDeserialize, BorshSerialize};
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

/// A validator set with voting weights (stake). Quorum = more than two
/// thirds of the total weight; "more than a third" guarantees at least one
/// honest member when the faulty weight is below a third.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Committee {
    pub keys: Vec<Pubkey>,
    pub weights: Vec<u64>,
}

impl Committee {
    pub fn new(keys: Vec<Pubkey>, weights: Vec<u64>) -> Committee {
        assert_eq!(keys.len(), weights.len(), "one weight per key");
        Committee { keys, weights }
    }

    /// Every member with weight 1.
    pub fn equal(keys: Vec<Pubkey>) -> Committee {
        let weights = vec![1; keys.len()];
        Committee { keys, weights }
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn total(&self) -> u128 {
        self.weights.iter().map(|w| *w as u128).sum()
    }

    /// Summed weight of distinct, in-range members.
    pub fn weight_of(&self, ids: impl IntoIterator<Item = NodeId>) -> u128 {
        let mut seen = vec![false; self.len()];
        let mut w = 0;
        for i in ids {
            let i = i as usize;
            if i < seen.len() && !seen[i] {
                seen[i] = true;
                w += self.weights[i] as u128;
            }
        }
        w
    }

    pub fn is_quorum(&self, weight: u128) -> bool {
        3 * weight > 2 * self.total()
    }

    pub fn exceeds_third(&self, weight: u128) -> bool {
        3 * weight > self.total()
    }
}

/// Signed consensus messages carry the instance's genesis hash (one per
/// epoch), so a signature from one epoch or chain never counts in another and
/// two votes for the same round of the same instance are proof of
/// equivocation.
pub fn vote_message(domain: &Hash, block: &Hash, round: Round) -> Hash {
    sha256(&[b"MEE/vote", domain, block, &round.to_le_bytes()])
}

pub fn timeout_message(domain: &Hash, round: Round, high_qc_round: Round) -> Hash {
    sha256(&[b"MEE/timeout", domain, &round.to_le_bytes(), &high_qc_round.to_le_bytes()])
}

pub fn proposal_message(domain: &Hash, block: &Hash) -> Hash {
    sha256(&[b"MEE/propose", domain, block])
}

/// Quorum certificate: 2f+1 votes for `block` in `round`. The genesis QC
/// (round 0) carries no votes.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Qc {
    pub block: Hash,
    pub round: Round,
    pub votes: Vec<(NodeId, Sig)>,
}

/// Timeout certificate: 2f+1 replicas gave up on `round`. Each entry signs
/// (round, its high-QC round); `high_qc` is the highest of those.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Tc {
    pub round: Round,
    pub timeouts: Vec<(NodeId, Round, Sig)>,
    pub high_qc: Qc,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
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
        // The QC's voter set is part of the hash: leader reputation reads it,
        // so nobody may swap it for another valid set in transit.
        let mut voters: Vec<u8> = self.qc.votes.iter().flat_map(|(v, _)| v.to_le_bytes()).collect();
        voters.sort_unstable();
        sha256(&[
            b"MEE/cblock",
            &self.round.to_le_bytes(),
            &self.author.to_le_bytes(),
            &self.qc.block,
            &self.qc.round.to_le_bytes(),
            &voters,
            &sha256(&[&self.payload]),
        ])
    }

    pub fn voter_mask(&self) -> u64 {
        self.qc.votes.iter().fold(0u64, |m, (v, _)| m | (1u64 << (*v as u32 % 64)))
    }

    pub fn parent(&self) -> Hash {
        self.qc.block
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Vote {
    pub block: Hash,
    pub round: Round,
    pub voter: NodeId,
    pub signature: Sig,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Timeout {
    pub round: Round,
    pub high_qc: Qc,
    pub voter: NodeId,
    pub signature: Sig,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Out {
    Broadcast(Message),
    Send(NodeId, Message),
    /// Write this to durable storage before sending anything that follows
    /// it in the same output batch (it guards against double voting after a
    /// restart).
    Persist(SafetyState),
    /// Proof that `voter` signed votes for two different blocks in one
    /// round (the chain slashes it).
    Evidence(Equivocation),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Equivocation {
    pub voter: NodeId,
    pub round: Round,
    pub a: (Hash, Sig),
    pub b: (Hash, Sig),
}

/// What a replica must remember across restarts to stay safe.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SafetyState {
    pub round: Round,
    pub last_voted_round: Round,
    pub high_qc: Qc,
    /// Uncommitted blocks this replica voted for, by round. Every certified
    /// block is thus on the disk of at least f+1 honest voters, so a
    /// committed chain can always be completed even after the whole network
    /// restarts (proposals extend QCs whose blocks nobody else may hold).
    pub voted: Vec<Block>,
}
