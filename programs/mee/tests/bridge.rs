//! Solana bridge ↔ MEE chain, end to end: deposit on Solana, attest and
//! credit on the chain, withdraw on the chain, pay out on Solana against
//! validator signatures checked by the Ed25519 precompile.

use ed25519_dalek::SigningKey;
use mee::bridge::{self, ASSET_QUOTE, Bridge, DepositRecord};
use mee::token;
use mee_chain::{self as chain, crypto};
use solana_keypair::Keypair;
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;
use solana_program_test::{ProgramTest, ProgramTestContext, processor};
use solana_signer::Signer;
use solana_transaction::Transaction;

const CHAIN_ID: u64 = 42;

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

    /// A Solana user with SOL for rent (the deposit record is theirs to pay).
    async fn user(&mut self) -> Keypair {
        let user = Keypair::new();
        let payer = self.ctx.payer.pubkey();
        let mut data = 2u32.to_le_bytes().to_vec();
        data.extend_from_slice(&1_000_000_000u64.to_le_bytes());
        let ix = Instruction {
            program_id: token::SYSTEM_PROGRAM_ID,
            accounts: vec![
                solana_program::instruction::AccountMeta::new(payer, true),
                solana_program::instruction::AccountMeta::new(user.pubkey(), false),
            ],
            data,
        };
        self.exec(&[ix], &[]).await.unwrap();
        user
    }

    async fn balance(&mut self, acct: &Pubkey) -> u64 {
        let a = self.ctx.banks_client.get_account(*acct).await.unwrap().unwrap();
        u64::from_le_bytes(a.data[64..72].try_into().unwrap())
    }

    async fn bridge(&mut self) -> Bridge {
        let a = self.ctx.banks_client.get_account(bridge::bridge_pda(&self.pid).0).await.unwrap().unwrap();
        *bytemuck::from_bytes(&a.data)
    }

    async fn deposit_record(&mut self, id: u64) -> DepositRecord {
        let a = self.ctx.banks_client.get_account(bridge::deposit_pda(&self.pid, id).0).await.unwrap().unwrap();
        *bytemuck::from_bytes(&a.data)
    }

    fn sigs(&self, msg: &[u8; 32], who: &[usize]) -> Vec<([u8; 32], [u8; 64])> {
        who.iter().map(|&i| (crypto::pubkey(&self.validators[i]), crypto::sign(&self.validators[i], msg))).collect()
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
    let ix = bridge::init_bridge_ix(&pid, &payer, &mints[0], &mints[1], CHAIN_ID, vals);
    env.exec(&[ix], &[]).await.unwrap();
    env
}

#[tokio::test]
async fn deposit_attest_trade_withdraw_roundtrip() {
    let mut env = setup().await;
    let (pid, mint) = (env.pid, env.quote_mint);
    let b = env.bridge().await;
    assert_eq!((b.validator_count, b.threshold, b.chain_id), (4, 3, CHAIN_ID));

    // --- deposit on Solana ---
    let user = env.user().await;
    let user_ta = env.token_account(&mint, &user.pubkey(), 5_000_000).await;
    let ix = bridge::deposit_ix(&pid, &user.pubkey(), &user_ta, &mint, 0, ASSET_QUOTE, 2_000_000);
    env.exec(&[ix], &[&user]).await.unwrap();
    let rec = env.deposit_record(0).await;
    assert_eq!((rec.id, rec.owner, rec.amount, rec.asset), (0, user.pubkey().to_bytes(), 2_000_000, ASSET_QUOTE));
    assert_eq!(env.bridge().await.next_deposit_id, 1);
    let vault = bridge::vault_pda(&pid, &mint).0;
    assert_eq!(env.balance(&vault).await, 2_000_000);

    // --- validators attest the record; the chain credits it ---
    let mut c = chain::Chain::new(chain::Genesis {
        chain_id: CHAIN_ID,
        params: mee_core::EngineParams::default(),
        validators: env.validators.iter().map(crypto::pubkey).collect(),
        timestamp_ms: 0,
        launch: Default::default(),
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
    let receipt = out.withdrawals[0];

    // --- validators sign the receipt; Solana pays out ---
    let rmsg = receipt.message(CHAIN_ID);
    assert_eq!(
        rmsg,
        bridge::withdrawal_message(CHAIN_ID, receipt.id, receipt.asset, &receipt.destination, receipt.amount),
        "chain and Solana program build the same message"
    );
    let payer = env.ctx.payer.pubkey();
    let withdraw = move |sigs: &[([u8; 32], [u8; 64])]| {
        vec![
            bridge::ed25519_instruction(&rmsg, sigs),
            bridge::withdraw_ix(
                &pid,
                &payer,
                &mint,
                &user_ta,
                receipt.id,
                receipt.asset,
                receipt.destination,
                receipt.amount,
            ),
        ]
    };

    // Two of four is not a quorum.
    let sigs = env.sigs(&rmsg, &[0, 2]);
    let few = withdraw(&sigs);
    let err = env.exec(&few, &[]).await.unwrap_err();
    assert!(err.contains("Custom(1005)"), "{err}");
    // A non-validator signature does not count.
    let outsider = SigningKey::from_bytes(&[99; 32]);
    let mut mixed = env.sigs(&rmsg, &[0, 2]);
    mixed.push((crypto::pubkey(&outsider), crypto::sign(&outsider, &rmsg)));
    let err = env.exec(&withdraw(&mixed), &[]).await.unwrap_err();
    assert!(err.contains("Custom(1005)"), "{err}");
    // A forged signature is rejected by the Ed25519 precompile itself.
    let mut forged = env.sigs(&rmsg, &[0, 1, 2]);
    forged[1].1[0] ^= 1;
    let err = env.exec(&withdraw(&forged), &[]).await.unwrap_err();
    // Instruction 0 is the precompile; its error 2 is InvalidSignature.
    assert!(err.contains("InstructionError(0, Custom(2))"), "rejected by the precompile: {err}");

    let before = env.balance(&user_ta).await;
    let sigs = env.sigs(&rmsg, &[3, 0, 2]);
    env.exec(&withdraw(&sigs), &[]).await.unwrap();
    assert_eq!(env.balance(&user_ta).await, before + 1_500_000);
    assert_eq!(env.balance(&vault).await, 500_000);

    // Replaying the same receipt fails (its record already exists).
    let sigs = env.sigs(&rmsg, &[0, 1, 2]);
    let err = env.exec(&withdraw(&sigs), &[]).await.unwrap_err();
    assert!(err.contains("Custom(1004)"), "{err}");
}

#[tokio::test]
async fn payout_only_to_the_receipt_destination() {
    let mut env = setup().await;
    let (pid, mint) = (env.pid, env.quote_mint);
    let user = env.user().await;
    let user_ta = env.token_account(&mint, &user.pubkey(), 1_000_000).await;
    let ix = bridge::deposit_ix(&pid, &user.pubkey(), &user_ta, &mint, 0, ASSET_QUOTE, 1_000_000);
    env.exec(&[ix], &[&user]).await.unwrap();

    let thief = Keypair::new();
    let thief_ta = env.token_account(&mint, &thief.pubkey(), 0).await;
    let dest = user.pubkey().to_bytes();
    let msg = bridge::withdrawal_message(CHAIN_ID, 0, ASSET_QUOTE, &dest, 1_000_000);
    let ixs = vec![
        bridge::ed25519_instruction(&msg, &env.sigs(&msg, &[0, 1, 2])),
        bridge::withdraw_ix(&pid, &env.ctx.payer.pubkey(), &mint, &thief_ta, 0, ASSET_QUOTE, dest, 1_000_000),
    ];
    let err = env.exec(&ixs, &[]).await.unwrap_err();
    assert!(err.contains("Custom(1006)"), "{err}");
}

#[tokio::test]
async fn validator_rotation_needs_old_quorum() {
    let mut env = setup().await;
    let pid = env.pid;
    let new: Vec<SigningKey> = (0..7u8).map(|i| SigningKey::from_bytes(&[80 + i; 32])).collect();
    let new_keys: Vec<[u8; 32]> = new.iter().map(crypto::pubkey).collect();
    let msg = bridge::valset_message(CHAIN_ID, 0, &new_keys);

    // Signed by the incoming set only: rejected.
    let wrong: Vec<_> = new.iter().take(5).map(|k| (crypto::pubkey(k), crypto::sign(k, &msg))).collect();
    let err = env
        .exec(&[bridge::ed25519_instruction(&msg, &wrong), bridge::rotate_ix(&pid, new_keys.clone())], &[])
        .await
        .unwrap_err();
    assert!(err.contains("Custom(1005)"), "{err}");

    let ok = env.sigs(&msg, &[0, 1, 2]);
    env.exec(&[bridge::ed25519_instruction(&msg, &ok), bridge::rotate_ix(&pid, new_keys.clone())], &[]).await.unwrap();
    let b = env.bridge().await;
    assert_eq!((b.validator_count, b.threshold, b.valset_nonce), (7, 5, 1));

    // The old signatures can't be replayed against the new nonce.
    let err =
        env.exec(&[bridge::ed25519_instruction(&msg, &ok), bridge::rotate_ix(&pid, new_keys)], &[]).await.unwrap_err();
    assert!(err.contains("Custom(1005)"), "{err}");
}
