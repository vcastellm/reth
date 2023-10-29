//! Support for building a pending block via local txpool.

use crate::eth::error::{EthApiError, EthResult};
use reth_primitives::{
    constants::{eip4844::MAX_DATA_GAS_PER_BLOCK, BEACON_NONCE},
    proofs,
    revm::{compat::into_reth_log, env::tx_env_with_recovered},
    Block, BlockId, BlockNumberOrTag, ChainSpec, Header, IntoRecoveredTransaction, Receipt,
    Receipts, SealedBlock, SealedHeader, B256, EMPTY_OMMER_ROOT_HASH, U256,
};
use reth_provider::{BundleStateWithReceipts, ChainSpecProvider, StateProviderFactory};
use reth_revm::{
    database::StateProviderDatabase,
    state_change::{apply_beacon_root_contract_call, post_block_withdrawals_balance_increments},
};
use reth_transaction_pool::TransactionPool;
use revm::{db::states::bundle_state::BundleRetention, Database, DatabaseCommit, State};
use revm_primitives::{
    BlockEnv, CfgEnv, EVMError, Env, InvalidTransaction, ResultAndState, SpecId,
};
use std::time::Instant;

/// Configured [BlockEnv] and [CfgEnv] for a pending block
#[derive(Debug, Clone)]
pub(crate) struct PendingBlockEnv {
    /// Configured [CfgEnv] for the pending block.
    pub(crate) cfg: CfgEnv,
    /// Configured [BlockEnv] for the pending block.
    pub(crate) block_env: BlockEnv,
    /// Origin block for the config
    pub(crate) origin: PendingBlockEnvOrigin,
}

impl PendingBlockEnv {
    /// Builds a pending block using the given client and pool.
    ///
    /// If the origin is the actual pending block, the block is built with withdrawals.
    ///
    /// After Cancun, if the origin is the actual pending block, the block includes the EIP-4788 pre
    /// block contract call using the parent beacon block root received from the CL.
    pub(crate) fn build_block<Client, Pool>(
        self,
        client: &Client,
        pool: &Pool,
    ) -> EthResult<SealedBlock>
    where
        Client: StateProviderFactory + ChainSpecProvider,
        Pool: TransactionPool,
    {
        let Self { cfg, block_env, origin } = self;

        let parent_hash = origin.build_target_hash();
        let state_provider = client.history_by_block_hash(parent_hash)?;
        let state = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder().with_database(Box::new(state)).with_bundle_update().build();

        let mut cumulative_gas_used = 0;
        let mut sum_blob_gas_used = 0;
        let block_gas_limit: u64 = block_env.gas_limit.to::<u64>();
        let base_fee = block_env.basefee.to::<u64>();
        let block_number = block_env.number.to::<u64>();

        let mut executed_txs = Vec::new();
        let mut best_txs = pool.best_transactions_with_base_fee(base_fee);

        let (withdrawals, withdrawals_root) = match origin {
            PendingBlockEnvOrigin::ActualPending(ref block) => {
                (block.withdrawals.clone(), block.withdrawals_root)
            }
            PendingBlockEnvOrigin::DerivedFromLatest(_) => (None, None),
        };

        let chain_spec = client.chain_spec();

        let parent_beacon_block_root = if origin.is_actual_pending() {
            // apply eip-4788 pre block contract call if we got the block from the CL with the real
            // parent beacon block root
            pre_block_beacon_root_contract_call(
                &mut db,
                chain_spec.as_ref(),
                block_number,
                &cfg,
                &block_env,
                origin.header().parent_beacon_block_root,
            )?;
            origin.header().parent_beacon_block_root
        } else {
            None
        };

        let mut receipts = Vec::new();

        while let Some(pool_tx) = best_txs.next() {
            // ensure we still have capacity for this transaction
            if cumulative_gas_used + pool_tx.gas_limit() > block_gas_limit {
                // we can't fit this transaction into the block, so we need to mark it as invalid
                // which also removes all dependent transaction from the iterator before we can
                // continue
                best_txs.mark_invalid(&pool_tx);
                continue
            }

            // convert tx to a signed transaction
            let tx = pool_tx.to_recovered_transaction();

            // There's only limited amount of blob space available per block, so we need to check if
            // the EIP-4844 can still fit in the block
            if let Some(blob_tx) = tx.transaction.as_eip4844() {
                let tx_blob_gas = blob_tx.blob_gas();
                if sum_blob_gas_used + tx_blob_gas > MAX_DATA_GAS_PER_BLOCK {
                    // we can't fit this _blob_ transaction into the block, so we mark it as
                    // invalid, which removes its dependent transactions from
                    // the iterator. This is similar to the gas limit condition
                    // for regular transactions above.
                    best_txs.mark_invalid(&pool_tx);
                    continue
                }
            }

            // Configure the environment for the block.
            let env =
                Env { cfg: cfg.clone(), block: block_env.clone(), tx: tx_env_with_recovered(&tx) };

            let mut evm = revm::EVM::with_env(env);
            evm.database(&mut db);

            let ResultAndState { result, state } = match evm.transact() {
                Ok(res) => res,
                Err(err) => {
                    match err {
                        EVMError::Transaction(err) => {
                            if matches!(err, InvalidTransaction::NonceTooLow { .. }) {
                                // if the nonce is too low, we can skip this transaction
                            } else {
                                // if the transaction is invalid, we can skip it and all of its
                                // descendants
                                best_txs.mark_invalid(&pool_tx);
                            }
                            continue
                        }
                        err => {
                            // this is an error that we should treat as fatal for this attempt
                            return Err(err.into())
                        }
                    }
                }
            };

            // commit changes
            db.commit(state);

            // add to the total blob gas used if the transaction successfully executed
            if let Some(blob_tx) = tx.transaction.as_eip4844() {
                let tx_blob_gas = blob_tx.blob_gas();
                sum_blob_gas_used += tx_blob_gas;

                // if we've reached the max data gas per block, we can skip blob txs entirely
                if sum_blob_gas_used == MAX_DATA_GAS_PER_BLOCK {
                    best_txs.skip_blobs();
                }
            }

            let gas_used = result.gas_used();

            // add gas used by the transaction to cumulative gas used, before creating the receipt
            cumulative_gas_used += gas_used;

            // Push transaction changeset and calculate header bloom filter for receipt.
            receipts.push(Some(Receipt {
                tx_type: tx.tx_type(),
                success: result.is_success(),
                cumulative_gas_used,
                logs: result.logs().into_iter().map(into_reth_log).collect(),
            }));

            // append transaction to the list of executed transactions
            executed_txs.push(tx.into_signed());
        }

        // executes the withdrawals and commits them to the Database and BundleState.
        let balance_increments = post_block_withdrawals_balance_increments(
            &chain_spec,
            block_env.timestamp.try_into().unwrap_or(u64::MAX),
            withdrawals.clone().unwrap_or_default().as_ref(),
        );

        // increment account balances for withdrawals
        db.increment_balances(balance_increments)?;

        // merge all transitions into bundle state.
        db.merge_transitions(BundleRetention::PlainState);

        let bundle = BundleStateWithReceipts::new(
            db.take_bundle(),
            Receipts::from_vec(vec![receipts]),
            block_number,
        );

        let receipts_root = bundle.receipts_root_slow(block_number).expect("Block is present");
        let logs_bloom = bundle.block_logs_bloom(block_number).expect("Block is present");

        // calculate the state root
        let state_root = state_provider.state_root(&bundle)?;

        // create the block header
        let transactions_root = proofs::calculate_transaction_root(&executed_txs);

        // check if cancun is activated to set eip4844 header fields correctly
        let blob_gas_used =
            if cfg.spec_id >= SpecId::CANCUN { Some(sum_blob_gas_used) } else { None };

        let header = Header {
            parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: block_env.coinbase,
            state_root,
            transactions_root,
            receipts_root,
            withdrawals_root,
            logs_bloom,
            timestamp: block_env.timestamp.to::<u64>(),
            mix_hash: block_env.prevrandao.unwrap_or_default(),
            nonce: BEACON_NONCE,
            base_fee_per_gas: Some(base_fee),
            number: block_number,
            gas_limit: block_gas_limit,
            difficulty: U256::ZERO,
            gas_used: cumulative_gas_used,
            blob_gas_used,
            excess_blob_gas: block_env.get_blob_excess_gas(),
            extra_data: Default::default(),
            parent_beacon_block_root,
        };

        // seal the block
        let block = Block { header, body: executed_txs, ommers: vec![], withdrawals };
        let sealed_block = block.seal_slow();

        Ok(sealed_block)
    }
}

/// Apply the [EIP-4788](https://eips.ethereum.org/EIPS/eip-4788) pre block contract call.
///
/// This constructs a new [EVM](revm::EVM) with the given DB, and environment ([CfgEnv] and
/// [BlockEnv]) to execute the pre block contract call.
///
/// This uses [apply_beacon_root_contract_call] to ultimately apply the beacon root contract state
/// change.
fn pre_block_beacon_root_contract_call<DB: Database + DatabaseCommit>(
    db: &mut DB,
    chain_spec: &ChainSpec,
    block_number: u64,
    initialized_cfg: &CfgEnv,
    initialized_block_env: &BlockEnv,
    parent_beacon_block_root: Option<B256>,
) -> EthResult<()>
where
    DB::Error: std::fmt::Display,
{
    // Configure the environment for the block.
    let env = Env {
        cfg: initialized_cfg.clone(),
        block: initialized_block_env.clone(),
        ..Default::default()
    };

    // apply pre-block EIP-4788 contract call
    let mut evm_pre_block = revm::EVM::with_env(env);
    evm_pre_block.database(db);

    // initialize a block from the env, because the pre block call needs the block itself
    apply_beacon_root_contract_call(
        chain_spec,
        initialized_block_env.timestamp.to::<u64>(),
        block_number,
        parent_beacon_block_root,
        &mut evm_pre_block,
    )
    .map_err(|err| EthApiError::Internal(err.into()))
}

/// The origin for a configured [PendingBlockEnv]
#[derive(Clone, Debug)]
pub(crate) enum PendingBlockEnvOrigin {
    /// The pending block as received from the CL.
    ActualPending(SealedBlock),
    /// The header of the latest block
    DerivedFromLatest(SealedHeader),
}

impl PendingBlockEnvOrigin {
    /// Returns true if the origin is the actual pending block as received from the CL.
    pub(crate) fn is_actual_pending(&self) -> bool {
        matches!(self, PendingBlockEnvOrigin::ActualPending(_))
    }

    /// Consumes the type and returns the actual pending block.
    pub(crate) fn into_actual_pending(self) -> Option<SealedBlock> {
        match self {
            PendingBlockEnvOrigin::ActualPending(block) => Some(block),
            _ => None,
        }
    }

    /// Returns the [BlockId] that represents the state of the block.
    ///
    /// If this is the actual pending block, the state is the "Pending" tag, otherwise we can safely
    /// identify the block by its hash.
    pub(crate) fn state_block_id(&self) -> BlockId {
        match self {
            PendingBlockEnvOrigin::ActualPending(_) => BlockNumberOrTag::Pending.into(),
            PendingBlockEnvOrigin::DerivedFromLatest(header) => BlockId::Hash(header.hash.into()),
        }
    }

    /// Returns the hash of the block the pending block should be built on.
    fn build_target_hash(&self) -> B256 {
        match self {
            PendingBlockEnvOrigin::ActualPending(block) => block.parent_hash,
            PendingBlockEnvOrigin::DerivedFromLatest(header) => header.hash,
        }
    }

    /// Returns the header this pending block is based on.
    pub(crate) fn header(&self) -> &SealedHeader {
        match self {
            PendingBlockEnvOrigin::ActualPending(block) => &block.header,
            PendingBlockEnvOrigin::DerivedFromLatest(header) => header,
        }
    }
}

/// In memory pending block for `pending` tag
#[derive(Debug)]
pub(crate) struct PendingBlock {
    /// The cached pending block
    pub(crate) block: SealedBlock,
    /// Timestamp when the pending block is considered outdated
    pub(crate) expires_at: Instant,
}
