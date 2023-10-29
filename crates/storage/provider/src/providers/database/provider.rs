use crate::{
    bundle_state::{BundleStateInit, BundleStateWithReceipts, RevertsInit},
    traits::{
        AccountExtReader, BlockSource, ChangeSetReader, ReceiptProvider, StageCheckpointWriter,
    },
    AccountReader, BlockExecutionWriter, BlockHashReader, BlockNumReader, BlockReader, BlockWriter,
    Chain, EvmEnvProvider, HashingWriter, HeaderProvider, HistoryWriter, OriginalValuesKnown,
    ProviderError, PruneCheckpointReader, PruneCheckpointWriter, StageCheckpointReader,
    StorageReader, TransactionVariant, TransactionsProvider, TransactionsProviderExt,
    WithdrawalsProvider,
};
use itertools::{izip, Itertools};
use reth_db::{
    common::KeyValue,
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO},
    database::{Database, DatabaseGAT},
    models::{
        sharded_key, storage_sharded_key::StorageShardedKey, AccountBeforeTx, BlockNumberAddress,
        ShardedKey, StoredBlockBodyIndices, StoredBlockOmmers, StoredBlockWithdrawals,
    },
    table::{Table, TableRow},
    tables,
    transaction::{DbTx, DbTxMut},
    BlockNumberList, DatabaseError,
};
use reth_interfaces::{
    executor::{BlockExecutionError, BlockValidationError},
    RethError, RethResult,
};
use reth_primitives::{
    keccak256,
    revm::{
        config::revm_spec,
        env::{fill_block_env, fill_cfg_and_block_env, fill_cfg_env},
    },
    stage::{StageCheckpoint, StageId},
    trie::Nibbles,
    Account, Address, Block, BlockHash, BlockHashOrNumber, BlockNumber, BlockWithSenders,
    ChainInfo, ChainSpec, Hardfork, Head, Header, PruneCheckpoint, PruneModes, PruneSegment,
    Receipt, SealedBlock, SealedBlockWithSenders, SealedHeader, StorageEntry, TransactionMeta,
    TransactionSigned, TransactionSignedEcRecovered, TransactionSignedNoHash, TxHash, TxNumber,
    Withdrawal, B256, U256,
};
use reth_trie::{prefix_set::PrefixSetMut, StateRoot};
use revm::primitives::{BlockEnv, CfgEnv, SpecId};
use std::{
    collections::{hash_map, BTreeMap, BTreeSet, HashMap, HashSet},
    fmt::Debug,
    ops::{Deref, DerefMut, Range, RangeBounds, RangeInclusive},
    sync::{mpsc, Arc},
};

/// A [`DatabaseProvider`] that holds a read-only database transaction.
pub type DatabaseProviderRO<'this, DB> = DatabaseProvider<<DB as DatabaseGAT<'this>>::TX>;

/// A [`DatabaseProvider`] that holds a read-write database transaction.
///
/// Ideally this would be an alias type. However, there's some weird compiler error (<https://github.com/rust-lang/rust/issues/102211>), that forces us to wrap this in a struct instead.
/// Once that issue is solved, we can probably revert back to being an alias type.
#[derive(Debug)]
pub struct DatabaseProviderRW<'this, DB: Database>(
    pub DatabaseProvider<<DB as DatabaseGAT<'this>>::TXMut>,
);

impl<'this, DB: Database> Deref for DatabaseProviderRW<'this, DB> {
    type Target = DatabaseProvider<<DB as DatabaseGAT<'this>>::TXMut>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<DB: Database> DerefMut for DatabaseProviderRW<'_, DB> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<'this, DB: Database> DatabaseProviderRW<'this, DB> {
    /// Commit database transaction
    pub fn commit(self) -> RethResult<bool> {
        self.0.commit()
    }

    /// Consume `DbTx` or `DbTxMut`.
    pub fn into_tx(self) -> <DB as DatabaseGAT<'this>>::TXMut {
        self.0.into_tx()
    }
}

/// A provider struct that fetchs data from the database.
/// Wrapper around [`DbTx`] and [`DbTxMut`]. Example: [`HeaderProvider`] [`BlockHashReader`]
#[derive(Debug)]
pub struct DatabaseProvider<TX> {
    /// Database transaction.
    tx: TX,
    /// Chain spec
    chain_spec: Arc<ChainSpec>,
}

impl<TX: DbTxMut> DatabaseProvider<TX> {
    /// Creates a provider with an inner read-write transaction.
    pub fn new_rw(tx: TX, chain_spec: Arc<ChainSpec>) -> Self {
        Self { tx, chain_spec }
    }
}

/// For a given key, unwind all history shards that are below the given block number.
///
/// S - Sharded key subtype.
/// T - Table to walk over.
/// C - Cursor implementation.
///
/// This function walks the entries from the given start key and deletes all shards that belong to
/// the key and are below the given block number.
///
/// The boundary shard (the shard is split by the block number) is removed from the database. Any
/// indices that are above the block number are filtered out. The boundary shard is returned for
/// reinsertion (if it's not empty).
fn unwind_history_shards<S, T, C>(
    cursor: &mut C,
    start_key: T::Key,
    block_number: BlockNumber,
    mut shard_belongs_to_key: impl FnMut(&T::Key) -> bool,
) -> RethResult<Vec<usize>>
where
    T: Table<Value = BlockNumberList>,
    T::Key: AsRef<ShardedKey<S>>,
    C: DbCursorRO<T> + DbCursorRW<T>,
{
    let mut item = cursor.seek_exact(start_key)?;
    while let Some((sharded_key, list)) = item {
        // If the shard does not belong to the key, break.
        if !shard_belongs_to_key(&sharded_key) {
            break
        }
        cursor.delete_current()?;

        // Check the first item.
        // If it is greater or eq to the block number, delete it.
        let first = list.iter(0).next().expect("List can't be empty");
        if first >= block_number as usize {
            item = cursor.prev()?;
            continue
        } else if block_number <= sharded_key.as_ref().highest_block_number {
            // Filter out all elements greater than block number.
            return Ok(list.iter(0).take_while(|i| *i < block_number as usize).collect::<Vec<_>>())
        } else {
            return Ok(list.iter(0).collect::<Vec<_>>())
        }
    }

    Ok(Vec::new())
}

impl<TX: DbTx> DatabaseProvider<TX> {
    /// Creates a provider with an inner read-only transaction.
    pub fn new(tx: TX, chain_spec: Arc<ChainSpec>) -> Self {
        Self { tx, chain_spec }
    }

    /// Consume `DbTx` or `DbTxMut`.
    pub fn into_tx(self) -> TX {
        self.tx
    }

    /// Pass `DbTx` or `DbTxMut` mutable reference.
    pub fn tx_mut(&mut self) -> &mut TX {
        &mut self.tx
    }

    /// Pass `DbTx` or `DbTxMut` immutable reference.
    pub fn tx_ref(&self) -> &TX {
        &self.tx
    }

    /// Return full table as Vec
    pub fn table<T: Table>(&self) -> Result<Vec<KeyValue<T>>, DatabaseError>
    where
        T::Key: Default + Ord,
    {
        self.tx
            .cursor_read::<T>()?
            .walk(Some(T::Key::default()))?
            .collect::<Result<Vec<_>, DatabaseError>>()
    }
}

impl<TX: DbTxMut + DbTx> DatabaseProvider<TX> {
    /// Commit database transaction.
    pub fn commit(self) -> RethResult<bool> {
        Ok(self.tx.commit()?)
    }

    // TODO(joshie) TEMPORARY should be moved to trait providers

    /// Unwind or peek at last N blocks of state recreating the [`BundleStateWithReceipts`].
    ///
    /// If UNWIND it set to true tip and latest state will be unwind
    /// and returned back with all the blocks
    ///
    /// If UNWIND is false we will just read the state/blocks and return them.
    ///
    /// 1. Iterate over the [BlockBodyIndices][tables::BlockBodyIndices] table to get all
    /// the transaction ids.
    /// 2. Iterate over the [StorageChangeSet][tables::StorageChangeSet] table
    /// and the [AccountChangeSet][tables::AccountChangeSet] tables in reverse order to reconstruct
    /// the changesets.
    ///     - In order to have both the old and new values in the changesets, we also access the
    ///       plain state tables.
    /// 3. While iterating over the changeset tables, if we encounter a new account or storage slot,
    /// we:
    ///     1. Take the old value from the changeset
    ///     2. Take the new value from the plain state
    ///     3. Save the old value to the local state
    /// 4. While iterating over the changeset tables, if we encounter an account/storage slot we
    /// have seen before we:
    ///     1. Take the old value from the changeset
    ///     2. Take the new value from the local state
    ///     3. Set the local state to the value in the changeset
    fn unwind_or_peek_state<const UNWIND: bool>(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> RethResult<BundleStateWithReceipts> {
        if range.is_empty() {
            return Ok(BundleStateWithReceipts::default())
        }
        let start_block_number = *range.start();

        // We are not removing block meta as it is used to get block changesets.
        let block_bodies = self.get_or_take::<tables::BlockBodyIndices, false>(range.clone())?;

        // get transaction receipts
        let from_transaction_num =
            block_bodies.first().expect("already checked if there are blocks").1.first_tx_num();
        let to_transaction_num =
            block_bodies.last().expect("already checked if there are blocks").1.last_tx_num();

        let storage_range = BlockNumberAddress::range(range.clone());

        let storage_changeset =
            self.get_or_take::<tables::StorageChangeSet, UNWIND>(storage_range)?;
        let account_changeset = self.get_or_take::<tables::AccountChangeSet, UNWIND>(range)?;

        // iterate previous value and get plain state value to create changeset
        // Double option around Account represent if Account state is know (first option) and
        // account is removed (Second Option)

        let mut state: BundleStateInit = HashMap::new();

        // This is not working for blocks that are not at tip. as plain state is not the last
        // state of end range. We should rename the functions or add support to access
        // History state. Accessing history state can be tricky but we are not gaining
        // anything.
        let mut plain_accounts_cursor = self.tx.cursor_write::<tables::PlainAccountState>()?;
        let mut plain_storage_cursor = self.tx.cursor_dup_write::<tables::PlainStorageState>()?;

        let mut reverts: RevertsInit = HashMap::new();

        // add account changeset changes
        for (block_number, account_before) in account_changeset.into_iter().rev() {
            let AccountBeforeTx { info: old_info, address } = account_before;
            match state.entry(address) {
                hash_map::Entry::Vacant(entry) => {
                    let new_info = plain_accounts_cursor.seek_exact(address)?.map(|kv| kv.1);
                    entry.insert((old_info, new_info, HashMap::new()));
                }
                hash_map::Entry::Occupied(mut entry) => {
                    // overwrite old account state.
                    entry.get_mut().0 = old_info;
                }
            }
            // insert old info into reverts.
            reverts.entry(block_number).or_default().entry(address).or_default().0 = Some(old_info);
        }

        // add storage changeset changes
        for (block_and_address, old_storage) in storage_changeset.into_iter().rev() {
            let BlockNumberAddress((block_number, address)) = block_and_address;
            // get account state or insert from plain state.
            let account_state = match state.entry(address) {
                hash_map::Entry::Vacant(entry) => {
                    let present_info = plain_accounts_cursor.seek_exact(address)?.map(|kv| kv.1);
                    entry.insert((present_info, present_info, HashMap::new()))
                }
                hash_map::Entry::Occupied(entry) => entry.into_mut(),
            };

            // match storage.
            match account_state.2.entry(old_storage.key) {
                hash_map::Entry::Vacant(entry) => {
                    let new_storage = plain_storage_cursor
                        .seek_by_key_subkey(address, old_storage.key)?
                        .filter(|storage| storage.key == old_storage.key)
                        .unwrap_or_default();
                    entry.insert((old_storage.value, new_storage.value));
                }
                hash_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().0 = old_storage.value;
                }
            };

            reverts
                .entry(block_number)
                .or_default()
                .entry(address)
                .or_default()
                .1
                .push(old_storage);
        }

        if UNWIND {
            // iterate over local plain state remove all account and all storages.
            for (address, (old_account, new_account, storage)) in state.iter() {
                // revert account if needed.
                if old_account != new_account {
                    let existing_entry = plain_accounts_cursor.seek_exact(*address)?;
                    if let Some(account) = old_account {
                        plain_accounts_cursor.upsert(*address, *account)?;
                    } else if existing_entry.is_some() {
                        plain_accounts_cursor.delete_current()?;
                    }
                }

                // revert storages
                for (storage_key, (old_storage_value, _new_storage_value)) in storage {
                    let storage_entry =
                        StorageEntry { key: *storage_key, value: *old_storage_value };
                    // delete previous value
                    // TODO: This does not use dupsort features
                    if plain_storage_cursor
                        .seek_by_key_subkey(*address, *storage_key)?
                        .filter(|s| s.key == *storage_key)
                        .is_some()
                    {
                        plain_storage_cursor.delete_current()?
                    }

                    // insert value if needed
                    if *old_storage_value != U256::ZERO {
                        plain_storage_cursor.upsert(*address, storage_entry)?;
                    }
                }
            }
        }

        // iterate over block body and create ExecutionResult
        let mut receipt_iter = self
            .get_or_take::<tables::Receipts, UNWIND>(from_transaction_num..=to_transaction_num)?
            .into_iter();

        let mut receipts = Vec::new();
        // loop break if we are at the end of the blocks.
        for (_, block_body) in block_bodies.into_iter() {
            let mut block_receipts = Vec::with_capacity(block_body.tx_count as usize);
            for _ in block_body.tx_num_range() {
                if let Some((_, receipt)) = receipt_iter.next() {
                    block_receipts.push(Some(receipt));
                }
            }
            receipts.push(block_receipts);
        }

        Ok(BundleStateWithReceipts::new_init(
            state,
            reverts,
            Vec::new(),
            reth_primitives::Receipts::from_vec(receipts),
            start_block_number,
        ))
    }

    /// Return list of entries from table
    ///
    /// If TAKE is true, opened cursor would be write and it would delete all values from db.
    #[inline]
    pub fn get_or_take<T: Table, const TAKE: bool>(
        &self,
        range: impl RangeBounds<T::Key>,
    ) -> Result<Vec<KeyValue<T>>, DatabaseError> {
        if TAKE {
            let mut cursor_write = self.tx.cursor_write::<T>()?;
            let mut walker = cursor_write.walk_range(range)?;
            let mut items = Vec::new();
            while let Some(i) = walker.next().transpose()? {
                walker.delete_current()?;
                items.push(i)
            }
            Ok(items)
        } else {
            self.tx.cursor_read::<T>()?.walk_range(range)?.collect::<Result<Vec<_>, _>>()
        }
    }

    /// Get requested blocks transaction with signer
    pub(crate) fn get_take_block_transaction_range<const TAKE: bool>(
        &self,
        range: impl RangeBounds<BlockNumber> + Clone,
    ) -> RethResult<Vec<(BlockNumber, Vec<TransactionSignedEcRecovered>)>> {
        // Raad range of block bodies to get all transactions id's of this range.
        let block_bodies = self.get_or_take::<tables::BlockBodyIndices, false>(range)?;

        if block_bodies.is_empty() {
            return Ok(Vec::new())
        }

        // Compute the first and last tx ID in the range
        let first_transaction = block_bodies.first().expect("If we have headers").1.first_tx_num();
        let last_transaction = block_bodies.last().expect("Not empty").1.last_tx_num();

        // If this is the case then all of the blocks in the range are empty
        if last_transaction < first_transaction {
            return Ok(block_bodies.into_iter().map(|(n, _)| (n, Vec::new())).collect())
        }

        // Get transactions and senders
        let transactions = self
            .get_or_take::<tables::Transactions, TAKE>(first_transaction..=last_transaction)?
            .into_iter()
            .map(|(id, tx)| (id, tx.into()))
            .collect::<Vec<(u64, TransactionSigned)>>();

        let mut senders =
            self.get_or_take::<tables::TxSenders, TAKE>(first_transaction..=last_transaction)?;

        // Recover senders manually if not found in db
        // SAFETY: Transactions are always guaranteed to be in the database whereas
        // senders might be pruned.
        if senders.len() != transactions.len() {
            senders.reserve(transactions.len() - senders.len());
            // Find all missing senders, their corresponding tx numbers and indexes to the original
            // `senders` vector at which the recovered senders will be inserted.
            let mut missing_senders = Vec::with_capacity(transactions.len() - senders.len());
            {
                let mut senders = senders.iter().peekable();

                // `transactions` contain all entries. `senders` contain _some_ of the senders for
                // these transactions. Both are sorted and indexed by `TxNumber`.
                //
                // The general idea is to iterate on both `transactions` and `senders`, and advance
                // the `senders` iteration only if it matches the current `transactions` entry's
                // `TxNumber`. Otherwise, add the transaction to the list of missing senders.
                for (i, (tx_number, transaction)) in transactions.iter().enumerate() {
                    if let Some((sender_tx_number, _)) = senders.peek() {
                        if sender_tx_number == tx_number {
                            // If current sender's `TxNumber` matches current transaction's
                            // `TxNumber`, advance the senders iterator.
                            senders.next();
                        } else {
                            // If current sender's `TxNumber` doesn't match current transaction's
                            // `TxNumber`, add it to missing senders.
                            missing_senders.push((i, tx_number, transaction));
                        }
                    } else {
                        // If there's no more senders left, but we're still iterating over
                        // transactions, add them to missing senders
                        missing_senders.push((i, tx_number, transaction));
                    }
                }
            }

            // Recover senders
            let recovered_senders = TransactionSigned::recover_signers(
                missing_senders.iter().map(|(_, _, tx)| *tx).collect::<Vec<_>>(),
                missing_senders.len(),
            )
            .ok_or(BlockExecutionError::Validation(BlockValidationError::SenderRecoveryError))?;

            // Insert recovered senders along with tx numbers at the corresponding indexes to the
            // original `senders` vector
            for ((i, tx_number, _), sender) in missing_senders.into_iter().zip(recovered_senders) {
                // Insert will put recovered senders at necessary positions and shift the rest
                senders.insert(i, (*tx_number, sender));
            }

            // Debug assertions which are triggered during the test to ensure that all senders are
            // present and sorted
            debug_assert_eq!(senders.len(), transactions.len(), "missing one or more senders");
            debug_assert!(
                senders.iter().tuple_windows().all(|(a, b)| a.0 < b.0),
                "senders not sorted"
            );
        }

        if TAKE {
            // Remove TxHashNumber
            let mut tx_hash_cursor = self.tx.cursor_write::<tables::TxHashNumber>()?;
            for (_, tx) in transactions.iter() {
                if tx_hash_cursor.seek_exact(tx.hash())?.is_some() {
                    tx_hash_cursor.delete_current()?;
                }
            }

            // Remove TransactionBlock index if there are transaction present
            if !transactions.is_empty() {
                let tx_id_range = transactions.first().unwrap().0..=transactions.last().unwrap().0;
                self.get_or_take::<tables::TransactionBlock, TAKE>(tx_id_range)?;
            }
        }

        // Merge transaction into blocks
        let mut block_tx = Vec::with_capacity(block_bodies.len());
        let mut senders = senders.into_iter();
        let mut transactions = transactions.into_iter();
        for (block_number, block_body) in block_bodies {
            let mut one_block_tx = Vec::with_capacity(block_body.tx_count as usize);
            for _ in block_body.tx_num_range() {
                let tx = transactions.next();
                let sender = senders.next();

                let recovered = match (tx, sender) {
                    (Some((tx_id, tx)), Some((sender_tx_id, sender))) => {
                        if tx_id != sender_tx_id {
                            Err(ProviderError::MismatchOfTransactionAndSenderId { tx_id })
                        } else {
                            Ok(TransactionSignedEcRecovered::from_signed_transaction(tx, sender))
                        }
                    }
                    (Some((tx_id, _)), _) | (_, Some((tx_id, _))) => {
                        Err(ProviderError::MismatchOfTransactionAndSenderId { tx_id })
                    }
                    (None, None) => Err(ProviderError::BlockBodyTransactionCount),
                }?;
                one_block_tx.push(recovered)
            }
            block_tx.push((block_number, one_block_tx));
        }

        Ok(block_tx)
    }

    /// Return range of blocks and its execution result
    fn get_take_block_range<const TAKE: bool>(
        &self,
        chain_spec: &ChainSpec,
        range: impl RangeBounds<BlockNumber> + Clone,
    ) -> RethResult<Vec<SealedBlockWithSenders>> {
        // For block we need Headers, Bodies, Uncles, withdrawals, Transactions, Signers

        let block_headers = self.get_or_take::<tables::Headers, TAKE>(range.clone())?;
        if block_headers.is_empty() {
            return Ok(Vec::new())
        }

        let block_header_hashes =
            self.get_or_take::<tables::CanonicalHeaders, TAKE>(range.clone())?;
        let block_ommers = self.get_or_take::<tables::BlockOmmers, TAKE>(range.clone())?;
        let block_withdrawals =
            self.get_or_take::<tables::BlockWithdrawals, TAKE>(range.clone())?;

        let block_tx = self.get_take_block_transaction_range::<TAKE>(range.clone())?;

        if TAKE {
            // rm HeaderTD
            self.get_or_take::<tables::HeaderTD, TAKE>(range)?;
            // rm HeaderNumbers
            let mut header_number_cursor = self.tx.cursor_write::<tables::HeaderNumbers>()?;
            for (_, hash) in block_header_hashes.iter() {
                if header_number_cursor.seek_exact(*hash)?.is_some() {
                    header_number_cursor.delete_current()?;
                }
            }
        }

        // merge all into block
        let block_header_iter = block_headers.into_iter();
        let block_header_hashes_iter = block_header_hashes.into_iter();
        let block_tx_iter = block_tx.into_iter();

        // Ommers can be empty for some blocks
        let mut block_ommers_iter = block_ommers.into_iter();
        let mut block_withdrawals_iter = block_withdrawals.into_iter();
        let mut block_ommers = block_ommers_iter.next();
        let mut block_withdrawals = block_withdrawals_iter.next();

        let mut blocks = Vec::new();
        for ((main_block_number, header), (_, header_hash), (_, tx)) in
            izip!(block_header_iter.into_iter(), block_header_hashes_iter, block_tx_iter)
        {
            let header = header.seal(header_hash);

            let (body, senders) = tx.into_iter().map(|tx| tx.to_components()).unzip();

            // Ommers can be missing
            let mut ommers = Vec::new();
            if let Some((block_number, _)) = block_ommers.as_ref() {
                if *block_number == main_block_number {
                    ommers = block_ommers.take().unwrap().1.ommers;
                    block_ommers = block_ommers_iter.next();
                }
            };

            // withdrawal can be missing
            let shanghai_is_active =
                chain_spec.fork(Hardfork::Shanghai).active_at_timestamp(header.timestamp);
            let mut withdrawals = Some(Vec::new());
            if shanghai_is_active {
                if let Some((block_number, _)) = block_withdrawals.as_ref() {
                    if *block_number == main_block_number {
                        withdrawals = Some(block_withdrawals.take().unwrap().1.withdrawals);
                        block_withdrawals = block_withdrawals_iter.next();
                    }
                }
            } else {
                withdrawals = None
            }

            blocks.push(SealedBlockWithSenders {
                block: SealedBlock { header, body, ommers, withdrawals },
                senders,
            })
        }

        Ok(blocks)
    }

    /// Unwind table by some number key.
    /// Returns number of rows unwound.
    ///
    /// Note: Key is not inclusive and specified key would stay in db.
    #[inline]
    pub fn unwind_table_by_num<T>(&self, num: u64) -> Result<usize, DatabaseError>
    where
        T: Table<Key = u64>,
    {
        self.unwind_table::<T, _>(num, |key| key)
    }

    /// Unwind the table to a provided number key.
    /// Returns number of rows unwound.
    ///
    /// Note: Key is not inclusive and specified key would stay in db.
    pub(crate) fn unwind_table<T, F>(
        &self,
        key: u64,
        mut selector: F,
    ) -> Result<usize, DatabaseError>
    where
        T: Table,
        F: FnMut(T::Key) -> u64,
    {
        let mut cursor = self.tx.cursor_write::<T>()?;
        let mut reverse_walker = cursor.walk_back(None)?;
        let mut deleted = 0;

        while let Some(Ok((entry_key, _))) = reverse_walker.next() {
            if selector(entry_key.clone()) <= key {
                break
            }
            reverse_walker.delete_current()?;
            deleted += 1;
        }

        Ok(deleted)
    }

    /// Unwind a table forward by a [Walker][reth_db::abstraction::cursor::Walker] on another table
    pub fn unwind_table_by_walker<T1, T2>(&self, start_at: T1::Key) -> Result<(), DatabaseError>
    where
        T1: Table,
        T2: Table<Key = T1::Value>,
    {
        let mut cursor = self.tx.cursor_write::<T1>()?;
        let mut walker = cursor.walk(Some(start_at))?;
        while let Some((_, value)) = walker.next().transpose()? {
            self.tx.delete::<T2>(value, None)?;
        }
        Ok(())
    }

    /// Prune the table for the specified pre-sorted key iterator.
    ///
    /// Returns number of rows pruned.
    pub fn prune_table_with_iterator<T: Table>(
        &self,
        keys: impl IntoIterator<Item = T::Key>,
        limit: usize,
        mut delete_callback: impl FnMut(TableRow<T>),
    ) -> Result<(usize, bool), DatabaseError> {
        let mut cursor = self.tx.cursor_write::<T>()?;
        let mut deleted = 0;

        let mut keys = keys.into_iter();

        if limit != 0 {
            for key in &mut keys {
                let row = cursor.seek_exact(key.clone())?;
                if let Some(row) = row {
                    cursor.delete_current()?;
                    deleted += 1;
                    delete_callback(row);
                }

                if deleted == limit {
                    break
                }
            }
        }

        Ok((deleted, keys.next().is_none()))
    }

    /// Prune the table for the specified key range.
    ///
    /// Returns number of rows pruned.
    pub fn prune_table_with_range<T: Table>(
        &self,
        keys: impl RangeBounds<T::Key> + Clone + Debug,
        limit: usize,
        mut skip_filter: impl FnMut(&TableRow<T>) -> bool,
        mut delete_callback: impl FnMut(TableRow<T>),
    ) -> Result<(usize, bool), DatabaseError> {
        let mut cursor = self.tx.cursor_write::<T>()?;
        let mut walker = cursor.walk_range(keys)?;
        let mut deleted = 0;

        if limit != 0 {
            while let Some(row) = walker.next().transpose()? {
                if !skip_filter(&row) {
                    walker.delete_current()?;
                    deleted += 1;
                    delete_callback(row);
                }

                if deleted == limit {
                    break
                }
            }
        }

        Ok((deleted, walker.next().transpose()?.is_none()))
    }

    /// Load shard and remove it. If list is empty, last shard was full or
    /// there are no shards at all.
    fn take_shard<T>(&self, key: T::Key) -> RethResult<Vec<u64>>
    where
        T: Table<Value = BlockNumberList>,
    {
        let mut cursor = self.tx.cursor_read::<T>()?;
        let shard = cursor.seek_exact(key)?;
        if let Some((shard_key, list)) = shard {
            // delete old shard so new one can be inserted.
            self.tx.delete::<T>(shard_key, None)?;
            let list = list.iter(0).map(|i| i as u64).collect::<Vec<_>>();
            return Ok(list)
        }
        Ok(Vec::new())
    }

    /// Insert history index to the database.
    ///
    /// For each updated partial key, this function removes the last shard from
    /// the database (if any), appends the new indices to it, chunks the resulting integer list and
    /// inserts the new shards back into the database.
    ///
    /// This function is used by history indexing stages.
    fn append_history_index<P, T>(
        &self,
        index_updates: BTreeMap<P, Vec<u64>>,
        mut sharded_key_factory: impl FnMut(P, BlockNumber) -> T::Key,
    ) -> RethResult<()>
    where
        P: Copy,
        T: Table<Value = BlockNumberList>,
    {
        for (partial_key, indices) in index_updates {
            let last_shard = self.take_shard::<T>(sharded_key_factory(partial_key, u64::MAX))?;
            // chunk indices and insert them in shards of N size.
            let indices = last_shard.iter().chain(indices.iter());
            let chunks = indices
                .chunks(sharded_key::NUM_OF_INDICES_IN_SHARD)
                .into_iter()
                .map(|chunks| chunks.map(|i| *i as usize).collect::<Vec<usize>>())
                .collect::<Vec<_>>();

            let mut chunks = chunks.into_iter().peekable();
            while let Some(list) = chunks.next() {
                let highest_block_number = if chunks.peek().is_some() {
                    *list.last().expect("`chunks` does not return empty list") as u64
                } else {
                    // Insert last list with u64::MAX
                    u64::MAX
                };
                self.tx.put::<T>(
                    sharded_key_factory(partial_key, highest_block_number),
                    BlockNumberList::new_pre_sorted(list),
                )?;
            }
        }
        Ok(())
    }
}

impl<TX: DbTx> AccountReader for DatabaseProvider<TX> {
    fn basic_account(&self, address: Address) -> RethResult<Option<Account>> {
        Ok(self.tx.get::<tables::PlainAccountState>(address)?)
    }
}

impl<TX: DbTx> AccountExtReader for DatabaseProvider<TX> {
    fn changed_accounts_with_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> RethResult<BTreeSet<Address>> {
        self.tx
            .cursor_read::<tables::AccountChangeSet>()?
            .walk_range(range)?
            .map(|entry| {
                entry.map(|(_, account_before)| account_before.address).map_err(Into::into)
            })
            .collect()
    }

    fn basic_accounts(
        &self,
        iter: impl IntoIterator<Item = Address>,
    ) -> RethResult<Vec<(Address, Option<Account>)>> {
        let mut plain_accounts = self.tx.cursor_read::<tables::PlainAccountState>()?;
        Ok(iter
            .into_iter()
            .map(|address| plain_accounts.seek_exact(address).map(|a| (address, a.map(|(_, v)| v))))
            .collect::<Result<Vec<_>, _>>()?)
    }

    fn changed_accounts_and_blocks_with_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> RethResult<BTreeMap<Address, Vec<u64>>> {
        let mut changeset_cursor = self.tx.cursor_read::<tables::AccountChangeSet>()?;

        let account_transitions = changeset_cursor.walk_range(range)?.try_fold(
            BTreeMap::new(),
            |mut accounts: BTreeMap<Address, Vec<u64>>, entry| -> RethResult<_> {
                let (index, account) = entry?;
                accounts.entry(account.address).or_default().push(index);
                Ok(accounts)
            },
        )?;

        Ok(account_transitions)
    }
}

impl<TX: DbTx> ChangeSetReader for DatabaseProvider<TX> {
    fn account_block_changeset(
        &self,
        block_number: BlockNumber,
    ) -> RethResult<Vec<AccountBeforeTx>> {
        let range = block_number..=block_number;
        self.tx
            .cursor_read::<tables::AccountChangeSet>()?
            .walk_range(range)?
            .map(|result| -> RethResult<_> {
                let (_, account_before) = result?;
                Ok(account_before)
            })
            .collect()
    }
}

impl<TX: DbTx> HeaderProvider for DatabaseProvider<TX> {
    fn header(&self, block_hash: &BlockHash) -> RethResult<Option<Header>> {
        if let Some(num) = self.block_number(*block_hash)? {
            Ok(self.header_by_number(num)?)
        } else {
            Ok(None)
        }
    }

    fn header_by_number(&self, num: BlockNumber) -> RethResult<Option<Header>> {
        Ok(self.tx.get::<tables::Headers>(num)?)
    }

    fn header_td(&self, block_hash: &BlockHash) -> RethResult<Option<U256>> {
        if let Some(num) = self.block_number(*block_hash)? {
            self.header_td_by_number(num)
        } else {
            Ok(None)
        }
    }

    fn header_td_by_number(&self, number: BlockNumber) -> RethResult<Option<U256>> {
        if let Some(td) = self.chain_spec.final_paris_total_difficulty(number) {
            // if this block is higher than the final paris(merge) block, return the final paris
            // difficulty
            return Ok(Some(td))
        }

        Ok(self.tx.get::<tables::HeaderTD>(number)?.map(|td| td.0))
    }

    fn headers_range(&self, range: impl RangeBounds<BlockNumber>) -> RethResult<Vec<Header>> {
        let mut cursor = self.tx.cursor_read::<tables::Headers>()?;
        cursor
            .walk_range(range)?
            .map(|result| result.map(|(_, header)| header).map_err(Into::into))
            .collect::<RethResult<Vec<_>>>()
    }

    fn sealed_headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> RethResult<Vec<SealedHeader>> {
        let mut headers = vec![];
        for entry in self.tx.cursor_read::<tables::Headers>()?.walk_range(range)? {
            let (number, header) = entry?;
            let hash = self
                .block_hash(number)?
                .ok_or_else(|| ProviderError::HeaderNotFound(number.into()))?;
            headers.push(header.seal(hash));
        }
        Ok(headers)
    }

    fn sealed_header(&self, number: BlockNumber) -> RethResult<Option<SealedHeader>> {
        if let Some(header) = self.header_by_number(number)? {
            let hash = self
                .block_hash(number)?
                .ok_or_else(|| ProviderError::HeaderNotFound(number.into()))?;
            Ok(Some(header.seal(hash)))
        } else {
            Ok(None)
        }
    }
}

impl<TX: DbTx> BlockHashReader for DatabaseProvider<TX> {
    fn block_hash(&self, number: u64) -> RethResult<Option<B256>> {
        Ok(self.tx.get::<tables::CanonicalHeaders>(number)?)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> RethResult<Vec<B256>> {
        let range = start..end;
        let mut cursor = self.tx.cursor_read::<tables::CanonicalHeaders>()?;
        cursor
            .walk_range(range)?
            .map(|result| result.map(|(_, hash)| hash).map_err(Into::into))
            .collect::<RethResult<Vec<_>>>()
    }
}

impl<TX: DbTx> BlockNumReader for DatabaseProvider<TX> {
    fn chain_info(&self) -> RethResult<ChainInfo> {
        let best_number = self.best_block_number()?;
        let best_hash = self.block_hash(best_number)?.unwrap_or_default();
        Ok(ChainInfo { best_hash, best_number })
    }

    fn best_block_number(&self) -> RethResult<BlockNumber> {
        Ok(self
            .get_stage_checkpoint(StageId::Finish)?
            .map(|checkpoint| checkpoint.block_number)
            .unwrap_or_default())
    }

    fn last_block_number(&self) -> RethResult<BlockNumber> {
        Ok(self.tx.cursor_read::<tables::CanonicalHeaders>()?.last()?.unwrap_or_default().0)
    }

    fn block_number(&self, hash: B256) -> RethResult<Option<BlockNumber>> {
        Ok(self.tx.get::<tables::HeaderNumbers>(hash)?)
    }
}

impl<TX: DbTx> BlockReader for DatabaseProvider<TX> {
    fn find_block_by_hash(&self, hash: B256, source: BlockSource) -> RethResult<Option<Block>> {
        if source.is_database() {
            self.block(hash.into())
        } else {
            Ok(None)
        }
    }

    /// Returns the block with matching number from database.
    ///
    /// If the header for this block is not found, this returns `None`.
    /// If the header is found, but the transactions either do not exist, or are not indexed, this
    /// will return None.
    fn block(&self, id: BlockHashOrNumber) -> RethResult<Option<Block>> {
        if let Some(number) = self.convert_hash_or_number(id)? {
            if let Some(header) = self.header_by_number(number)? {
                let withdrawals = self.withdrawals_by_block(number.into(), header.timestamp)?;
                let ommers = self.ommers(number.into())?.unwrap_or_default();
                // If the body indices are not found, this means that the transactions either do not
                // exist in the database yet, or they do exit but are not indexed.
                // If they exist but are not indexed, we don't have enough
                // information to return the block anyways, so we return `None`.
                let transactions = match self.transactions_by_block(number.into())? {
                    Some(transactions) => transactions,
                    None => return Ok(None),
                };

                return Ok(Some(Block { header, body: transactions, ommers, withdrawals }))
            }
        }

        Ok(None)
    }

    fn pending_block(&self) -> RethResult<Option<SealedBlock>> {
        Ok(None)
    }

    fn pending_block_and_receipts(&self) -> RethResult<Option<(SealedBlock, Vec<Receipt>)>> {
        Ok(None)
    }

    fn ommers(&self, id: BlockHashOrNumber) -> RethResult<Option<Vec<Header>>> {
        if let Some(number) = self.convert_hash_or_number(id)? {
            // If the Paris (Merge) hardfork block is known and block is after it, return empty
            // ommers.
            if self.chain_spec.final_paris_total_difficulty(number).is_some() {
                return Ok(Some(Vec::new()))
            }

            let ommers = self.tx.get::<tables::BlockOmmers>(number)?.map(|o| o.ommers);
            return Ok(ommers)
        }

        Ok(None)
    }

    fn block_body_indices(&self, num: u64) -> RethResult<Option<StoredBlockBodyIndices>> {
        Ok(self.tx.get::<tables::BlockBodyIndices>(num)?)
    }

    /// Returns the block with senders with matching number from database.
    ///
    /// **NOTE: The transactions have invalid hashes, since they would need to be calculated on the
    /// spot, and we want fast querying.**
    ///
    /// If the header for this block is not found, this returns `None`.
    /// If the header is found, but the transactions either do not exist, or are not indexed, this
    /// will return None.
    fn block_with_senders(
        &self,
        block_number: BlockNumber,
        transaction_kind: TransactionVariant,
    ) -> RethResult<Option<BlockWithSenders>> {
        let Some(header) = self.header_by_number(block_number)? else { return Ok(None) };

        let ommers = self.ommers(block_number.into())?.unwrap_or_default();
        let withdrawals = self.withdrawals_by_block(block_number.into(), header.timestamp)?;

        // Get the block body
        //
        // If the body indices are not found, this means that the transactions either do not exist
        // in the database yet, or they do exit but are not indexed. If they exist but are not
        // indexed, we don't have enough information to return the block anyways, so we return
        // `None`.
        let body = match self.block_body_indices(block_number)? {
            Some(body) => body,
            None => return Ok(None),
        };

        let tx_range = body.tx_num_range();

        let (transactions, senders) = if tx_range.is_empty() {
            (vec![], vec![])
        } else {
            (self.transactions_by_tx_range(tx_range.clone())?, self.senders_by_tx_range(tx_range)?)
        };

        let body = transactions
            .into_iter()
            .map(|tx| match transaction_kind {
                TransactionVariant::NoHash => TransactionSigned {
                    // Caller explicitly asked for no hash, so we don't calculate it
                    hash: Default::default(),
                    signature: tx.signature,
                    transaction: tx.transaction,
                },
                TransactionVariant::WithHash => tx.with_hash(),
            })
            .collect();

        Ok(Some(Block { header, body, ommers, withdrawals }.with_senders(senders)))
    }

    fn block_range(&self, range: RangeInclusive<BlockNumber>) -> RethResult<Vec<Block>> {
        if range.is_empty() {
            return Ok(Vec::new())
        }

        let len = range.end().saturating_sub(*range.start()) as usize;
        let mut blocks = Vec::with_capacity(len);

        let mut headers_cursor = self.tx.cursor_read::<tables::Headers>()?;
        let mut ommers_cursor = self.tx.cursor_read::<tables::BlockOmmers>()?;
        let mut withdrawals_cursor = self.tx.cursor_read::<tables::BlockWithdrawals>()?;
        let mut block_body_cursor = self.tx.cursor_read::<tables::BlockBodyIndices>()?;
        let mut tx_cursor = self.tx.cursor_read::<tables::Transactions>()?;

        for num in range {
            if let Some((_, header)) = headers_cursor.seek_exact(num)? {
                // If the body indices are not found, this means that the transactions either do
                // not exist in the database yet, or they do exit but are
                // not indexed. If they exist but are not indexed, we don't
                // have enough information to return the block anyways, so
                // we skip the block.
                if let Some((_, block_body_indices)) = block_body_cursor.seek_exact(num)? {
                    let tx_range = block_body_indices.tx_num_range();
                    let body = if tx_range.is_empty() {
                        Vec::new()
                    } else {
                        tx_cursor
                            .walk_range(tx_range)?
                            .map(|result| result.map(|(_, tx)| tx.into()))
                            .collect::<Result<Vec<_>, _>>()?
                    };

                    // If we are past shanghai, then all blocks should have a withdrawal list,
                    // even if empty
                    let withdrawals =
                        if self.chain_spec.is_shanghai_active_at_timestamp(header.timestamp) {
                            Some(
                                withdrawals_cursor
                                    .seek_exact(num)?
                                    .map(|(_, w)| w.withdrawals)
                                    .unwrap_or_default(),
                            )
                        } else {
                            None
                        };
                    let ommers = if self.chain_spec.final_paris_total_difficulty(num).is_some() {
                        Vec::new()
                    } else {
                        ommers_cursor.seek_exact(num)?.map(|(_, o)| o.ommers).unwrap_or_default()
                    };

                    blocks.push(Block { header, body, ommers, withdrawals });
                }
            }
        }
        Ok(blocks)
    }
}

impl<TX: DbTx> TransactionsProviderExt for DatabaseProvider<TX> {
    /// Recovers transaction hashes by walking through `Transactions` table and
    /// calculating them in a parallel manner. Returned unsorted.
    fn transaction_hashes_by_range(
        &self,
        tx_range: Range<TxNumber>,
    ) -> RethResult<Vec<(TxHash, TxNumber)>> {
        let mut tx_cursor = self.tx.cursor_read::<tables::Transactions>()?;
        let tx_range_size = tx_range.clone().count();
        let tx_walker = tx_cursor.walk_range(tx_range)?;

        let chunk_size = (tx_range_size / rayon::current_num_threads()).max(1);
        let mut channels = Vec::with_capacity(chunk_size);
        let mut transaction_count = 0;

        #[inline]
        fn calculate_hash(
            entry: Result<(TxNumber, TransactionSignedNoHash), DatabaseError>,
            rlp_buf: &mut Vec<u8>,
        ) -> Result<(B256, TxNumber), Box<RethError>> {
            let (tx_id, tx) = entry.map_err(|e| Box::new(e.into()))?;
            tx.transaction.encode_with_signature(&tx.signature, rlp_buf, false);
            Ok((keccak256(rlp_buf), tx_id))
        }

        for chunk in &tx_walker.chunks(chunk_size) {
            let (tx, rx) = mpsc::channel();
            channels.push(rx);

            // Note: Unfortunate side-effect of how chunk is designed in itertools (it is not Send)
            let chunk: Vec<_> = chunk.collect();
            transaction_count += chunk.len();

            // Spawn the task onto the global rayon pool
            // This task will send the results through the channel after it has calculated the hash.
            rayon::spawn(move || {
                let mut rlp_buf = Vec::with_capacity(128);
                for entry in chunk {
                    rlp_buf.clear();
                    let _ = tx.send(calculate_hash(entry, &mut rlp_buf));
                }
            });
        }
        let mut tx_list = Vec::with_capacity(transaction_count);

        // Iterate over channels and append the tx hashes unsorted
        for channel in channels {
            while let Ok(tx) = channel.recv() {
                let (tx_hash, tx_id) = tx.map_err(|boxed| *boxed)?;
                tx_list.push((tx_hash, tx_id));
            }
        }

        Ok(tx_list)
    }
}

/// Calculates the hash of the given transaction

impl<TX: DbTx> TransactionsProvider for DatabaseProvider<TX> {
    fn transaction_id(&self, tx_hash: TxHash) -> RethResult<Option<TxNumber>> {
        Ok(self.tx.get::<tables::TxHashNumber>(tx_hash)?)
    }

    fn transaction_by_id(&self, id: TxNumber) -> RethResult<Option<TransactionSigned>> {
        Ok(self.tx.get::<tables::Transactions>(id)?.map(Into::into))
    }

    fn transaction_by_id_no_hash(
        &self,
        id: TxNumber,
    ) -> RethResult<Option<TransactionSignedNoHash>> {
        Ok(self.tx.get::<tables::Transactions>(id)?)
    }

    fn transaction_by_hash(&self, hash: TxHash) -> RethResult<Option<TransactionSigned>> {
        if let Some(id) = self.transaction_id(hash)? {
            Ok(self.transaction_by_id_no_hash(id)?.map(|tx| TransactionSigned {
                hash,
                signature: tx.signature,
                transaction: tx.transaction,
            }))
        } else {
            Ok(None)
        }
        .map(|tx| tx.map(Into::into))
    }

    fn transaction_by_hash_with_meta(
        &self,
        tx_hash: TxHash,
    ) -> RethResult<Option<(TransactionSigned, TransactionMeta)>> {
        let mut transaction_cursor = self.tx.cursor_read::<tables::TransactionBlock>()?;
        if let Some(transaction_id) = self.transaction_id(tx_hash)? {
            if let Some(tx) = self.transaction_by_id_no_hash(transaction_id)? {
                let transaction = TransactionSigned {
                    hash: tx_hash,
                    signature: tx.signature,
                    transaction: tx.transaction,
                };
                if let Some(block_number) =
                    transaction_cursor.seek(transaction_id).map(|b| b.map(|(_, bn)| bn))?
                {
                    if let Some(sealed_header) = self.sealed_header(block_number)? {
                        let (header, block_hash) = sealed_header.split();
                        if let Some(block_body) = self.block_body_indices(block_number)? {
                            // the index of the tx in the block is the offset:
                            // len([start..tx_id])
                            // SAFETY: `transaction_id` is always `>=` the block's first
                            // index
                            let index = transaction_id - block_body.first_tx_num();

                            let meta = TransactionMeta {
                                tx_hash,
                                index,
                                block_hash,
                                block_number,
                                base_fee: header.base_fee_per_gas,
                                excess_blob_gas: header.excess_blob_gas,
                            };

                            return Ok(Some((transaction, meta)))
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    fn transaction_block(&self, id: TxNumber) -> RethResult<Option<BlockNumber>> {
        let mut cursor = self.tx.cursor_read::<tables::TransactionBlock>()?;
        Ok(cursor.seek(id)?.map(|(_, bn)| bn))
    }

    fn transactions_by_block(
        &self,
        id: BlockHashOrNumber,
    ) -> RethResult<Option<Vec<TransactionSigned>>> {
        let mut tx_cursor = self.tx.cursor_read::<tables::Transactions>()?;
        if let Some(block_number) = self.convert_hash_or_number(id)? {
            if let Some(body) = self.block_body_indices(block_number)? {
                let tx_range = body.tx_num_range();
                return if tx_range.is_empty() {
                    Ok(Some(Vec::new()))
                } else {
                    let transactions = tx_cursor
                        .walk_range(tx_range)?
                        .map(|result| result.map(|(_, tx)| tx.into()))
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(Some(transactions))
                }
            }
        }
        Ok(None)
    }

    fn transactions_by_block_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> RethResult<Vec<Vec<TransactionSigned>>> {
        let mut results = Vec::new();
        let mut body_cursor = self.tx.cursor_read::<tables::BlockBodyIndices>()?;
        let mut tx_cursor = self.tx.cursor_read::<tables::Transactions>()?;
        for entry in body_cursor.walk_range(range)? {
            let (_, body) = entry?;
            let tx_num_range = body.tx_num_range();
            if tx_num_range.is_empty() {
                results.push(Vec::new());
            } else {
                results.push(
                    tx_cursor
                        .walk_range(tx_num_range)?
                        .map(|result| result.map(|(_, tx)| tx.into()))
                        .collect::<Result<Vec<_>, _>>()?,
                );
            }
        }
        Ok(results)
    }

    fn transactions_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> RethResult<Vec<TransactionSignedNoHash>> {
        Ok(self
            .tx
            .cursor_read::<tables::Transactions>()?
            .walk_range(range)?
            .map(|entry| entry.map(|tx| tx.1))
            .collect::<Result<Vec<_>, _>>()?)
    }

    fn senders_by_tx_range(&self, range: impl RangeBounds<TxNumber>) -> RethResult<Vec<Address>> {
        Ok(self
            .tx
            .cursor_read::<tables::TxSenders>()?
            .walk_range(range)?
            .map(|entry| entry.map(|sender| sender.1))
            .collect::<Result<Vec<_>, _>>()?)
    }

    fn transaction_sender(&self, id: TxNumber) -> RethResult<Option<Address>> {
        Ok(self.tx.get::<tables::TxSenders>(id)?)
    }
}

impl<TX: DbTx> ReceiptProvider for DatabaseProvider<TX> {
    fn receipt(&self, id: TxNumber) -> RethResult<Option<Receipt>> {
        Ok(self.tx.get::<tables::Receipts>(id)?)
    }

    fn receipt_by_hash(&self, hash: TxHash) -> RethResult<Option<Receipt>> {
        if let Some(id) = self.transaction_id(hash)? {
            self.receipt(id)
        } else {
            Ok(None)
        }
    }

    fn receipts_by_block(&self, block: BlockHashOrNumber) -> RethResult<Option<Vec<Receipt>>> {
        if let Some(number) = self.convert_hash_or_number(block)? {
            if let Some(body) = self.block_body_indices(number)? {
                let tx_range = body.tx_num_range();
                return if tx_range.is_empty() {
                    Ok(Some(Vec::new()))
                } else {
                    let mut receipts_cursor = self.tx.cursor_read::<tables::Receipts>()?;
                    let receipts = receipts_cursor
                        .walk_range(tx_range)?
                        .map(|result| result.map(|(_, receipt)| receipt))
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(Some(receipts))
                }
            }
        }
        Ok(None)
    }
}

impl<TX: DbTx> WithdrawalsProvider for DatabaseProvider<TX> {
    fn withdrawals_by_block(
        &self,
        id: BlockHashOrNumber,
        timestamp: u64,
    ) -> RethResult<Option<Vec<Withdrawal>>> {
        if self.chain_spec.is_shanghai_active_at_timestamp(timestamp) {
            if let Some(number) = self.convert_hash_or_number(id)? {
                // If we are past shanghai, then all blocks should have a withdrawal list, even if
                // empty
                let withdrawals = self
                    .tx
                    .get::<tables::BlockWithdrawals>(number)
                    .map(|w| w.map(|w| w.withdrawals))?
                    .unwrap_or_default();
                return Ok(Some(withdrawals))
            }
        }
        Ok(None)
    }

    fn latest_withdrawal(&self) -> RethResult<Option<Withdrawal>> {
        let latest_block_withdrawal = self.tx.cursor_read::<tables::BlockWithdrawals>()?.last()?;
        Ok(latest_block_withdrawal
            .and_then(|(_, mut block_withdrawal)| block_withdrawal.withdrawals.pop()))
    }
}

impl<TX: DbTx> EvmEnvProvider for DatabaseProvider<TX> {
    fn fill_env_at(
        &self,
        cfg: &mut CfgEnv,
        block_env: &mut BlockEnv,
        at: BlockHashOrNumber,
    ) -> RethResult<()> {
        let hash = self.convert_number(at)?.ok_or(ProviderError::HeaderNotFound(at))?;
        let header = self.header(&hash)?.ok_or(ProviderError::HeaderNotFound(at))?;
        self.fill_env_with_header(cfg, block_env, &header)
    }

    fn fill_env_with_header(
        &self,
        cfg: &mut CfgEnv,
        block_env: &mut BlockEnv,
        header: &Header,
    ) -> RethResult<()> {
        let total_difficulty = self
            .header_td_by_number(header.number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(header.number.into()))?;
        fill_cfg_and_block_env(cfg, block_env, &self.chain_spec, header, total_difficulty);
        Ok(())
    }

    fn fill_block_env_at(&self, block_env: &mut BlockEnv, at: BlockHashOrNumber) -> RethResult<()> {
        let hash = self.convert_number(at)?.ok_or(ProviderError::HeaderNotFound(at))?;
        let header = self.header(&hash)?.ok_or(ProviderError::HeaderNotFound(at))?;

        self.fill_block_env_with_header(block_env, &header)
    }

    fn fill_block_env_with_header(
        &self,
        block_env: &mut BlockEnv,
        header: &Header,
    ) -> RethResult<()> {
        let total_difficulty = self
            .header_td_by_number(header.number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(header.number.into()))?;
        let spec_id = revm_spec(
            &self.chain_spec,
            Head {
                number: header.number,
                timestamp: header.timestamp,
                difficulty: header.difficulty,
                total_difficulty,
                // Not required
                hash: Default::default(),
            },
        );
        let after_merge = spec_id >= SpecId::MERGE;
        fill_block_env(block_env, &self.chain_spec, header, after_merge);
        Ok(())
    }

    fn fill_cfg_env_at(&self, cfg: &mut CfgEnv, at: BlockHashOrNumber) -> RethResult<()> {
        let hash = self.convert_number(at)?.ok_or(ProviderError::HeaderNotFound(at))?;
        let header = self.header(&hash)?.ok_or(ProviderError::HeaderNotFound(at))?;
        self.fill_cfg_env_with_header(cfg, &header)
    }

    fn fill_cfg_env_with_header(&self, cfg: &mut CfgEnv, header: &Header) -> RethResult<()> {
        let total_difficulty = self
            .header_td_by_number(header.number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(header.number.into()))?;
        fill_cfg_env(cfg, &self.chain_spec, header, total_difficulty);
        Ok(())
    }
}

impl<TX: DbTx> StageCheckpointReader for DatabaseProvider<TX> {
    fn get_stage_checkpoint(&self, id: StageId) -> RethResult<Option<StageCheckpoint>> {
        Ok(self.tx.get::<tables::SyncStage>(id.to_string())?)
    }

    /// Get stage checkpoint progress.
    fn get_stage_checkpoint_progress(&self, id: StageId) -> RethResult<Option<Vec<u8>>> {
        Ok(self.tx.get::<tables::SyncStageProgress>(id.to_string())?)
    }
}

impl<TX: DbTxMut> StageCheckpointWriter for DatabaseProvider<TX> {
    /// Save stage checkpoint progress.
    fn save_stage_checkpoint_progress(&self, id: StageId, checkpoint: Vec<u8>) -> RethResult<()> {
        Ok(self.tx.put::<tables::SyncStageProgress>(id.to_string(), checkpoint)?)
    }

    /// Save stage checkpoint.
    fn save_stage_checkpoint(&self, id: StageId, checkpoint: StageCheckpoint) -> RethResult<()> {
        Ok(self.tx.put::<tables::SyncStage>(id.to_string(), checkpoint)?)
    }

    fn update_pipeline_stages(
        &self,
        block_number: BlockNumber,
        drop_stage_checkpoint: bool,
    ) -> RethResult<()> {
        // iterate over all existing stages in the table and update its progress.
        let mut cursor = self.tx.cursor_write::<tables::SyncStage>()?;
        for stage_id in StageId::ALL {
            let (_, checkpoint) = cursor.seek_exact(stage_id.to_string())?.unwrap_or_default();
            cursor.upsert(
                stage_id.to_string(),
                StageCheckpoint {
                    block_number,
                    ..if drop_stage_checkpoint { Default::default() } else { checkpoint }
                },
            )?;
        }

        Ok(())
    }
}

impl<TX: DbTx> StorageReader for DatabaseProvider<TX> {
    fn plainstate_storages(
        &self,
        addresses_with_keys: impl IntoIterator<Item = (Address, impl IntoIterator<Item = B256>)>,
    ) -> RethResult<Vec<(Address, Vec<StorageEntry>)>> {
        let mut plain_storage = self.tx.cursor_dup_read::<tables::PlainStorageState>()?;

        addresses_with_keys
            .into_iter()
            .map(|(address, storage)| {
                storage
                    .into_iter()
                    .map(|key| -> RethResult<_> {
                        Ok(plain_storage
                            .seek_by_key_subkey(address, key)?
                            .filter(|v| v.key == key)
                            .unwrap_or_else(|| StorageEntry { key, value: Default::default() }))
                    })
                    .collect::<RethResult<Vec<_>>>()
                    .map(|storage| (address, storage))
            })
            .collect::<RethResult<Vec<(_, _)>>>()
    }

    fn changed_storages_with_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> RethResult<BTreeMap<Address, BTreeSet<B256>>> {
        self.tx
            .cursor_read::<tables::StorageChangeSet>()?
            .walk_range(BlockNumberAddress::range(range))?
            // fold all storages and save its old state so we can remove it from HashedStorage
            // it is needed as it is dup table.
            .try_fold(BTreeMap::new(), |mut accounts: BTreeMap<Address, BTreeSet<B256>>, entry| {
                let (BlockNumberAddress((_, address)), storage_entry) = entry?;
                accounts.entry(address).or_default().insert(storage_entry.key);
                Ok(accounts)
            })
    }

    fn changed_storages_and_blocks_with_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> RethResult<BTreeMap<(Address, B256), Vec<u64>>> {
        let mut changeset_cursor = self.tx.cursor_read::<tables::StorageChangeSet>()?;

        let storage_changeset_lists =
            changeset_cursor.walk_range(BlockNumberAddress::range(range))?.try_fold(
                BTreeMap::new(),
                |mut storages: BTreeMap<(Address, B256), Vec<u64>>, entry| -> RethResult<_> {
                    let (index, storage) = entry?;
                    storages
                        .entry((index.address(), storage.key))
                        .or_default()
                        .push(index.block_number());
                    Ok(storages)
                },
            )?;

        Ok(storage_changeset_lists)
    }
}

impl<TX: DbTxMut + DbTx> HashingWriter for DatabaseProvider<TX> {
    fn insert_hashes(
        &self,
        range: RangeInclusive<BlockNumber>,
        end_block_hash: B256,
        expected_state_root: B256,
    ) -> RethResult<()> {
        // Initialize prefix sets.
        let mut account_prefix_set = PrefixSetMut::default();
        let mut storage_prefix_set: HashMap<B256, PrefixSetMut> = HashMap::default();
        let mut destroyed_accounts = HashSet::default();

        // storage hashing stage
        {
            let lists = self.changed_storages_with_range(range.clone())?;
            let storages = self.plainstate_storages(lists)?;
            let storage_entries = self.insert_storage_for_hashing(storages)?;
            for (hashed_address, hashed_slots) in storage_entries {
                account_prefix_set.insert(Nibbles::unpack(hashed_address));
                for slot in hashed_slots {
                    storage_prefix_set
                        .entry(hashed_address)
                        .or_default()
                        .insert(Nibbles::unpack(slot));
                }
            }
        }

        // account hashing stage
        {
            let lists = self.changed_accounts_with_range(range.clone())?;
            let accounts = self.basic_accounts(lists)?;
            let hashed_addresses = self.insert_account_for_hashing(accounts)?;
            for (hashed_address, account) in hashed_addresses {
                account_prefix_set.insert(Nibbles::unpack(hashed_address));
                if account.is_none() {
                    destroyed_accounts.insert(hashed_address);
                }
            }
        }

        // merkle tree
        {
            // This is the same as `StateRoot::incremental_root_with_updates`, only the prefix sets
            // are pre-loaded.
            let (state_root, trie_updates) = StateRoot::new(&self.tx)
                .with_changed_account_prefixes(account_prefix_set.freeze())
                .with_changed_storage_prefixes(
                    storage_prefix_set.into_iter().map(|(k, v)| (k, v.freeze())).collect(),
                )
                .with_destroyed_accounts(destroyed_accounts)
                .root_with_updates()
                .map_err(Into::<reth_db::DatabaseError>::into)?;
            if state_root != expected_state_root {
                return Err(ProviderError::StateRootMismatch {
                    got: state_root,
                    expected: expected_state_root,
                    block_number: *range.end(),
                    block_hash: end_block_hash,
                }
                .into())
            }
            trie_updates.flush(&self.tx)?;
        }
        Ok(())
    }

    fn unwind_storage_hashing(
        &self,
        range: Range<BlockNumberAddress>,
    ) -> RethResult<HashMap<B256, BTreeSet<B256>>> {
        let mut hashed_storage = self.tx.cursor_dup_write::<tables::HashedStorage>()?;

        // Aggregate all block changesets and make list of accounts that have been changed.
        let hashed_storages = self
            .tx
            .cursor_read::<tables::StorageChangeSet>()?
            .walk_range(range)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .rev()
            // fold all account to get the old balance/nonces and account that needs to be removed
            .fold(
                BTreeMap::new(),
                |mut accounts: BTreeMap<(Address, B256), U256>,
                 (BlockNumberAddress((_, address)), storage_entry)| {
                    accounts.insert((address, storage_entry.key), storage_entry.value);
                    accounts
                },
            )
            .into_iter()
            // hash addresses and collect it inside sorted BTreeMap.
            // We are doing keccak only once per address.
            .map(|((address, key), value)| ((keccak256(address), keccak256(key)), value))
            .collect::<BTreeMap<_, _>>();

        let mut hashed_storage_keys: HashMap<B256, BTreeSet<B256>> = HashMap::default();
        for (hashed_address, hashed_slot) in hashed_storages.keys() {
            hashed_storage_keys.entry(*hashed_address).or_default().insert(*hashed_slot);
        }

        hashed_storages
            .into_iter()
            // Apply values to HashedStorage (if Value is zero just remove it);
            .try_for_each(|((hashed_address, key), value)| -> RethResult<()> {
                if hashed_storage
                    .seek_by_key_subkey(hashed_address, key)?
                    .filter(|entry| entry.key == key)
                    .is_some()
                {
                    hashed_storage.delete_current()?;
                }

                if value != U256::ZERO {
                    hashed_storage.upsert(hashed_address, StorageEntry { key, value })?;
                }
                Ok(())
            })?;

        Ok(hashed_storage_keys)
    }

    fn insert_storage_for_hashing(
        &self,
        storages: impl IntoIterator<Item = (Address, impl IntoIterator<Item = StorageEntry>)>,
    ) -> RethResult<HashMap<B256, BTreeSet<B256>>> {
        // hash values
        let hashed_storages =
            storages.into_iter().fold(BTreeMap::new(), |mut map, (address, storage)| {
                let storage = storage.into_iter().fold(BTreeMap::new(), |mut map, entry| {
                    map.insert(keccak256(entry.key), entry.value);
                    map
                });
                map.insert(keccak256(address), storage);
                map
            });

        let hashed_storage_keys =
            HashMap::from_iter(hashed_storages.iter().map(|(hashed_address, entries)| {
                (*hashed_address, BTreeSet::from_iter(entries.keys().copied()))
            }));

        let mut hashed_storage_cursor = self.tx.cursor_dup_write::<tables::HashedStorage>()?;
        // Hash the address and key and apply them to HashedStorage (if Storage is None
        // just remove it);
        hashed_storages.into_iter().try_for_each(|(hashed_address, storage)| {
            storage.into_iter().try_for_each(|(key, value)| -> RethResult<()> {
                if hashed_storage_cursor
                    .seek_by_key_subkey(hashed_address, key)?
                    .filter(|entry| entry.key == key)
                    .is_some()
                {
                    hashed_storage_cursor.delete_current()?;
                }

                if value != U256::ZERO {
                    hashed_storage_cursor.upsert(hashed_address, StorageEntry { key, value })?;
                }
                Ok(())
            })
        })?;

        Ok(hashed_storage_keys)
    }

    fn unwind_account_hashing(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> RethResult<BTreeMap<B256, Option<Account>>> {
        let mut hashed_accounts_cursor = self.tx.cursor_write::<tables::HashedAccount>()?;

        // Aggregate all block changesets and make a list of accounts that have been changed.
        let hashed_accounts = self
            .tx
            .cursor_read::<tables::AccountChangeSet>()?
            .walk_range(range)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .rev()
            // fold all account to get the old balance/nonces and account that needs to be removed
            .fold(
                BTreeMap::new(),
                |mut accounts: BTreeMap<Address, Option<Account>>, (_, account_before)| {
                    accounts.insert(account_before.address, account_before.info);
                    accounts
                },
            )
            .into_iter()
            // hash addresses and collect it inside sorted BTreeMap.
            // We are doing keccak only once per address.
            .map(|(address, account)| (keccak256(address), account))
            .collect::<BTreeMap<_, _>>();

        hashed_accounts
            .iter()
            // Apply values to HashedState (if Account is None remove it);
            .try_for_each(|(hashed_address, account)| -> RethResult<()> {
                if let Some(account) = account {
                    hashed_accounts_cursor.upsert(*hashed_address, *account)?;
                } else if hashed_accounts_cursor.seek_exact(*hashed_address)?.is_some() {
                    hashed_accounts_cursor.delete_current()?;
                }
                Ok(())
            })?;

        Ok(hashed_accounts)
    }

    fn insert_account_for_hashing(
        &self,
        accounts: impl IntoIterator<Item = (Address, Option<Account>)>,
    ) -> RethResult<BTreeMap<B256, Option<Account>>> {
        let mut hashed_accounts_cursor = self.tx.cursor_write::<tables::HashedAccount>()?;

        let hashed_accounts = accounts.into_iter().fold(
            BTreeMap::new(),
            |mut map: BTreeMap<B256, Option<Account>>, (address, account)| {
                map.insert(keccak256(address), account);
                map
            },
        );

        hashed_accounts.iter().try_for_each(|(hashed_address, account)| -> RethResult<()> {
            if let Some(account) = account {
                hashed_accounts_cursor.upsert(*hashed_address, *account)?
            } else if hashed_accounts_cursor.seek_exact(*hashed_address)?.is_some() {
                hashed_accounts_cursor.delete_current()?;
            }
            Ok(())
        })?;

        Ok(hashed_accounts)
    }
}

impl<TX: DbTxMut + DbTx> HistoryWriter for DatabaseProvider<TX> {
    fn calculate_history_indices(&self, range: RangeInclusive<BlockNumber>) -> RethResult<()> {
        // account history stage
        {
            let indices = self.changed_accounts_and_blocks_with_range(range.clone())?;
            self.insert_account_history_index(indices)?;
        }

        // storage history stage
        {
            let indices = self.changed_storages_and_blocks_with_range(range)?;
            self.insert_storage_history_index(indices)?;
        }

        Ok(())
    }

    fn insert_storage_history_index(
        &self,
        storage_transitions: BTreeMap<(Address, B256), Vec<u64>>,
    ) -> RethResult<()> {
        self.append_history_index::<_, tables::StorageHistory>(
            storage_transitions,
            |(address, storage_key), highest_block_number| {
                StorageShardedKey::new(address, storage_key, highest_block_number)
            },
        )
    }

    fn insert_account_history_index(
        &self,
        account_transitions: BTreeMap<Address, Vec<u64>>,
    ) -> RethResult<()> {
        self.append_history_index::<_, tables::AccountHistory>(account_transitions, ShardedKey::new)
    }

    fn unwind_storage_history_indices(
        &self,
        range: Range<BlockNumberAddress>,
    ) -> RethResult<usize> {
        let storage_changesets = self
            .tx
            .cursor_read::<tables::StorageChangeSet>()?
            .walk_range(range)?
            .collect::<Result<Vec<_>, _>>()?;
        let changesets = storage_changesets.len();

        let last_indices = storage_changesets
            .into_iter()
            // reverse so we can get lowest block number where we need to unwind account.
            .rev()
            // fold all storages and get last block number
            .fold(
                BTreeMap::new(),
                |mut accounts: BTreeMap<(Address, B256), u64>, (index, storage)| {
                    // we just need address and lowest block number.
                    accounts.insert((index.address(), storage.key), index.block_number());
                    accounts
                },
            );

        let mut cursor = self.tx.cursor_write::<tables::StorageHistory>()?;
        for ((address, storage_key), rem_index) in last_indices {
            let partial_shard = unwind_history_shards::<_, tables::StorageHistory, _>(
                &mut cursor,
                StorageShardedKey::last(address, storage_key),
                rem_index,
                |storage_sharded_key| {
                    storage_sharded_key.address == address &&
                        storage_sharded_key.sharded_key.key == storage_key
                },
            )?;

            // Check the last returned partial shard.
            // If it's not empty, the shard needs to be reinserted.
            if !partial_shard.is_empty() {
                cursor.insert(
                    StorageShardedKey::last(address, storage_key),
                    BlockNumberList::new_pre_sorted(partial_shard),
                )?;
            }
        }

        Ok(changesets)
    }

    fn unwind_account_history_indices(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> RethResult<usize> {
        let account_changeset = self
            .tx
            .cursor_read::<tables::AccountChangeSet>()?
            .walk_range(range)?
            .collect::<Result<Vec<_>, _>>()?;
        let changesets = account_changeset.len();

        let last_indices = account_changeset
            .into_iter()
            // reverse so we can get lowest block number where we need to unwind account.
            .rev()
            // fold all account and get last block number
            .fold(BTreeMap::new(), |mut accounts: BTreeMap<Address, u64>, (index, account)| {
                // we just need address and lowest block number.
                accounts.insert(account.address, index);
                accounts
            });

        // Unwind the account history index.
        let mut cursor = self.tx.cursor_write::<tables::AccountHistory>()?;
        for (address, rem_index) in last_indices {
            let partial_shard = unwind_history_shards::<_, tables::AccountHistory, _>(
                &mut cursor,
                ShardedKey::last(address),
                rem_index,
                |sharded_key| sharded_key.key == address,
            )?;

            // Check the last returned partial shard.
            // If it's not empty, the shard needs to be reinserted.
            if !partial_shard.is_empty() {
                cursor.insert(
                    ShardedKey::last(address),
                    BlockNumberList::new_pre_sorted(partial_shard),
                )?;
            }
        }

        Ok(changesets)
    }
}

impl<TX: DbTxMut + DbTx> BlockExecutionWriter for DatabaseProvider<TX> {
    /// Return range of blocks and its execution result
    fn get_or_take_block_and_execution_range<const TAKE: bool>(
        &self,
        chain_spec: &ChainSpec,
        range: RangeInclusive<BlockNumber>,
    ) -> RethResult<Chain> {
        if TAKE {
            let storage_range = BlockNumberAddress::range(range.clone());

            // Initialize prefix sets.
            let mut account_prefix_set = PrefixSetMut::default();
            let mut storage_prefix_set: HashMap<B256, PrefixSetMut> = HashMap::default();
            let mut destroyed_accounts = HashSet::default();

            // Unwind account hashes. Add changed accounts to account prefix set.
            let hashed_addresses = self.unwind_account_hashing(range.clone())?;
            for (hashed_address, account) in hashed_addresses {
                account_prefix_set.insert(Nibbles::unpack(hashed_address));
                if account.is_none() {
                    destroyed_accounts.insert(hashed_address);
                }
            }

            // Unwind account history indices.
            self.unwind_account_history_indices(range.clone())?;

            // Unwind storage hashes. Add changed account and storage keys to corresponding prefix
            // sets.
            let storage_entries = self.unwind_storage_hashing(storage_range.clone())?;
            for (hashed_address, hashed_slots) in storage_entries {
                account_prefix_set.insert(Nibbles::unpack(hashed_address));
                for slot in hashed_slots {
                    storage_prefix_set
                        .entry(hashed_address)
                        .or_default()
                        .insert(Nibbles::unpack(slot));
                }
            }

            // Unwind storage history indices.
            self.unwind_storage_history_indices(storage_range)?;

            // Calculate the reverted merkle root.
            // This is the same as `StateRoot::incremental_root_with_updates`, only the prefix sets
            // are pre-loaded.
            let (new_state_root, trie_updates) = StateRoot::new(&self.tx)
                .with_changed_account_prefixes(account_prefix_set.freeze())
                .with_changed_storage_prefixes(
                    storage_prefix_set.into_iter().map(|(k, v)| (k, v.freeze())).collect(),
                )
                .with_destroyed_accounts(destroyed_accounts)
                .root_with_updates()
                .map_err(Into::<reth_db::DatabaseError>::into)?;

            let parent_number = range.start().saturating_sub(1);
            let parent_state_root = self
                .header_by_number(parent_number)?
                .ok_or_else(|| ProviderError::HeaderNotFound(parent_number.into()))?
                .state_root;

            // state root should be always correct as we are reverting state.
            // but for sake of double verification we will check it again.
            if new_state_root != parent_state_root {
                let parent_hash = self
                    .block_hash(parent_number)?
                    .ok_or_else(|| ProviderError::HeaderNotFound(parent_number.into()))?;
                return Err(ProviderError::UnwindStateRootMismatch {
                    got: new_state_root,
                    expected: parent_state_root,
                    block_number: parent_number,
                    block_hash: parent_hash,
                }
                .into())
            }
            trie_updates.flush(&self.tx)?;
        }
        // get blocks
        let blocks = self.get_take_block_range::<TAKE>(chain_spec, range.clone())?;
        let unwind_to = blocks.first().map(|b| b.number.saturating_sub(1));
        // get execution res
        let execution_state = self.unwind_or_peek_state::<TAKE>(range.clone())?;

        // remove block bodies it is needed for both get block range and get block execution results
        // that is why it is deleted afterwards.
        if TAKE {
            // rm block bodies
            self.get_or_take::<tables::BlockBodyIndices, TAKE>(range)?;

            // Update pipeline progress
            if let Some(fork_number) = unwind_to {
                self.update_pipeline_stages(fork_number, true)?;
            }
        }

        Ok(Chain::new(blocks, execution_state))
    }
}

impl<TX: DbTxMut + DbTx> BlockWriter for DatabaseProvider<TX> {
    fn insert_block(
        &self,
        block: SealedBlock,
        senders: Option<Vec<Address>>,
        prune_modes: Option<&PruneModes>,
    ) -> RethResult<StoredBlockBodyIndices> {
        let block_number = block.number;
        self.tx.put::<tables::CanonicalHeaders>(block.number, block.hash())?;
        // Put header with canonical hashes.
        self.tx.put::<tables::Headers>(block.number, block.header.as_ref().clone())?;
        self.tx.put::<tables::HeaderNumbers>(block.hash(), block.number)?;

        // total difficulty
        let ttd = if block.number == 0 {
            block.difficulty
        } else {
            let parent_block_number = block.number - 1;
            let parent_ttd = self.header_td_by_number(parent_block_number)?.unwrap_or_default();
            parent_ttd + block.difficulty
        };

        self.tx.put::<tables::HeaderTD>(block.number, ttd.into())?;

        // insert body ommers data
        if !block.ommers.is_empty() {
            self.tx.put::<tables::BlockOmmers>(
                block.number,
                StoredBlockOmmers { ommers: block.ommers },
            )?;
        }

        let mut next_tx_num = self
            .tx
            .cursor_read::<tables::Transactions>()?
            .last()?
            .map(|(n, _)| n + 1)
            .unwrap_or_default();
        let first_tx_num = next_tx_num;

        let tx_count = block.body.len() as u64;

        let senders_len = senders.as_ref().map(|s| s.len());
        let tx_iter = if Some(block.body.len()) == senders_len {
            block.body.into_iter().zip(senders.unwrap()).collect::<Vec<(_, _)>>()
        } else {
            let senders = TransactionSigned::recover_signers(&block.body, block.body.len()).ok_or(
                BlockExecutionError::Validation(BlockValidationError::SenderRecoveryError),
            )?;
            debug_assert_eq!(senders.len(), block.body.len(), "missing one or more senders");
            block.body.into_iter().zip(senders).collect()
        };

        for (transaction, sender) in tx_iter {
            let hash = transaction.hash();

            if prune_modes
                .and_then(|modes| modes.sender_recovery)
                .filter(|prune_mode| prune_mode.is_full())
                .is_none()
            {
                self.tx.put::<tables::TxSenders>(next_tx_num, sender)?;
            }

            self.tx.put::<tables::Transactions>(next_tx_num, transaction.into())?;

            if prune_modes
                .and_then(|modes| modes.transaction_lookup)
                .filter(|prune_mode| prune_mode.is_full())
                .is_none()
            {
                self.tx.put::<tables::TxHashNumber>(hash, next_tx_num)?;
            }
            next_tx_num += 1;
        }

        if let Some(withdrawals) = block.withdrawals {
            if !withdrawals.is_empty() {
                self.tx.put::<tables::BlockWithdrawals>(
                    block_number,
                    StoredBlockWithdrawals { withdrawals },
                )?;
            }
        }

        let block_indices = StoredBlockBodyIndices { first_tx_num, tx_count };
        self.tx.put::<tables::BlockBodyIndices>(block_number, block_indices.clone())?;

        if !block_indices.is_empty() {
            self.tx.put::<tables::TransactionBlock>(block_indices.last_tx_num(), block_number)?;
        }

        Ok(block_indices)
    }

    fn append_blocks_with_bundle_state(
        &self,
        blocks: Vec<SealedBlockWithSenders>,
        state: BundleStateWithReceipts,
        prune_modes: Option<&PruneModes>,
    ) -> RethResult<()> {
        if blocks.is_empty() {
            return Ok(())
        }
        let new_tip = blocks.last().unwrap();
        let new_tip_number = new_tip.number;

        let first_number = blocks.first().unwrap().number;

        let last = blocks.last().unwrap();
        let last_block_number = last.number;
        let last_block_hash = last.hash();
        let expected_state_root = last.state_root;

        // Insert the blocks
        for block in blocks {
            let (block, senders) = block.into_components();
            self.insert_block(block, Some(senders), prune_modes)?;
        }

        // Write state and changesets to the database.
        // Must be written after blocks because of the receipt lookup.
        state.write_to_db(self.tx_ref(), OriginalValuesKnown::No)?;

        self.insert_hashes(first_number..=last_block_number, last_block_hash, expected_state_root)?;

        self.calculate_history_indices(first_number..=last_block_number)?;

        // Update pipeline progress
        self.update_pipeline_stages(new_tip_number, false)?;

        Ok(())
    }
}

impl<TX: DbTx> PruneCheckpointReader for DatabaseProvider<TX> {
    fn get_prune_checkpoint(&self, segment: PruneSegment) -> RethResult<Option<PruneCheckpoint>> {
        Ok(self.tx.get::<tables::PruneCheckpoints>(segment)?)
    }
}

impl<TX: DbTxMut> PruneCheckpointWriter for DatabaseProvider<TX> {
    fn save_prune_checkpoint(
        &self,
        segment: PruneSegment,
        checkpoint: PruneCheckpoint,
    ) -> RethResult<()> {
        Ok(self.tx.put::<tables::PruneCheckpoints>(segment, checkpoint)?)
    }
}
