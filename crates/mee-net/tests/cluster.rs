//! A real localhost cluster: four validators over TCP, clients talking HTTP
//! to different nodes (gossip has to carry their transactions to the
//! leader), a WebSocket subscriber, account proofs and signed state roots,
//! and an observer that joins late and catches up by verified sync.

use std::net::SocketAddr;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use futures_util::StreamExt;
use mee_chain::crypto;
use mee_chain::*;
use mee_consensus::Config;
use mee_net::{Handle, NetConfig, devnet_genesis, devnet_key};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const CHAIN: u64 = 501;

async fn http(addr: SocketAddr, method: &str, path: &str, body: Option<Value>) -> Value {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).await.unwrap();
    let resp = String::from_utf8(resp).unwrap();
    let (_, body) = resp.split_once("\r\n\r\n").unwrap();
    serde_json::from_str(body).unwrap_or_else(|_| panic!("not json: {resp}"))
}

async fn submit(addr: SocketAddr, tx: Tx) {
    let r = http(addr, "POST", "/tx", Some(json!({ "tx": hex::encode(borsh::to_vec(&tx).unwrap()) }))).await;
    assert_eq!(r["accepted"], true, "{r}");
}

async fn wait_for(what: &str, secs: u64, mut f: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !f().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn bind() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

async fn launch(i: u32, validators: u32, p2p: TcpListener, peers: Vec<SocketAddr>) -> Handle {
    let cfg = NetConfig {
        key: devnet_key(i),
        genesis: devnet_genesis(CHAIN, validators),
        consensus: Config { base_timeout_ms: 300, max_timeout_ms: 3_000 },
        data_dir: None,
        peers,
        solana: None,
    };
    mee_net::start(cfg, p2p, bind().await).await.unwrap()
}

fn deposit(id: u64, owner: &SigningKey, asset: u8, amount: u64) -> Tx {
    let owner = crypto::pubkey(owner);
    let msg = crypto::deposit_message(CHAIN, &[0; 32], id, &owner, asset, amount);
    let attestations =
        (0..4).map(|v| Attestation { validator: v, signature: crypto::sign(&devnet_key(v as u32), &msg) }).collect();
    Tx::System(SystemTx::BridgeDeposit { deposit_id: id, owner, asset, amount, attestations })
}

fn market() -> MarketConfig {
    MarketConfig {
        symbol: *b"TSLA\0\0\0\0\0\0\0\0\0\0\0\0",
        tick_size: 1,
        min_base_lots: 1,
        initial_margin_bps: 1_000,
        maintenance_margin_bps: 500,
        liquidation_fee_bps: 250,
        oi_cap: 1_000_000,
        initial_price: 10_000,
        funding_interval: 3_600,
        max_funding_bps: 50,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn four_validators_over_tcp_with_api_gossip_and_late_observer() {
    let listeners = [bind().await, bind().await, bind().await, bind().await];
    let addrs: Vec<SocketAddr> = listeners.iter().map(|l| l.local_addr().unwrap()).collect();
    let mut nodes = Vec::new();
    for (i, l) in listeners.into_iter().enumerate() {
        let peers = addrs.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, a)| *a).collect();
        nodes.push(launch(i as u32, 4, l, peers).await);
    }
    let api: Vec<SocketAddr> = nodes.iter().map(|n| n.api_addr).collect();

    wait_for("full mesh and first blocks", 20, async || {
        let s = http(api[0], "GET", "/status", None).await;
        s["peers"].as_array().unwrap().len() == 3 && s["height"].as_u64().unwrap() > 3
    })
    .await;

    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", api[3])).await.unwrap();
    let (_, mut ws) = ws.split();

    let (dep, a, b) =
        (SigningKey::from_bytes(&[1; 32]), SigningKey::from_bytes(&[2; 32]), SigningKey::from_bytes(&[3; 32]));
    let user = |k: &SigningKey, nonce, action| Tx::User(SignedTx::sign(k, CHAIN, nonce, action));
    // Each client talks to a different node.
    submit(api[0], deposit(0, &dep, ASSET_STAKE, 5_000)).await;
    submit(api[1], deposit(1, &a, ASSET_QUOTE, 1_000_000)).await;
    submit(api[2], deposit(2, &b, ASSET_QUOTE, 1_000_000)).await;
    let a_key = hex::encode(crypto::pubkey(&a));
    wait_for("deposits", 20, async || {
        http(api[3], "GET", &format!("/account/{a_key}"), None).await["balance"] == "1000000"
    })
    .await;

    let deploy =
        Action::DeployDex { name: [0; 16], fee_share_bps: 0, stake: 1_000, oracle_authority: crypto::pubkey(&dep) };
    submit(api[3], user(&dep, 1, deploy)).await;
    submit(api[0], user(&a, 1, Action::TransferToDex { dex: 0, amount: 1_000_000 })).await;
    submit(api[1], user(&b, 1, Action::TransferToDex { dex: 0, amount: 1_000_000 })).await;
    wait_for("dex", 20, async || {
        http(api[2], "GET", &format!("/account/{a_key}"), None).await["margins"]
            .as_array()
            .is_some_and(|m| !m.is_empty())
    })
    .await;
    submit(api[2], user(&dep, 2, Action::AddMarket { dex: 0, config: market() })).await;
    wait_for("market", 20, async || http(api[0], "GET", "/book/0", None).await["market"] == 0).await;

    for i in 0..5u64 {
        let ask = Action::PlaceOrder {
            market: 0,
            side: 1,
            kind: 1,
            price: 10_000 + i,
            size: 2,
            reduce_only: false,
            client_id: i,
        };
        submit(api[(i % 4) as usize], user(&a, 2 + i, ask)).await;
    }
    wait_for("asks resting", 20, async || {
        http(api[1], "GET", "/book/0?depth=10", None).await["asks"].as_array().unwrap().len() == 5
    })
    .await;
    let bid =
        Action::PlaceOrder { market: 0, side: 0, kind: 2, price: 10_002, size: 5, reduce_only: false, client_id: 9 };
    submit(api[3], user(&b, 2, bid)).await;

    // The WebSocket stream reports the fills (5 lots across 3 levels).
    let mut filled = 0;
    while filled < 5 {
        let msg = tokio::time::timeout(Duration::from_secs(20), ws.next()).await.expect("ws block").unwrap().unwrap();
        let block: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        for e in block["events"].as_array().into_iter().flatten() {
            if e["type"] == "fill" {
                filled += e["size"].as_u64().unwrap();
            }
        }
    }
    assert_eq!(filled, 5);
    let book = http(api[0], "GET", "/book/0", None).await;
    assert_eq!(book["open_interest"], 5);
    assert_eq!(book["asks"][0], json!([10_002, 1]));

    // Account proof from one node verifies against the root it reports.
    let acct = http(api[2], "GET", &format!("/account/{a_key}"), None).await;
    let p = &acct["proof"];
    let h32 = |v: &Value| -> [u8; 32] { hex::decode(v.as_str().unwrap()).unwrap().try_into().unwrap() };
    let proof = AccountProof {
        id: p["id"].as_u64().unwrap() as u32,
        leaf: h32(&p["leaf"]),
        path: p["path"].as_array().unwrap().iter().map(h32).collect(),
        roots: mee_engine::commit::StateRoots {
            accounts: h32(&p["roots"]["accounts"]),
            markets: h32(&p["roots"]["markets"]),
            dexes: h32(&p["roots"]["dexes"]),
            globals: h32(&p["roots"]["globals"]),
            root: h32(&p["roots"]["root"]),
        },
        globals: h32(&p["globals"]),
    };
    assert!(verify_account_proof(&h32(&acct["state_root"]), &proof));

    // Every validator signs the same state root for a committed height.
    let height = http(api[0], "GET", "/status", None).await["height"].as_u64().unwrap();
    wait_for("all at height", 20, async || {
        for a in &api {
            if http(*a, "GET", "/status", None).await["height"].as_u64().unwrap() < height {
                return false;
            }
        }
        true
    })
    .await;
    let mut root = None;
    for (i, a) in api.iter().enumerate() {
        let r = http(*a, "GET", &format!("/state_root/{height}"), None).await;
        let sr = h32(&r["state_root"]);
        assert_eq!(*root.get_or_insert(sr), sr, "node {i} disagrees at height {height}");
        let sig: [u8; 64] = hex::decode(r["signature"].as_str().unwrap()).unwrap().try_into().unwrap();
        assert!(crypto::verify(
            &crypto::pubkey(&devnet_key(i as u32)),
            &crypto::state_root_message(CHAIN, height, &sr),
            &sig
        ));
    }

    // A late observer dials one validator, syncs the history it missed and
    // then follows consensus.
    let target = http(api[0], "GET", "/status", None).await["height"].as_u64().unwrap();
    assert!(target > mee_net::SYNC_LAG);
    let observer = launch(9, 4, bind().await, vec![nodes[1].p2p_addr]).await;
    // A liar connects too, claims a huge tip and never serves sync: the
    // observer must give up on it and sync from the honest peer.
    let liar_addr = observer.p2p_addr;
    let liar = tokio::spawn(async move {
        let mut s = TcpStream::connect(liar_addr).await.unwrap();
        mee_net::wire::handshake(&mut s, &SigningKey::from_bytes(&[66; 32]), CHAIN).await.unwrap();
        let (mut rd, mut wr) = s.into_split();
        tokio::spawn(async move { while mee_net::wire::read_frame(&mut rd).await.is_ok() {} });
        loop {
            let lie = mee_net::wire::encode(&mee_net::wire::Wire::Status { epoch: 0, height: u64::MAX / 2 });
            if wr.write_all(&lie).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    wait_for("observer caught up", 30, async || observer.shared.node.lock().unwrap().height() > target + 5).await;
    let (h, mine) = {
        let n = observer.shared.node.lock().unwrap();
        (n.height(), n.app_hashes.clone())
    };
    let theirs = nodes[1].shared.node.lock().unwrap().app_hashes[..h as usize].to_vec();
    assert_eq!(mine[..h as usize], theirs[..], "observer agrees at every height");
    assert_eq!(observer.shared.node.lock().unwrap().chain.state.markets[0].open_interest, 5);

    liar.abort();
    observer.shutdown();
    for n in nodes {
        n.shutdown();
    }
}
