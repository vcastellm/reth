use super::{
    bench::{bench, BenchKind},
    Command,
};
use rand::{seq::SliceRandom, Rng};
use reth_db::{database::Database, open_db_read_only, snapshot::HeaderMask};
use reth_interfaces::db::LogLevel;
use reth_primitives::{
    snapshot::{Compression, Filters, InclusionFilter, PerfectHashingFunction},
    BlockHash, ChainSpec, Header, SnapshotSegment,
};
use reth_provider::{
    providers::SnapshotProvider, DatabaseProviderRO, HeaderProvider, ProviderError,
    ProviderFactory, TransactionsProviderExt,
};
use reth_snapshot::{segments, segments::Segment};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

impl Command {
    pub(crate) fn generate_headers_snapshot<DB: Database>(
        &self,
        provider: &DatabaseProviderRO<DB>,
        compression: Compression,
        inclusion_filter: InclusionFilter,
        phf: PerfectHashingFunction,
    ) -> eyre::Result<()> {
        let range = self.block_range();
        let filters = if self.with_filters {
            Filters::WithFilters(inclusion_filter, phf)
        } else {
            Filters::WithoutFilters
        };

        let segment = segments::Headers::new(compression, filters);

        segment.snapshot::<DB>(provider, PathBuf::default(), range.clone())?;

        // Default name doesn't have any configuration
        let tx_range = provider.transaction_range_by_block_range(range.clone())?;
        reth_primitives::fs::rename(
            SnapshotSegment::Headers.filename(&range, &tx_range),
            SnapshotSegment::Headers.filename_with_configuration(
                filters,
                compression,
                &range,
                &tx_range,
            ),
        )?;

        Ok(())
    }

    pub(crate) fn bench_headers_snapshot(
        &self,
        db_path: &Path,
        log_level: Option<LogLevel>,
        chain: Arc<ChainSpec>,
        compression: Compression,
        inclusion_filter: InclusionFilter,
        phf: PerfectHashingFunction,
    ) -> eyre::Result<()> {
        let filters = if self.with_filters {
            Filters::WithFilters(inclusion_filter, phf)
        } else {
            Filters::WithoutFilters
        };

        let block_range = self.block_range();

        let mut row_indexes = block_range.clone().collect::<Vec<_>>();
        let mut rng = rand::thread_rng();

        let tx_range = ProviderFactory::new(open_db_read_only(db_path, log_level)?, chain.clone())
            .provider()?
            .transaction_range_by_block_range(block_range.clone())?;

        let path: PathBuf = SnapshotSegment::Headers
            .filename_with_configuration(filters, compression, &block_range, &tx_range)
            .into();
        let provider = SnapshotProvider::default();
        let jar_provider = provider.get_segment_provider_from_block(
            SnapshotSegment::Headers,
            self.from,
            Some(&path),
        )?;
        let mut cursor = jar_provider.cursor()?;

        for bench_kind in [BenchKind::Walk, BenchKind::RandomAll] {
            bench(
                bench_kind,
                (open_db_read_only(db_path, log_level)?, chain.clone()),
                SnapshotSegment::Headers,
                filters,
                compression,
                || {
                    for num in row_indexes.iter() {
                        cursor
                            .get_one::<HeaderMask<Header>>((*num).into())?
                            .ok_or(ProviderError::HeaderNotFound((*num).into()))?;
                    }
                    Ok(())
                },
                |provider| {
                    for num in row_indexes.iter() {
                        provider
                            .header_by_number(*num)?
                            .ok_or(ProviderError::HeaderNotFound((*num).into()))?;
                    }
                    Ok(())
                },
            )?;

            // For random walk
            row_indexes.shuffle(&mut rng);
        }

        // BENCHMARK QUERYING A RANDOM HEADER BY NUMBER
        {
            let num = row_indexes[rng.gen_range(0..row_indexes.len())];
            bench(
                BenchKind::RandomOne,
                (open_db_read_only(db_path, log_level)?, chain.clone()),
                SnapshotSegment::Headers,
                filters,
                compression,
                || {
                    Ok(cursor
                        .get_one::<HeaderMask<Header>>(num.into())?
                        .ok_or(ProviderError::HeaderNotFound(num.into()))?)
                },
                |provider| {
                    Ok(provider
                        .header_by_number(num as u64)?
                        .ok_or(ProviderError::HeaderNotFound((num as u64).into()))?)
                },
            )?;
        }

        // BENCHMARK QUERYING A RANDOM HEADER BY HASH
        {
            let num = row_indexes[rng.gen_range(0..row_indexes.len())] as u64;
            let header_hash =
                ProviderFactory::new(open_db_read_only(db_path, log_level)?, chain.clone())
                    .header_by_number(num)?
                    .ok_or(ProviderError::HeaderNotFound(num.into()))?
                    .hash_slow();

            bench(
                BenchKind::RandomHash,
                (open_db_read_only(db_path, log_level)?, chain.clone()),
                SnapshotSegment::Headers,
                filters,
                compression,
                || {
                    let (header, hash) = cursor
                        .get_two::<HeaderMask<Header, BlockHash>>((&header_hash).into())?
                        .ok_or(ProviderError::HeaderNotFound(header_hash.into()))?;

                    // Might be a false positive, so in the real world we have to validate it
                    assert_eq!(hash, header_hash);

                    Ok(header)
                },
                |provider| {
                    Ok(provider
                        .header(&header_hash)?
                        .ok_or(ProviderError::HeaderNotFound(header_hash.into()))?)
                },
            )?;
        }
        Ok(())
    }
}
