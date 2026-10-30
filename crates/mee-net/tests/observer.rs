//! Deposits reach the chain with no relayer: four validators watch a (mock)
//! Solana RPC, sign what they see, gossip the attestations and submit the
//! deposit once a quorum signed. A record not owned by the bridge program is
//! ignored.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use mee::bridge::{DEPOSIT_DISCRIMINATOR, DepositRecord};
use mee_chain::crypto;
use mee_consensus::Config;
use mee_net::observer::SolanaConfig;
use mee_net::{NetConfig, devnet_genesis, devnet_key};
use serde_json::{Value, json};
use tokio::net::TcpListener;

const CHAIN: u64 = 808;

type Accounts = Arc<Mutex<HashMap<String, (String, Vec<u8>)>>>;

async fn rpc(State(accts): State<Accounts>, Json(req): Json<Value>) -> Json<Value> {
    assert_eq!(req["method"], "getAccountInfo");
    assert_eq!(req["params"][1]["commitment"], "finalized");
    let key = req["params"][0].as_str().unwrap();
    let value = match accts.lock().unwrap().get(key) {
        Some((owner, data)) => json!({
            "owner": owner,
            "data": [base64::engine::general_purpose::STANDARD.encode(data), "base64"],
            "lamports": 1_000_000,
            "executable": false,
        }),
        None => Value::Null,
    };
    Json(json!({ "jsonrpc": "2.0", "id": req["id"], "result": { "context": { "slot": 1 }, "value": value } }))
}

fn record(id: u64, owner: [u8; 32], amount: u64) -> Vec<u8> {
    let r = DepositRecord { discriminator: DEPOSIT_DISCRIMINATOR, id, owner, amount, slot: 1, asset: 0, _pad: [0; 7] };
    bytemuck::bytes_of(&r).to_vec()
}

fn b58(k: &[u8]) -> String {
    bs58::encode(k).into_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn validators_attest_solana_deposits_without_a_relayer() {
    let accts: Accounts = Default::default();
    let rpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc_url = format!("http://{}/", rpc_listener.local_addr().unwrap());
    let app = Router::new().route("/", post(rpc)).with_state(accts.clone());
    tokio::spawn(async move { axum::serve(rpc_listener, app).await.unwrap() });

    let program = mee::id();
    let pda = |id| mee::bridge::deposit_pda(&program, id).0.to_bytes();
    let user = crypto::pubkey(&ed25519_dalek::SigningKey::from_bytes(&[4; 32]));
    {
        let mut a = accts.lock().unwrap();
        a.insert(b58(&pda(0)), (b58(program.as_ref()), record(0, user, 700)));
        a.insert(b58(&pda(1)), (b58(program.as_ref()), record(1, user, 300)));
        // A look-alike account owned by some other program.
        a.insert(b58(&pda(2)), (b58(&[7; 32]), record(2, user, 1_000_000)));
    }

    let mut genesis = devnet_genesis(CHAIN, 4);
    genesis.bridge_program = program.to_bytes();
    let listeners = [
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
    ];
    let addrs: Vec<SocketAddr> = listeners.iter().map(|l| l.local_addr().unwrap()).collect();
    let mut nodes = Vec::new();
    for (i, l) in listeners.into_iter().enumerate() {
        let cfg = NetConfig {
            key: devnet_key(i as u32),
            genesis: genesis.clone(),
            consensus: Config { base_timeout_ms: 300, max_timeout_ms: 3_000 },
            data_dir: None,
            peers: addrs.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, a)| *a).collect(),
            solana: Some(SolanaConfig { rpc_url: rpc_url.clone(), poll: Duration::from_millis(100) }),
            api_rate_per_sec: 0,
            sigverify: Default::default(),
        };
        nodes.push(mee_net::start(cfg, l, TcpListener::bind("127.0.0.1:0").await.unwrap()).await.unwrap());
    }

    let balance = |n: &mee_net::Handle| {
        let node = n.shared.node.lock().unwrap();
        let st = &node.chain.state;
        st.account_id(&user).map(|a| st.accounts[a as usize].balance)
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !nodes.iter().all(|n| balance(n) == Some(1_000)) {
        assert!(tokio::time::Instant::now() < deadline, "deposits not credited: {:?}", balance(&nodes[0]));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Give the forged record a chance; it must never count.
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    for n in &nodes {
        assert_eq!(n.shared.node.lock().unwrap().chain.next_deposit_id, 2);
        assert_eq!(balance(n), Some(1_000));
    }

    // A deposit that shows up later is picked up too.
    accts.lock().unwrap().insert(b58(&pda(2)), (b58(program.as_ref()), record(2, user, 5)));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !nodes.iter().all(|n| balance(n) == Some(1_005)) {
        assert!(tokio::time::Instant::now() < deadline, "late deposit not credited");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for n in nodes {
        n.shutdown();
    }
}
