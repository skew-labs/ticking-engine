//! The relayer's planned transactions run against the real bridge program:
//! a withdrawal made on the chain is requested, approved with validator
//! signatures, and (after the dispute window) paid into the recipient's
//! associated token account, which the relayer creates.

use ed25519_dalek::SigningKey;
use mee::bridge::{self, ASSET_QUOTE, Bridge, WithdrawalRecord};
use mee::solana_program::clock::Clock;
use mee::solana_program::instruction::{AccountMeta, Instruction};
use mee::solana_program::pubkey::Pubkey;
use mee::token;
use mee_chain::{self as chain, crypto};
use mee_net::relayer::{self, Destination, OnSolana, Receipt};
use solana_keypair::Keypair;
use solana_program_test::{ProgramTest, ProgramTestContext, processor};
use solana_signer::Signer;
use solana_transaction::Transaction;

const CHAIN_ID: u64 = 42;
const DISPUTE: i64 = 200;

async fn exec(ctx: &mut ProgramTestContext, ixs: &[Instruction], extra: &[&Keypair]) -> Result<(), String> {
    let blockhash = ctx.get_new_latest_blockhash().await.unwrap();
    let mut signers: Vec<&Keypair> = vec![&ctx.payer];
    signers.extend_from_slice(extra);
    let tx = Transaction::new_signed_with_payer(ixs, Some(&ctx.payer.pubkey()), &signers, blockhash);
    // The relayer's own encoding of what it would send over RPC.
    let wire = relayer::wire_bytes(&tx);
    assert_eq!(wire[0] as usize, tx.signatures.len());
    assert!(wire.len() <= 1_232, "transaction too large: {} bytes", wire.len());
    ctx.banks_client.process_transaction(tx).await.map_err(|e| format!("{e:?}"))
}

async fn get<T: bytemuck::Pod>(ctx: &mut ProgramTestContext, key: &Pubkey) -> Option<T> {
    ctx.banks_client.get_account(*key).await.unwrap().map(|a| *bytemuck::from_bytes(&a.data))
}

async fn now(ctx: &mut ProgramTestContext) -> i64 {
    ctx.banks_client.get_sysvar::<Clock>().await.unwrap().unix_timestamp
}

#[tokio::test]
async fn relayer_carries_a_chain_withdrawal_to_a_solana_payout() {
    let pid = mee::id();
    let mut pt = ProgramTest::new("mee", pid, processor!(mee::processor::process_instruction));
    // With SBF_OUT_DIR set (holding mee.so from `cargo build-sbf`), run the
    // compiled SBF program instead of the native build.
    pt.prefer_bpf(std::env::var_os("SBF_OUT_DIR").is_some());
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.pubkey();
    // Payer is the upgrade authority (who alone may initialize).
    let pd = solana_account::Account {
        lamports: 1_000_000_000,
        data: mee::pda::program_data_header(&payer),
        owner: mee::pda::UPGRADEABLE_LOADER_ID,
        executable: false,
        rent_epoch: 0,
    };
    ctx.set_account(&mee::pda::program_data(&pid), &pd.into());

    // Mints, bridge with four validators, a funded vault.
    let rent = ctx.banks_client.get_rent().await.unwrap();
    let mut mints = Vec::new();
    for _ in 0..2 {
        let m = Keypair::new();
        let ixs = [
            token::create_account_ix(
                &payer,
                &m.pubkey(),
                rent.minimum_balance(token::MINT_LEN),
                token::MINT_LEN as u64,
                &token::TOKEN_PROGRAM_ID,
            ),
            token::initialize_mint2_ix(&m.pubkey(), &payer, 6),
        ];
        exec(&mut ctx, &ixs, &[&m]).await.unwrap();
        mints.push(m.pubkey());
    }
    let vals: Vec<SigningKey> = (0..4u8).map(|i| SigningKey::from_bytes(&[70 + i; 32])).collect();
    let set = vals.iter().map(|k| (crypto::pubkey(k), 1)).collect();
    exec(&mut ctx, &[bridge::init_bridge_ix(&pid, &payer, &mints[0], &mints[1], CHAIN_ID, set, DISPUTE)], &[])
        .await
        .unwrap();
    let funder = Keypair::new();
    let ta = Keypair::new();
    let ixs = [
        token::create_account_ix(
            &payer,
            &ta.pubkey(),
            rent.minimum_balance(token::TOKEN_ACCOUNT_LEN),
            token::TOKEN_ACCOUNT_LEN as u64,
            &token::TOKEN_PROGRAM_ID,
        ),
        token::initialize_account3_ix(&ta.pubkey(), &mints[0], &funder.pubkey()),
        token::mint_to_ix(&mints[0], &ta.pubkey(), &payer, 5_000_000),
        Instruction {
            program_id: token::SYSTEM_PROGRAM_ID,
            accounts: vec![AccountMeta::new(payer, true), AccountMeta::new(funder.pubkey(), false)],
            data: [2u32.to_le_bytes().as_slice(), &100_000_000u64.to_le_bytes()].concat(),
        },
    ];
    exec(&mut ctx, &ixs, &[&ta]).await.unwrap();
    let dep = bridge::deposit_ix(&pid, &funder.pubkey(), &ta.pubkey(), &mints[0], 0, ASSET_QUOTE, 5_000_000);
    exec(&mut ctx, &[dep], &[&funder]).await.unwrap();

    // On the chain: the deposit is credited and a user withdraws to a fresh
    // Solana wallet that has no token account yet.
    let mut c = chain::Chain::new(chain::Genesis {
        chain_id: CHAIN_ID,
        validators: vals.iter().map(crypto::pubkey).collect(),
        bridge_program: pid.to_bytes(),
        ..Default::default()
    });
    let user = SigningKey::from_bytes(&[5; 32]);
    let owner = crypto::pubkey(&user);
    let msg = crypto::deposit_message(CHAIN_ID, &pid.to_bytes(), 0, &owner, ASSET_QUOTE, 2_000_000);
    let attestations =
        (0..3).map(|i| chain::Attestation { validator: i, signature: crypto::sign(&vals[i as usize], &msg) }).collect();
    let recipient = Pubkey::new_unique();
    let txs = vec![
        chain::Tx::System(chain::SystemTx::BridgeDeposit {
            deposit_id: 0,
            owner,
            asset: ASSET_QUOTE,
            amount: 2_000_000,
            attestations,
        }),
        chain::Tx::User(chain::SignedTx::sign(
            &user,
            CHAIN_ID,
            1,
            chain::Action::Withdraw { asset: ASSET_QUOTE, amount: 1_500_000, destination: recipient.to_bytes() },
        )),
    ];
    let out = c.apply_block(&chain::Block { height: 1, timestamp_ms: 1_000, txs }).unwrap();
    assert!(out.results.iter().all(|r| r.is_ok()), "{:?}", out.results);
    let w = out.withdrawals[0];
    let r = Receipt { id: w.id, asset: w.asset, destination: w.destination, amount: w.amount };
    // What each validator's API would hand out.
    let wmsg = w.message(CHAIN_ID, &pid.to_bytes());
    let sigs: Vec<_> = vals.iter().map(|k| (crypto::pubkey(k), crypto::sign(k, &wmsg))).collect();

    // Round 1: request + approvals in one transaction.
    let b: Bridge = get(&mut ctx, &bridge::bridge_pda(&pid).0).await.unwrap();
    let record_key = bridge::withdrawal_pda(&pid, &wmsg).0;
    let t = now(&mut ctx).await;
    let txs = relayer::plan(
        &pid,
        &payer,
        &r,
        &OnSolana { bridge: &b, record: None, now: t },
        &sigs,
        &Destination::Associated,
    );
    assert_eq!(txs.len(), 1);
    for ixs in &txs {
        exec(&mut ctx, ixs, &[]).await.unwrap();
    }
    let rec: WithdrawalRecord = get(&mut ctx, &record_key).await.unwrap();
    assert_eq!(rec.approvals.count_ones(), 4);

    // Inside the dispute window there is nothing to do.
    let t = now(&mut ctx).await;
    let on = OnSolana { bridge: &b, record: Some(&rec), now: t };
    assert!(relayer::plan(&pid, &payer, &r, &on, &sigs, &Destination::Associated).is_empty());

    // After it: create the recipient's token account and pay.
    let mut clock: Clock = ctx.banks_client.get_sysvar().await.unwrap();
    clock.unix_timestamp += DISPUTE;
    ctx.set_sysvar(&clock);
    let t = now(&mut ctx).await;
    let on = OnSolana { bridge: &b, record: Some(&rec), now: t };
    let txs = relayer::plan(&pid, &payer, &r, &on, &sigs, &Destination::Associated);
    assert_eq!(txs.len(), 1);
    exec(&mut ctx, &txs[0], &[]).await.unwrap();
    let ata = relayer::associated_token_account(&recipient, &mints[0]);
    let acct = ctx.banks_client.get_account(ata).await.unwrap().expect("token account created");
    assert_eq!(u64::from_le_bytes(acct.data[64..72].try_into().unwrap()), 1_500_000);
    let rec: WithdrawalRecord = get(&mut ctx, &record_key).await.unwrap();
    assert_eq!(rec.status, bridge::WITHDRAWAL_PAID);
    let on = OnSolana { bridge: &b, record: Some(&rec), now: t };
    assert!(relayer::plan(&pid, &payer, &r, &on, &sigs, &Destination::Associated).is_empty(), "done");
}

#[test]
fn approvals_are_chunked_and_skip_signers_already_counted() {
    let program = mee::id();
    let mut b: Bridge = bytemuck::Zeroable::zeroed();
    b.chain_id = CHAIN_ID;
    b.validator_count = 12;
    b.total_weight = 12;
    let keys: Vec<SigningKey> = (0..12u8).map(|i| SigningKey::from_bytes(&[100 + i; 32])).collect();
    for (i, k) in keys.iter().enumerate() {
        b.validators[i] = crypto::pubkey(k);
        b.weights[i] = 1;
    }
    let r = Receipt { id: 3, asset: ASSET_QUOTE, destination: [9; 32], amount: 10 };
    let sigs: Vec<_> = keys.iter().map(|k| (crypto::pubkey(k), [0u8; 64])).collect();
    let payer = Pubkey::new_unique();
    let txs = relayer::plan(
        &program,
        &payer,
        &r,
        &OnSolana { bridge: &b, record: None, now: 0 },
        &sigs,
        &Destination::Associated,
    );
    // Request + 5 approvals, then 5, then 2.
    assert_eq!(txs.iter().map(|t| t.len()).collect::<Vec<_>>(), vec![3, 2, 2]);

    let mut rec: WithdrawalRecord = bytemuck::Zeroable::zeroed();
    rec.status = bridge::WITHDRAWAL_PENDING;
    rec.approvals = 0b1111_1111; // eight already approved
    let on = OnSolana { bridge: &b, record: Some(&rec), now: 0 };
    let txs = relayer::plan(&program, &payer, &r, &on, &sigs, &Destination::Associated);
    assert_eq!(txs.len(), 1, "only the four missing signatures, one transaction");
    assert_eq!(txs[0][0].data[0], 4);
}
