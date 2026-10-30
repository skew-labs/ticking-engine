//! Solana observer and attestation pool: how bridge deposits reach the chain
//! without anyone trusting a relayer.
//!
//! 1. Every validator polls a Solana RPC (finalized commitment) for the next
//!    `DepositRecord` PDAs of the bridge program, checks the account is owned
//!    by the program, and signs the chain's `deposit_message` for it.
//! 2. Signed attestations are gossiped (`Wire::Attest`); every node keeps a
//!    pool keyed by the attested facts and verifies each signature against
//!    the current validator set.
//! 3. Once the signers carry a quorum of the voting weight, any node turns
//!    the pool entry into a `SystemTx::BridgeDeposit` and submits it (and
//!    the mempool gossips it). Several nodes may do so; the chain accepts
//!    the first and the rest fail harmlessly as out of order.
//!
//! The RPC is trusted for what it reports, so validators should run their
//! own Solana node.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use borsh::{BorshDeserialize, BorshSerialize};
use mee::bridge::{DEPOSIT_DISCRIMINATOR, DepositRecord};
use mee_chain::crypto::{self, Hash, Pubkey, Sig};
use mee_chain::{Attestation, Chain, SystemTx, Tx};
use serde_json::{Value, json};

/// Deposit ids looked at past the chain's next expected one.
pub const LOOKAHEAD: u64 = 16;
/// Pool entries kept for ids up to this far past the next expected one.
const POOL_WINDOW: u64 = 1_024;

#[derive(Clone, Debug)]
pub struct SolanaConfig {
    /// JSON-RPC endpoint (preferably the validator's own node).
    pub rpc_url: String,
    pub poll: Duration,
}

/// A Solana fact a validator vouches for.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, BorshSerialize, BorshDeserialize)]
pub enum Fact {
    Deposit { id: u64, owner: Pubkey, asset: u8, amount: u64 },
}

impl Fact {
    /// The chain message validators sign for this fact.
    pub fn message(&self, chain: &Chain) -> Hash {
        match self {
            Fact::Deposit { id, owner, asset, amount } => {
                crypto::deposit_message(chain.chain_id, &chain.bridge_program, *id, owner, *asset, *amount)
            }
        }
    }

    fn id(&self) -> u64 {
        match self {
            Fact::Deposit { id, .. } => *id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SignedFact {
    pub fact: Fact,
    pub validator: Pubkey,
    pub signature: Sig,
}

/// Attestations collected so far: per deposit id, each validator's first
/// signed fact. One entry per (id, validator) bounds memory at
/// `POOL_WINDOW` × validators, whatever a misbehaving validator sends.
#[derive(Default)]
pub struct Pool {
    by_id: BTreeMap<u64, BTreeMap<Pubkey, (Fact, Sig)>>,
}

impl Pool {
    /// Adds a verified attestation; returns the system transaction once the
    /// fact has a quorum under the current set.
    pub fn add(&mut self, chain: &Chain, s: SignedFact) -> Option<Tx> {
        let (id, next) = (s.fact.id(), chain.next_deposit_id);
        if id < next || id > next + POOL_WINDOW || !chain.validators.contains(&s.validator) {
            return None;
        }
        let signers = self.by_id.entry(id).or_default();
        if signers.contains_key(&s.validator) {
            return None; // first fact per validator wins; no re-verification work
        }
        if !crypto::verify(&s.validator, &s.fact.message(chain), &s.signature) {
            return None;
        }
        signers.insert(s.validator, (s.fact.clone(), s.signature));
        Self::build(chain, &s.fact, signers)
    }

    fn build(chain: &Chain, fact: &Fact, signers: &BTreeMap<Pubkey, (Fact, Sig)>) -> Option<Tx> {
        let mut weight = 0u128;
        let mut attestations = Vec::new();
        for (i, (v, w)) in chain.validators.iter().zip(&chain.validator_weights).enumerate() {
            if let Some((f, sig)) = signers.get(v)
                && f == fact
            {
                weight += *w as u128;
                attestations.push(Attestation { validator: i as u16, signature: *sig });
            }
        }
        if !crypto::is_quorum(weight, chain.total_weight()) {
            return None;
        }
        Some(match fact {
            Fact::Deposit { id, owner, asset, amount } => Tx::System(SystemTx::BridgeDeposit {
                deposit_id: *id,
                owner: *owner,
                asset: *asset,
                amount: *amount,
                attestations,
            }),
        })
    }

    /// Drops ids the chain has moved past; returns transactions for the next
    /// few ids whose fact has a quorum (e.g. again after a set change).
    pub fn refresh(&mut self, chain: &Chain) -> Vec<Tx> {
        let next = chain.next_deposit_id;
        self.by_id.retain(|id, _| *id >= next);
        let mut out = Vec::new();
        for (_, signers) in self.by_id.range(next..next + LOOKAHEAD) {
            let mut facts: Vec<&Fact> = signers.values().map(|(f, _)| f).collect();
            facts.sort();
            facts.dedup();
            out.extend(facts.into_iter().filter_map(|f| Self::build(chain, f, signers)));
        }
        out
    }

    /// Number of (id, validator) entries held.
    pub fn len(&self) -> usize {
        self.by_id.values().map(BTreeMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

/// Minimal Solana JSON-RPC client.
pub struct Rpc {
    http: reqwest::Client,
    url: String,
}

impl Rpc {
    pub fn new(url: &str) -> Rpc {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("http client");
        Rpc { http, url: url.to_string() }
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// `(owner, data)` of a finalized account, or `None` if it doesn't exist.
    pub async fn account(&self, key: &Pubkey) -> Result<Option<(Pubkey, Vec<u8>)>, String> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getAccountInfo",
            "params": [bs58::encode(key).into_string(), { "encoding": "base64", "commitment": "finalized" }],
        });
        let resp: Value = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        if let Some(err) = resp.get("error") {
            return Err(err.to_string());
        }
        let value = &resp["result"]["value"];
        if value.is_null() {
            return Ok(None);
        }
        let owner: Pubkey = bs58::decode(value["owner"].as_str().ok_or("no owner")?)
            .into_vec()
            .map_err(|e| e.to_string())?
            .try_into()
            .map_err(|_| "bad owner")?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(value["data"][0].as_str().ok_or("no data")?)
            .map_err(|e| e.to_string())?;
        Ok(Some((owner, data)))
    }
}

/// Reads deposit record `id` of `bridge`, if it exists and is genuine.
pub async fn read_deposit(rpc: &Rpc, bridge: &Pubkey, id: u64) -> Result<Option<Fact>, String> {
    let program = solana_pubkey(bridge);
    let pda = mee::bridge::deposit_pda(&program, id).0.to_bytes();
    let Some((owner, data)) = rpc.account(&pda).await? else { return Ok(None) };
    if owner != *bridge {
        return Err(format!("deposit record {id} not owned by the bridge"));
    }
    let r: &DepositRecord = bytemuck::try_from_bytes(&data).map_err(|_| "bad deposit record size")?;
    if r.discriminator != DEPOSIT_DISCRIMINATOR || r.id != id {
        return Err(format!("deposit record {id} malformed"));
    }
    Ok(Some(Fact::Deposit { id, owner: r.owner, asset: r.asset, amount: r.amount }))
}

fn solana_pubkey(k: &Pubkey) -> mee::solana_program::pubkey::Pubkey {
    mee::solana_program::pubkey::Pubkey::new_from_array(*k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{devnet_genesis, devnet_key};

    #[test]
    fn a_spamming_validator_holds_one_entry_per_id_and_quorum_still_forms() {
        let chain = Chain::new(devnet_genesis(5, 4));
        let mut pool = Pool::default();
        let sign = |i: u32, f: &Fact| SignedFact {
            fact: f.clone(),
            validator: crypto::pubkey(&devnet_key(i)),
            signature: crypto::sign(&devnet_key(i), &f.message(&chain)),
        };
        let real = Fact::Deposit { id: 0, owner: [1; 32], asset: 0, amount: 100 };
        // Validator 3 floods variants of deposit 0 (and junk past the window).
        for amount in 0..1_000 {
            let fake = Fact::Deposit { id: 0, owner: [9; 32], asset: 0, amount };
            assert!(pool.add(&chain, sign(3, &fake)).is_none());
        }
        let far = Fact::Deposit { id: 1_000_000, owner: [9; 32], asset: 0, amount: 1 };
        assert!(pool.add(&chain, sign(3, &far)).is_none());
        assert_eq!(pool.len(), 1, "one entry for validator 3");
        // The honest three still reach a quorum (3 of 4) on the real deposit.
        assert!(pool.add(&chain, sign(0, &real)).is_none());
        assert!(pool.add(&chain, sign(1, &real)).is_none());
        let tx = pool.add(&chain, sign(2, &real)).expect("quorum");
        let Tx::System(SystemTx::BridgeDeposit { attestations, amount, .. }) = &tx else { panic!() };
        assert_eq!((attestations.len(), *amount), (3, 100));
        assert!(chain.check_tx(&tx).is_ok());
        assert_eq!(pool.len(), 4);
    }
}
