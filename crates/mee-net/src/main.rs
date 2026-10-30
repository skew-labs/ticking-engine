//! `mee-validator`: runs an MEE node.
//!
//! ```text
//! mee-validator keygen --out validator.json
//! mee-validator genesis --chain-id 1 --bridge <program b58> --validators <b58>,<b58>,... --out genesis.json
//! mee-validator run --config node.json
//! mee-validator --index 0 [--validators 4] [--nodes 4] [--data ./data] ...   (local devnet)
//! ```
//!
//! `run` reads a JSON config (`mee_net::config::ValidatorConfig`), a Solana
//! CLI keypair file and a genesis file, and prints the genesis hash for
//! operators to compare. The devnet mode uses public, index-derived keys and
//! localhost ports `p2p-base + i` / `api-base + i`.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use mee_consensus::Config;
use mee_net::config::{self, GenesisFile, ValidatorConfig, b58};
use mee_net::observer::SolanaConfig;
use mee_net::{NetConfig, devnet_genesis, devnet_key};
use tokio::net::TcpListener;

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: Option<T>) -> T {
    match args.iter().position(|a| a == name) {
        Some(i) => {
            args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or_else(|| fail(&format!("bad value for {name}")))
        }
        None => default.unwrap_or_else(|| fail(&format!("missing {name}"))),
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("mee-validator: {msg}");
    std::process::exit(2)
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("keygen") => {
            let out: PathBuf = arg(&args, "--out", None);
            let key = config::write_keypair(&out)?;
            println!("{}", b58(key.verifying_key().as_bytes()));
            Ok(())
        }
        Some("genesis") => {
            let validators: String = arg(&args, "--validators", None);
            let g = GenesisFile {
                chain_id: arg(&args, "--chain-id", None),
                bridge_program: arg(&args, "--bridge", None),
                validators: validators.split(',').map(str::to_string).collect(),
                timestamp_ms: arg(&args, "--timestamp-ms", Some(0)),
                params: Default::default(),
                launch: Default::default(),
                staking: Default::default(),
            };
            g.to_genesis()?; // validate keys before writing
            let out: PathBuf = arg(&args, "--out", None);
            std::fs::write(&out, serde_json::to_vec_pretty(&g).map_err(std::io::Error::other)?)?;
            println!("genesis hash {}", g.genesis_hash()?);
            Ok(())
        }
        Some("run") => run(&args).await,
        _ => devnet(&args).await,
    }
}

async fn run(args: &[String]) -> std::io::Result<()> {
    let path: PathBuf = arg(args, "--config", None);
    let c = ValidatorConfig::load(&path)?;
    let key = config::read_keypair(&c.key_file)?;
    let gf = GenesisFile::load(&c.genesis_file)?;
    let genesis = gf.to_genesis()?;
    let me = mee_chain::crypto::pubkey(&key);
    eprintln!(
        "node {} chain {} genesis {} ({})",
        b58(&me),
        genesis.chain_id,
        gf.genesis_hash()?,
        if genesis.validators.contains(&me) { "genesis validator" } else { "observer" }
    );
    let cfg = NetConfig {
        key,
        genesis,
        consensus: Config { base_timeout_ms: c.base_timeout_ms, max_timeout_ms: c.max_timeout_ms },
        data_dir: Some(c.data_dir.clone()),
        peers: c.peers.clone(),
        solana: c
            .solana_rpc
            .clone()
            .map(|rpc_url| SolanaConfig { rpc_url, poll: Duration::from_millis(c.solana_poll_ms) }),
        api_rate_per_sec: c.api_rate_per_sec,
        sigverify: mee_net::sigverify::Options { threads: c.sigverify_threads, pin_cores: c.pin_cores },
    };
    let handle =
        mee_net::start(cfg, TcpListener::bind(c.p2p_listen).await?, TcpListener::bind(c.api_listen).await?).await?;
    eprintln!("p2p {} api http://{}", handle.p2p_addr, handle.api_addr);
    tokio::signal::ctrl_c().await?;
    handle.shutdown();
    Ok(())
}

async fn devnet(args: &[String]) -> std::io::Result<()> {
    let index: u32 = arg(args, "--index", None);
    let validators: u32 = arg(args, "--validators", Some(4));
    let nodes: u32 = arg(args, "--nodes", Some(validators));
    let chain_id: u64 = arg(args, "--chain-id", Some(77));
    let p2p_base: u16 = arg(args, "--p2p-base", Some(26_600));
    let api_base: u16 = arg(args, "--api-base", Some(8_600));
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
        solana: None,
        api_rate_per_sec: 0,
        sigverify: Default::default(),
    };
    let handle = mee_net::start(cfg, p2p, api).await?;
    eprintln!("node {index} ({}) p2p {} api http://{}", b58(&handle.shared.me()), handle.p2p_addr, handle.api_addr);
    tokio::signal::ctrl_c().await?;
    handle.shutdown();
    Ok(())
}
