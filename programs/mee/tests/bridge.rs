//! Solana bridge ↔ MEE chain, end to end: deposit on Solana, attest and
//! credit on the chain, withdraw on the chain, then request → approve
//! (validator signatures via the Ed25519 precompile, possibly over several
//! transactions) → finalize after the dispute window.

use ed25519_dalek::SigningKey;
use mee::bridge::{self, ASSET_QUOTE, Bridge, DepositRecord, WITHDRAWAL_CANCELLED, WITHDRAWAL_PAID, WithdrawalRecord};
use mee::token;
use mee_chain::{self as chain, crypto};
use solana_keypair::Keypair;
use solana_program::clock::Clock;
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey::Pubkey;
use solana_program_test::{ProgramTest, ProgramTestContext, processor};
use solana_signer::Signer;
use solana_transaction::Transaction;

const CHAIN_ID: u64 = 42;
const DISPUTE: i64 = 200;

struct Env {
    ctx: ProgramTestContext,
    pid: Pubkey,
    quote_mint: Pubkey,
    validators: Vec<SigningKey>,
}

impl Env {
    async fn exec(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Result<(), String> {
        let blockhash = self.ctx.get_new_latest_blockhash().await.unwrap();
        let mut all: Vec<&Keypair> = vec![&self.ctx.payer];
        all.extend_from_slice(signers);
        let tx = Transaction::new_signed_with_payer(ixs, Some(&self.ctx.payer.pubkey()), &all, blockhash);
        self.ctx.banks_client.process_transaction(tx).await.map_err(|e| format!("{e:?}"))
    }

    async fn fails(&mut self, ixs: &[Instruction], code: u32) {
        let err = self.exec(ixs, &[]).await.unwrap_err();
        assert!(err.contains(&format!("Custom({code})")), "expected Custom({code}), got {err}");
    }

    async fn user(&mut self) -> Keypair {
        let user = Keypair::new();
        let payer = self.ctx.payer.pubkey();
        let ix = Instruction {
            program_id: token::SYSTEM_PROGRAM_ID,
            accounts: vec![AccountMeta::new(payer, true), AccountMeta::new(user.pubkey(), false)],
            data: [2u32.to_le_bytes().as_slice(), &1_000_000_000u64.to_le_bytes()].concat(),
        };
        self.exec(&[ix], &[]).await.unwrap();
        user
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

    async fn bridge(&mut self) -> Bridge {
        let a = self.ctx.banks_client.get_account(bridge::bridge_pda(&self.pid).0).await.unwrap().unwrap();
        *bytemuck::from_bytes(&a.data)
    }

    async fn record(&mut self, id: u64) -> WithdrawalRecord {
        let a = self.ctx.banks_client.get_account(bridge::withdrawal_pda(&self.pid, id).0).await.unwrap().unwrap();
        *bytemuck::from_bytes(&a.data)
    }

    async fn deposit_record(&mut self, id: u64) -> DepositRecord {
        let a = self.ctx.banks_client.get_account(bridge::deposit_pda(&self.pid, id).0).await.unwrap().unwrap();
        *bytemuck::from_bytes(&a.data)
    }

    async fn advance(&mut self, secs: i64) {
        let mut clock: Clock = self.ctx.banks_client.get_sysvar().await.unwrap();
        clock.unix_timestamp += secs;
        self.ctx.set_sysvar(&clock);
    }

    fn sigs(&self, msg: &[u8; 32], who: &[usize]) -> Vec<([u8; 32], [u8; 64])> {
        who.iter().map(|&i| (crypto::pubkey(&self.validators[i]), crypto::sign(&self.validators[i], msg))).collect()
    }

    fn approve(&self, msg: &[u8; 32], id: u64, who: &[usize]) -> Vec<Instruction> {
        vec![bridge::ed25519_instruction(msg, &self.sigs(msg, who)), bridge::approve_withdrawal_ix(&self.pid, id)]
    }

    /// Funds the vault with a deposit by a fresh user; returns the user and
    /// their quote token account.
    async fn funded_vault(&mut self, amount: u64) -> (Keypair, Pubkey) {
        let (pid, mint) = (self.pid, self.quote_mint);
        let user = self.user().await;
        let ta = self.token_account(&mint, &user.pubkey(), amount).await;
        let id = self.bridge().await.next_deposit_id;
        self.exec(&[bridge::deposit_ix(&pid, &user.pubkey(), &ta, &mint, id, ASSET_QUOTE, amount)], &[&user])
            .await
            .unwrap();
        (user, ta)
    }
}

async fn setup() -> Env {
    let pid = mee::id();
    let mut pt = ProgramTest::new("mee", pid, processor!(mee::processor::process_instruction));
    pt.prefer_bpf(false);
    let ctx = pt.start_with_context().await;
    let validators: Vec<SigningKey> = (0..4u8).map(|i| SigningKey::from_bytes(&[60 + i; 32])).collect();
    let mut env = Env { ctx, pid, quote_mint: Pubkey::default(), validators };
    let payer = env.ctx.payer.pubkey();
    let mut mints = Vec::new();
    for _ in 0..2 {
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
        mints.push(mint.pubkey());
    }
    env.quote_mint = mints[0];
    let vals = env.validators.iter().map(crypto::pubkey).collect();
    let ix = bridge::init_bridge_ix(&pid, &payer, &mints[0], &mints[1], CHAIN_ID, vals, DISPUTE);
    env.exec(&[ix], &[]).await.unwrap();
    env
}

#[tokio::test]
async fn deposit_attest_withdraw_roundtrip() {
    let mut env = setup().await;
    let (pid, mint) = (env.pid, env.quote_mint);
    let b = env.bridge().await;
    assert_eq!((b.validator_count, b.threshold, b.chain_id, b.dispute_secs), (4, 3, CHAIN_ID, DISPUTE));

    // --- deposit on Solana ---
    let (user, user_ta) = env.funded_vault(2_000_000).await;
    let rec = env.deposit_record(0).await;
    assert_eq!((rec.id, rec.owner, rec.amount, rec.asset), (0, user.pubkey().to_bytes(), 2_000_000, ASSET_QUOTE));
    let vault = bridge::vault_pda(&pid, &mint).0;
    assert_eq!(env.balance(&vault).await, 2_000_000);

    // --- validators attest; the chain credits; the user withdraws on chain ---
    let mut c = chain::Chain::new(chain::Genesis {
        chain_id: CHAIN_ID,
        validators: env.validators.iter().map(crypto::pubkey).collect(),
        ..Default::default()
    });
    let msg = crypto::deposit_message(CHAIN_ID, rec.id, &rec.owner, rec.asset, rec.amount);
    let attestations = [0usize, 1, 3]
        .iter()
        .map(|&i| chain::Attestation { validator: i as u16, signature: crypto::sign(&env.validators[i], &msg) })
        .collect();
    let dep = chain::Tx::System(chain::SystemTx::BridgeDeposit {
        deposit_id: rec.id,
        owner: rec.owner,
        asset: rec.asset,
        amount: rec.amount,
        attestations,
    });
    // The user signs chain transactions with the same Solana key.
    let user_key = SigningKey::from_bytes(user.secret_bytes());
    assert_eq!(crypto::pubkey(&user_key), user.pubkey().to_bytes());
    let w = chain::Tx::User(chain::SignedTx::sign(
        &user_key,
        CHAIN_ID,
        1,
        chain::Action::Withdraw { asset: ASSET_QUOTE, amount: 1_500_000, destination: user.pubkey().to_bytes() },
    ));
    let out = c.apply_block(&chain::Block { height: 1, timestamp_ms: 1_000, txs: vec![dep, w] }).unwrap();
    assert!(out.results.iter().all(|r| r.is_ok()), "{:?}", out.results);
    let r = out.withdrawals[0];
    let rmsg = r.message(CHAIN_ID);
    assert_eq!(
        rmsg,
        bridge::withdrawal_message(CHAIN_ID, r.id, r.asset, &r.destination, r.amount),
        "chain and Solana program build the same message"
    );

    // --- request + first approvals in one transaction, more in a second ---
    let payer = env.ctx.payer.pubkey();
    let mut ixs = vec![bridge::request_withdrawal_ix(&pid, &payer, r.id, r.asset, r.destination, r.amount)];
    ixs.extend(env.approve(&rmsg, r.id, &[0, 2]));
    env.exec(&ixs, &[]).await.unwrap();
    let finalize = bridge::finalize_withdrawal_ix(&pid, &mint, &user_ta, r.id);
    env.fails(std::slice::from_ref(&finalize), 1005).await; // two of four: no quorum yet

    // A non-validator signature adds nothing.
    let outsider = SigningKey::from_bytes(&[99; 32]);
    let ixs = vec![
        bridge::ed25519_instruction(&rmsg, &[(crypto::pubkey(&outsider), crypto::sign(&outsider, &rmsg))]),
        bridge::approve_withdrawal_ix(&pid, r.id),
    ];
    env.fails(&ixs, 1005).await;
    // A forged signature is rejected by the precompile itself (instruction 0,
    // error 2 = InvalidSignature).
    let mut forged = env.sigs(&rmsg, &[3]);
    forged[0].1[0] ^= 1;
    let ixs = vec![bridge::ed25519_instruction(&rmsg, &forged), bridge::approve_withdrawal_ix(&pid, r.id)];
    let err = env.exec(&ixs, &[]).await.unwrap_err();
    assert!(err.contains("InstructionError(0, Custom(2))"), "{err}");

    let ixs = env.approve(&rmsg, r.id, &[3]);
    env.exec(&ixs, &[]).await.unwrap();
    assert_eq!(env.record(r.id).await.approvals.count_ones(), 3);
    env.fails(std::slice::from_ref(&finalize), 1013).await; // dispute window

    env.advance(DISPUTE).await;
    let before = env.balance(&user_ta).await;
    env.exec(std::slice::from_ref(&finalize), &[]).await.unwrap();
    assert_eq!(env.balance(&user_ta).await, before + 1_500_000);
    assert_eq!(env.record(r.id).await.status, WITHDRAWAL_PAID);

    // Pays once; the receipt id can't be requested again either.
    env.fails(std::slice::from_ref(&finalize), 1005).await;
    env.fails(&[bridge::request_withdrawal_ix(&pid, &payer, r.id, r.asset, r.destination, r.amount)], 1004).await;
}

#[tokio::test]
async fn any_validator_locks_quorum_unlocks_or_cancels() {
    let mut env = setup().await;
    let (pid, mint) = (env.pid, env.quote_mint);
    let (user, user_ta) = env.funded_vault(3_000_000).await;
    let payer = env.ctx.payer.pubkey();
    let dest = user.pubkey().to_bytes();
    for id in [0u64, 1] {
        let msg = bridge::withdrawal_message(CHAIN_ID, id, ASSET_QUOTE, &dest, 1_000_000);
        let mut ixs = vec![bridge::request_withdrawal_ix(&pid, &payer, id, ASSET_QUOTE, dest, 1_000_000)];
        ixs.extend(env.approve(&msg, id, &[0, 1, 2]));
        env.exec(&ixs, &[]).await.unwrap();
    }
    env.advance(DISPUTE).await;

    // One validator alone halts finalization.
    let nonce = env.bridge().await.lock_nonce;
    let lmsg = bridge::lock_message(CHAIN_ID, nonce);
    let lock = [bridge::ed25519_instruction(&lmsg, &env.sigs(&lmsg, &[3])), bridge::lock_ix(&pid)];
    env.exec(&lock, &[]).await.unwrap();
    assert_eq!(env.bridge().await.locked, 1);
    env.fails(&[bridge::finalize_withdrawal_ix(&pid, &mint, &user_ta, 0)], 1012).await;

    // A quorum cancels the suspicious withdrawal and unlocks.
    let cmsg = bridge::cancel_message(CHAIN_ID, 1);
    let weak = [bridge::ed25519_instruction(&cmsg, &env.sigs(&cmsg, &[0, 1])), bridge::cancel_withdrawal_ix(&pid, 1)];
    env.fails(&weak, 1005).await;
    let strong =
        [bridge::ed25519_instruction(&cmsg, &env.sigs(&cmsg, &[0, 1, 2])), bridge::cancel_withdrawal_ix(&pid, 1)];
    env.exec(&strong, &[]).await.unwrap();
    assert_eq!(env.record(1).await.status, WITHDRAWAL_CANCELLED);
    let umsg = bridge::unlock_message(CHAIN_ID, nonce);
    env.fails(&[bridge::ed25519_instruction(&umsg, &env.sigs(&umsg, &[3])), bridge::unlock_ix(&pid)], 1005).await;
    let unlock = [bridge::ed25519_instruction(&umsg, &env.sigs(&umsg, &[0, 1, 3])), bridge::unlock_ix(&pid)];
    env.exec(&unlock, &[]).await.unwrap();
    let b = env.bridge().await;
    assert_eq!((b.locked, b.lock_nonce), (0, 1));

    // The old lock signature can't re-lock once the nonce moved.
    env.fails(&lock, 1005).await;

    let before = env.balance(&user_ta).await;
    env.exec(&[bridge::finalize_withdrawal_ix(&pid, &mint, &user_ta, 0)], &[]).await.unwrap();
    assert_eq!(env.balance(&user_ta).await, before + 1_000_000);
    env.fails(&[bridge::finalize_withdrawal_ix(&pid, &mint, &user_ta, 1)], 1005).await;
}

#[tokio::test]
async fn payout_only_to_the_receipt_destination() {
    let mut env = setup().await;
    let (pid, mint) = (env.pid, env.quote_mint);
    let (user, _) = env.funded_vault(1_000_000).await;
    let thief = Keypair::new();
    let thief_ta = env.token_account(&mint, &thief.pubkey(), 0).await;
    let dest = user.pubkey().to_bytes();
    let msg = bridge::withdrawal_message(CHAIN_ID, 0, ASSET_QUOTE, &dest, 1_000_000);
    let payer = env.ctx.payer.pubkey();
    let mut ixs = vec![bridge::request_withdrawal_ix(&pid, &payer, 0, ASSET_QUOTE, dest, 1_000_000)];
    ixs.extend(env.approve(&msg, 0, &[0, 1, 2]));
    env.exec(&ixs, &[]).await.unwrap();
    env.advance(DISPUTE).await;
    env.fails(&[bridge::finalize_withdrawal_ix(&pid, &mint, &thief_ta, 0)], 1006).await;
}

#[tokio::test]
async fn rotation_needs_old_quorum_and_resets_approvals() {
    let mut env = setup().await;
    let (pid, mint) = (env.pid, env.quote_mint);
    let (user, user_ta) = env.funded_vault(1_000_000).await;
    let dest = user.pubkey().to_bytes();
    let wmsg = bridge::withdrawal_message(CHAIN_ID, 0, ASSET_QUOTE, &dest, 1_000_000);
    let payer = env.ctx.payer.pubkey();
    let mut ixs = vec![bridge::request_withdrawal_ix(&pid, &payer, 0, ASSET_QUOTE, dest, 1_000_000)];
    ixs.extend(env.approve(&wmsg, 0, &[0, 1, 2]));
    env.exec(&ixs, &[]).await.unwrap();

    let new: Vec<SigningKey> = (0..7u8).map(|i| SigningKey::from_bytes(&[80 + i; 32])).collect();
    let new_keys: Vec<[u8; 32]> = new.iter().map(crypto::pubkey).collect();
    let msg = bridge::valset_message(CHAIN_ID, 0, &new_keys);
    let wrong: Vec<_> = new.iter().take(5).map(|k| (crypto::pubkey(k), crypto::sign(k, &msg))).collect();
    env.fails(&[bridge::ed25519_instruction(&msg, &wrong), bridge::rotate_ix(&pid, new_keys.clone())], 1005).await;
    let ok = env.sigs(&msg, &[0, 1, 2]);
    env.exec(&[bridge::ed25519_instruction(&msg, &ok), bridge::rotate_ix(&pid, new_keys.clone())], &[]).await.unwrap();
    let b = env.bridge().await;
    assert_eq!((b.validator_count, b.threshold, b.valset_nonce), (7, 5, 1));
    env.fails(&[bridge::ed25519_instruction(&msg, &ok), bridge::rotate_ix(&pid, new_keys)], 1005).await;

    // Approvals by the old set no longer count.
    env.advance(DISPUTE).await;
    env.fails(&[bridge::finalize_withdrawal_ix(&pid, &mint, &user_ta, 0)], 1005).await;
    // The new quorum (5 of 7) arrives over two transactions.
    for who in [&new[..3], &new[3..5]] {
        let sigs: Vec<_> = who.iter().map(|k| (crypto::pubkey(k), crypto::sign(k, &wmsg))).collect();
        env.exec(&[bridge::ed25519_instruction(&wmsg, &sigs), bridge::approve_withdrawal_ix(&pid, 0)], &[])
            .await
            .unwrap();
    }
    assert_eq!(env.record(0).await.approvals.count_ones(), 5);
    env.advance(DISPUTE).await;
    env.exec(&[bridge::finalize_withdrawal_ix(&pid, &mint, &user_ta, 0)], &[]).await.unwrap();
}
