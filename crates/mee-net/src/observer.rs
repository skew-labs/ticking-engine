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

/// Attestations collected so far, by fact then signer.
#[derive(Default)]
pub struct Pool {
    facts: BTreeMap<Fact, BTreeMap<Pubkey, Sig>>,
}

impl Pool {
    /// Adds a verified attestation; returns the system transaction once the
    /// fact has a quorum under the current set.
    pub fn add(&mut self, chain: &Chain, s: SignedFact) -> Option<Tx> {
        let next = chain.next_deposit_id;
        if s.fact.id() < next || s.fact.id() > next + POOL_WINDOW || !chain.validators.contains(&s.validator) {
            return None;
        }
        if !crypto::verify(&s.validator, &s.fact.message(chain), &s.signature) {
            return None;
        }
        let signers = self.facts.entry(s.fact.clone()).or_default();
        signers.insert(s.validator, s.signature);
        Self::build(chain, &s.fact, signers)
    }

    fn build(chain: &Chain, fact: &Fact, signers: &BTreeMap<Pubkey, Sig>) -> Option<Tx> {
        let mut weight = 0u128;
        let mut attestations = Vec::new();
        for (i, (v, w)) in chain.validators.iter().zip(&chain.validator_weights).enumerate() {
            if let Some(sig) = signers.get(v) {
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

    /// Drops facts the chain has moved past; returns transactions for facts
    /// that have a quorum (e.g. again after a validator-set change).
    pub fn refresh(&mut self, chain: &Chain) -> Vec<Tx> {
        let next = chain.next_deposit_id;
        self.facts.retain(|f, _| f.id() >= next);
        self.facts.iter().filter_map(|(f, s)| Self::build(chain, f, s)).collect()
    }

    pub fn len(&self) -> usize {
        self.facts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
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
