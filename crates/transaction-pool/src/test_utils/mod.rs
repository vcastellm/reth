//! Internal helpers for testing.
#![allow(missing_docs, unused, missing_debug_implementations, unreachable_pub)]

mod gen;
mod mock;
mod pool;

use crate::{
    blobstore::InMemoryBlobStore, noop::MockTransactionValidator, Pool, PoolTransaction,
    TransactionOrigin, TransactionValidationOutcome, TransactionValidator,
};
pub use gen::*;
pub use mock::*;
use std::{marker::PhantomData, sync::Arc};

/// A [Pool] used for testing
pub type TestPool =
    Pool<MockTransactionValidator<MockTransaction>, MockOrdering, InMemoryBlobStore>;

/// Returns a new [Pool] used for testing purposes
pub fn testing_pool() -> TestPool {
    testing_pool_with_validator(MockTransactionValidator::default())
}
/// Returns a new [Pool] used for testing purposes
pub fn testing_pool_with_validator(
    validator: MockTransactionValidator<MockTransaction>,
) -> TestPool {
    Pool::new(validator, MockOrdering::default(), InMemoryBlobStore::default(), Default::default())
}
