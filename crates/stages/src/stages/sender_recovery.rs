use crate::{BlockErrorKind, ExecInput, ExecOutput, Stage, StageError, UnwindInput, UnwindOutput};
use itertools::Itertools;
use reth_db::{
    cursor::{DbCursorRO, DbCursorRW},
    database::Database,
    tables,
    transaction::{DbTx, DbTxMut},
    DatabaseError, RawKey, RawTable, RawValue,
};
use reth_interfaces::consensus;
use reth_primitives::{
    keccak256,
    stage::{EntitiesCheckpoint, StageCheckpoint, StageId},
    Address, PruneSegment, TransactionSignedNoHash, TxNumber,
};
use reth_provider::{
    BlockReader, DatabaseProviderRW, HeaderProvider, ProviderError, PruneCheckpointReader,
};
use std::fmt::Debug;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::*;

/// The sender recovery stage iterates over existing transactions,
/// recovers the transaction signer and stores them
/// in [`TxSenders`][reth_db::tables::TxSenders] table.
#[derive(Clone, Debug)]
pub struct SenderRecoveryStage {
    /// The size of inserted items after which the control
    /// flow will be returned to the pipeline for commit
    pub commit_threshold: u64,
}

impl SenderRecoveryStage {
    /// Create new instance of [SenderRecoveryStage].
    pub fn new(commit_threshold: u64) -> Self {
        Self { commit_threshold }
    }
}

impl Default for SenderRecoveryStage {
    fn default() -> Self {
        Self { commit_threshold: 5_000_000 }
    }
}

#[async_trait::async_trait]
impl<DB: Database> Stage<DB> for SenderRecoveryStage {
    /// Return the id of the stage
    fn id(&self) -> StageId {
        StageId::SenderRecovery
    }

    /// Retrieve the range of transactions to iterate over by querying
    /// [`BlockBodyIndices`][reth_db::tables::BlockBodyIndices],
    /// collect transactions within that range,
    /// recover signer for each transaction and store entries in
    /// the [`TxSenders`][reth_db::tables::TxSenders] table.
    async fn execute(
        &mut self,
        provider: &DatabaseProviderRW<'_, &DB>,
        input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        if input.target_reached() {
            return Ok(ExecOutput::done(input.checkpoint()))
        }

        let (tx_range, block_range, is_final_range) =
            input.next_block_range_with_transaction_threshold(provider, self.commit_threshold)?;
        let end_block = *block_range.end();

        // No transactions to walk over
        if tx_range.is_empty() {
            info!(target: "sync::stages::sender_recovery", ?tx_range, "Target transaction already reached");
            return Ok(ExecOutput {
                checkpoint: StageCheckpoint::new(end_block)
                    .with_entities_stage_checkpoint(stage_checkpoint(provider)?),
                done: is_final_range,
            })
        }

        let tx = provider.tx_ref();

        // Acquire the cursor for inserting elements
        let mut senders_cursor = tx.cursor_write::<tables::TxSenders>()?;

        // Acquire the cursor over the transactions
        let mut tx_cursor = tx.cursor_read::<RawTable<tables::Transactions>>()?;
        // Walk the transactions from start to end index (inclusive)
        let raw_tx_range = RawKey::new(tx_range.start)..RawKey::new(tx_range.end);
        let tx_walker = tx_cursor.walk_range(raw_tx_range)?;

        // Iterate over transactions in chunks
        info!(target: "sync::stages::sender_recovery", ?tx_range, "Recovering senders");

        // channels used to return result of sender recovery.
        let mut channels = Vec::new();

        // Spawn recovery jobs onto the default rayon threadpool and send the result through the
        // channel.
        //
        // We try to evenly divide the transactions to recover across all threads in the threadpool.
        // Chunks are submitted instead of individual transactions to reduce the overhead of work
        // stealing in the threadpool workers.
        let chunk_size = self.commit_threshold as usize / rayon::current_num_threads();
        // prevents an edge case
        // where the chunk size is either 0 or too small
        // to gain anything from using more than 1 thread
        let chunk_size = chunk_size.max(16);

        for chunk in &tx_walker.chunks(chunk_size) {
            // An _unordered_ channel to receive results from a rayon job
            let (recovered_senders_tx, recovered_senders_rx) = mpsc::unbounded_channel();
            channels.push(recovered_senders_rx);
            // Note: Unfortunate side-effect of how chunk is designed in itertools (it is not Send)
            let chunk: Vec<_> = chunk.collect();

            // Spawn the sender recovery task onto the global rayon pool
            // This task will send the results through the channel after it recovered the senders.
            rayon::spawn(move || {
                let mut rlp_buf = Vec::with_capacity(128);
                for entry in chunk {
                    rlp_buf.clear();
                    let recovery_result = recover_sender(entry, &mut rlp_buf);
                    let _ = recovered_senders_tx.send(recovery_result);
                }
            });
        }

        // Iterate over channels and append the sender in the order that they are received.
        for mut channel in channels {
            while let Some(recovered) = channel.recv().await {
                let (tx_id, sender) = match recovered {
                    Ok(result) => result,
                    Err(error) => {
                        match *error {
                            SenderRecoveryStageError::FailedRecovery(err) => {
                                // get the block number for the bad transaction
                                let block_number = tx
                                    .get::<tables::TransactionBlock>(err.tx)?
                                    .ok_or(ProviderError::BlockNumberForTransactionIndexNotFound)?;

                                // fetch the sealed header so we can use it in the sender recovery
                                // unwind
                                let sealed_header = provider
                                    .sealed_header(block_number)?
                                    .ok_or(ProviderError::HeaderNotFound(block_number.into()))?;
                                return Err(StageError::Block {
                                    block: sealed_header,
                                    error: BlockErrorKind::Validation(
                                        consensus::ConsensusError::TransactionSignerRecoveryError,
                                    ),
                                })
                            }
                            SenderRecoveryStageError::StageError(err) => return Err(err),
                        }
                    }
                };
                senders_cursor.append(tx_id, sender)?;
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
        let (_, unwind_to, _) = input.unwind_block_range_with_threshold(self.commit_threshold);

        // Lookup latest tx id that we should unwind to
        let latest_tx_id = provider
            .block_body_indices(unwind_to)?
            .ok_or(ProviderError::BlockBodyIndicesNotFound(unwind_to))?
            .last_tx_num();
        provider.unwind_table_by_num::<tables::TxSenders>(latest_tx_id)?;

        Ok(UnwindOutput {
            checkpoint: StageCheckpoint::new(unwind_to)
                .with_entities_stage_checkpoint(stage_checkpoint(provider)?),
        })
    }
}

fn recover_sender(
    entry: Result<(RawKey<TxNumber>, RawValue<TransactionSignedNoHash>), DatabaseError>,
    rlp_buf: &mut Vec<u8>,
) -> Result<(u64, Address), Box<SenderRecoveryStageError>> {
    let (tx_id, transaction) =
        entry.map_err(|e| Box::new(SenderRecoveryStageError::StageError(e.into())))?;
    let tx_id = tx_id.key().expect("key to be formated");

    let tx = transaction.value().expect("value to be formated");
    tx.transaction.encode_without_signature(rlp_buf);

    let sender = tx
        .signature
        .recover_signer(keccak256(rlp_buf))
        .ok_or(SenderRecoveryStageError::FailedRecovery(FailedSenderRecoveryError { tx: tx_id }))?;

    Ok((tx_id, sender))
}

fn stage_checkpoint<DB: Database>(
    provider: &DatabaseProviderRW<'_, &DB>,
) -> Result<EntitiesCheckpoint, StageError> {
    let pruned_entries = provider
        .get_prune_checkpoint(PruneSegment::SenderRecovery)?
        .and_then(|checkpoint| checkpoint.tx_number)
        .unwrap_or_default();
    Ok(EntitiesCheckpoint {
        // If `TxSenders` table was pruned, we will have a number of entries in it not matching
        // the actual number of processed transactions. To fix that, we add the number of pruned
        // `TxSenders` entries.
        processed: provider.tx_ref().entries::<tables::TxSenders>()? as u64 + pruned_entries,
        total: provider.tx_ref().entries::<tables::Transactions>()? as u64,
    })
}

#[derive(Error, Debug)]
#[error(transparent)]
enum SenderRecoveryStageError {
    /// A transaction failed sender recovery
    #[error(transparent)]
    FailedRecovery(#[from] FailedSenderRecoveryError),

    /// A different type of stage error occurred
    #[error(transparent)]
    StageError(#[from] StageError),
}

#[derive(Error, Debug)]
#[error("sender recovery failed for transaction {tx}")]
struct FailedSenderRecoveryError {
    /// The transaction that failed sender recovery
    tx: TxNumber,
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use reth_interfaces::test_utils::{
        generators,
        generators::{random_block, random_block_range},
    };
    use reth_primitives::{
        stage::StageUnitCheckpoint, BlockNumber, PruneCheckpoint, PruneMode, SealedBlock,
        TransactionSigned, B256, MAINNET,
    };
    use reth_provider::{ProviderFactory, PruneCheckpointWriter, TransactionsProvider};

    use super::*;
    use crate::test_utils::{
        stage_test_suite_ext, ExecuteStageTestRunner, StageTestRunner, TestRunnerError,
        TestTransaction, UnwindStageTestRunner,
    };

    stage_test_suite_ext!(SenderRecoveryTestRunner, sender_recovery);

    /// Execute a block range with a single transaction
    #[tokio::test]
    async fn execute_single_transaction() {
        let (previous_stage, stage_progress) = (500, 100);
        let mut rng = generators::rng();

        // Set up the runner
        let runner = SenderRecoveryTestRunner::default();
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
            Ok(ExecOutput { checkpoint: StageCheckpoint {
                block_number,
                stage_checkpoint: Some(StageUnitCheckpoint::Entities(EntitiesCheckpoint {
                    processed: 1,
                    total: 1
                }))
            }, done: true }) if block_number == previous_stage
        );

        // Validate the stage execution
        assert!(runner.validate_execution(input, result.ok()).is_ok(), "execution validation");
    }

    /// Execute the stage twice with input range that exceeds the commit threshold
    #[tokio::test]
    async fn execute_intermediate_commit() {
        let mut rng = generators::rng();

        let threshold = 10;
        let mut runner = SenderRecoveryTestRunner::default();
        runner.set_threshold(threshold);
        let (stage_progress, previous_stage) = (1000, 1100); // input exceeds threshold

        // Manually seed once with full input range
        let seed =
            random_block_range(&mut rng, stage_progress + 1..=previous_stage, B256::ZERO, 0..4); // set tx count range high enough to hit the threshold
        runner.tx.insert_blocks(seed.iter(), None).expect("failed to seed execution");

        let total_transactions = runner.tx.table::<tables::Transactions>().unwrap().len() as u64;

        let first_input = ExecInput {
            target: Some(previous_stage),
            checkpoint: Some(StageCheckpoint::new(stage_progress)),
        };

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
                        processed: runner.tx.table::<tables::TxSenders>().unwrap().len() as u64,
                        total: total_transactions
                    }
                ),
                done: false
            }
        );

        // Execute second time to completion
        runner.set_threshold(u64::MAX);
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
                    EntitiesCheckpoint { processed: total_transactions, total: total_transactions }
                ),
                done: true
            }
        );

        assert!(runner.validate_execution(first_input, result.ok()).is_ok(), "validation failed");
    }

    #[test]
    fn stage_checkpoint_pruned() {
        let tx = TestTransaction::default();
        let mut rng = generators::rng();

        let blocks = random_block_range(&mut rng, 0..=100, B256::ZERO, 0..10);
        tx.insert_blocks(blocks.iter(), None).expect("insert blocks");

        let max_pruned_block = 30;
        let max_processed_block = 70;

        let mut tx_senders = Vec::new();
        let mut tx_number = 0;
        for block in &blocks[..=max_processed_block] {
            for transaction in &block.body {
                if block.number > max_pruned_block {
                    tx_senders
                        .push((tx_number, transaction.recover_signer().expect("recover signer")));
                }
                tx_number += 1;
            }
        }
        tx.insert_transaction_senders(tx_senders).expect("insert tx hash numbers");

        let provider = tx.inner_rw();
        provider
            .save_prune_checkpoint(
                PruneSegment::SenderRecovery,
                PruneCheckpoint {
                    block_number: Some(max_pruned_block),
                    tx_number: Some(
                        blocks[..=max_pruned_block as usize]
                            .iter()
                            .map(|block| block.body.len() as u64)
                            .sum::<u64>(),
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

    struct SenderRecoveryTestRunner {
        tx: TestTransaction,
        threshold: u64,
    }

    impl Default for SenderRecoveryTestRunner {
        fn default() -> Self {
            Self { threshold: 1000, tx: TestTransaction::default() }
        }
    }

    impl SenderRecoveryTestRunner {
        fn set_threshold(&mut self, threshold: u64) {
            self.threshold = threshold;
        }

        /// # Panics
        ///
        /// 1. If there are any entries in the [tables::TxSenders] table above a given block number.
        ///
        /// 2. If the is no requested block entry in the bodies table, but [tables::TxSenders] is
        ///    not empty.
        fn ensure_no_senders_by_block(&self, block: BlockNumber) -> Result<(), TestRunnerError> {
            let body_result = self
                .tx
                .inner_rw()
                .block_body_indices(block)?
                .ok_or(ProviderError::BlockBodyIndicesNotFound(block));
            match body_result {
                Ok(body) => self
                    .tx
                    .ensure_no_entry_above::<tables::TxSenders, _>(body.last_tx_num(), |key| key)?,
                Err(_) => {
                    assert!(self.tx.table_is_empty::<tables::TxSenders>()?);
                }
            };

            Ok(())
        }
    }

    impl StageTestRunner for SenderRecoveryTestRunner {
        type S = SenderRecoveryStage;

        fn tx(&self) -> &TestTransaction {
            &self.tx
        }

        fn stage(&self) -> Self::S {
            SenderRecoveryStage { commit_threshold: self.threshold }
        }
    }

    impl ExecuteStageTestRunner for SenderRecoveryTestRunner {
        type Seed = Vec<SealedBlock>;

        fn seed_execution(&mut self, input: ExecInput) -> Result<Self::Seed, TestRunnerError> {
            let mut rng = generators::rng();
            let stage_progress = input.checkpoint().block_number;
            let end = input.target();

            let blocks = random_block_range(&mut rng, stage_progress..=end, B256::ZERO, 0..2);
            self.tx.insert_blocks(blocks.iter(), None)?;
            Ok(blocks)
        }

        fn validate_execution(
            &self,
            input: ExecInput,
            output: Option<ExecOutput>,
        ) -> Result<(), TestRunnerError> {
            match output {
                Some(output) => {
                    let provider = self.tx.inner();
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
                            let transaction: TransactionSigned = provider
                                .transaction_by_id_no_hash(tx_id)?
                                .map(|tx| TransactionSigned {
                                    hash: Default::default(), // we don't require the hash
                                    signature: tx.signature,
                                    transaction: tx.transaction,
                                })
                                .expect("no transaction entry");
                            let signer =
                                transaction.recover_signer().expect("failed to recover signer");
                            assert_eq!(Some(signer), provider.transaction_sender(tx_id)?)
                        }
                    }
                }
                None => self.ensure_no_senders_by_block(input.checkpoint().block_number)?,
            };

            Ok(())
        }
    }

    impl UnwindStageTestRunner for SenderRecoveryTestRunner {
        fn validate_unwind(&self, input: UnwindInput) -> Result<(), TestRunnerError> {
            self.ensure_no_senders_by_block(input.unwind_to)
        }
    }
}
