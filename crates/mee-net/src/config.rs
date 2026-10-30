//! Files a validator operator works with: the node config, the key (Solana
//! CLI keypair format, so `solana-keygen` keys work as validator keys) and
//! the genesis file. Keys are base58 like everywhere on Solana.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;
use mee_chain::{Genesis, LaunchMarkets, Staking, crypto};
use serde::{Deserialize, Serialize};

fn err(msg: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string())
}

pub fn b58(k: &[u8]) -> String {
    bs58::encode(k).into_string()
}

pub fn parse_b58(s: &str) -> std::io::Result<[u8; 32]> {
    bs58::decode(s).into_vec().map_err(err)?.try_into().map_err(|_| err(format!("{s}: not a 32-byte key")))
}

/// Reads a Solana CLI keypair file (JSON array of 64 bytes: secret, public).
pub fn read_keypair(path: &Path) -> std::io::Result<SigningKey> {
    let bytes: Vec<u8> = serde_json::from_slice(&std::fs::read(path)?).map_err(err)?;
    let bytes: [u8; 64] = bytes.try_into().map_err(|_| err("keypair file must hold 64 bytes"))?;
    let key = SigningKey::from_keypair_bytes(&bytes).map_err(|_| err("public half does not match the secret"))?;
    Ok(key)
}

/// Writes a new random keypair in Solana CLI format (owner-only permissions
/// on Unix).
pub fn write_keypair(path: &Path) -> std::io::Result<SigningKey> {
    let key = SigningKey::generate(&mut rand_core::OsRng);
    let json = serde_json::to_vec(&key.to_keypair_bytes().to_vec()).map_err(err)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    std::io::Write::write_all(&mut opts.open(path)?, &json)?;
    Ok(key)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatorConfig {
    pub key_file: PathBuf,
    pub genesis_file: PathBuf,
    pub data_dir: PathBuf,
    pub p2p_listen: SocketAddr,
    /// Keep on localhost behind a reverse proxy unless it must be public.
    pub api_listen: SocketAddr,
    #[serde(default)]
    pub peers: Vec<SocketAddr>,
    /// Solana JSON-RPC used to observe the bridge (validators).
    #[serde(default)]
    pub solana_rpc: Option<String>,
    #[serde(default = "default_poll_ms")]
    pub solana_poll_ms: u64,
    #[serde(default = "default_base_timeout")]
    pub base_timeout_ms: u64,
    #[serde(default = "default_max_timeout")]
    pub max_timeout_ms: u64,
    /// Requests per second per client IP on the API (0 = unlimited).
    #[serde(default = "default_rate")]
    pub api_rate_per_sec: u32,
    /// Signature-check threads (0 = one per core, minus one).
    #[serde(default)]
    pub sigverify_threads: usize,
    /// Pin each signature-check thread to its own core (Linux).
    #[serde(default)]
    pub pin_cores: bool,
}

fn default_poll_ms() -> u64 {
    1_000
}
fn default_base_timeout() -> u64 {
    1_000
}
fn default_max_timeout() -> u64 {
    30_000
}
fn default_rate() -> u32 {
    50
}

impl ValidatorConfig {
    pub fn load(path: &Path) -> std::io::Result<ValidatorConfig> {
        let mut c: ValidatorConfig = serde_json::from_slice(&std::fs::read(path)?).map_err(err)?;
        // Relative paths are relative to the config file.
        let base = path.parent().unwrap_or(Path::new("."));
        for p in [&mut c.key_file, &mut c.genesis_file, &mut c.data_dir] {
            if p.is_relative() {
                *p = base.join(&*p);
            }
        }
        Ok(c)
    }
}

/// Genesis as JSON. Every section has defaults, so a minimal file only
/// names the chain id, the bridge program and the validators.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenesisFile {
    pub chain_id: u64,
    /// Solana bridge program id (base58).
    pub bridge_program: String,
    /// Validator keys (base58).
    pub validators: Vec<String>,
    #[serde(default)]
    pub timestamp_ms: i64,
    #[serde(default)]
    pub params: ParamsFile,
    #[serde(default)]
    pub launch: LaunchFile,
    #[serde(default)]
    pub staking: StakingFile,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParamsFile {
    pub taker_fee_bps: u64,
    pub maker_fee_bps: u64,
    pub max_fee_share_bps: u64,
    pub max_oracle_move_bps: u64,
    pub min_oracle_interval: i64,
    pub max_oracle_staleness: i64,
    pub mark_band_bps: u64,
    pub order_band_bps: u64,
    pub max_fills_per_order: u64,
    pub liquidation_insurance_share_bps: u64,
    pub min_deployer_stake: u64,
    pub min_stake_hold_secs: i64,
    pub unstake_delay_secs: i64,
    pub min_initial_margin_bps: u64,
}

impl Default for ParamsFile {
    fn default() -> Self {
        ParamsFile {
            taker_fee_bps: 5,
            maker_fee_bps: 1,
            max_fee_share_bps: 30_000,
            max_oracle_move_bps: 500,
            min_oracle_interval: 0,
            max_oracle_staleness: 3_600,
            mark_band_bps: 100,
            order_band_bps: 5_000,
            max_fills_per_order: 1_000,
            liquidation_insurance_share_bps: 5_000,
            min_deployer_stake: 1_000,
            min_stake_hold_secs: 30 * 24 * 3_600,
            unstake_delay_secs: 7 * 24 * 3_600,
            min_initial_margin_bps: 500,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LaunchFile {
    pub initial_margin_bps: u64,
    pub maintenance_margin_bps: u64,
    pub liquidation_fee_bps: u64,
    pub funding_interval: i64,
    pub max_funding_bps: u64,
    pub creator_fee_share_bps: u64,
    pub oi_cap_ratio_bps: u64,
    pub min_lot_value: u64,
}

impl Default for LaunchFile {
    fn default() -> Self {
        let l = LaunchMarkets::default();
        LaunchFile {
            initial_margin_bps: l.initial_margin_bps,
            maintenance_margin_bps: l.maintenance_margin_bps,
            liquidation_fee_bps: l.liquidation_fee_bps,
            funding_interval: l.funding_interval,
            max_funding_bps: l.max_funding_bps,
            creator_fee_share_bps: l.creator_fee_share_bps,
            oi_cap_ratio_bps: l.oi_cap_ratio_bps,
            min_lot_value: l.min_lot_value,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StakingFile {
    pub epoch_blocks: u64,
    pub max_validators: usize,
    pub min_validator_stake: u64,
    pub unbond_secs: i64,
}

impl Default for StakingFile {
    fn default() -> Self {
        let s = Staking::default();
        StakingFile {
            epoch_blocks: s.epoch_blocks,
            max_validators: s.max_validators,
            min_validator_stake: s.min_validator_stake,
            unbond_secs: s.unbond_secs,
        }
    }
}

impl GenesisFile {
    pub fn load(path: &Path) -> std::io::Result<GenesisFile> {
        serde_json::from_slice(&std::fs::read(path)?).map_err(err)
    }

    pub fn to_genesis(&self) -> std::io::Result<Genesis> {
        let p = &self.params;
        let (l, s) = (&self.launch, &self.staking);
        if self.validators.is_empty() {
            return Err(err("genesis needs at least one validator"));
        }
        Ok(Genesis {
            chain_id: self.chain_id,
            params: mee_core::EngineParams {
                taker_fee_bps: p.taker_fee_bps,
                maker_fee_bps: p.maker_fee_bps,
                max_fee_share_bps: p.max_fee_share_bps,
                max_oracle_move_bps: p.max_oracle_move_bps,
                min_oracle_interval: p.min_oracle_interval,
                max_oracle_staleness: p.max_oracle_staleness,
                mark_band_bps: p.mark_band_bps,
                order_band_bps: p.order_band_bps,
                max_fills_per_order: p.max_fills_per_order,
                liquidation_insurance_share_bps: p.liquidation_insurance_share_bps,
                min_deployer_stake: p.min_deployer_stake,
                min_stake_hold_secs: p.min_stake_hold_secs,
                unstake_delay_secs: p.unstake_delay_secs,
                min_initial_margin_bps: p.min_initial_margin_bps,
            },
            validators: self.validators.iter().map(|v| parse_b58(v)).collect::<Result<_, _>>()?,
            timestamp_ms: self.timestamp_ms,
            launch: LaunchMarkets {
                initial_margin_bps: l.initial_margin_bps,
                maintenance_margin_bps: l.maintenance_margin_bps,
                liquidation_fee_bps: l.liquidation_fee_bps,
                funding_interval: l.funding_interval,
                max_funding_bps: l.max_funding_bps,
                creator_fee_share_bps: l.creator_fee_share_bps,
                oi_cap_ratio_bps: l.oi_cap_ratio_bps,
                min_lot_value: l.min_lot_value,
            },
            staking: Staking {
                epoch_blocks: s.epoch_blocks,
                max_validators: s.max_validators,
                min_validator_stake: s.min_validator_stake,
                unbond_secs: s.unbond_secs,
            },
            bridge_program: parse_b58(&self.bridge_program)?,
        })
    }

    /// Hash operators compare out of band before starting a network.
    pub fn genesis_hash(&self) -> std::io::Result<String> {
        let canonical = serde_json::to_vec(self).map_err(err)?;
        Ok(b58(&crypto::sha256(&[b"MEE/genesis-file", &canonical])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypair_round_trip_and_minimal_genesis() {
        let dir = std::env::temp_dir().join(format!("mee-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v.json");
        let k = write_keypair(&path).unwrap();
        assert!(write_keypair(&path).is_err(), "never overwrites a key");
        assert_eq!(read_keypair(&path).unwrap().to_bytes(), k.to_bytes());

        let json = format!(
            r#"{{"chain_id": 9, "bridge_program": "{}", "validators": ["{}"], "staking": {{"epoch_blocks": 100}}}}"#,
            b58(&[3; 32]),
            b58(&crypto::pubkey(&k))
        );
        let g: GenesisFile = serde_json::from_str(&json).unwrap();
        let genesis = g.to_genesis().unwrap();
        assert_eq!(genesis.validators, vec![crypto::pubkey(&k)]);
        assert_eq!(genesis.bridge_program, [3; 32]);
        assert_eq!(genesis.staking.epoch_blocks, 100);
        assert_eq!(genesis.staking.max_validators, Staking::default().max_validators);
        assert!(serde_json::from_str::<GenesisFile>(&json.replace("chain_id", "chainid")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
