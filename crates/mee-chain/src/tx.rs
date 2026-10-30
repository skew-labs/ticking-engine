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
}

impl Action {
    /// Block ordering class: cancels run first, then post-only orders, then
    /// everything else (Hyperliquid's ordering, which protects makers).
    pub fn priority(&self) -> u8 {
        match self {
            Action::Cancel { .. } | Action::CancelAll { .. } => 1,
            Action::PlaceOrder { kind: 1, .. } => 2,
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
}

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
    pub fn message(&self, chain_id: u64) -> Hash {
        crypto::withdrawal_message(chain_id, self.id, self.asset, &self.destination, self.amount)
    }
}
