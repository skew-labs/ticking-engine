//! The replicated state machine: every validator applies the same ordered
//! blocks and must reach the same app hash.

use mee_core::{EngineError, EngineParams, OrderKind, Side};
use mee_engine::engine::Result as EngineResult;
use mee_engine::{AccountId, CancelReason, DexId, Event, MarketId, OrderRequest, State};
use rustc_hash::FxHashMap;

use crate::crypto::{self, Hash, Pubkey};
use crate::tx::*;

#[derive(Clone, Debug, Default)]
pub struct Genesis {
    pub chain_id: u64,
    pub params: EngineParams,
    pub validators: Vec<Pubkey>,
    pub timestamp_ms: i64,
    pub launch: LaunchMarkets,
}

/// Risk template for perps of graduated launchpad tokens. Conservative by
/// default: these are young, thin tokens.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LaunchMarkets {
    pub initial_margin_bps: u64,
    pub maintenance_margin_bps: u64,
    pub liquidation_fee_bps: u64,
    pub funding_interval: i64,
    pub max_funding_bps: u64,
    /// Share of the protocol fee paid to the token's creator.
    pub creator_fee_share_bps: u64,
    /// Open interest notional allowed per unit of pool quote depth.
    pub oi_cap_ratio_bps: u64,
    /// A lot is the smallest power of ten of token atoms worth at least this
    /// many quote atoms, so prices stay precise for tiny tokens.
    pub min_lot_value: u64,
}

impl Default for LaunchMarkets {
    fn default() -> Self {
        LaunchMarkets {
            initial_margin_bps: 2_000,
            maintenance_margin_bps: 1_000,
            liquidation_fee_bps: 500,
            funding_interval: 3_600,
            max_funding_bps: 100,
            creator_fee_share_bps: 5_000,
            oi_cap_ratio_bps: 20_000,
            min_lot_value: 10_000,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    pub market: MarketId,
    pub lot_atoms: u64,
}

/// Lot size (token atoms) and price per lot for a TWAP.
pub fn lot_for(twap_scaled: u128, min_lot_value: u64) -> Option<(u64, u64)> {
    let mut lot: u128 = 1;
    for _ in 0..19 {
        let price = twap_scaled.checked_mul(lot)? / PRICE_SCALE;
        if price >= min_lot_value as u128 {
            return Some((lot as u64, u64::try_from(price).ok()?));
        }
        lot *= 10;
    }
    None
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TxError {
    BadSignature,
    StaleNonce,
    UnknownAccount,
    BadAction,
    /// Bridge deposit id is not the next expected one.
    DepositOutOfOrder,
    GovernanceNonce,
    /// Not enough distinct, valid validator signatures.
    NoQuorum,
    AlreadyListed,
    /// Oracle batch from a Solana slot not after the last one.
    StaleOracle,
    Engine(EngineError),
}

impl From<EngineError> for TxError {
    fn from(e: EngineError) -> Self {
        TxError::Engine(e)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlockError {
    WrongHeight,
    TimeWentBackwards,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountProof {
    pub id: AccountId,
    pub leaf: Hash,
    pub path: Vec<Hash>,
    pub roots: mee_engine::commit::StateRoots,
    pub globals: Hash,
}

/// Checks an account proof against a chain `state_root`. `leaf` must be the
/// hash of the account the client expects (`mee_engine::commit::account_leaf`).
pub fn verify_account_proof(state_root: &Hash, p: &AccountProof) -> bool {
    let r = &p.roots;
    mee_engine::commit::verify_proof(&r.accounts, p.id as usize, &p.leaf, &p.path)
        && mee_engine::commit::StateRoots::combine(&r.accounts, &r.markets, &r.dexes, &r.globals) == r.root
        && crypto::sha256(&[b"MEE/chain-state", &r.root, &p.globals]) == *state_root
}

#[derive(Clone, Debug, Default)]
pub struct BlockOutcome {
    /// Per transaction in execution order (system, cancels, post-only, rest).
    pub results: Vec<Result<(), TxError>>,
    pub events: Vec<Event>,
    pub withdrawals: Vec<WithdrawalReceipt>,
    /// State root after this block; validators sign it for light clients.
    pub state_root: Hash,
    pub app_hash: Hash,
}

#[derive(Clone, Debug)]
pub struct Chain {
    pub chain_id: u64,
    pub validators: Vec<Pubkey>,
    pub state: State,
    pub height: u64,
    pub timestamp_ms: i64,
    pub app_hash: Hash,
    /// Engine state root after the last block (see `prove_account`).
    pub state_root: Hash,
    /// Hash chains over append-only chain data, so it is committed cheaply.
    pub withdrawals_hash: Hash,
    pub listings_hash: Hash,
    pub next_deposit_id: u64,
    pub next_withdrawal_id: u64,
    pub governance_nonce: u64,
    /// Every withdrawal ever queued, by id (the Solana bridge dedups payouts).
    pub withdrawals: Vec<WithdrawalReceipt>,
    pub launch: LaunchMarkets,
    /// Protocol DEX holding launchpad perps, created with the first listing.
    pub launch_dex: Option<DexId>,
    /// Launch id → perp listing. Lookup only.
    pub listings: FxHashMap<u64, Listing>,
    pub last_oracle_slot: u64,
}

impl Chain {
    pub fn new(g: Genesis) -> Chain {
        let mut state = State::new(g.params);
        state.now = g.timestamp_ms / 1_000;
        Chain {
            chain_id: g.chain_id,
            validators: g.validators,
            state,
            height: 0,
            timestamp_ms: g.timestamp_ms,
            app_hash: crypto::sha256(&[b"MEE/genesis", &g.chain_id.to_le_bytes()]),
            state_root: [0; 32],
            withdrawals_hash: [0; 32],
            listings_hash: [0; 32],
            next_deposit_id: 0,
            next_withdrawal_id: 0,
            governance_nonce: 0,
            withdrawals: Vec::new(),
            launch: g.launch,
            launch_dex: None,
            listings: FxHashMap::default(),
            last_oracle_slot: 0,
        }
    }

    /// Mempool admission: signature and nonce, no execution.
    pub fn check_tx(&self, tx: &Tx) -> Result<(), TxError> {
        self.verify_tx(tx)?;
        self.check_nonce(tx)
    }

    /// Stateless signature checks (user signature or validator quorum).
    pub fn verify_tx(&self, tx: &Tx) -> Result<(), TxError> {
        match tx {
            Tx::User(t) => {
                if t.verify(self.chain_id) {
                    Ok(())
                } else {
                    Err(TxError::BadSignature)
                }
            }
            Tx::System(SystemTx::BridgeDeposit { deposit_id, owner, asset, amount, attestations }) => {
                let msg = crypto::deposit_message(self.chain_id, *deposit_id, owner, *asset, *amount);
                self.check_quorum(&msg, attestations)
            }
            Tx::System(SystemTx::Governance { nonce, action, attestations }) => {
                let msg = crypto::governance_message(self.chain_id, *nonce, &borsh::to_vec(action).expect("borsh"));
                self.check_quorum(&msg, attestations)
            }
            Tx::System(SystemTx::ListLaunch {
                launch_id,
                mint,
                creator,
                symbol,
                twap_scaled,
                pool_quote,
                attestations,
            }) => {
                let msg =
                    crypto::list_message(self.chain_id, *launch_id, mint, creator, symbol, *twap_scaled, *pool_quote);
                self.check_quorum(&msg, attestations)
            }
            Tx::System(SystemTx::LaunchOracle { solana_slot, updates, attestations }) => {
                let msg =
                    crypto::launch_oracle_message(self.chain_id, *solana_slot, &borsh::to_vec(updates).expect("borsh"));
                self.check_quorum(&msg, attestations)
            }
        }
    }

    fn check_nonce(&self, tx: &Tx) -> Result<(), TxError> {
        if let Tx::User(t) = tx {
            // Nonces live in the account (so they are committed); a key with
            // no account can't act, and its transactions consume nothing.
            let a = self.state.account_id(&t.signer).ok_or(TxError::UnknownAccount)?;
            if t.nonce <= self.state.accounts[a as usize].nonce {
                return Err(TxError::StaleNonce);
            }
        }
        Ok(())
    }

    /// Verifies every transaction's signatures across all cores. Pure, so
    /// running it in parallel keeps execution deterministic.
    pub fn verify_parallel(&self, txs: &[Tx]) -> Vec<Result<(), TxError>> {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        if threads == 1 || txs.len() < 64 {
            return txs.iter().map(|t| self.verify_tx(t)).collect();
        }
        let chunk = txs.len().div_ceil(threads);
        std::thread::scope(|scope| {
            let handles: Vec<_> = txs.chunks(chunk).map(|c| scope.spawn(move || self.verify_chunk(c))).collect();
            handles.into_iter().flat_map(|h| h.join().expect("verifier thread")).collect()
        })
    }

    /// Batch-verifies the user signatures of a chunk (about twice as fast as
    /// one by one); falls back to single checks to pinpoint a bad one.
    fn verify_chunk(&self, txs: &[Tx]) -> Vec<Result<(), TxError>> {
        let mut msgs = Vec::new();
        let mut sigs = Vec::new();
        let mut keys = Vec::new();
        for tx in txs {
            if let Tx::User(t) = tx {
                let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&t.signer) else {
                    return txs.iter().map(|t| self.verify_tx(t)).collect();
                };
                msgs.push(SignedTx::message(self.chain_id, &t.signer, t.nonce, &t.action));
                sigs.push(ed25519_dalek::Signature::from_bytes(&t.signature));
                keys.push(vk);
            }
        }
        let refs: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();
        if !keys.is_empty() && ed25519_dalek::verify_batch(&refs, &sigs, &keys).is_ok() {
            return txs.iter().map(|t| if matches!(t, Tx::User(_)) { Ok(()) } else { self.verify_tx(t) }).collect();
        }
        txs.iter().map(|t| self.verify_tx(t)).collect()
    }

    fn check_quorum(&self, msg: &Hash, attestations: &[Attestation]) -> Result<(), TxError> {
        let mut seen = vec![false; self.validators.len()];
        let mut valid = 0;
        for a in attestations {
            let i = a.validator as usize;
            if i >= self.validators.len() || seen[i] {
                continue;
            }
            if crypto::verify(&self.validators[i], msg, &a.signature) {
                seen[i] = true;
                valid += 1;
            }
        }
        if valid >= crypto::quorum(self.validators.len()) { Ok(()) } else { Err(TxError::NoQuorum) }
    }

    /// Applies a committed block. Individual transactions may fail (their
    /// result says why) without failing the block.
    pub fn apply_block(&mut self, block: &Block) -> Result<BlockOutcome, BlockError> {
        if block.height != self.height + 1 {
            return Err(BlockError::WrongHeight);
        }
        if block.timestamp_ms < self.timestamp_ms {
            return Err(BlockError::TimeWentBackwards);
        }
        self.height = block.height;
        self.timestamp_ms = block.timestamp_ms;

        let mut out = BlockOutcome::default();
        let verified = self.verify_parallel(&block.txs);
        let mut order: Vec<usize> = (0..block.txs.len()).collect();
        order.sort_by_key(|&i| block.txs[i].priority()); // stable: keeps proposer order within a class

        let _ = self.state.begin_block(block.timestamp_ms / 1_000, &mut out.events);
        for i in order {
            let tx = &block.txs[i];
            let r = verified[i].and_then(|_| self.check_nonce(tx)).and_then(|_| self.execute(tx, &mut out));
            out.results.push(r);
        }

        let roots = self.state.commit();
        self.state_root = crypto::sha256(&[b"MEE/chain-state", &roots.root, &self.globals_hash()]);
        out.state_root = self.state_root;
        let mut h = sha2_chain(&self.app_hash, block);
        h = crypto::sha256(&[&h, &self.state_root]);
        for r in &out.results {
            h = crypto::sha256(&[&h, format!("{r:?}").as_bytes()]);
        }
        for e in &out.events {
            h = crypto::sha256(&[&h, format!("{e:?}").as_bytes()]);
        }
        for w in &out.withdrawals {
            h = crypto::sha256(&[&h, &borsh::to_vec(w).expect("borsh")]);
        }
        self.app_hash = h;
        out.app_hash = h;
        Ok(out)
    }

    fn execute(&mut self, tx: &Tx, out: &mut BlockOutcome) -> Result<(), TxError> {
        match tx {
            Tx::System(SystemTx::BridgeDeposit { deposit_id, owner, asset, amount, .. }) => {
                if *deposit_id != self.next_deposit_id {
                    return Err(TxError::DepositOutOfOrder);
                }
                match *asset {
                    ASSET_QUOTE => self.state.deposit(owner, *amount)?,
                    ASSET_STAKE => self.state.deposit_stake(owner, *amount)?,
                    _ => return Err(TxError::BadAction),
                };
                self.next_deposit_id += 1;
                Ok(())
            }
            Tx::System(SystemTx::Governance { nonce, action, .. }) => {
                if *nonce != self.governance_nonce {
                    return Err(TxError::GovernanceNonce);
                }
                match action {
                    Governance::Slash { dex, bps, freeze } => {
                        self.state.slash(*dex, *bps, *freeze, &mut out.events)?;
                    }
                    Governance::HaltMarket { market, price } => {
                        self.state.halt_market(None, *market, *price, &mut out.events)?;
                    }
                }
                self.governance_nonce += 1;
                Ok(())
            }
            Tx::System(SystemTx::ListLaunch { launch_id, creator, symbol, twap_scaled, pool_quote, .. }) => {
                if self.listings.contains_key(launch_id) {
                    return Err(TxError::AlreadyListed);
                }
                let c = self.launch;
                let (lot_atoms, price) = lot_for(*twap_scaled, c.min_lot_value).ok_or(TxError::BadAction)?;
                let cfg = mee_core::engine::MarketArgs {
                    symbol: *symbol,
                    tick_size: 1,
                    min_base_lots: 1,
                    initial_margin_bps: c.initial_margin_bps,
                    maintenance_margin_bps: c.maintenance_margin_bps,
                    liquidation_fee_bps: c.liquidation_fee_bps,
                    oi_cap: self.oi_cap(*pool_quote, price),
                    initial_price: price,
                    funding_interval: c.funding_interval,
                    max_funding_bps: c.max_funding_bps,
                };
                let dex = match self.launch_dex {
                    Some(d) => d,
                    None => {
                        let d = self.state.create_protocol_dex(*b"launchpad\0\0\0\0\0\0\0", &mut out.events);
                        self.launch_dex = Some(d);
                        d
                    }
                };
                let creator = self.state.account_or_create(creator);
                let market =
                    self.state.add_protocol_market(dex, cfg, creator, c.creator_fee_share_bps, &mut out.events)?;
                self.listings.insert(*launch_id, Listing { market, lot_atoms });
                self.listings_hash = crypto::sha256(&[
                    &self.listings_hash,
                    &launch_id.to_le_bytes(),
                    &market.to_le_bytes(),
                    &lot_atoms.to_le_bytes(),
                ]);
                Ok(())
            }
            Tx::System(SystemTx::LaunchOracle { solana_slot, updates, .. }) => {
                if *solana_slot <= self.last_oracle_slot {
                    return Err(TxError::StaleOracle);
                }
                let max_move = self.state.params.max_oracle_move_bps as u128;
                let mut prices = Vec::new();
                let mut caps = Vec::new();
                for u in updates {
                    let Some(l) = self.listings.get(&u.launch_id).copied() else { continue };
                    let Ok(m) = self.state.market(l.market) else { continue };
                    if m.status != mee_core::MARKET_ACTIVE {
                        continue;
                    }
                    let target = (u.twap_scaled.saturating_mul(l.lot_atoms as u128) / PRICE_SCALE).max(1);
                    // Memecoins move fast: follow in bounded steps instead of
                    // rejecting the update.
                    let cur = m.oracle_price as u128;
                    let band = cur * max_move / 10_000;
                    let price = target.clamp(cur.saturating_sub(band).max(1), cur + band) as u64;
                    prices.push((l.market, price));
                    caps.push((l.market, self.oi_cap(u.pool_quote, price)));
                }
                if let Some(dex) = self.launch_dex {
                    self.state.set_oracle_attested(dex, &prices)?;
                }
                for (m, cap) in caps {
                    self.state.set_oi_cap_attested(m, cap)?;
                }
                self.last_oracle_slot = *solana_slot;
                Ok(())
            }
            Tx::User(t) => {
                // A valid signature with a fresh nonce consumes the nonce even
                // if the action fails, so it can never be replayed.
                let a = self.state.account_id(&t.signer).expect("checked by check_nonce");
                self.state.accounts[a as usize].nonce = t.nonce;
                self.execute_action(&t.signer, &t.action, out)
            }
        }
    }

    /// Chain-level values outside the engine state.
    pub fn globals_hash(&self) -> Hash {
        crypto::sha256(&[
            b"MEE/chain-globals",
            &self.height.to_le_bytes(),
            &self.timestamp_ms.to_le_bytes(),
            &self.next_deposit_id.to_le_bytes(),
            &self.next_withdrawal_id.to_le_bytes(),
            &self.governance_nonce.to_le_bytes(),
            &self.last_oracle_slot.to_le_bytes(),
            &self.launch_dex.map_or(u32::MAX, |d| d).to_le_bytes(),
            &self.withdrawals_hash,
            &self.listings_hash,
        ])
    }

    /// Proof that `owner`'s account is in the state committed by the last
    /// block. Check it with `verify_account_proof` against a `state_root`
    /// that a validator quorum signed (`crypto::state_root_message`).
    pub fn prove_account(&self, owner: &Pubkey) -> Option<AccountProof> {
        let id = self.state.account_id(owner)?;
        let (leaf, path) = self.state.prove_account(id);
        let roots = self.state.last_roots;
        Some(AccountProof { id, leaf, path, roots, globals: self.globals_hash() })
    }

    /// Open interest cap in lots: `oi_cap_ratio` × pool quote depth.
    fn oi_cap(&self, pool_quote: u64, price_per_lot: u64) -> u64 {
        let notional = pool_quote as u128 * self.launch.oi_cap_ratio_bps as u128 / 10_000;
        (notional / price_per_lot.max(1) as u128).clamp(1, u64::MAX as u128) as u64
    }

    fn account(&self, key: &Pubkey) -> Result<AccountId, TxError> {
        self.state.account_id(key).ok_or(TxError::UnknownAccount)
    }

    fn execute_action(&mut self, signer: &Pubkey, action: &Action, out: &mut BlockOutcome) -> Result<(), TxError> {
        let r: EngineResult<()> = match action {
            Action::PlaceOrder { market, side, kind, price, size, reduce_only, client_id } => {
                let a = self.account(signer)?;
                let req = OrderRequest {
                    market: *market,
                    side: Side::from_u8(*side).ok_or(TxError::BadAction)?,
                    kind: OrderKind::from_u8(*kind).ok_or(TxError::BadAction)?,
                    price: *price,
                    size: *size,
                    reduce_only: *reduce_only,
                    client_id: *client_id,
                };
                self.state.place_order(a, &req, &mut out.events).map(|_| ())
            }
            Action::Cancel { market, order_id } => {
                let a = self.account(signer)?;
                self.state.cancel(a, *market, *order_id, &mut out.events)
            }
            Action::CancelAll { market } => {
                let a = self.account(signer)?;
                self.state.cancel_all(a, *market, CancelReason::User, &mut out.events).map(|_| ())
            }
            Action::Liquidate { target, market, max_size } => {
                let a = self.account(signer)?;
                let t = self.account(target)?;
                self.state.liquidate(a, t, *market, *max_size, &mut out.events).map(|_| ())
            }
            Action::TransferToDex { dex, amount } => {
                let a = self.account(signer)?;
                self.state.transfer_to_dex(a, *dex, *amount)
            }
            Action::TransferFromDex { dex, amount } => {
                let a = self.account(signer)?;
                self.state.transfer_from_dex(a, *dex, *amount)
            }
            Action::FundInsurance { dex, amount } => {
                let a = self.account(signer)?;
                self.state.fund_insurance(a, *dex, *amount)
            }
            Action::Withdraw { asset, amount, destination } => {
                let a = self.account(signer)?;
                match *asset {
                    ASSET_QUOTE => self.state.withdraw(a, *amount)?,
                    ASSET_STAKE => self.state.withdraw_stake(a, *amount)?,
                    _ => return Err(TxError::BadAction),
                }
                let receipt = WithdrawalReceipt {
                    id: self.next_withdrawal_id,
                    asset: *asset,
                    destination: *destination,
                    amount: *amount,
                };
                self.next_withdrawal_id += 1;
                self.withdrawals.push(receipt);
                self.withdrawals_hash =
                    crypto::sha256(&[&self.withdrawals_hash, &borsh::to_vec(&receipt).expect("borsh")]);
                out.withdrawals.push(receipt);
                Ok(())
            }
            Action::DeployDex { name, fee_share_bps, stake, oracle_authority } => {
                let a = self.account(signer)?;
                self.state.deploy_dex(a, *name, *fee_share_bps, *stake, *oracle_authority, &mut out.events).map(|_| ())
            }
            Action::AddMarket { dex, config } => {
                let a = self.account(signer)?;
                self.state.add_market(a, *dex, (*config).into(), &mut out.events).map(|_| ())
            }
            Action::SetOracle { dex, prices } => self.state.set_oracle(signer, *dex, prices),
            Action::SetFeeShare { dex, bps } => {
                let a = self.account(signer)?;
                self.state.set_fee_share(a, *dex, *bps)
            }
            Action::SetOracleAuthority { dex, authority } => {
                let a = self.account(signer)?;
                self.state.set_oracle_authority(a, *dex, *authority)
            }
            Action::HaltMarket { market, price } => {
                let a = self.account(signer)?;
                self.state.halt_market(Some(a), *market, *price, &mut out.events)
            }
            Action::RequestUnstake { dex } => {
                let a = self.account(signer)?;
                self.state.request_unstake(a, *dex)
            }
            Action::CompleteUnstake { dex } => {
                let a = self.account(signer)?;
                self.state.complete_unstake(a, *dex).map(|_| ())
            }
            Action::ClaimDeployerFees { dex } => {
                let a = self.account(signer)?;
                self.state.claim_deployer_fees(a, *dex).map(|_| ())
            }
            Action::SweepProtocolFees { dex } => self.state.sweep_protocol_fees(*dex).map(|_| ()),
        };
        r.map_err(TxError::from)
    }
}

fn sha2_chain(prev: &Hash, block: &Block) -> Hash {
    crypto::sha256(&[b"MEE/app", prev, &block.hash()])
}
