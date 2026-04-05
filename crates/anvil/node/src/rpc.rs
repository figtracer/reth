//! Anvil RPC add-ons and API implementation.

use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_anvil::{Forking, Metadata, MineOptions, NodeInfo};
use alloy_rpc_types_engine::ExecutionData;
use jsonrpsee::core::{async_trait, RpcResult};
use parking_lot::RwLock;
use reth_chainspec::{EthereumHardforks, Hardforks};
use reth_engine_primitives::EngineTypes;
use reth_ethereum_engine_primitives::EthPayloadAttributes;
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::{ConfigureEvm, EvmFactory, EvmFactoryFor, NextBlockEnvAttributes};
use reth_node_api::{FullNodeComponents, NodeAddOns};
use reth_node_builder::{
    rpc::{
        BasicEngineApiBuilder, BasicEngineValidatorBuilder, EngineApiBuilder,
        EngineValidatorBuilder, EthApiBuilder, Identity, PayloadValidatorBuilder, RethRpcAddOns,
        RpcAddOns, RpcHandle,
    },
    NodeAdapter, NodeTypes,
};
use reth_node_ethereum::node::{EthereumEngineValidatorBuilder, EthereumEthApiBuilder};
use reth_payload_primitives::PayloadTypes;
use reth_rpc_api::anvil::AnvilApiServer;
use reth_rpc_builder::middleware::RethRpcMiddleware;
use reth_rpc_eth_api::{
    helpers::config::{EthConfigApiServer, EthConfigHandler},
    EthApiTypes,
};
use reth_rpc_eth_types::{error::FromEvmError, EthApiError};
use reth_rpc_server_types::RethRpcModule;
use reth_tracing::tracing::info;
use revm::context::TxEnv;
use std::{collections::HashSet, sync::Arc};

/// Anvil-specific RPC add-ons.
///
/// Wraps the standard ethereum RPC modules and adds the `anvil_*` namespace.
#[derive(Debug)]
pub struct AnvilAddOns<
    N: FullNodeComponents,
    EthB: EthApiBuilder<N> = EthereumEthApiBuilder,
    RpcMiddleware = Identity,
> {
    inner: RpcAddOns<
        N,
        EthB,
        EthereumEngineValidatorBuilder,
        BasicEngineApiBuilder<EthereumEngineValidatorBuilder>,
        BasicEngineValidatorBuilder<EthereumEngineValidatorBuilder>,
        RpcMiddleware,
    >,
}

impl<N> Default for AnvilAddOns<N, EthereumEthApiBuilder>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec: EthereumHardforks + Clone + 'static,
            Payload: EngineTypes<ExecutionData = ExecutionData>
                         + PayloadTypes<PayloadAttributes = EthPayloadAttributes>,
            Primitives = EthPrimitives,
        >,
    >,
    EthereumEthApiBuilder: EthApiBuilder<N>,
{
    fn default() -> Self {
        Self {
            inner: RpcAddOns::new(
                EthereumEthApiBuilder::default(),
                EthereumEngineValidatorBuilder::default(),
                BasicEngineApiBuilder::default(),
                BasicEngineValidatorBuilder::default(),
                Default::default(),
            ),
        }
    }
}

impl<N, EthB, RpcMiddleware> NodeAddOns<N> for AnvilAddOns<N, EthB, RpcMiddleware>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec: Hardforks + EthereumHardforks,
            Primitives = EthPrimitives,
            Payload: EngineTypes<ExecutionData = ExecutionData>,
        >,
        Evm: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
    >,
    EthB: EthApiBuilder<N>,
    BasicEngineApiBuilder<EthereumEngineValidatorBuilder>: EngineApiBuilder<N>,
    BasicEngineValidatorBuilder<EthereumEngineValidatorBuilder>: EngineValidatorBuilder<N>,
    EthApiError: FromEvmError<N::Evm>,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = TxEnv>,
    RpcMiddleware: RethRpcMiddleware,
{
    type Handle = RpcHandle<N, EthB::EthApi>;

    async fn launch_add_ons(
        self,
        ctx: reth_node_api::AddOnsContext<'_, N>,
    ) -> eyre::Result<Self::Handle> {
        let eth_config =
            EthConfigHandler::new(ctx.node.provider().clone(), ctx.node.evm_config().clone());

        self.inner
            .launch_add_ons_with(ctx, move |container| {
                container
                    .modules
                    .merge_if_module_configured(RethRpcModule::Eth, eth_config.into_rpc())?;

                // register anvil_* namespace
                let anvil_api = AnvilRpcHandler::new();
                container.modules.merge_configured(anvil_api.into_rpc())?;

                info!(target: "reth::cli", "Anvil RPC extensions registered");

                Ok(())
            })
            .await
    }
}

impl<N, EthB, RpcMiddleware> RethRpcAddOns<N> for AnvilAddOns<N, EthB, RpcMiddleware>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec: Hardforks + EthereumHardforks,
            Primitives = EthPrimitives,
            Payload: EngineTypes<ExecutionData = ExecutionData>,
        >,
        Evm: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
    >,
    EthB: EthApiBuilder<N>,
    BasicEngineApiBuilder<EthereumEngineValidatorBuilder>: EngineApiBuilder<N>,
    BasicEngineValidatorBuilder<EthereumEngineValidatorBuilder>: EngineValidatorBuilder<N>,
    EthApiError: FromEvmError<N::Evm>,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = TxEnv>,
    RpcMiddleware: RethRpcMiddleware,
{
    type EthApi = EthB::EthApi;

    fn hooks_mut(&mut self) -> &mut reth_node_builder::rpc::RpcHooks<N, Self::EthApi> {
        &mut self.inner.hooks
    }
}

/// Anvil RPC handler.
///
/// This is a stub implementation. Each method will be wired to the actual
/// backend as subsystems get ported.
#[derive(Debug, Clone)]
pub struct AnvilRpcHandler {
    /// Accounts currently being impersonated.
    impersonated: Arc<RwLock<HashSet<Address>>>,
    /// Whether automine is enabled.
    automine: Arc<RwLock<bool>>,
}

impl AnvilRpcHandler {
    /// Create a new handler with defaults (automine on, no impersonation).
    pub fn new() -> Self {
        Self {
            impersonated: Arc::new(RwLock::new(HashSet::new())),
            automine: Arc::new(RwLock::new(true)),
        }
    }
}

#[async_trait]
impl AnvilApiServer for AnvilRpcHandler {
    async fn anvil_impersonate_account(&self, address: Address) -> RpcResult<()> {
        self.impersonated.write().insert(address);
        Ok(())
    }

    async fn anvil_stop_impersonating_account(&self, address: Address) -> RpcResult<()> {
        self.impersonated.write().remove(&address);
        Ok(())
    }

    async fn anvil_auto_impersonate_account(&self, _enabled: bool) -> RpcResult<()> {
        Ok(())
    }

    async fn anvil_get_automine(&self) -> RpcResult<bool> {
        Ok(*self.automine.read())
    }

    async fn anvil_mine(&self, _blocks: Option<U256>, _interval: Option<U256>) -> RpcResult<()> {
        // TODO: trigger mining via LocalMiner's trigger channel
        Ok(())
    }

    async fn anvil_set_automine(&self, enabled: bool) -> RpcResult<()> {
        *self.automine.write() = enabled;
        Ok(())
    }

    async fn anvil_set_interval_mining(&self, _interval: u64) -> RpcResult<()> {
        Ok(())
    }

    async fn anvil_drop_transaction(&self, _tx_hash: B256) -> RpcResult<Option<B256>> {
        Ok(None)
    }

    async fn anvil_reset(&self, _fork: Option<Forking>) -> RpcResult<()> {
        Ok(())
    }

    async fn anvil_set_rpc_url(&self, _url: String) -> RpcResult<()> {
        Ok(())
    }

    async fn anvil_set_balance(&self, _address: Address, _balance: U256) -> RpcResult<()> {
        // TODO: direct state mutation via provider
        Ok(())
    }

    async fn anvil_set_code(&self, _address: Address, _code: Bytes) -> RpcResult<()> {
        Ok(())
    }

    async fn anvil_set_nonce(&self, _address: Address, _nonce: U256) -> RpcResult<()> {
        Ok(())
    }

    async fn anvil_set_storage_at(
        &self,
        _address: Address,
        _slot: U256,
        _value: B256,
    ) -> RpcResult<bool> {
        Ok(true)
    }

    async fn anvil_set_coinbase(&self, _address: Address) -> RpcResult<()> {
        Ok(())
    }

    async fn anvil_set_chain_id(&self, _chain_id: u64) -> RpcResult<()> {
        Ok(())
    }

    async fn anvil_set_logging_enabled(&self, _enabled: bool) -> RpcResult<()> {
        Ok(())
    }

    async fn anvil_set_min_gas_price(&self, _gas_price: U256) -> RpcResult<()> {
        Ok(())
    }

    async fn anvil_set_next_block_base_fee_per_gas(&self, _base_fee: U256) -> RpcResult<()> {
        Ok(())
    }

    async fn anvil_set_time(&self, _timestamp: u64) -> RpcResult<u64> {
        Ok(0)
    }

    async fn anvil_dump_state(&self) -> RpcResult<Bytes> {
        Ok(Bytes::new())
    }

    async fn anvil_load_state(&self, _state: Bytes) -> RpcResult<bool> {
        Ok(true)
    }

    async fn anvil_node_info(&self) -> RpcResult<NodeInfo> {
        Err(jsonrpsee::types::ErrorObject::owned(
            -32000,
            "not yet implemented",
            None::<()>,
        ))
    }

    async fn anvil_metadata(&self) -> RpcResult<Metadata> {
        Err(jsonrpsee::types::ErrorObject::owned(
            -32000,
            "not yet implemented",
            None::<()>,
        ))
    }

    async fn anvil_snapshot(&self) -> RpcResult<U256> {
        // TODO: state snapshot
        Ok(U256::ZERO)
    }

    async fn anvil_revert(&self, _id: U256) -> RpcResult<bool> {
        Ok(true)
    }

    async fn anvil_increase_time(&self, _seconds: U256) -> RpcResult<i64> {
        Ok(0)
    }

    async fn anvil_set_next_block_timestamp(&self, _seconds: u64) -> RpcResult<()> {
        Ok(())
    }

    async fn anvil_set_block_gas_limit(&self, _gas_limit: U256) -> RpcResult<bool> {
        Ok(true)
    }

    async fn anvil_set_block_timestamp_interval(&self, _seconds: u64) -> RpcResult<()> {
        Ok(())
    }

    async fn anvil_remove_block_timestamp_interval(&self) -> RpcResult<bool> {
        Ok(true)
    }

    async fn anvil_mine_detailed(
        &self,
        _opts: Option<MineOptions>,
    ) -> RpcResult<Vec<alloy_rpc_types_eth::Block>> {
        Ok(vec![])
    }

    async fn anvil_enable_traces(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn anvil_remove_pool_transactions(&self, _address: Address) -> RpcResult<()> {
        Ok(())
    }
}
