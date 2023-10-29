use crate::{ExecInput, ExecOutput, Stage, StageError, UnwindInput, UnwindOutput};
use futures_util::StreamExt;
use reth_db::{
    cursor::{DbCursorRO, DbCursorRW},
    database::Database,
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_interfaces::{
    p2p::headers::{
        downloader::{HeaderDownloader, SyncTarget},
        error::HeadersDownloaderError,
    },
    provider::ProviderError,
};
use reth_primitives::{
    stage::{
        CheckpointBlockRange, EntitiesCheckpoint, HeadersCheckpoint, StageCheckpoint, StageId,
    },
    BlockHashOrNumber, BlockNumber, SealedHeader, B256,
};
use reth_provider::DatabaseProviderRW;
use tokio::sync::watch;
use tracing::*;

/// The header sync mode.
#[derive(Debug)]
pub enum HeaderSyncMode {
    /// A sync mode in which the stage continuously requests the downloader for
    /// next blocks.
    Continuous,
    /// A sync mode in which the stage polls the receiver for the next tip
    /// to download from.
    Tip(watch::Receiver<B256>),
}

/// The headers stage.
///
/// The headers stage downloads all block headers from the highest block in the local database to
/// the perceived highest block on the network.
///
/// The headers are processed and data is inserted into these tables:
///
/// - [`HeaderNumbers`][reth_db::tables::HeaderNumbers]
/// - [`Headers`][reth_db::tables::Headers]
/// - [`CanonicalHeaders`][reth_db::tables::CanonicalHeaders]
///
/// NOTE: This stage downloads headers in reverse. Upon returning the control flow to the pipeline,
/// the stage checkpoint is not updated until this stage is done.
#[derive(Debug)]
pub struct HeaderStage<D: HeaderDownloader> {
    /// Strategy for downloading the headers
    downloader: D,
    /// The sync mode for the stage.
    mode: HeaderSyncMode,
}

// === impl HeaderStage ===

impl<D> HeaderStage<D>
where
    D: HeaderDownloader,
{
    /// Create a new header stage
    pub fn new(downloader: D, mode: HeaderSyncMode) -> Self {
        Self { downloader, mode }
    }

    fn is_stage_done<DB: Database>(
        &self,
        tx: &<DB as reth_db::database::DatabaseGAT<'_>>::TXMut,
        checkpoint: u64,
    ) -> Result<bool, StageError> {
        let mut header_cursor = tx.cursor_read::<tables::CanonicalHeaders>()?;
        let (head_num, _) = header_cursor
            .seek_exact(checkpoint)?
            .ok_or_else(|| ProviderError::HeaderNotFound(checkpoint.into()))?;
        // Check if the next entry is congruent
        Ok(header_cursor.next()?.map(|(next_num, _)| head_num + 1 == next_num).unwrap_or_default())
    }

    /// Get the head and tip of the range we need to sync
    ///
    /// See also [SyncTarget]
    async fn get_sync_gap<DB: Database>(
        &mut self,
        provider: &DatabaseProviderRW<'_, &DB>,
        checkpoint: u64,
    ) -> Result<SyncGap, StageError> {
        // Create a cursor over canonical header hashes
        let mut cursor = provider.tx_ref().cursor_read::<tables::CanonicalHeaders>()?;
        let mut header_cursor = provider.tx_ref().cursor_read::<tables::Headers>()?;

        // Get head hash and reposition the cursor
        let (head_num, head_hash) = cursor
            .seek_exact(checkpoint)?
            .ok_or_else(|| ProviderError::HeaderNotFound(checkpoint.into()))?;

        // Construct head
        let (_, head) = header_cursor
            .seek_exact(head_num)?
            .ok_or_else(|| ProviderError::HeaderNotFound(head_num.into()))?;
        let local_head = head.seal(head_hash);

        // Look up the next header
        let next_header = cursor
            .next()?
            .map(|(next_num, next_hash)| -> Result<SealedHeader, StageError> {
                let (_, next) = header_cursor
                    .seek_exact(next_num)?
                    .ok_or_else(|| ProviderError::HeaderNotFound(next_num.into()))?;
                Ok(next.seal(next_hash))
            })
            .transpose()?;

        // Decide the tip or error out on invalid input.
        // If the next element found in the cursor is not the "expected" next block per our current
        // checkpoint, then there is a gap in the database and we should start downloading in
        // reverse from there. Else, it should use whatever the forkchoice state reports.
        let target = match next_header {
            Some(header) if checkpoint + 1 != header.number => SyncTarget::Gap(header),
            None => self
                .next_sync_target(head_num)
                .await
                .ok_or(StageError::StageCheckpoint(checkpoint))?,
            _ => return Err(StageError::StageCheckpoint(checkpoint)),
        };

        Ok(SyncGap { local_head, target })
    }

    async fn next_sync_target(&mut self, head: BlockNumber) -> Option<SyncTarget> {
        match self.mode {
            HeaderSyncMode::Tip(ref mut rx) => {
                let tip = rx.wait_for(|tip| !tip.is_zero()).await.ok()?;
                Some(SyncTarget::Tip(*tip))
            }
            HeaderSyncMode::Continuous => {
                trace!(target: "sync::stages::headers", head, "No next header found, using continuous sync strategy");
                Some(SyncTarget::TipNum(head + 1))
            }
        }
    }

    /// Write downloaded headers to the given transaction
    ///
    /// Note: this writes the headers with rising block numbers.
    fn write_headers<DB: Database>(
        &self,
        tx: &<DB as reth_db::database::DatabaseGAT<'_>>::TXMut,
        headers: Vec<SealedHeader>,
    ) -> Result<Option<BlockNumber>, StageError> {
        trace!(target: "sync::stages::headers", len = headers.len(), "writing headers");

        let mut cursor_header = tx.cursor_write::<tables::Headers>()?;
        let mut cursor_canonical = tx.cursor_write::<tables::CanonicalHeaders>()?;

        let mut latest = None;
        // Since the headers were returned in descending order,
        // iterate them in the reverse order
        for header in headers.into_iter().rev() {
            if header.number == 0 {
                continue
            }

            let header_hash = header.hash();
            let header_number = header.number;
            let header = header.unseal();
            latest = Some(header.number);

            // NOTE: HeaderNumbers are not sorted and can't be inserted with cursor.
            tx.put::<tables::HeaderNumbers>(header_hash, header_number)?;
            cursor_header.insert(header_number, header)?;
            cursor_canonical.insert(header_number, header_hash)?;
        }

        Ok(latest)
    }
}

#[async_trait::async_trait]
impl<DB, D> Stage<DB> for HeaderStage<D>
where
    DB: Database,
    D: HeaderDownloader,
{
    /// Return the id of the stage
    fn id(&self) -> StageId {
        StageId::Headers
    }

    /// Download the headers in reverse order (falling block numbers)
    /// starting from the tip of the chain
    async fn execute(
        &mut self,
        provider: &DatabaseProviderRW<'_, &DB>,
        input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        let tx = provider.tx_ref();
        let current_checkpoint = input.checkpoint();

        // Lookup the head and tip of the sync range
        let gap = self.get_sync_gap(provider, current_checkpoint.block_number).await?;
        let local_head = gap.local_head.number;
        let tip = gap.target.tip();

        // Nothing to sync
        if gap.is_closed() {
            info!(
                target: "sync::stages::headers",
                checkpoint = %current_checkpoint.block_number,
                target = ?tip,
                "Target block already reached"
            );
            return Ok(ExecOutput::done(current_checkpoint))
        }

        debug!(target: "sync::stages::headers", ?tip, head = ?gap.local_head.hash(), "Commencing sync");

        // let the downloader know what to sync
        self.downloader.update_sync_gap(gap.local_head, gap.target);

        // The downloader returns the headers in descending order starting from the tip
        // down to the local head (latest block in db).
        // Task downloader can return `None` only if the response relaying channel was closed. This
        // is a fatal error to prevent the pipeline from running forever.
        let downloaded_headers = match self.downloader.next().await {
            Some(Ok(headers)) => headers,
            Some(Err(HeadersDownloaderError::DetachedHead { local_head, header, error })) => {
                error!(target: "sync::stages::headers", ?error, "Cannot attach header to head");
                return Err(StageError::DetachedHead { local_head, header, error })
            }
            None => return Err(StageError::ChannelClosed),
        };

        info!(target: "sync::stages::headers", len = downloaded_headers.len(), "Received headers");

        let tip_block_number = match tip {
            // If tip is hash and it equals to the first downloaded header's hash, we can use
            // the block number of this header as tip.
            BlockHashOrNumber::Hash(hash) => downloaded_headers.first().and_then(|header| {
                if header.hash == hash {
                    Some(header.number)
                } else {
                    None
                }
            }),
            // If tip is number, we can just grab it and not resolve using downloaded headers.
            BlockHashOrNumber::Number(number) => Some(number),
        };

        // Since we're syncing headers in batches, gap tip will move in reverse direction towards
        // our local head with every iteration. To get the actual target block number we're
        // syncing towards, we need to take into account already synced headers from the database.
        // It is `None`, if tip didn't change and we're still downloading headers for previously
        // calculated gap.
        let target_block_number = if let Some(tip_block_number) = tip_block_number {
            let local_max_block_number = tx
                .cursor_read::<tables::CanonicalHeaders>()?
                .last()?
                .map(|(canonical_block, _)| canonical_block);

            Some(tip_block_number.max(local_max_block_number.unwrap_or(tip_block_number)))
        } else {
            None
        };

        let mut stage_checkpoint = match current_checkpoint.headers_stage_checkpoint() {
            // If checkpoint block range matches our range, we take the previously used
            // stage checkpoint as-is.
            Some(stage_checkpoint)
                if stage_checkpoint.block_range.from == input.checkpoint().block_number =>
            {
                stage_checkpoint
            }
            // Otherwise, we're on the first iteration of new gap sync, so we recalculate the number
            // of already processed and total headers.
            // `target_block_number` is guaranteed to be `Some`, because on the first iteration
            // we download the header for missing tip and use its block number.
            _ => {
                HeadersCheckpoint {
                    block_range: CheckpointBlockRange {
                        from: input.checkpoint().block_number,
                        to: target_block_number.expect("No downloaded header for tip found"),
                    },
                    progress: EntitiesCheckpoint {
                        // Set processed to the local head block number + number
                        // of block already filled in the gap.
                        processed: local_head +
                            (target_block_number.unwrap_or_default() -
                                tip_block_number.unwrap_or_default()),
                        total: target_block_number.expect("No downloaded header for tip found"),
                    },
                }
            }
        };

        // Total headers can be updated if we received new tip from the network, and need to fill
        // the local gap.
        if let Some(target_block_number) = target_block_number {
            stage_checkpoint.progress.total = target_block_number;
        }
        stage_checkpoint.progress.processed += downloaded_headers.len() as u64;

        // Write the headers to db
        self.write_headers::<DB>(tx, downloaded_headers)?.unwrap_or_default();

        if self.is_stage_done::<DB>(tx, current_checkpoint.block_number)? {
            let checkpoint = current_checkpoint.block_number.max(
                tx.cursor_read::<tables::CanonicalHeaders>()?
                    .last()?
                    .map(|(num, _)| num)
                    .unwrap_or_default(),
            );
            Ok(ExecOutput {
                checkpoint: StageCheckpoint::new(checkpoint)
                    .with_headers_stage_checkpoint(stage_checkpoint),
                done: true,
            })
        } else {
            Ok(ExecOutput {
                checkpoint: current_checkpoint.with_headers_stage_checkpoint(stage_checkpoint),
                done: false,
            })
        }
    }

    /// Unwind the stage.
    async fn unwind(
        &mut self,
        provider: &DatabaseProviderRW<'_, &DB>,
        input: UnwindInput,
    ) -> Result<UnwindOutput, StageError> {
        // TODO: handle bad block
        provider.unwind_table_by_walker::<tables::CanonicalHeaders, tables::HeaderNumbers>(
            input.unwind_to + 1,
        )?;
        provider.unwind_table_by_num::<tables::CanonicalHeaders>(input.unwind_to)?;
        let unwound_headers = provider.unwind_table_by_num::<tables::Headers>(input.unwind_to)?;

        let stage_checkpoint =
            input.checkpoint.headers_stage_checkpoint().map(|stage_checkpoint| HeadersCheckpoint {
                block_range: stage_checkpoint.block_range,
                progress: EntitiesCheckpoint {
                    processed: stage_checkpoint
                        .progress
                        .processed
                        .saturating_sub(unwound_headers as u64),
                    total: stage_checkpoint.progress.total,
                },
            });

        let mut checkpoint = StageCheckpoint::new(input.unwind_to);
        if let Some(stage_checkpoint) = stage_checkpoint {
            checkpoint = checkpoint.with_headers_stage_checkpoint(stage_checkpoint);
        }

        Ok(UnwindOutput { checkpoint })
    }
}

/// Represents a gap to sync: from `local_head` to `target`
#[derive(Debug)]
pub struct SyncGap {
    /// The local head block. Represents lower bound of sync range.
    pub local_head: SealedHeader,

    /// The sync target. Represents upper bound of sync range.
    pub target: SyncTarget,
}

// === impl SyncGap ===

impl SyncGap {
    /// Returns `true` if the gap from the head to the target was closed
    #[inline]
    pub fn is_closed(&self) -> bool {
        match self.target.tip() {
            BlockHashOrNumber::Hash(hash) => self.local_head.hash() == hash,
            BlockHashOrNumber::Number(num) => self.local_head.number == num,
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::test_utils::{
        stage_test_suite, ExecuteStageTestRunner, StageTestRunner, UnwindStageTestRunner,
    };
    use assert_matches::assert_matches;
    use rand::Rng;
    use reth_interfaces::test_utils::{generators, generators::random_header};
    use reth_primitives::{stage::StageUnitCheckpoint, B256, MAINNET};
    use reth_provider::ProviderFactory;
    use test_runner::HeadersTestRunner;

    mod test_runner {
        use super::*;
        use crate::test_utils::{TestRunnerError, TestTransaction};
        use reth_downloaders::headers::reverse_headers::{
            ReverseHeadersDownloader, ReverseHeadersDownloaderBuilder,
        };
        use reth_interfaces::test_utils::{
            generators, generators::random_header_range, TestConsensus, TestHeaderDownloader,
            TestHeadersClient,
        };
        use reth_primitives::U256;
        use reth_provider::{BlockHashReader, BlockNumReader, HeaderProvider};
        use std::sync::Arc;

        pub(crate) struct HeadersTestRunner<D: HeaderDownloader> {
            pub(crate) client: TestHeadersClient,
            channel: (watch::Sender<B256>, watch::Receiver<B256>),
            downloader_factory: Box<dyn Fn() -> D + Send + Sync + 'static>,
            tx: TestTransaction,
        }

        impl Default for HeadersTestRunner<TestHeaderDownloader> {
            fn default() -> Self {
                let client = TestHeadersClient::default();
                Self {
                    client: client.clone(),
                    channel: watch::channel(B256::ZERO),
                    downloader_factory: Box::new(move || {
                        TestHeaderDownloader::new(
                            client.clone(),
                            Arc::new(TestConsensus::default()),
                            1000,
                            1000,
                        )
                    }),
                    tx: TestTransaction::default(),
                }
            }
        }

        impl<D: HeaderDownloader + 'static> StageTestRunner for HeadersTestRunner<D> {
            type S = HeaderStage<D>;

            fn tx(&self) -> &TestTransaction {
                &self.tx
            }

            fn stage(&self) -> Self::S {
                HeaderStage {
                    mode: HeaderSyncMode::Tip(self.channel.1.clone()),
                    downloader: (*self.downloader_factory)(),
                }
            }
        }

        #[async_trait::async_trait]
        impl<D: HeaderDownloader + 'static> ExecuteStageTestRunner for HeadersTestRunner<D> {
            type Seed = Vec<SealedHeader>;

            fn seed_execution(&mut self, input: ExecInput) -> Result<Self::Seed, TestRunnerError> {
                let mut rng = generators::rng();
                let start = input.checkpoint().block_number;
                let head = random_header(&mut rng, start, None);
                self.tx.insert_headers(std::iter::once(&head))?;
                // patch td table for `update_head` call
                self.tx.commit(|tx| tx.put::<tables::HeaderTD>(head.number, U256::ZERO.into()))?;

                // use previous checkpoint as seed size
                let end = input.target.unwrap_or_default() + 1;

                if start + 1 >= end {
                    return Ok(Vec::default())
                }

                let mut headers = random_header_range(&mut rng, start + 1..end, head.hash());
                headers.insert(0, head);
                Ok(headers)
            }

            /// Validate stored headers
            fn validate_execution(
                &self,
                input: ExecInput,
                output: Option<ExecOutput>,
            ) -> Result<(), TestRunnerError> {
                let initial_checkpoint = input.checkpoint().block_number;
                match output {
                    Some(output) if output.checkpoint.block_number > initial_checkpoint => {
                        let provider = self.tx.factory.provider()?;
                        for block_num in (initial_checkpoint..output.checkpoint.block_number).rev()
                        {
                            // look up the header hash
                            let hash = provider.block_hash(block_num)?.expect("no header hash");

                            // validate the header number
                            assert_eq!(provider.block_number(hash)?, Some(block_num));

                            // validate the header
                            let header = provider.header_by_number(block_num)?;
                            assert!(header.is_some());
                            let header = header.unwrap().seal_slow();
                            assert_eq!(header.hash(), hash);
                        }
                    }
                    _ => self.check_no_header_entry_above(initial_checkpoint)?,
                };
                Ok(())
            }

            async fn after_execution(&self, headers: Self::Seed) -> Result<(), TestRunnerError> {
                self.client.extend(headers.iter().map(|h| h.clone().unseal())).await;
                let tip = if !headers.is_empty() {
                    headers.last().unwrap().hash()
                } else {
                    let tip = random_header(&mut generators::rng(), 0, None);
                    self.tx.insert_headers(std::iter::once(&tip))?;
                    tip.hash()
                };
                self.send_tip(tip);
                Ok(())
            }
        }

        impl<D: HeaderDownloader + 'static> UnwindStageTestRunner for HeadersTestRunner<D> {
            fn validate_unwind(&self, input: UnwindInput) -> Result<(), TestRunnerError> {
                self.check_no_header_entry_above(input.unwind_to)
            }
        }

        impl HeadersTestRunner<ReverseHeadersDownloader<TestHeadersClient>> {
            pub(crate) fn with_linear_downloader() -> Self {
                let client = TestHeadersClient::default();
                Self {
                    client: client.clone(),
                    channel: watch::channel(B256::ZERO),
                    downloader_factory: Box::new(move || {
                        ReverseHeadersDownloaderBuilder::default()
                            .stream_batch_size(500)
                            .build(client.clone(), Arc::new(TestConsensus::default()))
                    }),
                    tx: TestTransaction::default(),
                }
            }
        }

        impl<D: HeaderDownloader> HeadersTestRunner<D> {
            pub(crate) fn check_no_header_entry_above(
                &self,
                block: BlockNumber,
            ) -> Result<(), TestRunnerError> {
                self.tx
                    .ensure_no_entry_above_by_value::<tables::HeaderNumbers, _>(block, |val| val)?;
                self.tx.ensure_no_entry_above::<tables::CanonicalHeaders, _>(block, |key| key)?;
                self.tx.ensure_no_entry_above::<tables::Headers, _>(block, |key| key)?;
                Ok(())
            }

            pub(crate) fn send_tip(&self, tip: B256) {
                self.channel.0.send(tip).expect("failed to send tip");
            }
        }
    }

    stage_test_suite!(HeadersTestRunner, headers);

    /// Execute the stage with linear downloader
    #[tokio::test]
    async fn execute_with_linear_downloader() {
        let mut runner = HeadersTestRunner::with_linear_downloader();
        let (checkpoint, previous_stage) = (1000, 1200);
        let input = ExecInput {
            target: Some(previous_stage),
            checkpoint: Some(StageCheckpoint::new(checkpoint)),
        };
        let headers = runner.seed_execution(input).expect("failed to seed execution");
        let rx = runner.execute(input);

        runner.client.extend(headers.iter().rev().map(|h| h.clone().unseal())).await;

        // skip `after_execution` hook for linear downloader
        let tip = headers.last().unwrap();
        runner.send_tip(tip.hash());

        let result = rx.await.unwrap();
        assert_matches!( result, Ok(ExecOutput { checkpoint: StageCheckpoint {
            block_number,
            stage_checkpoint: Some(StageUnitCheckpoint::Headers(HeadersCheckpoint {
                block_range: CheckpointBlockRange {
                    from,
                    to
                },
                progress: EntitiesCheckpoint {
                    processed,
                    total,
                }
            }))
        }, done: true }) if block_number == tip.number &&
            from == checkpoint && to == previous_stage &&
            // -1 because we don't need to download the local head
            processed == checkpoint + headers.len() as u64 - 1 && total == tip.number);
        assert!(runner.validate_execution(input, result.ok()).is_ok(), "validation failed");
    }

    /// Test the head and tip range lookup
    #[tokio::test]
    async fn head_and_tip_lookup() {
        let runner = HeadersTestRunner::default();
        let factory = ProviderFactory::new(runner.tx().tx.as_ref(), MAINNET.clone());
        let provider = factory.provider_rw().unwrap();
        let tx = provider.tx_ref();
        let mut stage = runner.stage();

        let mut rng = generators::rng();

        let consensus_tip = rng.gen();
        runner.send_tip(consensus_tip);

        // Genesis
        let checkpoint = 0;
        let head = random_header(&mut rng, 0, None);
        let gap_fill = random_header(&mut rng, 1, Some(head.hash()));
        let gap_tip = random_header(&mut rng, 2, Some(gap_fill.hash()));

        // Empty database
        assert_matches!(
            stage.get_sync_gap(&provider, checkpoint).await,
            Err(StageError::DatabaseIntegrity(ProviderError::HeaderNotFound(block_number)))
                if block_number.as_number().unwrap() == checkpoint
        );

        // Checkpoint and no gap
        tx.put::<tables::CanonicalHeaders>(head.number, head.hash())
            .expect("failed to write canonical");
        tx.put::<tables::Headers>(head.number, head.clone().unseal())
            .expect("failed to write header");

        let gap = stage.get_sync_gap(&provider, checkpoint).await.unwrap();
        assert_eq!(gap.local_head, head);
        assert_eq!(gap.target.tip(), consensus_tip.into());

        // Checkpoint and gap
        tx.put::<tables::CanonicalHeaders>(gap_tip.number, gap_tip.hash())
            .expect("failed to write canonical");
        tx.put::<tables::Headers>(gap_tip.number, gap_tip.clone().unseal())
            .expect("failed to write header");

        let gap = stage.get_sync_gap(&provider, checkpoint).await.unwrap();
        assert_eq!(gap.local_head, head);
        assert_eq!(gap.target.tip(), gap_tip.parent_hash.into());

        // Checkpoint and gap closed
        tx.put::<tables::CanonicalHeaders>(gap_fill.number, gap_fill.hash())
            .expect("failed to write canonical");
        tx.put::<tables::Headers>(gap_fill.number, gap_fill.clone().unseal())
            .expect("failed to write header");

        assert_matches!(
            stage.get_sync_gap(&provider, checkpoint).await,
            Err(StageError::StageCheckpoint(_checkpoint)) if _checkpoint == checkpoint
        );
    }

    /// Execute the stage in two steps
    #[tokio::test]
    async fn execute_from_previous_checkpoint() {
        let mut runner = HeadersTestRunner::with_linear_downloader();
        // pick range that's larger than the configured headers batch size
        let (checkpoint, previous_stage) = (600, 1200);
        let mut input = ExecInput {
            target: Some(previous_stage),
            checkpoint: Some(StageCheckpoint::new(checkpoint)),
        };
        let headers = runner.seed_execution(input).expect("failed to seed execution");
        let rx = runner.execute(input);

        runner.client.extend(headers.iter().rev().map(|h| h.clone().unseal())).await;

        // skip `after_execution` hook for linear downloader
        let tip = headers.last().unwrap();
        runner.send_tip(tip.hash());

        let result = rx.await.unwrap();
        assert_matches!(result, Ok(ExecOutput { checkpoint: StageCheckpoint {
            block_number,
            stage_checkpoint: Some(StageUnitCheckpoint::Headers(HeadersCheckpoint {
                block_range: CheckpointBlockRange {
                    from,
                    to
                },
                progress: EntitiesCheckpoint {
                    processed,
                    total,
                }
            }))
        }, done: false }) if block_number == checkpoint &&
            from == checkpoint && to == previous_stage &&
            processed == checkpoint + 500 && total == tip.number);

        runner.client.clear().await;
        runner.client.extend(headers.iter().take(101).map(|h| h.clone().unseal()).rev()).await;
        input.checkpoint = Some(result.unwrap().checkpoint);

        let rx = runner.execute(input);
        let result = rx.await.unwrap();

        assert_matches!(result, Ok(ExecOutput { checkpoint: StageCheckpoint {
            block_number,
            stage_checkpoint: Some(StageUnitCheckpoint::Headers(HeadersCheckpoint {
                block_range: CheckpointBlockRange {
                    from,
                    to
                },
                progress: EntitiesCheckpoint {
                    processed,
                    total,
                }
            }))
        }, done: true }) if block_number == tip.number &&
            from == checkpoint && to == previous_stage &&
            // -1 because we don't need to download the local head
            processed == checkpoint + headers.len() as u64 - 1 && total == tip.number);
        assert!(runner.validate_execution(input, result.ok()).is_ok(), "validation failed");
    }
}
