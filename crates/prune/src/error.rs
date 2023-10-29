use reth_db::DatabaseError;
use reth_interfaces::RethError;
use reth_primitives::PruneSegmentError;
use reth_provider::ProviderError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum PrunerError {
    #[error(transparent)]
    PruneSegment(#[from] PruneSegmentError),

    #[error("inconsistent data: {0}")]
    InconsistentData(&'static str),

    #[error(transparent)]
    Interface(#[from] RethError),

    #[error(transparent)]
    Database(#[from] DatabaseError),

    #[error(transparent)]
    Provider(#[from] ProviderError),
}
