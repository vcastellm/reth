use std::collections::HashMap;
use alloy_primitives::{Address, U256, Bloom};

#[derive(Debug)]
pub struct ContractCodeUsage {
    pub read: U256,
    pub write:Vec<u8>,
}

#[derive(Debug)]
pub struct TxnTrace {
    pub balance: Option<U256>,
    pub nonce: Option<u64>,
    pub storage_read: Vec<U256>,
    pub storage_written: HashMap<U256, U256>,
    pub code_usage: ContractCodeUsage,
}

#[derive(Debug)]
pub struct TxnMeta {
    pub byte_code: Vec<u8>,
    pub new_txn_trie_node: Vec<u8>,
    pub new_receipt_trie_node: Vec<u8>,
    pub gas_used: u64,
    pub bloom: Bloom,
}

#[derive(Debug)]
pub struct TxnInfo {
    pub traces: HashMap<Address, TxnTrace>,
    pub meta: TxnMeta,
}

pub type BlockUsedCodeHashes = Vec<U256>;
pub type TriePreImage = Vec<u8>;
pub type StorageTriesPreImage = HashMap<Address, TriePreImage>;

#[derive(Debug)]
pub struct BlockTrace {
    pub state_trie: TriePreImage,
    pub storage_tries: StorageTriesPreImage,
    pub contract_code: BlockUsedCodeHashes,
    pub txn_info: TxnInfo,
}
