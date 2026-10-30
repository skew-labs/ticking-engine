//! HTTP/JSON API and WebSocket block stream.
//!
//! - `POST /tx` `{"tx": "<hex borsh Tx>"}` -> `{"accepted": bool, "hash"}`
//! - `GET /status`
//! - `GET /book/{market}?depth=N`
//! - `GET /account/{pubkey}` (base58 or hex): balances, positions and a Merkle proof
//!   against the latest state root
//! - `GET /withdrawals?from=ID&limit=N`: receipts from `from` on (max 500)
//! - `GET /withdrawals/{id}`: the receipt and this validator's signature
//!   over the bridge message (a relayer collects a quorum of these)
//! - `GET /triggers/{pubkey}`: the account's pending TP/SL orders
//! - `GET /state_root/{height}`: the state root and this validator's
//!   signature over it (light clients)
//! - `GET /ws`: one JSON message per committed block (events, withdrawals,
//!   roots)
//! - `GET /metrics`: Prometheus text format
//!
//! Integers that can exceed 2^53 are encoded as strings. Each client IP gets
//! a token bucket of `api_rate_per_sec` requests per second (burst 2x);
//! beyond it requests get 429.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::ConnectInfo;
use axum::middleware::{self, Next};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use borsh::BorshDeserialize;
use mee_chain::{BlockOutcome, Tx, crypto};
use mee_core::Side;
use mee_engine::{CancelReason, Event};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::broadcast::error::RecvError;

use crate::Shared;

type S = State<Arc<Shared>>;

/// Per-IP token buckets.
struct RateLimit {
    per_sec: f64,
    buckets: Mutex<HashMap<IpAddr, (f64, Instant)>>,
}

impl RateLimit {
    fn allow(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let burst = 2.0 * self.per_sec;
        let mut b = self.buckets.lock().unwrap();
        if b.len() > 100_000 {
            b.retain(|_, (_, t)| now.duration_since(*t).as_secs() < 60);
        }
        let (tokens, last) = b.entry(ip).or_insert((burst, now));
        *tokens = (*tokens + now.duration_since(*last).as_secs_f64() * self.per_sec).min(burst);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

async fn limit(
    State(rl): State<Arc<RateLimit>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    if rl.per_sec > 0.0 && !rl.allow(addr.ip()) {
        return err(StatusCode::TOO_MANY_REQUESTS, "rate limited");
    }
    next.run(req).await
}

pub async fn serve(shared: Arc<Shared>, listener: TcpListener) {
    let rl = Arc::new(RateLimit { per_sec: shared.api_rate_per_sec as f64, buckets: Mutex::default() });
    let app = Router::new()
        .route("/tx", post(submit))
        .route("/status", get(status))
        .route("/book/{market}", get(book))
        .route("/account/{key}", get(account))
        .route("/withdrawals", get(withdrawals))
        .route("/withdrawals/{id}", get(withdrawal))
        .route("/triggers/{key}", get(triggers))
        .route("/state_root/{height}", get(state_root))
        .route("/ws", get(ws))
        .route("/metrics", get(metrics))
        .with_state(shared)
        .layer(middleware::from_fn_with_state(rl, limit));
    let _ = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await;
}

async fn metrics(State(s): S) -> Response {
    let peers = s.peer_keys().len();
    let pool = s.pool.lock().unwrap().len();
    let n = s.node.lock().unwrap();
    let (txs, failed) =
        n.outcomes.last().map_or((0, 0), |o| (o.results.len(), o.results.iter().filter(|r| r.is_err()).count()));
    let body = format!(
        "# TYPE mee_height gauge\nmee_height {}\n\
         # TYPE mee_epoch gauge\nmee_epoch {}\n\
         # TYPE mee_consensus_round gauge\nmee_consensus_round {}\n\
         # TYPE mee_is_validator gauge\nmee_is_validator {}\n\
         # TYPE mee_validators gauge\nmee_validators {}\n\
         # TYPE mee_peers gauge\nmee_peers {}\n\
         # TYPE mee_mempool_txs gauge\nmee_mempool_txs {}\n\
         # TYPE mee_attestation_pool gauge\nmee_attestation_pool {}\n\
         # TYPE mee_last_block_txs gauge\nmee_last_block_txs {}\n\
         # TYPE mee_last_block_failed_txs gauge\nmee_last_block_failed_txs {}\n\
         # TYPE mee_next_deposit_id gauge\nmee_next_deposit_id {}\n\
         # TYPE mee_withdrawals_total counter\nmee_withdrawals_total {}\n",
        n.height(),
        n.epoch,
        n.replica.round,
        n.replica.is_validator() as u8,
        n.chain.validators.len(),
        peers,
        n.mempool().count(),
        pool,
        txs,
        failed,
        n.chain.next_deposit_id,
        n.chain.withdrawals.len(),
    );
    ([(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

/// Hashes and signatures: hex.
fn h(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// Keys: base58, as on Solana.
fn k58(key: &[u8]) -> String {
    crate::config::b58(key)
}

#[derive(Deserialize)]
struct SubmitBody {
    tx: String,
}

async fn submit(State(s): S, Json(body): Json<SubmitBody>) -> Response {
    let Ok(bytes) = hex::decode(body.tx.trim_start_matches("0x")) else {
        return err(StatusCode::BAD_REQUEST, "tx must be hex");
    };
    let Ok(tx) = Tx::try_from_slice(&bytes) else {
        return err(StatusCode::BAD_REQUEST, "tx does not decode");
    };
    let hash = mee_node::tx_hash(&tx);
    let accepted = s.submit(tx).await;
    Json(json!({ "accepted": accepted, "hash": h(&hash) })).into_response()
}

async fn status(State(s): S) -> Json<Value> {
    let peers: Vec<String> = s.peer_keys().iter().map(|k| k58(k)).collect();
    let n = s.node.lock().unwrap();
    Json(json!({
        "chain_id": s.chain_id,
        "node": k58(&n.me),
        "height": n.height(),
        "epoch": n.epoch,
        "timestamp_ms": n.chain.timestamp_ms,
        "app_hash": h(&n.chain.app_hash),
        "state_root": h(&n.chain.state_root),
        "validators": n.chain.committee().iter().map(|(k, w)| json!({ "key": k58(k), "weight": w.to_string() })).collect::<Vec<_>>(),
        "is_validator": n.replica.is_validator(),
        "mempool": n.mempool().count(),
        "peers": peers,
    }))
}

#[derive(Deserialize)]
struct DepthQuery {
    depth: Option<usize>,
}

async fn book(State(s): S, Path(market): Path<u32>, Query(q): Query<DepthQuery>) -> Response {
    let depth = q.depth.unwrap_or(20).min(500);
    let n = s.node.lock().unwrap();
    let Some(m) = n.chain.state.markets.get(market as usize) else {
        return err(StatusCode::NOT_FOUND, "no such market");
    };
    let levels = |side| m.book.depth(side, depth).into_iter().map(|(p, q)| json!([p, q])).collect::<Vec<_>>();
    Json(json!({
        "market": market,
        "dex": m.dex,
        "status": m.status,
        "height": n.height(),
        "oracle_price": m.oracle_price,
        "last_trade_price": m.last_trade_price,
        "open_interest": m.open_interest,
        "bids": levels(Side::Bid),
        "asks": levels(Side::Ask),
    }))
    .into_response()
}

/// Keys are accepted as base58 (Solana style) or hex.
fn parse_key(s: &str) -> Option<[u8; 32]> {
    let hex_key = s.strip_prefix("0x").unwrap_or(s);
    if hex_key.len() == 64 {
        return hex::decode(hex_key).ok()?.try_into().ok();
    }
    crate::config::parse_b58(s).ok()
}

async fn account(State(s): S, Path(key): Path<String>) -> Response {
    let Some(owner) = parse_key(&key) else {
        return err(StatusCode::BAD_REQUEST, "key must be 32 bytes hex");
    };
    let n = s.node.lock().unwrap();
    let state = &n.chain.state;
    let Some(a) = state.account_id(&owner).and_then(|id| state.accounts.get(id as usize)) else {
        return err(StatusCode::NOT_FOUND, "no such account");
    };
    let margins: Vec<Value> = a
        .margins
        .iter()
        .map(|(dex, m)| {
            json!({
                "dex": dex,
                "collateral": m.collateral.to_string(),
                "positions": m.positions.iter().map(|(market, p)| json!({
                    "market": market,
                    "base": p.base.to_string(),
                    "quote": p.quote.to_string(),
                    "open_bids": p.open_bids,
                    "open_asks": p.open_asks,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let proof = n.chain.prove_account(&owner).map(|p| {
        json!({
            "id": p.id,
            "leaf": h(&p.leaf),
            "path": p.path.iter().map(|x| h(x)).collect::<Vec<_>>(),
            "roots": {
                "accounts": h(&p.roots.accounts),
                "markets": h(&p.roots.markets),
                "dexes": h(&p.roots.dexes),
                "globals": h(&p.roots.globals),
                "root": h(&p.roots.root),
            },
            "globals": h(&p.globals),
        })
    });
    Json(json!({
        "owner": k58(&a.owner),
        "height": n.height(),
        "state_root": h(&n.chain.state_root),
        "balance": a.balance.to_string(),
        "stake_balance": a.stake_balance.to_string(),
        "nonce": a.nonce,
        "margins": margins,
        "proof": proof,
    }))
    .into_response()
}

async fn triggers(State(s): S, Path(key): Path<String>) -> Response {
    let Some(owner) = parse_key(&key) else {
        return err(StatusCode::BAD_REQUEST, "bad key");
    };
    let n = s.node.lock().unwrap();
    let Some(a) = n.chain.state.account_id(&owner) else {
        return err(StatusCode::NOT_FOUND, "no such account");
    };
    let list: Vec<Value> = n
        .chain
        .triggers
        .iter()
        .filter(|(_, t)| t.account == a)
        .map(|(id, t)| {
            json!({
                "id": id, "market": t.market, "side": side(t.side), "above": t.above,
                "trigger_price": t.trigger_price, "limit_price": t.limit_price, "size": t.size,
                "reduce_only": t.reduce_only,
            })
        })
        .collect();
    Json(json!({ "triggers": list })).into_response()
}

#[derive(Deserialize)]
struct RangeQuery {
    from: Option<u64>,
    limit: Option<usize>,
}

async fn withdrawals(State(s): S, Query(q): Query<RangeQuery>) -> Json<Value> {
    let n = s.node.lock().unwrap();
    let from = q.from.unwrap_or(0) as usize;
    let list: Vec<Value> = n
        .chain
        .withdrawals
        .iter()
        .skip(from)
        .take(q.limit.unwrap_or(100).min(500))
        .map(|w| json!({ "id": w.id, "asset": w.asset, "destination": k58(&w.destination), "amount": w.amount.to_string() }))
        .collect();
    Json(json!({ "withdrawals": list, "next": n.chain.withdrawals.len() }))
}

async fn withdrawal(State(s): S, Path(id): Path<u64>) -> Response {
    // Receipt ids are sequential from 0, so the id is the index. Copy it out
    // and sign without holding the node lock.
    let found = {
        let n = s.node.lock().unwrap();
        n.chain.withdrawals.get(id as usize).filter(|w| w.id == id).map(|w| (*w, n.chain.bridge_program))
    };
    let Some((w, bridge)) = found else {
        return err(StatusCode::NOT_FOUND, "no such withdrawal");
    };
    let msg = w.message(s.chain_id, &bridge);
    Json(json!({
        "id": w.id,
        "asset": w.asset,
        "destination": k58(&w.destination),
        "amount": w.amount.to_string(),
        "message": h(&msg),
        "signer": k58(&s.me()),
        "signature": h(&s.sign(&msg)),
    }))
    .into_response()
}

async fn state_root(State(s): S, Path(height): Path<u64>) -> Response {
    let roots = {
        let n = s.node.lock().unwrap();
        n.outcome(height).map(|o| (o.state_root, o.app_hash))
    };
    let Some((state_root, app_hash)) = roots else {
        return err(StatusCode::NOT_FOUND, "height not committed here");
    };
    let msg = crypto::state_root_message(s.chain_id, height, &state_root);
    Json(json!({
        "height": height,
        "state_root": h(&state_root),
        "app_hash": h(&app_hash),
        "signer": k58(&s.me()),
        "signature": h(&s.sign(&msg)),
    }))
    .into_response()
}

async fn ws(State(s): S, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| stream_blocks(s, socket))
}

async fn stream_blocks(s: Arc<Shared>, mut socket: WebSocket) {
    let mut rx = s.blocks.subscribe();
    loop {
        match rx.recv().await {
            Ok(msg) => {
                if socket.send(Message::Text(msg.as_ref().into())).await.is_err() {
                    return;
                }
            }
            // A slow subscriber is told how much it missed and continues.
            Err(RecvError::Lagged(k)) => {
                let note = json!({ "lagged": k }).to_string();
                if socket.send(Message::Text(note.into())).await.is_err() {
                    return;
                }
            }
            Err(RecvError::Closed) => return,
        }
    }
}

fn side(s: Side) -> &'static str {
    match s {
        Side::Bid => "bid",
        Side::Ask => "ask",
    }
}

pub fn event_json(e: &Event) -> Value {
    match *e {
        Event::Placed { market, order_id, account, side: sd, price, size } => {
            json!({ "type": "placed", "market": market, "order_id": order_id, "account": account,
                    "side": side(sd), "price": price, "size": size })
        }
        Event::Fill { market, maker_order, maker, taker, taker_side, price, size, taker_fee, maker_fee } => {
            json!({ "type": "fill", "market": market, "maker_order": maker_order, "maker": maker, "taker": taker,
                    "taker_side": side(taker_side), "price": price, "size": size,
                    "taker_fee": taker_fee, "maker_fee": maker_fee })
        }
        Event::Canceled { market, order_id, account, remaining, reason } => {
            let reason = match reason {
                CancelReason::User => "user",
                CancelReason::SelfTrade => "self_trade",
                CancelReason::Liquidation => "liquidation",
                CancelReason::MarketHalted => "market_halted",
            };
            json!({ "type": "canceled", "market": market, "order_id": order_id, "account": account,
                    "remaining": remaining, "reason": reason })
        }
        Event::Liquidated { market, liquidator, liquidatee, size, price, penalty } => {
            json!({ "type": "liquidated", "market": market, "liquidator": liquidator, "liquidatee": liquidatee,
                    "size": size, "price": price, "penalty": penalty })
        }
        Event::BadDebt { dex, account, amount, from_insurance, socialized, unrecovered } => {
            json!({ "type": "bad_debt", "dex": dex, "account": account, "amount": amount,
                    "from_insurance": from_insurance, "socialized": socialized, "unrecovered": unrecovered })
        }
        Event::Funding { market, rate_bps } => json!({ "type": "funding", "market": market, "rate_bps": rate_bps }),
        Event::MarketSettled { market, price } => json!({ "type": "settled", "market": market, "price": price }),
        Event::DexDeployed { dex, deployer, stake } => {
            json!({ "type": "dex_deployed", "dex": dex, "deployer": deployer, "stake": stake })
        }
        Event::MarketAdded { dex, market } => json!({ "type": "market_added", "dex": dex, "market": market }),
        Event::Slashed { dex, amount, frozen } => {
            json!({ "type": "slashed", "dex": dex, "amount": amount, "frozen": frozen })
        }
        Event::Adl { market, target, counterparty, size, price } => {
            json!({ "type": "adl", "market": market, "target": target, "counterparty": counterparty,
                    "size": size, "price": price })
        }
    }
}

pub fn block_json(height: u64, out: &BlockOutcome) -> Value {
    json!({
        "height": height,
        "app_hash": h(&out.app_hash),
        "state_root": h(&out.state_root),
        "txs": out.results.len(),
        "failed": out.results.iter().filter(|r| r.is_err()).count(),
        "triggered": out.triggered.iter().map(|(id, r)| json!({
            "id": id, "ok": r.is_ok(), "error": r.err().map(|e| format!("{e:?}")),
        })).collect::<Vec<_>>(),
        "events": out.events.iter().map(event_json).collect::<Vec<_>>(),
        "withdrawals": out.withdrawals.iter().map(|w| json!({
            "id": w.id, "asset": w.asset, "destination": k58(&w.destination), "amount": w.amount.to_string(),
        })).collect::<Vec<_>>(),
        "validator_set": out.validator_set.as_ref().map(|(epoch, v)| json!({
            "epoch": epoch,
            "validators": v.iter().map(|(k, w)| json!({ "key": k58(k), "weight": w.to_string() })).collect::<Vec<_>>(),
        })),
    })
}
