//! `mee-validator`: runs one node of a local devnet.
//!
//! ```text
//! mee-validator --index 0 --validators 4 [--nodes 4] [--chain-id 77]
//!               [--p2p-base 26600] [--api-base 8600] [--data ./data]
//! ```
//!
//! Node `i` listens on `p2p-base + i` (P2P) and `api-base + i` (HTTP/WS) and
//! dials the other `nodes - 1` nodes on localhost. Keys come from
//! `mee_net::devnet_key`; nodes with `index >= validators` run as observers.

use std::net::{Ipv4Addr, SocketAddr};

use mee_consensus::Config;
use mee_net::{NetConfig, devnet_genesis, devnet_key};
use tokio::net::TcpListener;

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: Option<T>) -> T {
    match args.iter().position(|a| a == name) {
        Some(i) => args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or_else(|| panic!("bad value for {name}")),
        None => default.unwrap_or_else(|| panic!("missing {name}")),
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let index: u32 = arg(&args, "--index", None);
    let validators: u32 = arg(&args, "--validators", Some(4));
    let nodes: u32 = arg(&args, "--nodes", Some(validators));
    let chain_id: u64 = arg(&args, "--chain-id", Some(77));
    let p2p_base: u16 = arg(&args, "--p2p-base", Some(26_600));
    let api_base: u16 = arg(&args, "--api-base", Some(8_600));
    let data: Option<String> = args.iter().position(|a| a == "--data").and_then(|i| args.get(i + 1).cloned());

    let local = |port: u16| SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let p2p = TcpListener::bind(local(p2p_base + index as u16)).await?;
    let api = TcpListener::bind(local(api_base + index as u16)).await?;
    let cfg = NetConfig {
        key: devnet_key(index),
        genesis: devnet_genesis(chain_id, validators),
        consensus: Config { base_timeout_ms: 500, max_timeout_ms: 5_000 },
        data_dir: data.map(|d| std::path::Path::new(&d).join(format!("node{index}"))),
        peers: (0..nodes).filter(|&j| j != index).map(|j| local(p2p_base + j as u16)).collect(),
    };
    let handle = mee_net::start(cfg, p2p, api).await?;
    eprintln!(
        "node {index} ({}) p2p {} api http://{}",
        hex::encode(handle.shared.me()),
        handle.p2p_addr,
        handle.api_addr
    );
    tokio::signal::ctrl_c().await?;
    handle.shutdown();
    Ok(())
}
