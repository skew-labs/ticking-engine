//! Launchpad end to end: create → curve trading → sell-out → graduation →
//! pool swaps with a TWAP accumulator → creator fee claim. Vault balances
//! are checked against the launch's accounting after every step.

mod common;

use mee::launchpad::{self, Launch, LaunchParams, STATUS_COMPLETE, STATUS_CURVE, STATUS_POOL};
use mee::token;
use solana_keypair::Keypair;
use solana_program::clock::Clock;
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey::Pubkey;
use solana_program_test::{ProgramTest, ProgramTestContext, processor};
use solana_signer::Signer;
use solana_transaction::Transaction;

const PARAMS: LaunchParams = LaunchParams {
    total_supply: 1_000_000_000_000, // 1M tokens (6 decimals)
    curve_supply: 800_000_000_000,   // 800k on the curve
    virtual_quote: 5_000_000_000,    // 5k USDC virtual
    virtual_tokens: 1_073_000_000_000,
    protocol_fee_bps: 50,
    creator_fee_bps: 50,
    pool_fee_bps: 25,
};

struct Env {
    ctx: ProgramTestContext,
    pid: Pubkey,
    usdc: Pubkey,
    treasury: Pubkey,
}

impl Env {
    async fn exec(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Result<(), String> {
        let blockhash = self.ctx.get_new_latest_blockhash().await.unwrap();
        let mut all: Vec<&Keypair> = vec![&self.ctx.payer];
        all.extend_from_slice(signers);
        let tx = Transaction::new_signed_with_payer(ixs, Some(&self.ctx.payer.pubkey()), &all, blockhash);
        common::process(&self.ctx.banks_client, tx).await
    }

    async fn funded(&mut self) -> Keypair {
        let k = Keypair::new();
        let mut data = 2u32.to_le_bytes().to_vec();
        data.extend_from_slice(&1_000_000_000u64.to_le_bytes());
        let ix = Instruction {
            program_id: token::SYSTEM_PROGRAM_ID,
            accounts: vec![AccountMeta::new(self.ctx.payer.pubkey(), true), AccountMeta::new(k.pubkey(), false)],
            data,
        };
        self.exec(&[ix], &[]).await.unwrap();
        k
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
        self.exec(&ixs, &[&acct]).await.unwrap();
        acct.pubkey()
    }

    async fn balance(&mut self, acct: &Pubkey) -> u64 {
        let a = self.ctx.banks_client.get_account(*acct).await.unwrap().unwrap();
        u64::from_le_bytes(a.data[64..72].try_into().unwrap())
    }

    async fn launch(&mut self, id: u64) -> Launch {
        let a = self.ctx.banks_client.get_account(launchpad::launch_pda(&self.pid, id).0).await.unwrap().unwrap();
        *bytemuck::from_bytes(&a.data)
    }

    /// Vault balances must equal the launch's books.
    async fn check(&mut self, id: u64) -> Launch {
        let l = self.launch(id).await;
        let key = launchpad::launch_pda(&self.pid, id).0;
        let qv = self.balance(&launchpad::quote_vault_pda(&self.pid, &key).0).await;
        let tv = self.balance(&launchpad::token_vault_pda(&self.pid, &key).0).await;
        if l.status == STATUS_POOL {
            assert_eq!(qv, l.pool_quote + l.creator_fees, "quote vault");
            assert_eq!(tv, l.pool_tokens + l.locked_tokens, "token vault");
        } else {
            assert_eq!(qv, l.real_quote + l.creator_fees, "quote vault");
            assert_eq!(tv, l.real_tokens + l.reserve_tokens, "token vault");
        }
        l
    }

    async fn advance(&mut self, secs: i64) {
        let mut clock: Clock = self.ctx.banks_client.get_sysvar().await.unwrap();
        clock.unix_timestamp += secs;
        self.ctx.set_sysvar(&clock);
    }
}

async fn setup() -> Env {
    let pid = mee::id();
    let mut pt = ProgramTest::new("mee", pid, processor!(mee::processor::process_instruction));
    // With SBF_OUT_DIR set (holding mee.so from `cargo build-sbf`), run the
    // compiled SBF program instead of the native build.
    pt.prefer_bpf(std::env::var_os("SBF_OUT_DIR").is_some());
    let mut ctx = pt.start_with_context().await;
    set_upgrade_authority(&mut ctx, &pid);
    let mut env = Env { ctx, pid, usdc: Pubkey::default(), treasury: Pubkey::default() };
    let payer = env.ctx.payer.pubkey();
    let mint = Keypair::new();
    let rent = env.ctx.banks_client.get_rent().await.unwrap();
    let ixs = [
        token::create_account_ix(
            &payer,
            &mint.pubkey(),
            rent.minimum_balance(token::MINT_LEN),
            token::MINT_LEN as u64,
            &token::TOKEN_PROGRAM_ID,
        ),
        token::initialize_mint2_ix(&mint.pubkey(), &payer, 6),
    ];
    env.exec(&ixs, &[&mint]).await.unwrap();
    env.usdc = mint.pubkey();
    env.treasury = env.token_account(&env.usdc.clone(), &payer, 0).await;
    let ix = launchpad::init_ix(&pid, &payer, &env.usdc, &env.treasury, PARAMS);
    env.exec(&[ix], &[]).await.unwrap();
    env
}

fn name(s: &str) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[..s.len()].copy_from_slice(s.as_bytes());
    b
}

fn symbol(s: &str) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..s.len()].copy_from_slice(s.as_bytes());
    b
}

#[tokio::test]
async fn curve_to_pool_lifecycle() {
    let mut env = setup().await;
    let (pid, usdc, treasury) = (env.pid, env.usdc, env.treasury);
    let creator = env.funded().await;
    let ix = launchpad::create_ix(&pid, &creator.pubkey(), &usdc, 0, name("Machine Dog"), symbol("MDOG"));
    env.exec(&[ix], &[&creator]).await.unwrap();
    let l = env.check(0).await;
    assert_eq!((l.status, l.real_tokens, l.reserve_tokens), (STATUS_CURVE, PARAMS.curve_supply, 200_000_000_000));
    let mint = Pubkey::new_from_array(l.mint);

    // --- curve trading ---
    let alice = Keypair::new();
    let a_usdc = env.token_account(&usdc, &alice.pubkey(), 100_000_000_000).await;
    let a_tok = env.token_account(&mint, &alice.pubkey(), 0).await;
    let p0 = l.spot_price();
    let buy = launchpad::buy_ix(&pid, &alice.pubkey(), 0, &a_tok, &a_usdc, &treasury, 1_000_000_000, 0);
    env.exec(&[buy], &[&alice]).await.unwrap();
    let l = env.check(0).await;
    let bought = env.balance(&a_tok).await;
    assert!(bought > 0 && l.spot_price() > p0, "buying moves the price up");
    assert_eq!(env.balance(&treasury).await, 5_000_000, "0.5% protocol fee");
    assert_eq!(l.creator_fees, 5_000_000, "0.5% creator fee");

    // Slippage guard.
    let greedy = launchpad::buy_ix(&pid, &alice.pubkey(), 0, &a_tok, &a_usdc, &treasury, 1_000_000, u64::MAX);
    let err = env.exec(&[greedy], &[&alice]).await.unwrap_err();
    assert!(err.contains("Custom(1011)"), "{err}");

    let sell = launchpad::sell_ix(&pid, &alice.pubkey(), 0, &a_tok, &a_usdc, &treasury, bought / 2, 0);
    env.exec(&[sell], &[&alice]).await.unwrap();
    env.check(0).await;

    // --- a whale buys out the rest of the curve ---
    let whale = Keypair::new();
    let w_usdc = env.token_account(&usdc, &whale.pubkey(), 1_000_000_000_000).await;
    let w_tok = env.token_account(&mint, &whale.pubkey(), 0).await;
    let buy = launchpad::buy_ix(&pid, &whale.pubkey(), 0, &w_tok, &w_usdc, &treasury, 1_000_000_000_000, 0);
    env.exec(&[buy], &[&whale]).await.unwrap();
    let l = env.check(0).await;
    assert_eq!((l.status, l.real_tokens), (STATUS_COMPLETE, 0));
    let spent = 1_000_000_000_000 - env.balance(&w_usdc).await;
    assert!(spent < 20_000_000_000, "charged only what the remaining allotment cost: {spent}");
    let curve_end_price = l.spot_price();

    // Curve closed; trading waits for graduation.
    let late = launchpad::buy_ix(&pid, &alice.pubkey(), 0, &a_tok, &a_usdc, &treasury, 1_000_000, 0);
    assert!(env.exec(&[late], &[&alice]).await.is_err());

    // --- graduation ---
    env.exec(&[launchpad::graduate_ix(&pid, 0)], &[]).await.unwrap();
    let l = env.check(0).await;
    assert_eq!(l.status, STATUS_POOL);
    assert_eq!(l.pool_quote, l.real_quote);
    let pool_price = l.spot_price();
    // Continuity: the pool opens at (or, if the reserve binds, just above)
    // the curve's last price.
    assert!(
        pool_price >= curve_end_price && pool_price * 100 <= curve_end_price * 103,
        "{pool_price} vs {curve_end_price}"
    );
    assert!(env.exec(&[launchpad::graduate_ix(&pid, 0)], &[]).await.is_err(), "graduates once");

    // --- pool swaps and TWAP ---
    let c0 = (l.cumulative(), l.last_ts);
    env.advance(60).await;
    let swap = launchpad::swap_ix(&pid, &alice.pubkey(), 0, &a_tok, &a_usdc, &treasury, true, 500_000_000, 0);
    env.exec(&[swap], &[&alice]).await.unwrap();
    let l = env.check(0).await;
    let c1 = (l.cumulative(), l.last_ts);
    assert_eq!(launchpad::twap(c0, c1), Some(pool_price), "TWAP over the minute before the swap");
    env.advance(30).await;
    let tokens = env.balance(&a_tok).await;
    let swap = launchpad::swap_ix(&pid, &alice.pubkey(), 0, &a_tok, &a_usdc, &treasury, false, tokens / 3, 0);
    env.exec(&[swap], &[&alice]).await.unwrap();
    let l2 = env.check(0).await;
    let twap = launchpad::twap(c1, (l2.cumulative(), l2.last_ts)).unwrap();
    assert_eq!(twap, l.spot_price(), "price held between the two swaps");
    assert!((l2.pool_quote as u128) * (l2.pool_tokens as u128) > (l.pool_quote as u128) * (l.pool_tokens as u128) / 2);

    // --- creator claims fees ---
    let c_usdc = env.token_account(&usdc, &creator.pubkey(), 0).await;
    let owed = l2.creator_fees;
    let mallory = Keypair::new();
    let steal = launchpad::claim_creator_fees_ix(&pid, &mallory.pubkey(), 0, &c_usdc);
    assert!(env.exec(&[steal], &[&mallory]).await.unwrap_err().contains("Custom(1005)"));
    env.exec(&[launchpad::claim_creator_fees_ix(&pid, &creator.pubkey(), 0, &c_usdc)], &[&creator]).await.unwrap();
    assert_eq!(env.balance(&c_usdc).await, owed);
    let l3 = env.check(0).await;
    assert_eq!(l3.creator_fees, 0);
}

#[tokio::test]
async fn launches_are_independent() {
    let mut env = setup().await;
    let (pid, usdc, treasury) = (env.pid, env.usdc, env.treasury);
    let creator = env.funded().await;
    for i in 0..3 {
        let ix = launchpad::create_ix(&pid, &creator.pubkey(), &usdc, i, name("x"), symbol("X"));
        env.exec(&[ix], &[&creator]).await.unwrap();
    }
    let l1 = env.launch(1).await;
    let mint1 = Pubkey::new_from_array(l1.mint);
    let bob = Keypair::new();
    let b_usdc = env.token_account(&usdc, &bob.pubkey(), 10_000_000_000).await;
    let b_tok = env.token_account(&mint1, &bob.pubkey(), 0).await;
    // Using launch 2's id with launch 1's token account fails at the token program.
    let wrong = launchpad::buy_ix(&pid, &bob.pubkey(), 2, &b_tok, &b_usdc, &treasury, 1_000_000, 0);
    assert!(env.exec(&[wrong], &[&bob]).await.is_err());
    let ok = launchpad::buy_ix(&pid, &bob.pubkey(), 1, &b_tok, &b_usdc, &treasury, 1_000_000, 0);
    env.exec(&[ok], &[&bob]).await.unwrap();
    assert_eq!(env.check(0).await.real_quote, 0);
    assert!(env.check(1).await.real_quote > 0);
    assert_eq!(env.check(2).await.real_quote, 0);
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
