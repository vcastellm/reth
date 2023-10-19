//! Reth RPC interface definitions
//!
//! Provides all RPC interfaces.
//!
//! ## Feature Flags
//!
//! - `client`: Enables JSON-RPC client support.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![warn(missing_debug_implementations, missing_docs, unreachable_pub, rustdoc::all)]
#![deny(unused_must_use, rust_2018_idioms)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod admin;
mod bundle;
mod debug;
mod engine;
mod eth;
mod eth_filter;
mod eth_pubsub;
mod mev;
mod net;
mod otterscan;
mod reth;
mod rpc;
mod trace;
mod txpool;
mod web3;

/// re-export of all server traits
pub use servers::*;

/// Aggregates all server traits.
pub mod servers {
    pub use crate::{
        admin::AdminApiServer,
        bundle::EthBundleApiServer,
        debug::DebugApiServer,
        engine::{EngineApiServer, EngineEthApiServer},
        eth::EthApiServer,
        eth_filter::EthFilterApiServer,
        eth_pubsub::EthPubSubApiServer,
        mev::MevApiServer,
        net::NetApiServer,
        otterscan::OtterscanServer,
        reth::RethApiServer,
        rpc::RpcApiServer,
        trace::TraceApiServer,
        txpool::TxPoolApiServer,
        web3::Web3ApiServer,
    };
}

/// re-export of all client traits
#[cfg(feature = "client")]
pub use clients::*;

/// Aggregates all client traits.
#[cfg(feature = "client")]
pub mod clients {
    pub use crate::{
        admin::AdminApiClient,
        bundle::EthBundleApiClient,
        debug::DebugApiClient,
        engine::{EngineApiClient, EngineEthApiClient},
        eth::EthApiClient,
        eth_filter::EthFilterApiClient,
        mev::MevApiClient,
        net::NetApiClient,
        otterscan::OtterscanClient,
        rpc::RpcApiServer,
        trace::TraceApiClient,
        txpool::TxPoolApiClient,
        web3::Web3ApiClient,
    };
}
