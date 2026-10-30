//! Shared test helper: runs a transaction and, with `MEE_CU_REPORT` set,
//! prints the compute units it consumed, labelled with the MEE
//! instructions in it (`MEE_CU<TAB>label<TAB>units`). Run with
//! `SBF_OUT_DIR` pointing at the compiled program for real SBF numbers.

use borsh::BorshDeserialize;
use mee::instruction::MeeInstruction;
use solana_program_test::BanksClient;
use solana_transaction::Transaction;

fn label(tx: &Transaction) -> Option<String> {
    let keys = &tx.message.account_keys;
    let names: Vec<String> = tx
        .message
        .instructions
        .iter()
        .filter(|ix| keys[ix.program_id_index as usize] == mee::id())
        .filter_map(|ix| MeeInstruction::try_from_slice(&ix.data).ok())
        .map(|i| format!("{i:?}").chars().take_while(|c| c.is_alphanumeric()).collect())
        .collect();
    (!names.is_empty()).then(|| names.join("+"))
}

pub async fn process(banks: &BanksClient, tx: Transaction) -> Result<(), String> {
    let name = label(&tx);
    let r = banks.process_transaction_with_metadata(tx).await.map_err(|e| format!("{e:?}"))?;
    if std::env::var_os("MEE_CU_REPORT").is_some()
        && r.result.is_ok()
        && let (Some(name), Some(m)) = (name, &r.metadata)
    {
        eprintln!("MEE_CU\t{name}\t{}", m.compute_units_consumed);
    }
    r.result.map_err(|e| format!("{e:?}"))
}
