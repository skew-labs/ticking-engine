//! Solana launchpad → MEE chain perp, end to end.
//!
//! A token launches on the bonding curve, sells out and graduates into the
//! pool. Validators read the pool's TWAP accumulator from the launch account,
//! attest the listing, and the chain opens a perp for the token. Trading
//! pays the creator, the oracle follows pool swaps in bounded steps, and the
//! OI cap tracks pool depth.

use ed25519_dalek::SigningKey;
use mee::launchpad::{self, Launch, LaunchParams};
use mee::token;
use mee_chain::{self as chain, Attestation, LaunchObservation, SystemTx, Tx, crypto};
use mee_core::EngineParams;
use mee_engine::Event;
use solana_keypair::Keypair;
use solana_program::clock::Clock;
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey::Pubkey;
use solana_program_test::{ProgramTest, ProgramTestContext, processor};
use solana_signer::Signer;
use solana_transaction::Transaction;

const CHAIN_ID: u64 = 9;

struct Sol {
    ctx: ProgramTestContext,
    pid: Pubkey,
}

impl Sol {
    async fn exec(&mut self, ixs: &[Instruction], signers: &[&Keypair]) {
        let blockhash = self.ctx.get_new_latest_blockhash().await.unwrap();
        let mut all: Vec<&Keypair> = vec![&self.ctx.payer];
        all.extend_from_slice(signers);
        let tx = Transaction::new_signed_with_payer(ixs, Some(&self.ctx.payer.pubkey()), &all, blockhash);
        self.ctx.banks_client.process_transaction(tx).await.unwrap();
    }

    async fn token_account(&mut self, mint: &Pubkey, owner: &Pubkey, amount: u64) -> Pubkey {
        let acct = Keypair::new();
        let rent = self.ctx.banks_client.get_rent().await.unwrap();
        let payer = self.ctx.payer.pubkey();
        let mut ixs = vec![
            token::create_account_ix(
                &payer,
                &acct.pubkey(),
                rent.minimum_balance(token::TOKEN_ACCOUNT_LEN),
                token::TOKEN_ACCOUNT_LEN as u64,
                &token::TOKEN_PROGRAM_ID,
            ),
            token::initialize_account3_ix(&acct.pubkey(), mint, owner),
        ];
        if amount > 0 {
            ixs.push(token::mint_to_ix(mint, &acct.pubkey(), &payer, amount));
        }
        self.exec(&ixs, &[&acct]).await;
        acct.pubkey()
    }

    async fn launch(&mut self, id: u64) -> Launch {
        let a = self.ctx.banks_client.get_account(launchpad::launch_pda(&self.pid, id).0).await.unwrap().unwrap();
        *bytemuck::from_bytes(&a.data)
    }

    async fn advance(&mut self, secs: i64) -> u64 {
        let mut clock: Clock = self.ctx.banks_client.get_sysvar().await.unwrap();
        clock.unix_timestamp += secs;
        clock.slot += 1;
        self.ctx.set_sysvar(&clock);
        clock.slot
    }
}

struct Validators(Vec<SigningKey>);

impl Validators {
    fn sign(&self, msg: &[u8; 32]) -> Vec<Attestation> {
        (0..3).map(|i| Attestation { validator: i as u16, signature: crypto::sign(&self.0[i], msg) }).collect()
    }

    /// What each validator does: read the launch account, compute the TWAP
    /// since its previous observation, sign the listing.
    fn list(&self, l: &Launch, since: (u128, i64)) -> Tx {
        let twap = launchpad::twap(since, (l.cumulative(), l.last_ts)).unwrap();
        let msg = crypto::list_message(CHAIN_ID, l.id, &l.mint, &l.creator, &l.symbol, twap, l.pool_quote);
        Tx::System(SystemTx::ListLaunch {
            launch_id: l.id,
            mint: l.mint,
            creator: l.creator,
            symbol: l.symbol,
            twap_scaled: twap,
            pool_quote: l.pool_quote,
            attestations: self.sign(&msg),
        })
    }

    fn oracle(&self, slot: u64, updates: Vec<LaunchObservation>) -> Tx {
        let msg = crypto::launch_oracle_message(CHAIN_ID, slot, &borsh::to_vec(&updates).unwrap());
        Tx::System(SystemTx::LaunchOracle { solana_slot: slot, updates, attestations: self.sign(&msg) })
    }

    fn deposit(&self, id: u64, owner: [u8; 32], amount: u64) -> Tx {
        let msg = crypto::deposit_message(CHAIN_ID, id, &owner, chain::ASSET_QUOTE, amount);
        Tx::System(SystemTx::BridgeDeposit {
            deposit_id: id,
            owner,
            asset: chain::ASSET_QUOTE,
            amount,
            attestations: self.sign(&msg),
        })
    }
}

#[tokio::test]
async fn graduated_token_gets_a_perp() {
    assert_eq!(launchpad::PRICE_SCALE, chain::PRICE_SCALE, "both sides scale prices the same way");

    // --- Solana: launch, sell out, graduate ---
    let pid = mee::id();
    let mut pt = ProgramTest::new("mee", pid, processor!(mee::processor::process_instruction));
    pt.prefer_bpf(false);
    let mut sol = Sol { ctx: pt.start_with_context().await, pid };
    set_upgrade_authority(&mut sol.ctx, &pid);
    let payer = sol.ctx.payer.pubkey();
    let usdc = Keypair::new();
    let rent = sol.ctx.banks_client.get_rent().await.unwrap();
    sol.exec(
        &[
            token::create_account_ix(
                &payer,
                &usdc.pubkey(),
                rent.minimum_balance(token::MINT_LEN),
                token::MINT_LEN as u64,
                &token::TOKEN_PROGRAM_ID,
            ),
            token::initialize_mint2_ix(&usdc.pubkey(), &payer, 6),
        ],
        &[&usdc],
    )
    .await;
    let usdc = usdc.pubkey();
    let treasury = sol.token_account(&usdc, &payer, 0).await;
    let params = LaunchParams {
        total_supply: 1_000_000_000_000_000, // 1B tokens
        curve_supply: 800_000_000_000_000,
        virtual_quote: 5_000_000_000,
        virtual_tokens: 1_073_000_000_000_000,
        protocol_fee_bps: 50,
        creator_fee_bps: 50,
        pool_fee_bps: 25,
    };
    sol.exec(&[launchpad::init_ix(&pid, &payer, &usdc, &treasury, params)], &[]).await;

    let creator_key = SigningKey::from_bytes(&[42; 32]);
    let creator = Keypair::new_from_array(*creator_key.as_bytes());
    let fund = Instruction {
        program_id: token::SYSTEM_PROGRAM_ID,
        accounts: vec![AccountMeta::new(payer, true), AccountMeta::new(creator.pubkey(), false)],
        data: [2u32.to_le_bytes().as_slice(), &1_000_000_000u64.to_le_bytes()].concat(),
    };
    sol.exec(&[fund], &[]).await;
    let mut sym = [0u8; 16];
    sym[..4].copy_from_slice(b"MDOG");
    sol.exec(&[launchpad::create_ix(&pid, &creator.pubkey(), &usdc, 0, [7; 32], sym)], &[&creator]).await;
    let mint = Pubkey::new_from_array(sol.launch(0).await.mint);

    let whale = Keypair::new();
    let w_usdc = sol.token_account(&usdc, &whale.pubkey(), 1_000_000_000_000).await;
    let w_tok = sol.token_account(&mint, &whale.pubkey(), 0).await;
    sol.exec(
        &[launchpad::buy_ix(&pid, &whale.pubkey(), 0, &w_tok, &w_usdc, &treasury, 1_000_000_000_000, 0)],
        &[&whale],
    )
    .await;
    sol.exec(&[launchpad::graduate_ix(&pid, 0)], &[]).await;
    let l = sol.launch(0).await;
    let obs0 = (l.cumulative(), l.last_ts);
    sol.advance(120).await;
    // A tiny swap advances the accumulator over the two minutes.
    sol.exec(
        &[launchpad::swap_ix(&pid, &whale.pubkey(), 0, &w_tok, &w_usdc, &treasury, true, 1_000_000, 0)],
        &[&whale],
    )
    .await;
    let l = sol.launch(0).await;

    // --- chain: validators attest the listing ---
    let vals = Validators((0..4u8).map(|i| SigningKey::from_bytes(&[90 + i; 32])).collect());
    let mut c = chain::Chain::new(chain::Genesis {
        chain_id: CHAIN_ID,
        params: EngineParams {
            taker_fee_bps: 10,
            maker_fee_bps: 2,
            max_fee_share_bps: 30_000,
            max_oracle_move_bps: 500,
            max_oracle_staleness: 3_600,
            mark_band_bps: 200,
            order_band_bps: 5_000,
            max_fills_per_order: 1_000,
            liquidation_insurance_share_bps: 5_000,
            min_deployer_stake: 1,
            min_initial_margin_bps: 500,
            ..EngineParams::default()
        },
        validators: vals.0.iter().map(crypto::pubkey).collect(),
        timestamp_ms: 0,
        launch: Default::default(),
        staking: Default::default(),
    });
    let mut height = 0;
    let mut block = |c: &mut chain::Chain, txs: Vec<Tx>| {
        height += 1;
        c.apply_block(&chain::Block { height, timestamp_ms: height as i64 * 1_000, txs }).unwrap()
    };
    let out = block(&mut c, vec![vals.list(&l, obs0)]);
    assert_eq!(out.results, vec![Ok(())]);
    let listing = c.listings[&0];
    let dex = c.launch_dex.unwrap();
    let m = &c.state.markets[listing.market as usize];
    assert!(m.oracle_price >= 10_000, "lot sized so a lot is worth >= 0.01 USDC: {}", m.oracle_price);
    let expected = (launchpad::twap(obs0, (l.cumulative(), l.last_ts)).unwrap() * listing.lot_atoms as u128
        / chain::PRICE_SCALE) as u64;
    assert_eq!(m.oracle_price, expected);
    assert_eq!(m.cfg.initial_margin_bps, 2_000, "5x max on launch perps");
    let (first_cap, price) = (m.cfg.oi_cap, m.oracle_price);
    assert!(first_cap > 0);
    let out = block(&mut c, vec![vals.list(&l, obs0)]);
    assert_eq!(out.results, vec![Err(chain::TxError::AlreadyListed)]);

    // --- traders use the perp; the creator earns on chain ---
    let (a, b) = (SigningKey::from_bytes(&[1; 32]), SigningKey::from_bytes(&[2; 32]));
    let txs = vec![
        vals.deposit(0, crypto::pubkey(&a), 100_000_000_000),
        vals.deposit(1, crypto::pubkey(&b), 100_000_000_000),
        Tx::User(chain::SignedTx::sign(&a, CHAIN_ID, 1, chain::Action::TransferToDex { dex, amount: 100_000_000_000 })),
        Tx::User(chain::SignedTx::sign(&b, CHAIN_ID, 1, chain::Action::TransferToDex { dex, amount: 100_000_000_000 })),
    ];
    let out = block(&mut c, txs);
    assert!(out.results.iter().all(|r| r.is_ok()), "{:?}", out.results);
    let size = (first_cap / 4).max(1);
    let txs = vec![
        Tx::User(chain::SignedTx::sign(
            &a,
            CHAIN_ID,
            2,
            chain::Action::PlaceOrder {
                market: listing.market,
                side: 1,
                kind: 1,
                price,
                size,
                reduce_only: false,
                client_id: 0,
            },
        )),
        Tx::User(chain::SignedTx::sign(
            &b,
            CHAIN_ID,
            2,
            chain::Action::PlaceOrder {
                market: listing.market,
                side: 0,
                kind: 2,
                price,
                size,
                reduce_only: false,
                client_id: 0,
            },
        )),
    ];
    let out = block(&mut c, txs);
    assert!(out.events.iter().any(|e| matches!(e, Event::Fill { .. })), "{:?}", out.results);
    let creator_acct = c.state.account_id(&l.creator).unwrap();
    let earned = c.state.accounts[creator_acct as usize].balance;
    assert!(earned > 0, "the token creator gets half the protocol fee on its perp");

    // --- the oracle follows the pool ---
    let obs1 = (l.cumulative(), l.last_ts);
    let slot = sol.advance(300).await;
    // Big buys pump the pool price.
    for _ in 0..3 {
        sol.exec(
            &[launchpad::swap_ix(&pid, &whale.pubkey(), 0, &w_tok, &w_usdc, &treasury, true, 20_000_000_000, 0)],
            &[&whale],
        )
        .await;
    }
    sol.advance(300).await;
    sol.exec(
        &[launchpad::swap_ix(&pid, &whale.pubkey(), 0, &w_tok, &w_usdc, &treasury, true, 1_000_000, 0)],
        &[&whale],
    )
    .await;
    let l2 = sol.launch(0).await;
    let twap = launchpad::twap(obs1, (l2.cumulative(), l2.last_ts)).unwrap();
    let target = (twap * listing.lot_atoms as u128 / chain::PRICE_SCALE) as u64;
    assert!(target > price * 11 / 10, "TWAP rose more than 10%");
    let upd = vec![LaunchObservation { launch_id: 0, twap_scaled: twap, pool_quote: l2.pool_quote }];
    let out = block(&mut c, vec![vals.oracle(slot, upd.clone())]);
    assert_eq!(out.results, vec![Ok(())]);
    let m = &c.state.markets[listing.market as usize];
    assert_eq!(m.oracle_price, price + price * 500 / 10_000, "moves at most 5% per update");
    assert!(m.cfg.oi_cap > first_cap, "deeper pool, larger OI cap");
    let out = block(&mut c, vec![vals.oracle(slot, upd)]);
    assert_eq!(out.results, vec![Err(chain::TxError::StaleOracle)], "an old observation can't be replayed");
}

/// Makes the payer the program's upgrade authority (who alone may init).
fn set_upgrade_authority(ctx: &mut ProgramTestContext, pid: &Pubkey) {
    let account = solana_account::Account {
        lamports: 1_000_000_000,
        data: mee::pda::program_data_header(&ctx.payer.pubkey()),
        owner: mee::pda::UPGRADEABLE_LOADER_ID,
        executable: false,
        rent_epoch: 0,
    };
    ctx.set_account(&mee::pda::program_data(pid), &account.into());
}
