//! HTTP/JSON API and WebSocket block stream.
//!
//! - `POST /tx` `{"tx": "<hex borsh Tx>"}` -> `{"accepted": bool, "hash"}`
//! - `GET /status`
//! - `GET /book/{market}?depth=N`
//! - `GET /account/{pubkey hex}`: balances, positions and a Merkle proof
//!   against the latest state root
//! - `GET /withdrawals/{id}`: the receipt and this validator's signature
//!   over the bridge message (a relayer collects a quorum of these)
//! - `GET /state_root/{height}`: the state root and this validator's
//!   signature over it (light clients)
//! - `GET /ws`: one JSON message per committed block (events, withdrawals,
//!   roots)
//!
//! Integers that can exceed 2^53 are encoded as strings.

use std::sync::Arc;

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

pub async fn serve(shared: Arc<Shared>, listener: TcpListener) {
    let app = Router::new()
        .route("/tx", post(submit))
        .route("/status", get(status))
        .route("/book/{market}", get(book))
        .route("/account/{key}", get(account))
        .route("/withdrawals/{id}", get(withdrawal))
        .route("/state_root/{height}", get(state_root))
        .route("/ws", get(ws))
        .with_state(shared);
    let _ = axum::serve(listener, app).await;
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

fn h(bytes: &[u8]) -> String {
    hex::encode(bytes)
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
    let peers: Vec<String> = s.peer_keys().iter().map(|k| h(k)).collect();
    let n = s.node.lock().unwrap();
    Json(json!({
        "chain_id": s.chain_id,
        "node": h(&n.me),
        "height": n.height(),
        "epoch": n.epoch,
        "timestamp_ms": n.chain.timestamp_ms,
        "app_hash": h(&n.chain.app_hash),
        "state_root": h(&n.chain.state_root),
        "validators": n.chain.committee().iter().map(|(k, w)| json!({ "key": h(k), "weight": w.to_string() })).collect::<Vec<_>>(),
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

fn parse_key(s: &str) -> Option<[u8; 32]> {
    hex::decode(s.trim_start_matches("0x")).ok()?.try_into().ok()
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
        "owner": h(&a.owner),
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

async fn withdrawal(State(s): S, Path(id): Path<u64>) -> Response {
    // Receipt ids are sequential from 0, so the id is the index. Copy it out
    // and sign without holding the node lock.
    let w = s.node.lock().unwrap().chain.withdrawals.get(id as usize).filter(|w| w.id == id).copied();
    let Some(w) = w else {
        return err(StatusCode::NOT_FOUND, "no such withdrawal");
    };
    let msg = w.message(s.chain_id);
    Json(json!({
        "id": w.id,
        "asset": w.asset,
        "destination": h(&w.destination),
        "amount": w.amount.to_string(),
        "message": h(&msg),
        "signer": h(&s.me()),
        "signature": h(&s.sign(&msg)),
    }))
    .into_response()
}

async fn state_root(State(s): S, Path(height): Path<u64>) -> Response {
    let roots = {
        let n = s.node.lock().unwrap();
        height.checked_sub(1).and_then(|i| n.outcomes.get(i as usize)).map(|o| (o.state_root, o.app_hash))
    };
    let Some((state_root, app_hash)) = roots else {
        return err(StatusCode::NOT_FOUND, "height not committed here");
    };
    let msg = crypto::state_root_message(s.chain_id, height, &state_root);
    Json(json!({
        "height": height,
        "state_root": h(&state_root),
        "app_hash": h(&app_hash),
        "signer": h(&s.me()),
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
        "events": out.events.iter().map(event_json).collect::<Vec<_>>(),
        "withdrawals": out.withdrawals.iter().map(|w| json!({
            "id": w.id, "asset": w.asset, "destination": h(&w.destination), "amount": w.amount.to_string(),
        })).collect::<Vec<_>>(),
        "validator_set": out.validator_set.as_ref().map(|(epoch, v)| json!({
            "epoch": epoch,
            "validators": v.iter().map(|(k, w)| json!({ "key": h(k), "weight": w.to_string() })).collect::<Vec<_>>(),
        })),
    })
}
