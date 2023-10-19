//! Contains RPC handler implementations specific to state.

use crate::{
    eth::error::{EthApiError, EthResult, RpcInvalidTransactionError},
    EthApi,
};
use reth_primitives::{
    serde_helper::JsonStorageKey, Address, BlockId, BlockNumberOrTag, Bytes, B256, U256,
};
use reth_provider::{
    BlockReaderIdExt, ChainSpecProvider, EvmEnvProvider, StateProvider, StateProviderFactory,
};
use reth_rpc_types::EIP1186AccountProofResponse;
use reth_rpc_types_compat::proof::from_primitive_account_proof;
use reth_transaction_pool::{PoolTransaction, TransactionPool};

impl<Provider, Pool, Network> EthApi<Provider, Pool, Network>
where
    Provider:
        BlockReaderIdExt + ChainSpecProvider + StateProviderFactory + EvmEnvProvider + 'static,
    Pool: TransactionPool + Clone + 'static,
    Network: Send + Sync + 'static,
{
    pub(crate) fn get_code(&self, address: Address, block_id: Option<BlockId>) -> EthResult<Bytes> {
        let state = self.state_at_block_id_or_latest(block_id)?;
        let code = state.account_code(address)?.unwrap_or_default();
        Ok(code.original_bytes())
    }

    pub(crate) fn balance(&self, address: Address, block_id: Option<BlockId>) -> EthResult<U256> {
        let state = self.state_at_block_id_or_latest(block_id)?;
        let balance = state.account_balance(address)?.unwrap_or_default();
        Ok(balance)
    }

    /// Returns the number of transactions sent from an address at the given block identifier.
    ///
    /// If this is [BlockNumberOrTag::Pending] then this will look up the highest transaction in
    /// pool and return the next nonce (highest + 1).
    pub(crate) fn get_transaction_count(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> EthResult<U256> {
        if let Some(BlockId::Number(BlockNumberOrTag::Pending)) = block_id {
            // lookup transactions in pool
            let address_txs = self.pool().get_transactions_by_sender(address);

            if !address_txs.is_empty() {
                // get max transaction with the highest nonce
                let highest_nonce_tx = address_txs
                    .into_iter()
                    .reduce(|accum, item| {
                        if item.transaction.nonce() > accum.transaction.nonce() {
                            item
                        } else {
                            accum
                        }
                    })
                    .expect("Not empty; qed");

                let tx_count = highest_nonce_tx
                    .transaction
                    .nonce()
                    .checked_add(1)
                    .ok_or(RpcInvalidTransactionError::NonceMaxValue)?;
                return Ok(U256::from(tx_count))
            }
        }

        let state = self.state_at_block_id_or_latest(block_id)?;
        Ok(U256::from(state.account_nonce(address)?.unwrap_or_default()))
    }

    pub(crate) fn storage_at(
        &self,
        address: Address,
        index: JsonStorageKey,
        block_id: Option<BlockId>,
    ) -> EthResult<B256> {
        let state = self.state_at_block_id_or_latest(block_id)?;
        let value = state.storage(address, index.0)?.unwrap_or_default();
        Ok(B256::new(value.to_be_bytes()))
    }

    pub(crate) async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block_id: Option<BlockId>,
    ) -> EthResult<EIP1186AccountProofResponse> {
        let chain_info = self.provider().chain_info()?;
        let block_id = block_id.unwrap_or(BlockId::Number(BlockNumberOrTag::Latest));

        // if we are trying to create a proof for the latest block, but have a BlockId as input
        // that is not BlockNumberOrTag::Latest, then we need to figure out whether or not the
        // BlockId corresponds to the latest block
        let is_latest_block = match block_id {
            BlockId::Number(BlockNumberOrTag::Number(num)) => num == chain_info.best_number,
            BlockId::Hash(hash) => hash == chain_info.best_hash.into(),
            BlockId::Number(BlockNumberOrTag::Latest) => true,
            _ => false,
        };

        // TODO: remove when HistoricalStateProviderRef::proof is implemented
        if !is_latest_block {
            return Err(EthApiError::InvalidBlockRange)
        }

        let this = self.clone();
        self.inner
            .blocking_task_pool
            .spawn(move || {
                let state = this.state_at_block_id(block_id)?;
                let storage_keys = keys.iter().map(|key| key.0).collect::<Vec<_>>();
                let proof = state.proof(address, &storage_keys)?;
                Ok(from_primitive_account_proof(proof))
            })
            .await
            .map_err(|_| EthApiError::InternalBlockingTaskError)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        eth::{cache::EthStateCache, gas_oracle::GasPriceOracle},
        BlockingTaskPool,
    };
    use reth_primitives::{constants::ETHEREUM_BLOCK_GAS_LIMIT, StorageKey, StorageValue};
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider, NoopProvider};
    use reth_transaction_pool::test_utils::testing_pool;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_storage() {
        // === Noop ===
        let pool = testing_pool();

        let cache = EthStateCache::spawn(NoopProvider::default(), Default::default());
        let eth_api = EthApi::new(
            NoopProvider::default(),
            pool.clone(),
            (),
            cache.clone(),
            GasPriceOracle::new(NoopProvider::default(), Default::default(), cache),
            ETHEREUM_BLOCK_GAS_LIMIT,
            BlockingTaskPool::build().expect("failed to build tracing pool"),
        );
        let address = Address::random();
        let storage = eth_api.storage_at(address, U256::ZERO.into(), None).unwrap();
        assert_eq!(storage, U256::ZERO.to_be_bytes());

        // === Mock ===
        let mock_provider = MockEthProvider::default();
        let storage_value = StorageValue::from(1337);
        let storage_key = StorageKey::random();
        let storage = HashMap::from([(storage_key, storage_value)]);
        let account = ExtendedAccount::new(0, U256::ZERO).extend_storage(storage);
        mock_provider.add_account(address, account);

        let cache = EthStateCache::spawn(mock_provider.clone(), Default::default());
        let eth_api = EthApi::new(
            mock_provider.clone(),
            pool,
            (),
            cache.clone(),
            GasPriceOracle::new(mock_provider, Default::default(), cache),
            ETHEREUM_BLOCK_GAS_LIMIT,
            BlockingTaskPool::build().expect("failed to build tracing pool"),
        );

        let storage_key: U256 = storage_key.into();
        let storage = eth_api.storage_at(address, storage_key.into(), None).unwrap();
        assert_eq!(storage, storage_value.to_be_bytes());
    }
}
