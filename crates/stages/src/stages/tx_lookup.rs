use crate::{ExecInput, ExecOutput, Stage, StageError, UnwindInput, UnwindOutput};
use rayon::prelude::*;
use reth_db::{
    cursor::{DbCursorRO, DbCursorRW},
    database::Database,
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_interfaces::provider::ProviderError;
use reth_primitives::{
    stage::{EntitiesCheckpoint, StageCheckpoint, StageId},
    PruneCheckpoint, PruneMode, PruneSegment,
};
use reth_provider::{
    BlockReader, DatabaseProviderRW, PruneCheckpointReader, PruneCheckpointWriter,
    TransactionsProviderExt,
};
use tracing::*;

/// The transaction lookup stage.
///
/// This stage walks over the bodies table, and sets the transaction hash of each transaction in a
/// block to the corresponding `BlockNumber` at each block. This is written to the
/// [`tables::TxHashNumber`] This is used for looking up changesets via the transaction hash.
#[derive(Debug, Clone)]
pub struct TransactionLookupStage {
    /// The number of lookup entries to commit at once
    commit_threshold: u64,
    prune_mode: Option<PruneMode>,
}

impl Default for TransactionLookupStage {
    fn default() -> Self {
        Self { commit_threshold: 5_000_000, prune_mode: None }
    }
}

impl TransactionLookupStage {
    /// Create new instance of [TransactionLookupStage].
    pub fn new(commit_threshold: u64, prune_mode: Option<PruneMode>) -> Self {
        Self { commit_threshold, prune_mode }
    }
}

#[async_trait::async_trait]
impl<DB: Database> Stage<DB> for TransactionLookupStage {
    /// Return the id of the stage
    fn id(&self) -> StageId {
        StageId::TransactionLookup
    }

    /// Write transaction hash -> id entries
    async fn execute(
        &mut self,
        provider: &DatabaseProviderRW<'_, &DB>,
        mut input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        if let Some((target_prunable_block, prune_mode)) = self
            .prune_mode
            .map(|mode| mode.prune_target_block(input.target(), PruneSegment::TransactionLookup))
            .transpose()?
            .flatten()
        {
            if target_prunable_block > input.checkpoint().block_number {
                input.checkpoint = Some(StageCheckpoint::new(target_prunable_block));

                // Save prune checkpoint only if we don't have one already.
                // Otherwise, pruner may skip the unpruned range of blocks.
                if provider.get_prune_checkpoint(PruneSegment::TransactionLookup)?.is_none() {
                    let target_prunable_tx_number = provider
                        .block_body_indices(target_prunable_block)?
                        .ok_or(ProviderError::BlockBodyIndicesNotFound(target_prunable_block))?
                        .last_tx_num();

                    provider.save_prune_checkpoint(
                        PruneSegment::TransactionLookup,
                        PruneCheckpoint {
                            block_number: Some(target_prunable_block),
                            tx_number: Some(target_prunable_tx_number),
                            prune_mode,
                        },
                    )?;
                }
            }
        }
        if input.target_reached() {
            return Ok(ExecOutput::done(input.checkpoint()))
        }

        let (tx_range, block_range, is_final_range) =
            input.next_block_range_with_transaction_threshold(provider, self.commit_threshold)?;
        let end_block = *block_range.end();

        debug!(target: "sync::stages::transaction_lookup", ?tx_range, "Updating transaction lookup");

        let mut tx_list = provider.transaction_hashes_by_range(tx_range)?;

        // Sort before inserting the reverse lookup for hash -> tx_id.
        tx_list.par_sort_unstable_by(|txa, txb| txa.0.cmp(&txb.0));

        let tx = provider.tx_ref();
        let mut txhash_cursor = tx.cursor_write::<tables::TxHashNumber>()?;

        // If the last inserted element in the database is equal or bigger than the first
        // in our set, then we need to insert inside the DB. If it is smaller then last
        // element in the DB, we can append to the DB.
        // Append probably only ever happens during sync, on the first table insertion.
        let insert = tx_list
            .first()
            .zip(txhash_cursor.last()?)
            .map(|((first, _), (last, _))| first <= &last)
            .unwrap_or_default();
        // if txhash_cursor.last() is None we will do insert. `zip` would return none if any item is
        // none. if it is some and if first is smaller than last, we will do append.
        for (tx_hash, id) in tx_list {
            if insert {
                txhash_cursor.insert(tx_hash, id)?;
            } else {
                txhash_cursor.append(tx_hash, id)?;
            }
        }

        Ok(ExecOutput {
            checkpoint: StageCheckpoint::new(end_block)
                .with_entities_stage_checkpoint(stage_checkpoint(provider)?),
            done: is_final_range,
        })
    }

    /// Unwind the stage.
    async fn unwind(
        &mut self,
        provider: &DatabaseProviderRW<'_, &DB>,
        input: UnwindInput,
    ) -> Result<UnwindOutput, StageError> {
        let tx = provider.tx_ref();
        let (range, unwind_to, _) = input.unwind_block_range_with_threshold(self.commit_threshold);

        // Cursors to unwind tx hash to number
        let mut body_cursor = tx.cursor_read::<tables::BlockBodyIndices>()?;
        let mut tx_hash_number_cursor = tx.cursor_write::<tables::TxHashNumber>()?;
        let mut transaction_cursor = tx.cursor_read::<tables::Transactions>()?;
        let mut rev_walker = body_cursor.walk_back(Some(*range.end()))?;
        while let Some((number, body)) = rev_walker.next().transpose()? {
            if number <= unwind_to {
                break
            }

            // Delete all transactions that belong to this block
            for tx_id in body.tx_num_range() {
                // First delete the transaction and hash to id mapping
                if let Some((_, transaction)) = transaction_cursor.seek_exact(tx_id)? {
                    if tx_hash_number_cursor.seek_exact(transaction.hash())?.is_some() {
                        tx_hash_number_cursor.delete_current()?;
                    }
                }
            }
        }

        Ok(UnwindOutput {
            checkpoint: StageCheckpoint::new(unwind_to)
                .with_entities_stage_checkpoint(stage_checkpoint(provider)?),
        })
    }
}

fn stage_checkpoint<DB: Database>(
    provider: &DatabaseProviderRW<'_, &DB>,
) -> Result<EntitiesCheckpoint, StageError> {
    let pruned_entries = provider
        .get_prune_checkpoint(PruneSegment::TransactionLookup)?
        .and_then(|checkpoint| checkpoint.tx_number)
        // `+1` is needed because `TxNumber` is 0-indexed
        .map(|tx_number| tx_number + 1)
        .unwrap_or_default();
    Ok(EntitiesCheckpoint {
        // If `TxHashNumber` table was pruned, we will have a number of entries in it not matching
        // the actual number of processed transactions. To fix that, we add the number of pruned
        // `TxHashNumber` entries.
        processed: provider.tx_ref().entries::<tables::TxHashNumber>()? as u64 + pruned_entries,
        total: provider.tx_ref().entries::<tables::Transactions>()? as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{
        stage_test_suite_ext, ExecuteStageTestRunner, StageTestRunner, TestRunnerError,
        TestTransaction, UnwindStageTestRunner,
    };
    use assert_matches::assert_matches;
    use reth_interfaces::test_utils::{
        generators,
        generators::{random_block, random_block_range},
    };
    use reth_primitives::{
        stage::StageUnitCheckpoint, BlockNumber, PruneCheckpoint, PruneMode, SealedBlock, B256,
        MAINNET,
    };
    use reth_provider::{
        BlockReader, ProviderError, ProviderFactory, PruneCheckpointWriter, TransactionsProvider,
    };
    use std::ops::Sub;

    // Implement stage test suite.
    stage_test_suite_ext!(TransactionLookupTestRunner, transaction_lookup);

    #[tokio::test]
    async fn execute_single_transaction_lookup() {
        let (previous_stage, stage_progress) = (500, 100);
        let mut rng = generators::rng();

        // Set up the runner
        let runner = TransactionLookupTestRunner::default();
        let input = ExecInput {
            target: Some(previous_stage),
            checkpoint: Some(StageCheckpoint::new(stage_progress)),
        };

        // Insert blocks with a single transaction at block `stage_progress + 10`
        let non_empty_block_number = stage_progress + 10;
        let blocks = (stage_progress..=input.target())
            .map(|number| {
                random_block(
                    &mut rng,
                    number,
                    None,
                    Some((number == non_empty_block_number) as u8),
                    None,
                )
            })
            .collect::<Vec<_>>();
        runner.tx.insert_blocks(blocks.iter(), None).expect("failed to insert blocks");

        let rx = runner.execute(input);

        // Assert the successful result
        let result = rx.await.unwrap();
        assert_matches!(
            result,
            Ok(ExecOutput {
                checkpoint: StageCheckpoint {
                block_number,
                stage_checkpoint: Some(StageUnitCheckpoint::Entities(EntitiesCheckpoint {
                    processed,
                    total
                }))
            }, done: true }) if block_number == previous_stage && processed == total &&
                total == runner.tx.table::<tables::Transactions>().unwrap().len() as u64
        );

        // Validate the stage execution
        assert!(runner.validate_execution(input, result.ok()).is_ok(), "execution validation");
    }

    /// Execute the stage twice with input range that exceeds the commit threshold
    #[tokio::test]
    async fn execute_intermediate_commit_transaction_lookup() {
        let threshold = 50;
        let mut runner = TransactionLookupTestRunner::default();
        runner.set_commit_threshold(threshold);
        let (stage_progress, previous_stage) = (1000, 1100); // input exceeds threshold
        let first_input = ExecInput {
            target: Some(previous_stage),
            checkpoint: Some(StageCheckpoint::new(stage_progress)),
        };
        let mut rng = generators::rng();

        // Seed only once with full input range
        let seed =
            random_block_range(&mut rng, stage_progress + 1..=previous_stage, B256::ZERO, 0..4); // set tx count range high enough to hit the threshold
        runner.tx.insert_blocks(seed.iter(), None).expect("failed to seed execution");

        let total_txs = runner.tx.table::<tables::Transactions>().unwrap().len() as u64;

        // Execute first time
        let result = runner.execute(first_input).await.unwrap();
        let mut tx_count = 0;
        let expected_progress = seed
            .iter()
            .find(|x| {
                tx_count += x.body.len();
                tx_count as u64 > threshold
            })
            .map(|x| x.number)
            .unwrap_or(previous_stage);
        assert_matches!(result, Ok(_));
        assert_eq!(
            result.unwrap(),
            ExecOutput {
                checkpoint: StageCheckpoint::new(expected_progress).with_entities_stage_checkpoint(
                    EntitiesCheckpoint {
                        processed: runner.tx.table::<tables::TxHashNumber>().unwrap().len() as u64,
                        total: total_txs
                    }
                ),
                done: false
            }
        );

        // Execute second time to completion
        runner.set_commit_threshold(u64::MAX);
        let second_input = ExecInput {
            target: Some(previous_stage),
            checkpoint: Some(StageCheckpoint::new(expected_progress)),
        };
        let result = runner.execute(second_input).await.unwrap();
        assert_matches!(result, Ok(_));
        assert_eq!(
            result.as_ref().unwrap(),
            &ExecOutput {
                checkpoint: StageCheckpoint::new(previous_stage).with_entities_stage_checkpoint(
                    EntitiesCheckpoint { processed: total_txs, total: total_txs }
                ),
                done: true
            }
        );

        assert!(runner.validate_execution(first_input, result.ok()).is_ok(), "validation failed");
    }

    #[tokio::test]
    async fn execute_pruned_transaction_lookup() {
        let (previous_stage, prune_target, stage_progress) = (500, 400, 100);
        let mut rng = generators::rng();

        // Set up the runner
        let mut runner = TransactionLookupTestRunner::default();
        let input = ExecInput {
            target: Some(previous_stage),
            checkpoint: Some(StageCheckpoint::new(stage_progress)),
        };

        // Seed only once with full input range
        let seed =
            random_block_range(&mut rng, stage_progress + 1..=previous_stage, B256::ZERO, 0..2);
        runner.tx.insert_blocks(seed.iter(), None).expect("failed to seed execution");

        runner.set_prune_mode(PruneMode::Before(prune_target));

        let rx = runner.execute(input);

        // Assert the successful result
        let result = rx.await.unwrap();
        assert_matches!(
            result,
            Ok(ExecOutput {
                checkpoint: StageCheckpoint {
                block_number,
                stage_checkpoint: Some(StageUnitCheckpoint::Entities(EntitiesCheckpoint {
                    processed,
                    total
                }))
            }, done: true }) if block_number == previous_stage && processed == total &&
                total == runner.tx.table::<tables::Transactions>().unwrap().len() as u64
        );

        // Validate the stage execution
        assert!(runner.validate_execution(input, result.ok()).is_ok(), "execution validation");
    }

    #[test]
    fn stage_checkpoint_pruned() {
        let tx = TestTransaction::default();
        let mut rng = generators::rng();

        let blocks = random_block_range(&mut rng, 0..=100, B256::ZERO, 0..10);
        tx.insert_blocks(blocks.iter(), None).expect("insert blocks");

        let max_pruned_block = 30;
        let max_processed_block = 70;

        let mut tx_hash_numbers = Vec::new();
        let mut tx_hash_number = 0;
        for block in &blocks[..=max_processed_block] {
            for transaction in &block.body {
                if block.number > max_pruned_block {
                    tx_hash_numbers.push((transaction.hash, tx_hash_number));
                }
                tx_hash_number += 1;
            }
        }
        tx.insert_tx_hash_numbers(tx_hash_numbers).expect("insert tx hash numbers");

        let provider = tx.inner_rw();
        provider
            .save_prune_checkpoint(
                PruneSegment::TransactionLookup,
                PruneCheckpoint {
                    block_number: Some(max_pruned_block),
                    tx_number: Some(
                        blocks[..=max_pruned_block as usize]
                            .iter()
                            .map(|block| block.body.len() as u64)
                            .sum::<u64>()
                            .sub(1), // `TxNumber` is 0-indexed
                    ),
                    prune_mode: PruneMode::Full,
                },
            )
            .expect("save stage checkpoint");
        provider.commit().expect("commit");

        let db = tx.inner_raw();
        let factory = ProviderFactory::new(db.as_ref(), MAINNET.clone());
        let provider = factory.provider_rw().expect("provider rw");

        assert_eq!(
            stage_checkpoint(&provider).expect("stage checkpoint"),
            EntitiesCheckpoint {
                processed: blocks[..=max_processed_block]
                    .iter()
                    .map(|block| block.body.len() as u64)
                    .sum::<u64>(),
                total: blocks.iter().map(|block| block.body.len() as u64).sum::<u64>()
            }
        );
    }

    struct TransactionLookupTestRunner {
        tx: TestTransaction,
        commit_threshold: u64,
        prune_mode: Option<PruneMode>,
    }

    impl Default for TransactionLookupTestRunner {
        fn default() -> Self {
            Self { tx: TestTransaction::default(), commit_threshold: 1000, prune_mode: None }
        }
    }

    impl TransactionLookupTestRunner {
        fn set_commit_threshold(&mut self, threshold: u64) {
            self.commit_threshold = threshold;
        }

        fn set_prune_mode(&mut self, prune_mode: PruneMode) {
            self.prune_mode = Some(prune_mode);
        }

        /// # Panics
        ///
        /// 1. If there are any entries in the [tables::TxHashNumber] table above a given block
        ///    number.
        ///
        /// 2. If the is no requested block entry in the bodies table, but [tables::TxHashNumber] is
        ///    not empty.
        fn ensure_no_hash_by_block(&self, number: BlockNumber) -> Result<(), TestRunnerError> {
            let body_result = self
                .tx
                .inner_rw()
                .block_body_indices(number)?
                .ok_or(ProviderError::BlockBodyIndicesNotFound(number));
            match body_result {
                Ok(body) => self.tx.ensure_no_entry_above_by_value::<tables::TxHashNumber, _>(
                    body.last_tx_num(),
                    |key| key,
                )?,
                Err(_) => {
                    assert!(self.tx.table_is_empty::<tables::TxHashNumber>()?);
                }
            };

            Ok(())
        }
    }

    impl StageTestRunner for TransactionLookupTestRunner {
        type S = TransactionLookupStage;

        fn tx(&self) -> &TestTransaction {
            &self.tx
        }

        fn stage(&self) -> Self::S {
            TransactionLookupStage {
                commit_threshold: self.commit_threshold,
                prune_mode: self.prune_mode,
            }
        }
    }

    impl ExecuteStageTestRunner for TransactionLookupTestRunner {
        type Seed = Vec<SealedBlock>;

        fn seed_execution(&mut self, input: ExecInput) -> Result<Self::Seed, TestRunnerError> {
            let stage_progress = input.checkpoint().block_number;
            let end = input.target();
            let mut rng = generators::rng();

            let blocks = random_block_range(&mut rng, stage_progress + 1..=end, B256::ZERO, 0..2);
            self.tx.insert_blocks(blocks.iter(), None)?;
            Ok(blocks)
        }

        fn validate_execution(
            &self,
            mut input: ExecInput,
            output: Option<ExecOutput>,
        ) -> Result<(), TestRunnerError> {
            match output {
                Some(output) => {
                    let provider = self.tx.inner();

                    if let Some((target_prunable_block, _)) = self
                        .prune_mode
                        .map(|mode| {
                            mode.prune_target_block(input.target(), PruneSegment::TransactionLookup)
                        })
                        .transpose()
                        .expect("prune target block for transaction lookup")
                        .flatten()
                    {
                        if target_prunable_block > input.checkpoint().block_number {
                            input.checkpoint = Some(StageCheckpoint::new(target_prunable_block));
                        }
                    }
                    let start_block = input.next_block();
                    let end_block = output.checkpoint.block_number;

                    if start_block > end_block {
                        return Ok(())
                    }

                    let mut body_cursor =
                        provider.tx_ref().cursor_read::<tables::BlockBodyIndices>()?;
                    body_cursor.seek_exact(start_block)?;

                    while let Some((_, body)) = body_cursor.next()? {
                        for tx_id in body.tx_num_range() {
                            let transaction =
                                provider.transaction_by_id(tx_id)?.expect("no transaction entry");
                            assert_eq!(Some(tx_id), provider.transaction_id(transaction.hash())?);
                        }
                    }
                }
                None => self.ensure_no_hash_by_block(input.checkpoint().block_number)?,
            };
            Ok(())
        }
    }

    impl UnwindStageTestRunner for TransactionLookupTestRunner {
        fn validate_unwind(&self, input: UnwindInput) -> Result<(), TestRunnerError> {
            self.ensure_no_hash_by_block(input.unwind_to)
        }
    }
}
