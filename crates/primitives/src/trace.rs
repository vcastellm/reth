use std::collections::HashMap;
use ethers_core::types::{H256, U256, AddressOrBytes};

pub struct ContractCodeUsage {
    pub read: H256,
    pub write:Vec<u8>,
}

struct TxnTrace {
    balance: Option<U256>,
    nonce: Option<u64>,
    storage_read: Vec<H256>,
    storage_written: HashMap<H256, U256>,
    code_usage: ContractCodeUsage,
}

struct TxnMeta {
    byte_code: Vec<u8>,
    new_txn_trie_node: Vec<u8>,
    new_receipt_trie_node: Vec<u8>,
    gas_used: u64,
    bloom: Bloom,
}

struct TxnInfo {
    traces: HashMap<libcommon::Address, TxnTrace>,
    meta: TxnMeta,
}

type BlockUsedCodeHashes = Vec<libcommon::Hash>;

type TriePreImage = Vec<u8>;

type StorageTriesPreImage = HashMap<libcommon::Address, TriePreImage>;

struct BlockTrace {
    state_trie: TriePreImage,
    storage_tries: StorageTriesPreImage,
    contract_code: BlockUsedCodeHashes,
    txn_info: TxnInfo,
}
