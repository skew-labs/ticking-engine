//! Withdrawal relayer: carries chain withdrawal receipts to the Solana
//! bridge. It is untrusted and permissionless (anyone can run one, and the
//! bridge checks everything): it only pays fees.
//!
//! For each receipt, in id order:
//! 1. read the bridge account and the receipt's record (if any) on Solana;
//! 2. collect validators' signatures over the withdrawal message from their
//!    APIs (`GET /withdrawals/{id}`), checking each one locally, until the
//!    signers hold a quorum of the bridge's voting weight;
//! 3. send `RequestWithdrawal` plus `ApproveWithdrawal` transactions (the
//!    signatures go through the Ed25519 precompile, `CHUNK` per
//!    transaction); once the dispute window has passed, create the
//!    recipient's associated token account if needed and `FinalizeWithdrawal`.
//!
//! `plan` is the pure part: what to send given what is on Solana.

use std::time::Duration;

use base64::Engine as _;
use mee::bridge::{self, Bridge, WITHDRAWAL_PENDING, WithdrawalRecord};
use mee::solana_program::instruction::{AccountMeta, Instruction};
use mee::solana_program::pubkey::Pubkey;
use mee_chain::crypto;
use serde_json::{Value, json};

use crate::config::{b58, parse_b58};
use crate::observer::Rpc;

/// Validator signatures per Ed25519 precompile instruction (keeps each
/// transaction under Solana's 1232-byte limit).
pub const CHUNK: usize = 5;

pub const ATA_PROGRAM_ID: Pubkey = mee::solana_program::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub id: u64,
    pub asset: u8,
    pub destination: [u8; 32],
    pub amount: u64,
}

pub fn associated_token_account(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    let token = mee::token::TOKEN_PROGRAM_ID;
    Pubkey::find_program_address(&[owner.as_ref(), token.as_ref(), mint.as_ref()], &ATA_PROGRAM_ID).0
}

/// `CreateIdempotent` of the associated token account program.
pub fn create_ata_ix(payer: &Pubkey, owner: &Pubkey, mint: &Pubkey) -> Instruction {
    Instruction {
        program_id: ATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(associated_token_account(owner, mint), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(mee::token::SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(mee::token::TOKEN_PROGRAM_ID, false),
        ],
        data: vec![1],
    }
}

/// Where a receipt's payout goes on Solana.
pub enum Destination {
    /// A token account that exists already.
    Account(Pubkey),
    /// The destination's associated token account, created if missing.
    Associated,
}

/// What a relayer knows about a receipt on Solana.
pub struct OnSolana<'a> {
    pub bridge: &'a Bridge,
    pub record: Option<&'a WithdrawalRecord>,
    pub now: i64,
}

/// Transactions (instruction lists, in order) that move `r` forward.
/// `sigs` are validator signatures over the withdrawal message (unchecked
/// ones are ignored by the bridge, but callers should verify them).
pub fn plan(
    program: &Pubkey,
    payer: &Pubkey,
    r: &Receipt,
    on: &OnSolana,
    sigs: &[([u8; 32], [u8; 64])],
    dest: &Destination,
) -> Vec<Vec<Instruction>> {
    let b = on.bridge;
    let msg = bridge::withdrawal_message(b.chain_id, &program.to_bytes(), r.id, r.asset, &r.destination, r.amount);
    let record = bridge::withdrawal_pda(program, &msg).0;
    let (approvals, current) = match on.record {
        Some(rec) if rec.status != WITHDRAWAL_PENDING => return Vec::new(), // paid or cancelled
        Some(rec) if rec.valset_nonce == b.valset_nonce => (rec.approvals, true),
        Some(_) => (0, true), // approvals by an old set no longer count
        None => (0, false),
    };
    let set = &b.validators[..b.validator_count as usize];
    let missing: Vec<([u8; 32], [u8; 64])> = sigs
        .iter()
        .filter(|(k, _)| set.iter().position(|v| v == k).is_some_and(|i| approvals & (1 << i) == 0))
        .copied()
        .collect();
    let mut txs = Vec::new();
    if !current {
        txs.push(vec![bridge::request_withdrawal_ix(
            program,
            payer,
            b.chain_id,
            r.id,
            r.asset,
            r.destination,
            r.amount,
        )]);
    }
    for (i, chunk) in missing.chunks(CHUNK).enumerate() {
        let approve =
            vec![bridge::ed25519_instruction(&msg, chunk), bridge::approve_withdrawal_ix(program, &record, r.id)];
        // The request rides with the first approval when it fits.
        if i == 0 && !current {
            txs[0].extend(approve);
        } else {
            txs.push(approve);
        }
    }
    if let Some(rec) = on.record
        && rec.status == WITHDRAWAL_PENDING
        && rec.valset_nonce == b.valset_nonce
        && rec.quorum_ts != 0
        && b.is_quorum(rec.approvals)
        && b.locked == 0
        && on.now >= rec.quorum_ts.saturating_add(b.dispute_secs)
    {
        let mint = Pubkey::new_from_array(if r.asset == bridge::ASSET_QUOTE { b.quote_mint } else { b.stake_mint });
        let owner = Pubkey::new_from_array(r.destination);
        let mut ixs = Vec::new();
        let token_account = match dest {
            Destination::Account(a) => *a,
            Destination::Associated => {
                ixs.push(create_ata_ix(payer, &owner, &mint));
                associated_token_account(&owner, &mint)
            }
        };
        ixs.push(bridge::finalize_withdrawal_ix(program, &mint, &token_account, &record, r.id));
        txs.push(ixs);
    }
    txs
}

/// Solana transaction wire format: compact length, signatures, message.
pub fn wire_bytes(tx: &solana_transaction::Transaction) -> Vec<u8> {
    let mut out = Vec::new();
    let mut n = tx.signatures.len();
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    for s in &tx.signatures {
        out.extend_from_slice(s.as_ref());
    }
    out.extend_from_slice(&tx.message_data());
    out
}

pub struct RelayerConfig {
    pub solana_rpc: String,
    /// Validator API base URLs (`http://host:port`).
    pub validators: Vec<String>,
    pub program: Pubkey,
    pub payer: solana_keypair::Keypair,
    pub poll: Duration,
    /// First receipt id to look at.
    pub from: u64,
}

impl Rpc {
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp: Value = self
            .http()
            .post(self.url())
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        if let Some(e) = resp.get("error") {
            return Err(e.to_string());
        }
        Ok(resp["result"].clone())
    }

    pub async fn latest_blockhash(&self) -> Result<[u8; 32], String> {
        let r = self.call("getLatestBlockhash", json!([{ "commitment": "finalized" }])).await?;
        parse_b58(r["value"]["blockhash"].as_str().ok_or("no blockhash")?).map_err(|e| e.to_string())
    }

    pub async fn send(&self, tx: &[u8]) -> Result<String, String> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(tx);
        let r = self.call("sendTransaction", json!([b64, { "encoding": "base64" }])).await?;
        Ok(r.as_str().unwrap_or_default().to_string())
    }

    pub async fn unix_time(&self) -> Result<i64, String> {
        let slot = self.call("getSlot", json!([{ "commitment": "finalized" }])).await?;
        let t = self.call("getBlockTime", json!([slot])).await?;
        t.as_i64().ok_or_else(|| "no block time".to_string())
    }
}

/// Signatures the validators' APIs hand out for receipt `id`, each checked
/// against the withdrawal message.
async fn collect(http: &reqwest::Client, urls: &[String], id: u64, msg: &[u8; 32]) -> Vec<([u8; 32], [u8; 64])> {
    let mut out = Vec::new();
    for u in urls {
        let Ok(resp) = http.get(format!("{u}/withdrawals/{id}")).send().await else { continue };
        let Ok(v) = resp.json::<Value>().await else { continue };
        let (Some(signer), Some(sig)) = (v["signer"].as_str(), v["signature"].as_str()) else { continue };
        let (Ok(signer), Ok(sig)) = (parse_b58(signer), hex::decode(sig)) else { continue };
        let Ok(sig) = <[u8; 64]>::try_from(sig) else { continue };
        if crypto::verify(&signer, msg, &sig) && !out.iter().any(|(k, _)| *k == signer) {
            out.push((signer, sig));
        }
    }
    out
}

async fn account<T: bytemuck::Pod>(rpc: &Rpc, key: &Pubkey) -> Result<Option<T>, String> {
    Ok(rpc.account(&key.to_bytes()).await?.and_then(|(_, d)| bytemuck::try_from_bytes::<T>(&d).ok().copied()))
}

/// Runs forever: relays receipts from `cfg.from` on, in order.
pub async fn run(cfg: RelayerConfig) {
    use solana_signer::Signer as _;
    let rpc = Rpc::new(&cfg.solana_rpc);
    let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("http client");
    let payer = cfg.payer.pubkey();
    let mut next = cfg.from;
    loop {
        if let Err(e) = step(&rpc, &http, &cfg, &payer, &mut next).await {
            eprintln!("relayer: {e}");
        }
        tokio::time::sleep(cfg.poll).await;
    }
}

async fn step(
    rpc: &Rpc,
    http: &reqwest::Client,
    cfg: &RelayerConfig,
    payer: &Pubkey,
    next: &mut u64,
) -> Result<(), String> {
    let url = cfg.validators.first().ok_or("no validator API configured")?;
    let list: Value = http
        .get(format!("{url}/withdrawals?from={next}&limit=50"))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let bridge_key = bridge::bridge_pda(&cfg.program).0;
    let b: Bridge = account(rpc, &bridge_key).await?.ok_or("bridge account not found")?;
    let now = rpc.unix_time().await?;
    for w in list["withdrawals"].as_array().into_iter().flatten() {
        let r = Receipt {
            id: w["id"].as_u64().ok_or("bad id")?,
            asset: w["asset"].as_u64().ok_or("bad asset")? as u8,
            destination: parse_b58(w["destination"].as_str().ok_or("bad destination")?).map_err(|e| e.to_string())?,
            amount: w["amount"].as_str().and_then(|a| a.parse().ok()).ok_or("bad amount")?,
        };
        let msg =
            bridge::withdrawal_message(b.chain_id, &cfg.program.to_bytes(), r.id, r.asset, &r.destination, r.amount);
        let record: Option<WithdrawalRecord> = account(rpc, &bridge::withdrawal_pda(&cfg.program, &msg).0).await?;
        if record.is_some_and(|rec| rec.status != WITHDRAWAL_PENDING) {
            if r.id == *next {
                *next += 1; // done; move the cursor past it
            }
            continue;
        }
        let sigs = collect(http, &cfg.validators, r.id, &msg).await;
        let on = OnSolana { bridge: &b, record: record.as_ref(), now };
        for ixs in plan(&cfg.program, payer, &r, &on, &sigs, &Destination::Associated) {
            let blockhash = solana_transaction::Hash::new_from_array(rpc.latest_blockhash().await?);
            let tx =
                solana_transaction::Transaction::new_signed_with_payer(&ixs, Some(payer), &[&cfg.payer], blockhash);
            let sig = rpc.send(&wire_bytes(&tx)).await?;
            eprintln!("relayer: receipt {} -> {} ({})", r.id, sig, b58(&r.destination));
        }
    }
    Ok(())
}
