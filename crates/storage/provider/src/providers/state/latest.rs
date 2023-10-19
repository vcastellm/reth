use crate::{
    providers::state::macros::delegate_provider_impls, AccountReader, BlockHashReader,
    BundleStateWithReceipts, StateProvider, StateRootProvider,
};
use reth_db::{
    cursor::{DbCursorRO, DbDupCursorRO},
    tables,
    transaction::DbTx,
};
use reth_interfaces::{provider::ProviderError, RethError, RethResult};
use reth_primitives::{
    keccak256, trie::AccountProof, Account, Address, BlockNumber, Bytecode, StorageKey,
    StorageValue, B256,
};

/// State provider over latest state that takes tx reference.
#[derive(Debug)]
pub struct LatestStateProviderRef<'b, TX: DbTx> {
    /// database transaction
    db: &'b TX,
}

impl<'b, TX: DbTx> LatestStateProviderRef<'b, TX> {
    /// Create new state provider
    pub fn new(db: &'b TX) -> Self {
        Self { db }
    }
}

impl<'b, TX: DbTx> AccountReader for LatestStateProviderRef<'b, TX> {
    /// Get basic account information.
    fn basic_account(&self, address: Address) -> RethResult<Option<Account>> {
        self.db.get::<tables::PlainAccountState>(address).map_err(Into::into)
    }
}

impl<'b, TX: DbTx> BlockHashReader for LatestStateProviderRef<'b, TX> {
    /// Get block hash by number.
    fn block_hash(&self, number: u64) -> RethResult<Option<B256>> {
        self.db.get::<tables::CanonicalHeaders>(number).map_err(Into::into)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> RethResult<Vec<B256>> {
        let range = start..end;
        self.db
            .cursor_read::<tables::CanonicalHeaders>()
            .map(|mut cursor| {
                cursor
                    .walk_range(range)?
                    .map(|result| result.map(|(_, hash)| hash).map_err(Into::into))
                    .collect::<RethResult<Vec<_>>>()
            })?
            .map_err(Into::into)
    }
}

impl<'b, TX: DbTx> StateRootProvider for LatestStateProviderRef<'b, TX> {
    fn state_root(&self, bundle_state: &BundleStateWithReceipts) -> RethResult<B256> {
        bundle_state.state_root_slow(self.db).map_err(|err| RethError::Database(err.into()))
    }
}

impl<'b, TX: DbTx> StateProvider for LatestStateProviderRef<'b, TX> {
    /// Get storage.
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> RethResult<Option<StorageValue>> {
        let mut cursor = self.db.cursor_dup_read::<tables::PlainStorageState>()?;
        if let Some(entry) = cursor.seek_by_key_subkey(account, storage_key)? {
            if entry.key == storage_key {
                return Ok(Some(entry.value))
            }
        }
        Ok(None)
    }

    /// Get account code by its hash
    fn bytecode_by_hash(&self, code_hash: B256) -> RethResult<Option<Bytecode>> {
        self.db.get::<tables::Bytecodes>(code_hash).map_err(Into::into)
    }

    fn proof(&self, address: Address, _keys: &[B256]) -> RethResult<AccountProof> {
        let _hashed_address = keccak256(address);
        let _root = self
            .db
            .cursor_read::<tables::Headers>()?
            .last()?
            .ok_or_else(|| ProviderError::HeaderNotFound(0.into()))?
            .1
            .state_root;

        unimplemented!()
    }
}

/// State provider for the latest state.
#[derive(Debug)]
pub struct LatestStateProvider<TX: DbTx> {
    /// database transaction
    db: TX,
}

impl<TX: DbTx> LatestStateProvider<TX> {
    /// Create new state provider
    pub fn new(db: TX) -> Self {
        Self { db }
    }

    /// Returns a new provider that takes the `TX` as reference
    #[inline(always)]
    fn as_ref(&self) -> LatestStateProviderRef<'_, TX> {
        LatestStateProviderRef::new(&self.db)
    }
}

// Delegates all provider impls to [LatestStateProviderRef]
delegate_provider_impls!(LatestStateProvider<TX> where [TX: DbTx]);

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_state_provider<T: StateProvider>() {}
    #[allow(unused)]
    fn assert_latest_state_provider<T: DbTx>() {
        assert_state_provider::<LatestStateProvider<T>>();
    }
}
