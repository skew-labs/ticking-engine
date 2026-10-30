//! Transaction formats. User actions are signed by the account's ed25519
//! key with a strictly increasing nonce; system actions carry validator
//! quorum signatures instead.

use borsh::{BorshDeserialize, BorshSerialize};
use ed25519_dalek::SigningKey;

use crate::crypto::{self, Hash, Pubkey, Sig};

pub const ASSET_QUOTE: u8 = 0;
pub const ASSET_STAKE: u8 = 1;

#[derive(Copy, Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct MarketConfig {
    pub symbol: [u8; 16],
    pub tick_size: u64,
    pub min_base_lots: u64,
    pub initial_margin_bps: u64,
    pub maintenance_margin_bps: u64,
    pub liquidation_fee_bps: u64,
    pub oi_cap: u64,
    pub initial_price: u64,
    pub funding_interval: i64,
    pub max_funding_bps: u64,
}

impl From<MarketConfig> for mee_core::engine::MarketArgs {
    fn from(c: MarketConfig) -> Self {
        mee_core::engine::MarketArgs {
            symbol: c.symbol,
            tick_size: c.tick_size,
            min_base_lots: c.min_base_lots,
            initial_margin_bps: c.initial_margin_bps,
            maintenance_margin_bps: c.maintenance_margin_bps,
            liquidation_fee_bps: c.liquidation_fee_bps,
            oi_cap: c.oi_cap,
            initial_price: c.initial_price,
            funding_interval: c.funding_interval,
            max_funding_bps: c.max_funding_bps,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Action {
    // trading
    PlaceOrder {
        market: u32,
        side: u8,
        kind: u8,
        price: u64,
        size: u64,
        reduce_only: bool,
        client_id: u64,
    },
    Cancel {
        market: u32,
        order_id: u64,
    },
    CancelAll {
        market: u32,
    },
    Liquidate {
        target: Pubkey,
        market: u32,
        max_size: u64,
    },
    // balances
    TransferToDex {
        dex: u32,
        amount: u64,
    },
    TransferFromDex {
        dex: u32,
        amount: u64,
    },
    FundInsurance {
        dex: u32,
        amount: u64,
    },
    /// Queues a bridge withdrawal of quote (`asset` 0) or stake (1) to a
    /// Solana owner.
    Withdraw {
        asset: u8,
        amount: u64,
        destination: Pubkey,
    },
    // HIP-3 deployers
    DeployDex {
        name: [u8; 16],
        fee_share_bps: u64,
        stake: u64,
        oracle_authority: Pubkey,
    },
    AddMarket {
        dex: u32,
        config: MarketConfig,
    },
    SetOracle {
        dex: u32,
        prices: Vec<(u32, u64)>,
    },
    SetFeeShare {
        dex: u32,
        bps: u64,
    },
    SetOracleAuthority {
        dex: u32,
        authority: Pubkey,
    },
    HaltMarket {
        market: u32,
        price: u64,
    },
    RequestUnstake {
        dex: u32,
    },
    CompleteUnstake {
        dex: u32,
    },
    ClaimDeployerFees {
        dex: u32,
    },
    /// Permissionless: moves a DEX's protocol fees to the treasury.
    SweepProtocolFees {
        dex: u32,
    },
    // validator staking
    /// Delegates free stake tokens to a validator key.
    Delegate {
        validator: Pubkey,
        amount: u64,
    },
    /// Starts unbonding. The stake stops counting at the next election but
    /// stays locked until that election has passed and `unbond_secs` have
    /// elapsed, so it backs every epoch it voted in.
    Undelegate {
        validator: Pubkey,
        amount: u64,
    },
    /// Makes the signer's key eligible for election. Signed by the key
    /// itself, so stake can't elect keys nobody controls.
    RegisterValidator,
    // advanced trading
    /// Atomic cancel-replace of a resting order; shrinking at the same price
    /// keeps its queue position (see `mee_engine::State::modify_order`).
    Modify {
        market: u32,
        order_id: u64,
        price: u64,
        size: u64,
        kind: u8,
        reduce_only: bool,
    },
    /// Up to `MAX_BATCH` actions in one signed transaction, run in order.
    /// Each stands on its own: one failing doesn't undo the others (the
    /// result reports how many failed). No nesting (refused while decoding).
    Batch(BatchActions),
    /// Take-profit / stop-loss. Once the mark price is at or beyond
    /// `trigger_price` (at or above it if `above`, else at or below), an
    /// immediate-or-cancel order for `size` with worst price `limit_price` is
    /// sent for the signer at the start of the next block.
    PlaceTrigger {
        market: u32,
        side: u8,
        above: bool,
        trigger_price: u64,
        limit_price: u64,
        size: u64,
        reduce_only: bool,
    },
    CancelTrigger {
        id: u64,
    },
}

/// Most actions in one `Action::Batch`.
pub const MAX_BATCH: usize = 32;

/// The actions of a batch. Decoding refuses a nested batch and more than
/// `MAX_BATCH` items before reading them, so untrusted bytes (API, gossip,
/// proposals) can't recurse deeply and overflow the stack.
#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize)]
pub struct BatchActions(pub Vec<Action>);

impl std::ops::Deref for BatchActions {
    type Target = Vec<Action>;
    fn deref(&self) -> &Vec<Action> {
        &self.0
    }
}

impl From<Vec<Action>> for BatchActions {
    fn from(v: Vec<Action>) -> Self {
        BatchActions(v)
    }
}

/// Borsh tag of `Action::Batch`.
fn batch_tag() -> u8 {
    borsh::to_vec(&Action::Batch(BatchActions(Vec::new()))).expect("borsh")[0]
}

impl BorshDeserialize for BatchActions {
    fn deserialize_reader<R: std::io::Read>(r: &mut R) -> std::io::Result<Self> {
        use borsh::de::EnumExt as _;
        let invalid = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
        let n = u32::deserialize_reader(r)? as usize;
        if n > MAX_BATCH {
            return Err(invalid("batch too long"));
        }
        let nested = batch_tag();
        let mut items = Vec::with_capacity(n);
        for _ in 0..n {
            let tag = u8::deserialize_reader(r)?;
            if tag == nested {
                return Err(invalid("nested batch"));
            }
            items.push(Action::deserialize_variant(r, tag)?);
        }
        Ok(BatchActions(items))
    }
}

impl Action {
    /// Block ordering class: cancels run first, then post-only orders, then
    /// everything else (Hyperliquid's ordering, which protects makers).
    pub fn priority(&self) -> u8 {
        match self {
            Action::Cancel { .. } | Action::CancelAll { .. } => 1,
            Action::PlaceOrder { kind: 1, .. } | Action::Modify { kind: 1, .. } => 2,
            // A batch runs in the latest class of anything in it, so bundling
            // a cancel can't pull an order ahead of its class.
            Action::Batch(items) => items.iter().map(Action::priority).max().unwrap_or(3),
            _ => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SignedTx {
    pub signer: Pubkey,
    pub nonce: u64,
    pub action: Action,
    pub signature: Sig,
}

impl SignedTx {
    pub fn message(chain_id: u64, signer: &Pubkey, nonce: u64, action: &Action) -> Hash {
        let body = borsh::to_vec(&(signer, nonce, action)).expect("borsh");
        crypto::sha256(&[b"MEE/tx", &chain_id.to_le_bytes(), &body])
    }

    pub fn sign(key: &SigningKey, chain_id: u64, nonce: u64, action: Action) -> SignedTx {
        let signer = crypto::pubkey(key);
        let msg = Self::message(chain_id, &signer, nonce, &action);
        SignedTx { signer, nonce, action, signature: crypto::sign(key, &msg) }
    }

    pub fn verify(&self, chain_id: u64) -> bool {
        crypto::verify(&self.signer, &Self::message(chain_id, &self.signer, self.nonce, &self.action), &self.signature)
    }
}

/// Checks many user signatures at once: one batch verification (about twice
/// as fast as one by one), falling back to single checks only when the batch
/// fails, to find which ones are bad.
pub fn verify_batch(chain_id: u64, txs: &[&SignedTx]) -> Vec<bool> {
    let mut msgs = Vec::with_capacity(txs.len());
    let mut sigs = Vec::with_capacity(txs.len());
    let mut keys = Vec::with_capacity(txs.len());
    for t in txs {
        let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&t.signer) else {
            return txs.iter().map(|t| t.verify(chain_id)).collect();
        };
        msgs.push(SignedTx::message(chain_id, &t.signer, t.nonce, &t.action));
        sigs.push(ed25519_dalek::Signature::from_bytes(&t.signature));
        keys.push(vk);
    }
    let refs: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();
    if keys.len() > 1 && ed25519_dalek::verify_batch(&refs, &sigs, &keys).is_ok() {
        return vec![true; txs.len()];
    }
    txs.iter().map(|t| t.verify(chain_id)).collect()
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Attestation {
    pub validator: u16,
    pub signature: Sig,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Governance {
    /// HIP-3 slashing: take `bps` of the deployer's stake, optionally freeze.
    Slash { dex: u32, bps: u64, freeze: bool },
    /// Wind down a market of a frozen DEX.
    HaltMarket { market: u32, price: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum SystemTx {
    /// A deposit seen on the Solana bridge, attested by a validator quorum.
    /// Ids are the bridge's sequence numbers and must arrive in order.
    BridgeDeposit { deposit_id: u64, owner: Pubkey, asset: u8, amount: u64, attestations: Vec<Attestation> },
    /// A quorum-signed governance action; `nonce` must equal the chain's
    /// governance nonce, so each signature set is used once.
    Governance { nonce: u64, action: Governance, attestations: Vec<Attestation> },
    /// A launchpad token graduated on Solana: list its perp. `twap_scaled` is
    /// the pool TWAP in quote atoms per token atom × `PRICE_SCALE`.
    ListLaunch {
        launch_id: u64,
        mint: Pubkey,
        creator: Pubkey,
        symbol: [u8; 16],
        twap_scaled: u128,
        pool_quote: u64,
        attestations: Vec<Attestation>,
    },
    /// Pool TWAPs and depths observed at Solana slot `solana_slot` (strictly
    /// increasing, so an old batch can't be replayed).
    LaunchOracle { solana_slot: u64, updates: Vec<LaunchObservation>, attestations: Vec<Attestation> },
    /// Two votes by `validator` for different blocks in the same round of
    /// epoch `epoch`'s consensus. Self-proving: anyone may submit it. The
    /// validator's stake (its delegations, including unbonding ones) is cut
    /// by `EQUIVOCATION_SLASH_BPS` and the key is barred for good.
    Evidence { epoch: u64, validator: Pubkey, round: u64, block_a: Hash, sig_a: Sig, block_b: Hash, sig_b: Sig },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct LaunchObservation {
    pub launch_id: u64,
    pub twap_scaled: u128,
    pub pool_quote: u64,
}

/// Launchpad pool prices are quote atoms per token atom × 1e12 (same as the
/// Solana program's `launchpad::PRICE_SCALE`).
pub const PRICE_SCALE: u128 = 1_000_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Tx {
    User(SignedTx),
    System(SystemTx),
}

impl Tx {
    pub fn priority(&self) -> u8 {
        match self {
            Tx::System(_) => 0,
            Tx::User(t) => t.action.priority(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Block {
    pub height: u64,
    pub timestamp_ms: i64,
    pub txs: Vec<Tx>,
}

impl Block {
    pub fn hash(&self) -> Hash {
        crypto::sha256(&[b"MEE/block", &borsh::to_vec(self).expect("borsh")])
    }
}

/// A queued bridge withdrawal. Validators sign `crypto::withdrawal_message`
/// for it and the Solana bridge pays out once it sees a quorum.
#[derive(Copy, Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct WithdrawalReceipt {
    pub id: u64,
    pub asset: u8,
    pub destination: Pubkey,
    pub amount: u64,
}

impl WithdrawalReceipt {
    /// `bridge` = the Solana bridge program id.
    pub fn message(&self, chain_id: u64, bridge: &Pubkey) -> Hash {
        crypto::withdrawal_message(chain_id, bridge, self.id, self.asset, &self.destination, self.amount)
    }
}
