//! `mee-relayer`: carries MEE withdrawal receipts to the Solana bridge.
//!
//! ```text
//! mee-relayer --rpc http://127.0.0.1:8899 --program <bridge b58> --keypair payer.json \
//!             --validators http://v1:8600,http://v2:8600,... [--from 0] [--poll-ms 2000]
//! ```
//!
//! Untrusted and permissionless: it only pays Solana fees. Run several.

use std::path::PathBuf;
use std::time::Duration;

use mee::solana_program::pubkey::Pubkey;
use mee_net::config::{parse_b58, read_keypair};
use mee_net::relayer::{RelayerConfig, run};

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: Option<T>) -> T {
    match args.iter().position(|a| a == name) {
        Some(i) => {
            args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or_else(|| fail(&format!("bad value for {name}")))
        }
        None => default.unwrap_or_else(|| fail(&format!("missing {name}"))),
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("mee-relayer: {msg}");
    std::process::exit(2)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let program: String = arg(&args, "--program", None);
    let program = Pubkey::new_from_array(parse_b58(&program).unwrap_or_else(|e| fail(&e.to_string())));
    let keypair: PathBuf = arg(&args, "--keypair", None);
    let key = read_keypair(&keypair).unwrap_or_else(|e| fail(&e.to_string()));
    let validators: String = arg(&args, "--validators", None);
    run(RelayerConfig {
        solana_rpc: arg(&args, "--rpc", None),
        validators: validators.split(',').map(|s| s.trim_end_matches('/').to_string()).collect(),
        program,
        payer: solana_keypair::Keypair::new_from_array(key.to_bytes()),
        poll: Duration::from_millis(arg(&args, "--poll-ms", Some(2_000))),
        from: arg(&args, "--from", Some(0)),
    })
    .await
}
