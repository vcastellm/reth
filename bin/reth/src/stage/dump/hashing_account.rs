use super::setup;
use crate::utils::DbTool;
use eyre::Result;
use reth_db::{database::Database, table::TableImporter, tables, DatabaseEnv};
use reth_primitives::{stage::StageCheckpoint, BlockNumber, ChainSpec};
use reth_provider::ProviderFactory;
use reth_stages::{stages::AccountHashingStage, Stage, UnwindInput};
use std::{path::PathBuf, sync::Arc};
use tracing::info;

pub(crate) async fn dump_hashing_account_stage<DB: Database>(
    db_tool: &DbTool<'_, DB>,
    from: BlockNumber,
    to: BlockNumber,
    output_db: &PathBuf,
    should_run: bool,
) -> Result<()> {
    let (output_db, tip_block_number) = setup(from, to, output_db, db_tool)?;

    // Import relevant AccountChangeSets
    output_db.update(|tx| {
        tx.import_table_with_range::<tables::AccountChangeSet, _>(&db_tool.db.tx()?, Some(from), to)
    })??;

    unwind_and_copy(db_tool, from, tip_block_number, &output_db)?;

    if should_run {
        dry_run(db_tool.chain.clone(), output_db, to, from).await?;
    }

    Ok(())
}

/// Dry-run an unwind to FROM block and copy the necessary table data to the new database.
fn unwind_and_copy<DB: Database>(
    db_tool: &DbTool<'_, DB>,
    from: u64,
    tip_block_number: u64,
    output_db: &DatabaseEnv,
) -> eyre::Result<()> {
    let factory = ProviderFactory::new(db_tool.db, db_tool.chain.clone());
    let provider = factory.provider_rw()?;
    let mut exec_stage = AccountHashingStage::default();

    exec_stage.unwind(
        &provider,
        UnwindInput {
            unwind_to: from,
            checkpoint: StageCheckpoint::new(tip_block_number),
            bad_block: None,
        },
    )?;
    let unwind_inner_tx = provider.into_tx();

    output_db.update(|tx| tx.import_table::<tables::PlainAccountState, _>(&unwind_inner_tx))??;

    Ok(())
}

/// Try to re-execute the stage straightaway
async fn dry_run<DB: Database>(
    chain: Arc<ChainSpec>,
    output_db: DB,
    to: u64,
    from: u64,
) -> eyre::Result<()> {
    info!(target: "reth::cli", "Executing stage.");

    let factory = ProviderFactory::new(&output_db, chain);
    let provider = factory.provider_rw()?;
    let mut stage = AccountHashingStage {
        clean_threshold: 1, // Forces hashing from scratch
        ..Default::default()
    };

    loop {
        let input = reth_stages::ExecInput {
            target: Some(to),
            checkpoint: Some(StageCheckpoint::new(from)),
        };
        if stage.execute(&provider, input)?.done {
            break
        }
    }

    info!(target: "reth::cli", "Success.");

    Ok(())
}
