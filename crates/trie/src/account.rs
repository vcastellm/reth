use alloy_rlp::{RlpDecodable, RlpEncodable};
use reth_primitives::{constants::EMPTY_ROOT_HASH, Account, B256, KECCAK_EMPTY, U256};

/// An Ethereum account as represented in the trie.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, RlpEncodable, RlpDecodable)]
pub struct EthAccount {
    /// Account nonce.
    nonce: u64,
    /// Account balance.
    balance: U256,
    /// Account's storage root.
    storage_root: B256,
    /// Hash of the account's bytecode.
    code_hash: B256,
}

impl From<Account> for EthAccount {
    fn from(acc: Account) -> Self {
        EthAccount {
            nonce: acc.nonce,
            balance: acc.balance,
            storage_root: EMPTY_ROOT_HASH,
            code_hash: acc.bytecode_hash.unwrap_or(KECCAK_EMPTY),
        }
    }
}

impl EthAccount {
    /// Set storage root on account.
    pub fn with_storage_root(mut self, storage_root: B256) -> Self {
        self.storage_root = storage_root;
        self
    }

    /// Get account's storage root.
    pub fn storage_root(&self) -> B256 {
        self.storage_root
    }
}
