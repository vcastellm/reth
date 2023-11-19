//! Support for integrating customizations into the CLI.

use crate::cli::{
    components::{RethNodeComponents, RethRpcComponents},
    config::{PayloadBuilderConfig, RethRpcConfig},
};
use clap::Args;
use reth_basic_payload_builder::{BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig};
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService};
use reth_tasks::TaskSpawner;
use std::{fmt, marker::PhantomData};

use crate::cli::components::RethRpcServerHandles;

/// A trait that allows for extending parts of the CLI with additional functionality.
///
/// This is intended as a way to allow to _extend_ the node command. For example, to register
/// additional RPC namespaces.
pub trait RethCliExt {
    /// Provides additional configuration for the node CLI command.
    ///
    /// This supports additional CLI arguments that can be used to modify the node configuration.
    ///
    /// If no additional CLI arguments are required, the [NoArgs] wrapper type can be used.
    type Node: RethNodeCommandExt;
}

/// The default CLI extension.
impl RethCliExt for () {
    type Node = DefaultRethNodeCommandConfig;
}

/// A trait that allows for extending and customizing parts of the node command
/// [NodeCommand](crate::node::NodeCommand).
///
/// The functions are invoked during the initialization of the node command in the following order:
///
/// 1. [on_components_initialized](RethNodeCommandConfig::on_components_initialized)
/// 2. [spawn_payload_builder_service](RethNodeCommandConfig::spawn_payload_builder_service)
/// 3. [extend_rpc_modules](RethNodeCommandConfig::extend_rpc_modules)
/// 4. [on_rpc_server_started](RethNodeCommandConfig::on_rpc_server_started)
/// 5. [on_node_started](RethNodeCommandConfig::on_node_started)
pub trait RethNodeCommandConfig: fmt::Debug {
    /// Event hook called once all components have been initialized.
    ///
    /// This is called as soon as the node components have been initialized.
    fn on_components_initialized<Reth: RethNodeComponents>(
        &mut self,
        components: &Reth,
    ) -> eyre::Result<()> {
        let _ = components;
        Ok(())
    }

    /// Event hook called once the node has been launched.
    ///
    /// This is called last after the node has been launched.
    fn on_node_started<Reth: RethNodeComponents>(&mut self, components: &Reth) -> eyre::Result<()> {
        let _ = components;
        Ok(())
    }

    /// Event hook called once the rpc servers has been started.
    ///
    /// This is called after the rpc server has been started.
    fn on_rpc_server_started<Conf, Reth>(
        &mut self,
        config: &Conf,
        components: &Reth,
        rpc_components: RethRpcComponents<'_, Reth>,
        handles: RethRpcServerHandles,
    ) -> eyre::Result<()>
    where
        Conf: RethRpcConfig,
        Reth: RethNodeComponents,
    {
        let _ = config;
        let _ = components;
        let _ = rpc_components;
        let _ = handles;
        Ok(())
    }

    /// Allows for registering additional RPC modules for the transports.
    ///
    /// This is expected to call the merge functions of [reth_rpc_builder::TransportRpcModules], for
    /// example [reth_rpc_builder::TransportRpcModules::merge_configured].
    ///
    /// This is called before the rpc server will be started [Self::on_rpc_server_started].
    fn extend_rpc_modules<Conf, Reth>(
        &mut self,
        config: &Conf,
        components: &Reth,
        rpc_components: RethRpcComponents<'_, Reth>,
    ) -> eyre::Result<()>
    where
        Conf: RethRpcConfig,
        Reth: RethNodeComponents,
    {
        let _ = config;
        let _ = components;
        let _ = rpc_components;
        Ok(())
    }

    /// Configures the [PayloadBuilderService] for the node, spawns it and returns the
    /// [PayloadBuilderHandle].
    ///
    /// By default this spawns a [BasicPayloadJobGenerator] with the default configuration
    /// [BasicPayloadJobGeneratorConfig].
    fn spawn_payload_builder_service<Conf, Reth>(
        &mut self,
        conf: &Conf,
        components: &Reth,
    ) -> eyre::Result<PayloadBuilderHandle>
    where
        Conf: PayloadBuilderConfig,
        Reth: RethNodeComponents,
    {
        let payload_job_config = BasicPayloadJobGeneratorConfig::default()
            .interval(conf.interval())
            .deadline(conf.deadline())
            .max_payload_tasks(conf.max_payload_tasks())
            .extradata(conf.extradata_rlp_bytes())
            .max_gas_limit(conf.max_gas_limit());

        #[cfg(feature = "optimism")]
        let payload_job_config =
            payload_job_config.compute_pending_block(conf.compute_pending_block());

        // The default payload builder is implemented on the unit type.
        #[cfg(not(feature = "optimism"))]
        #[allow(clippy::let_unit_value)]
        let payload_builder = reth_basic_payload_builder::EthereumPayloadBuilder::default();

        // Optimism's payload builder is implemented on the OptimismPayloadBuilder type.
        #[cfg(feature = "optimism")]
        let payload_builder = reth_basic_payload_builder::OptimismPayloadBuilder::default();

        let payload_generator = BasicPayloadJobGenerator::with_builder(
            components.provider(),
            components.pool(),
            components.task_executor(),
            payload_job_config,
            components.chain_spec(),
            payload_builder,
        );
        let (payload_service, payload_builder) = PayloadBuilderService::new(payload_generator);

        components
            .task_executor()
            .spawn_critical("payload builder service", Box::pin(payload_service));

        Ok(payload_builder)
    }
}

/// A trait that allows for extending parts of the CLI with additional functionality.
pub trait RethNodeCommandExt: RethNodeCommandConfig + fmt::Debug + clap::Args {}

// blanket impl for all types that implement the required traits.
impl<T> RethNodeCommandExt for T where T: RethNodeCommandConfig + fmt::Debug + clap::Args {}

/// The default configuration for the reth node command [Command](crate::node::NodeCommand).
///
/// This is a convenience type for [NoArgs<()>].
#[derive(Debug, Clone, Copy, Default, Args)]
#[non_exhaustive]
pub struct DefaultRethNodeCommandConfig;

impl RethNodeCommandConfig for DefaultRethNodeCommandConfig {}

impl RethNodeCommandConfig for () {}

/// A helper type for [RethCliExt] extension that don't require any additional clap Arguments.
#[derive(Debug, Clone, Copy)]
pub struct NoArgsCliExt<Conf>(PhantomData<Conf>);

impl<Conf: RethNodeCommandConfig> RethCliExt for NoArgsCliExt<Conf> {
    type Node = NoArgs<Conf>;
}

/// A helper struct that allows for wrapping a [RethNodeCommandConfig] value without providing
/// additional CLI arguments.
///
/// Note: This type must be manually filled with a [RethNodeCommandConfig] manually before executing
/// the [NodeCommand](crate::node::NodeCommand).
#[derive(Debug, Clone, Copy, Default, Args)]
pub struct NoArgs<T = ()> {
    #[clap(skip)]
    inner: Option<T>,
}

impl<T> NoArgs<T> {
    /// Creates a new instance of the wrapper type.
    pub fn with(inner: T) -> Self {
        Self { inner: Some(inner) }
    }

    /// Sets the inner value.
    pub fn set(&mut self, inner: T) {
        self.inner = Some(inner)
    }

    /// Transforms the configured value.
    pub fn map<U>(self, inner: U) -> NoArgs<U> {
        NoArgs::with(inner)
    }

    /// Returns the inner value if it exists.
    pub fn inner(&self) -> Option<&T> {
        self.inner.as_ref()
    }

    /// Returns a mutable reference to the inner value if it exists.
    pub fn inner_mut(&mut self) -> Option<&mut T> {
        self.inner.as_mut()
    }

    /// Consumes the wrapper and returns the inner value if it exists.
    pub fn into_inner(self) -> Option<T> {
        self.inner
    }
}

impl<T: RethNodeCommandConfig> RethNodeCommandConfig for NoArgs<T> {
    fn on_components_initialized<Reth: RethNodeComponents>(
        &mut self,
        components: &Reth,
    ) -> eyre::Result<()> {
        if let Some(conf) = self.inner_mut() {
            conf.on_components_initialized(components)
        } else {
            Ok(())
        }
    }

    fn on_node_started<Reth: RethNodeComponents>(&mut self, components: &Reth) -> eyre::Result<()> {
        if let Some(conf) = self.inner_mut() {
            conf.on_node_started(components)
        } else {
            Ok(())
        }
    }

    fn on_rpc_server_started<Conf, Reth>(
        &mut self,
        config: &Conf,
        components: &Reth,
        rpc_components: RethRpcComponents<'_, Reth>,
        handles: RethRpcServerHandles,
    ) -> eyre::Result<()>
    where
        Conf: RethRpcConfig,
        Reth: RethNodeComponents,
    {
        if let Some(conf) = self.inner_mut() {
            conf.on_rpc_server_started(config, components, rpc_components, handles)
        } else {
            Ok(())
        }
    }

    fn extend_rpc_modules<Conf, Reth>(
        &mut self,
        config: &Conf,
        components: &Reth,
        rpc_components: RethRpcComponents<'_, Reth>,
    ) -> eyre::Result<()>
    where
        Conf: RethRpcConfig,
        Reth: RethNodeComponents,
    {
        if let Some(conf) = self.inner_mut() {
            conf.extend_rpc_modules(config, components, rpc_components)
        } else {
            Ok(())
        }
    }

    fn spawn_payload_builder_service<Conf, Reth>(
        &mut self,
        conf: &Conf,
        components: &Reth,
    ) -> eyre::Result<PayloadBuilderHandle>
    where
        Conf: PayloadBuilderConfig,
        Reth: RethNodeComponents,
    {
        self.inner_mut()
            .ok_or_else(|| eyre::eyre!("config value must be set"))?
            .spawn_payload_builder_service(conf, components)
    }
}

impl<T> From<T> for NoArgs<T> {
    fn from(value: T) -> Self {
        Self::with(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_ext<T: RethNodeCommandExt>() {}

    #[test]
    fn ensure_ext() {
        assert_ext::<DefaultRethNodeCommandConfig>();
        assert_ext::<NoArgs<()>>();
    }
}
