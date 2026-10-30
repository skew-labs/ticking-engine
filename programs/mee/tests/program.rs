//! End-to-end tests: the program runs natively inside `solana-program-test`
//! next to the real SPL Token program.

use mee::instruction::{self as mi, AddMarketArgs, OrderArgs, ParamsArgs};
use mee::state::{BOOK_SIZE, DEX_SIZE};
use mee::token;
use mee_core::{DEX_FROZEN, DEX_RETIRED, DexState, EngineError, MARKET_SETTLED};
use solana_keypair::Keypair;
use solana_program::clock::Clock;
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;
use solana_program_test::{ProgramTest, ProgramTestContext, processor};
use solana_signer::Signer;
use solana_transaction::Transaction;

const PRICE: u64 = 10_000;
const DECIMALS: u8 = 6;

fn params() -> ParamsArgs {
    ParamsArgs {
        taker_fee_bps: 5,
        maker_fee_bps: 1,
        max_fee_share_bps: 30_000,
        max_oracle_move_bps: 500,
        min_oracle_interval: 0,
        max_oracle_staleness: 3_600,
        mark_band_bps: 100,
        order_band_bps: 5_000,
        max_fills_per_order: 16,
        liquidation_insurance_share_bps: 5_000,
        min_deployer_stake: 500_000,
        min_stake_hold_secs: 1_000,
        unstake_delay_secs: 100,
        min_initial_margin_bps: 500,
    }
}

fn market_args() -> AddMarketArgs {
    AddMarketArgs {
        symbol: *b"NVDA-PERP\0\0\0\0\0\0\0",
        tick_size: 1,
        min_base_lots: 1,
        initial_margin_bps: 1_000,
        maintenance_margin_bps: 500,
        liquidation_fee_bps: 250,
        oi_cap: 1_000_000,
        initial_price: PRICE,
        funding_interval: 3_600,
        max_funding_bps: 50,
    }
}

struct Env {
    ctx: ProgramTestContext,
    pid: Pubkey,
    quote_mint: Keypair,
    treasury_quote: Pubkey,
    treasury_stake: Pubkey,
    guardians: Vec<Keypair>,
    deployer: Keypair,
    deployer_quote: Pubkey,
    deployer_stake: Pubkey,
    dex: Keypair,
    book: Keypair,
}

impl Env {
    async fn exec(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Result<(), String> {
        let blockhash = self.ctx.get_new_latest_blockhash().await.unwrap();
        let mut all: Vec<&Keypair> = vec![&self.ctx.payer];
        all.extend_from_slice(signers);
        let tx = Transaction::new_signed_with_payer(ixs, Some(&self.ctx.payer.pubkey()), &all, blockhash);
        self.ctx.banks_client.process_transaction(tx).await.map_err(|e| format!("{e:?}"))
    }

    async fn ok(&mut self, ixs: &[Instruction], signers: &[&Keypair]) {
        self.exec(ixs, signers).await.unwrap();
    }

    async fn fails(&mut self, ixs: &[Instruction], signers: &[&Keypair], code: u32) {
        let err = self.exec(ixs, signers).await.unwrap_err();
        assert!(err.contains(&format!("Custom({code})")), "expected Custom({code}), got {err}");
    }

    async fn create_account_ix(&mut self, key: &Pubkey, space: usize, owner: &Pubkey) -> Instruction {
        let rent = self.ctx.banks_client.get_rent().await.unwrap();
        token::create_account_ix(&self.ctx.payer.pubkey(), key, rent.minimum_balance(space), space as u64, owner)
    }

    async fn token_account(&mut self, mint: &Pubkey, owner: &Pubkey, amount: u64) -> Pubkey {
        let acct = Keypair::new();
        let create = self.create_account_ix(&acct.pubkey(), token::TOKEN_ACCOUNT_LEN, &token::TOKEN_PROGRAM_ID).await;
        let mut ixs = vec![create, token::initialize_account3_ix(&acct.pubkey(), mint, owner)];
        if amount > 0 {
            ixs.push(token::mint_to_ix(mint, &acct.pubkey(), &self.ctx.payer.pubkey(), amount));
        }
        self.ok(&ixs, &[&acct]).await;
        acct.pubkey()
    }

    async fn balance(&mut self, acct: &Pubkey) -> u64 {
        let a = self.ctx.banks_client.get_account(*acct).await.unwrap().unwrap();
        u64::from_le_bytes(a.data[64..72].try_into().unwrap())
    }

    async fn dex_state(&mut self) -> Box<DexState> {
        let a = self.ctx.banks_client.get_account(self.dex.pubkey()).await.unwrap().unwrap();
        let mut boxed: Box<DexState> = bytemuck::zeroed_box();
        bytemuck::bytes_of_mut(&mut *boxed).copy_from_slice(&a.data);
        boxed
    }

    async fn set_time(&mut self, unix_timestamp: i64) {
        let mut clock: Clock = self.ctx.banks_client.get_sysvar().await.unwrap();
        clock.unix_timestamp = unix_timestamp;
        self.ctx.set_sysvar(&clock);
    }

    async fn now(&mut self) -> i64 {
        let clock: Clock = self.ctx.banks_client.get_sysvar().await.unwrap();
        clock.unix_timestamp
    }

    /// A funded, registered trader with `collateral` deposited.
    async fn trader(&mut self, collateral: u64) -> (Keypair, Pubkey) {
        let kp = Keypair::new();
        let mint = self.quote_mint.pubkey();
        let ta = self.token_account(&mint, &kp.pubkey(), collateral * 2).await;
        let (pid, dex) = (self.pid, self.dex.pubkey());
        self.ok(
            &[mi::register_trader(&pid, &kp.pubkey(), &dex), mi::deposit(&pid, &kp.pubkey(), &dex, &ta, collateral)],
            &[&kp],
        )
        .await;
        (kp, ta)
    }

    fn order(&self, owner: &Keypair, side: u8, kind: u8, price: u64, lots: u64) -> Instruction {
        let o = OrderArgs { market: 0, side, kind, reduce_only: false, price, base_lots: lots, client_order_id: 7 };
        mi::place_order(&self.pid, &owner.pubkey(), &self.dex.pubkey(), &self.book.pubkey(), o)
    }

    async fn walk_oracle(&mut self, target: u64) {
        loop {
            let cur = self.dex_state().await.markets[0].oracle_price;
            if cur == target {
                return;
            }
            let step = cur * 500 / 10_000;
            let next = if target > cur { (cur + step).min(target) } else { (cur - step).max(target) };
            let ix = mi::set_oracle(&self.pid, &self.deployer.pubkey(), &self.dex.pubkey(), vec![(0, next)]);
            let deployer = self.deployer.insecure_clone();
            self.ok(&[ix], &[&deployer]).await;
        }
    }
}

/// Exchange + one deployed DEX with one market. The deployer is its own
/// oracle authority.
async fn setup() -> Env {
    let pid = mee::id();
    let mut pt = ProgramTest::new("mee", pid, processor!(mee::processor::process_instruction));
    pt.prefer_bpf(false);
    let ctx = pt.start_with_context().await;

    let quote_mint = Keypair::new();
    let stake_mint = Keypair::new();
    let mut env = Env {
        ctx,
        pid,
        quote_mint: quote_mint.insecure_clone(),
        treasury_quote: Pubkey::default(),
        treasury_stake: Pubkey::default(),
        guardians: (0..3).map(|_| Keypair::new()).collect(),
        deployer: Keypair::new(),
        deployer_quote: Pubkey::default(),
        deployer_stake: Pubkey::default(),
        dex: Keypair::new(),
        book: Keypair::new(),
    };
    let payer = env.ctx.payer.pubkey();
    for mint in [&quote_mint, &stake_mint] {
        let create = env.create_account_ix(&mint.pubkey(), token::MINT_LEN, &token::TOKEN_PROGRAM_ID).await;
        env.ok(&[create, token::initialize_mint2_ix(&mint.pubkey(), &payer, DECIMALS)], &[mint]).await;
    }
    env.treasury_quote = env.token_account(&quote_mint.pubkey(), &payer, 0).await;
    env.treasury_stake = env.token_account(&stake_mint.pubkey(), &payer, 0).await;

    let guardians: Vec<[u8; 32]> = env.guardians.iter().map(|g| g.pubkey().to_bytes()).collect();
    let ix = mi::init_exchange(
        &pid,
        &payer,
        &quote_mint.pubkey(),
        &stake_mint.pubkey(),
        &env.treasury_quote,
        &env.treasury_stake,
        params(),
        guardians,
        2,
    );
    env.ok(&[ix], &[]).await;

    let deployer = env.deployer.insecure_clone();
    env.ok(&[solana_program_test_transfer(&payer, &deployer.pubkey(), 10_000_000_000)], &[]).await;
    env.deployer_stake = env.token_account(&stake_mint.pubkey(), &deployer.pubkey(), 1_000_000).await;
    env.deployer_quote = env.token_account(&quote_mint.pubkey(), &deployer.pubkey(), 0).await;

    // Below the minimum stake: rejected.
    let dex = env.dex.insecure_clone();
    let create_dex = env.create_account_ix(&dex.pubkey(), DEX_SIZE, &pid).await;
    let deploy = |stake| {
        mi::deploy_dex(
            &pid,
            &deployer.pubkey(),
            &dex.pubkey(),
            &quote_mint.pubkey(),
            &stake_mint.pubkey(),
            &env.deployer_stake,
            *b"skew-equities\0\0\0",
            10_000,
            &deployer.pubkey(),
            stake,
        )
    };
    let (low, ok) = (deploy(100), deploy(500_000));
    env.fails(&[create_dex.clone(), low], &[&deployer, &dex], EngineError::StakeTooLow.code()).await;
    env.ok(&[create_dex, ok], &[&deployer, &dex]).await;

    let book = env.book.insecure_clone();
    let create_book = env.create_account_ix(&book.pubkey(), BOOK_SIZE, &pid).await;
    let add = mi::add_market(&pid, &deployer.pubkey(), &dex.pubkey(), &book.pubkey(), market_args());
    env.ok(&[create_book, add], &[&deployer, &book]).await;
    env
}

fn solana_program_test_transfer(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: token::SYSTEM_PROGRAM_ID,
        accounts: vec![
            solana_program::instruction::AccountMeta::new(*from, true),
            solana_program::instruction::AccountMeta::new(*to, false),
        ],
        data,
    }
}

#[tokio::test]
async fn deploy_trade_liquidate_withdraw() {
    let mut env = setup().await;
    let (pid, dex_key, book_key) = (env.pid, env.dex.pubkey(), env.book.pubkey());
    let stake_vault = mee::pda::stake_vault(&pid, &dex_key).0;
    let quote_vault = mee::pda::quote_vault(&pid, &dex_key).0;
    assert_eq!(env.balance(&stake_vault).await, 500_000);

    let s = env.dex_state().await;
    assert_eq!(s.header.market_count, 1);
    assert_eq!(s.markets[0].book, book_key.to_bytes());

    let (maker, maker_ta) = env.trader(100_000_000).await;
    let (taker, _) = env.trader(12_000).await;
    let (liq, _) = env.trader(100_000_000).await;
    assert_eq!(env.balance(&quote_vault).await, 200_012_000);

    let ask = env.order(&maker, 1, 0, PRICE, 10);
    env.ok(&[ask], &[&maker]).await;
    // 10 lots at 10_000 = 100_000 notional; 10% IM = 10_000 + fees fits in 12_000.
    let bid = env.order(&taker, 0, 2, PRICE, 10);
    env.ok(&[bid], &[&taker]).await;

    let s = env.dex_state().await;
    let t = mee_core::engine::find_trader(&s, &taker.pubkey().to_bytes()).unwrap();
    let m = mee_core::engine::find_trader(&s, &maker.pubkey().to_bytes()).unwrap();
    assert_eq!(s.traders[t].positions[0].base, 10);
    assert_eq!(s.traders[m].positions[0].base, -10);
    assert_eq!(s.markets[0].open_interest, 10);
    // 100% deployer share: taker pays 50 + 50, maker 10 + 10.
    assert_eq!(s.traders[t].collateral, 12_000 - 100);
    assert_eq!(s.header.protocol_fees, 60);
    assert_eq!(s.header.deployer_fees, 60);

    // Oversized order fails the margin check and changes nothing.
    let big = env.order(&taker, 0, 0, PRICE - 100, 50);
    env.fails(&[big], &[&taker], EngineError::InsufficientMargin.code()).await;

    // Not liquidatable yet.
    let l = mi::liquidate(&pid, &liq.pubkey(), &dex_key, &book_key, 0, &taker.pubkey(), 0);
    env.fails(std::slice::from_ref(&l), &[&liq], EngineError::NotLiquidatable.code()).await;

    env.walk_oracle(9_200).await;
    env.ok(&[l], &[&liq]).await;
    let s = env.dex_state().await;
    let li = mee_core::engine::find_trader(&s, &liq.pubkey().to_bytes()).unwrap();
    assert_eq!(s.traders[t].positions[0].base, 0);
    assert_eq!(s.traders[li].positions[0].base, 10);
    assert_eq!(s.header.insurance, 1_150);

    // Withdraw moves tokens out of the vault.
    let before = env.balance(&maker_ta).await;
    let w = mi::withdraw(&pid, &maker.pubkey(), &dex_key, &maker_ta, 1_000_000);
    env.ok(&[w], &[&maker]).await;
    assert_eq!(env.balance(&maker_ta).await, before + 1_000_000);

    // Fees: deployer claims its share, protocol fees go to the treasury.
    let deployer = env.deployer.insecure_clone();
    let (dq, tq) = (env.deployer_quote, env.treasury_quote);
    env.ok(&[mi::claim_deployer_fees(&pid, &deployer.pubkey(), &dex_key, &dq)], &[&deployer]).await;
    env.ok(&[mi::claim_protocol_fees(&pid, &dex_key, &tq)], &[]).await;
    assert_eq!(env.balance(&dq).await, 60);
    assert_eq!(env.balance(&tq).await, 60);

    // Vault still covers every claim.
    let s = env.dex_state().await;
    let claims = mee_core::engine::total_claims(&s).unwrap();
    assert!(env.balance(&quote_vault).await as i128 >= claims);
}

#[tokio::test]
async fn access_control() {
    let mut env = setup().await;
    let (pid, dex_key, book_key) = (env.pid, env.dex.pubkey(), env.book.pubkey());
    let (mallory, mallory_ta) = env.trader(1_000_000).await;

    // Only the oracle authority may push prices.
    let ix = mi::set_oracle(&pid, &mallory.pubkey(), &dex_key, vec![(0, PRICE)]);
    env.fails(&[ix], &[&mallory], mee::error::MeeError::Unauthorized as u32).await;

    // Only the deployer may add markets or halt.
    let book2 = Keypair::new();
    let create = env.create_account_ix(&book2.pubkey(), BOOK_SIZE, &pid).await;
    let add = mi::add_market(&pid, &mallory.pubkey(), &dex_key, &book2.pubkey(), market_args());
    env.fails(&[create, add], &[&mallory, &book2], mee::error::MeeError::Unauthorized as u32).await;

    // A DEX account created by someone else cannot be taken over.
    let victim = Keypair::new();
    let create = env.create_account_ix(&victim.pubkey(), DEX_SIZE, &pid).await;
    env.ok(&[create], &[&victim]).await;
    let mut hijack = mi::deploy_dex(
        &pid,
        &mallory.pubkey(),
        &victim.pubkey(),
        &env.quote_mint.pubkey(),
        &Pubkey::new_unique(),
        &mallory_ta,
        [0; 16],
        0,
        &mallory.pubkey(),
        500_000,
    );
    hijack.accounts[2].is_signer = false;
    let err = env.exec(&[hijack], &[&mallory]).await.unwrap_err();
    assert!(err.contains("MissingRequiredSignature"), "{err}");
    let halt = mi::halt_market(&pid, &mallory.pubkey(), &dex_key, &book_key, 0, PRICE);
    env.fails(&[halt], &[&mallory], mee::error::MeeError::Unauthorized as u32).await;

    // A book that is not bound to the market is rejected.
    let fake = Keypair::new();
    let create = env.create_account_ix(&fake.pubkey(), BOOK_SIZE, &pid).await;
    env.ok(&[create], &[&fake]).await;
    let o =
        OrderArgs { market: 0, side: 0, kind: 0, reduce_only: false, price: PRICE, base_lots: 1, client_order_id: 0 };
    let ix = mi::place_order(&pid, &mallory.pubkey(), &dex_key, &fake.pubkey(), o);
    env.fails(&[ix], &[&mallory], mee::error::MeeError::BookMismatch as u32).await;

    // Depositing into anything but the DEX vault is rejected.
    let mut dep = mi::deposit(&pid, &mallory.pubkey(), &dex_key, &mallory_ta, 10);
    dep.accounts[3].pubkey = mallory_ta;
    env.fails(&[dep], &[&mallory], mee::error::MeeError::Unauthorized as u32).await;

    // Withdrawing more than the free collateral fails.
    let w = mi::withdraw(&pid, &mallory.pubkey(), &dex_key, &mallory_ta, 2_000_000);
    env.fails(&[w], &[&mallory], EngineError::InsufficientCollateral.code()).await;
}

#[tokio::test]
async fn halt_settle_unstake() {
    let mut env = setup().await;
    let (pid, dex_key, book_key) = (env.pid, env.dex.pubkey(), env.book.pubkey());
    let (a, _) = env.trader(1_000_000).await;
    let (b, _) = env.trader(1_000_000).await;
    let ask = env.order(&a, 1, 0, PRICE, 10);
    let bid = env.order(&b, 0, 2, PRICE, 4);
    env.ok(&[ask], &[&a]).await;
    env.ok(&[bid], &[&b]).await;

    let deployer = env.deployer.insecure_clone();
    let ds = env.deployer_stake;
    env.fails(
        &[mi::request_unstake(&pid, &deployer.pubkey(), &dex_key)],
        &[&deployer],
        EngineError::MarketsOpen.code(),
    )
    .await;

    env.ok(&[mi::halt_market(&pid, &deployer.pubkey(), &dex_key, &book_key, 0, 10_200)], &[&deployer]).await;
    for _ in 0..3 {
        env.ok(&[mi::settle_market(&pid, &dex_key, 0, 100)], &[]).await;
    }
    let s = env.dex_state().await;
    assert_eq!(s.markets[0].status, MARKET_SETTLED);
    let bi = mee_core::engine::find_trader(&s, &b.pubkey().to_bytes()).unwrap();
    assert_eq!(s.traders[bi].positions[0].base, 0);
    assert!(s.traders[bi].collateral > 1_000_000, "long settled at a profit");

    let now = env.now().await;
    env.fails(
        &[mi::request_unstake(&pid, &deployer.pubkey(), &dex_key)],
        &[&deployer],
        EngineError::StakeLocked.code(),
    )
    .await;
    env.set_time(now + 1_000).await;
    env.ok(&[mi::request_unstake(&pid, &deployer.pubkey(), &dex_key)], &[&deployer]).await;
    let complete = mi::complete_unstake(&pid, &deployer.pubkey(), &dex_key, &ds);
    env.fails(std::slice::from_ref(&complete), &[&deployer], EngineError::UnstakeDelay.code()).await;
    env.set_time(now + 1_100).await;
    env.ok(&[complete], &[&deployer]).await;
    assert_eq!(env.balance(&ds).await, 1_000_000);
    assert_eq!(env.dex_state().await.header.status, DEX_RETIRED);
}

#[tokio::test]
async fn guardians_slash_and_freeze() {
    let mut env = setup().await;
    let (pid, dex_key, book_key) = (env.pid, env.dex.pubkey(), env.book.pubkey());
    let g: Vec<Keypair> = env.guardians.iter().map(|k| k.insecure_clone()).collect();
    let deployer = env.deployer.insecure_clone();
    let ts = env.treasury_stake;

    let outsider = Keypair::new();
    env.fails(&[mi::vote_slash(&pid, &outsider.pubkey(), &dex_key, 2_000, true)], &[&outsider], 1005).await;

    env.ok(&[mi::vote_slash(&pid, &g[0].pubkey(), &dex_key, 2_000, true)], &[&g[0]]).await;
    env.fails(&[mi::execute_slash(&pid, &dex_key, &ts)], &[], EngineError::ThresholdNotMet.code()).await;
    env.ok(&[mi::vote_slash(&pid, &g[2].pubkey(), &dex_key, 2_000, true)], &[&g[2]]).await;
    env.ok(&[mi::execute_slash(&pid, &dex_key, &ts)], &[]).await;

    assert_eq!(env.balance(&ts).await, 100_000);
    let s = env.dex_state().await;
    assert_eq!(s.header.stake_amount, 400_000);
    assert_eq!(s.header.status, DEX_FROZEN);

    // A frozen deployer can no longer claim fees.
    let dq = env.deployer_quote;
    env.fails(
        &[mi::claim_deployer_fees(&pid, &deployer.pubkey(), &dex_key, &dq)],
        &[&deployer],
        EngineError::DexNotActive.code(),
    )
    .await;

    // The deployer lost control; the admin winds the market down.
    let ix = mi::set_oracle(&pid, &deployer.pubkey(), &dex_key, vec![(0, PRICE)]);
    env.fails(&[ix], &[&deployer], EngineError::DexNotActive.code()).await;
    env.fails(&[mi::halt_market(&pid, &deployer.pubkey(), &dex_key, &book_key, 0, PRICE)], &[&deployer], 1005).await;
    let admin = env.ctx.payer.insecure_clone();
    env.ok(&[mi::halt_market(&pid, &admin.pubkey(), &dex_key, &book_key, 0, PRICE)], &[]).await;
}
