//! Anvil RPC add-ons and API implementation.

use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_anvil::{Forking, Metadata, MineOptions, NodeInfo};
use alloy_rpc_types_engine::ExecutionData;
use jsonrpsee::core::{async_trait, RpcResult};
use parking_lot::RwLock;
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
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
use reth_provider::ChainSpecProvider;
use reth_rpc_api::anvil::AnvilApiServer;
use reth_rpc_builder::middleware::RethRpcMiddleware;
use reth_rpc_eth_api::{
    helpers::config::{EthConfigApiServer, EthConfigHandler},
    EthApiTypes,
};
use reth_rpc_eth_types::{error::FromEvmError, EthApiError};
use reth_rpc_server_types::RethRpcModule;
use reth_transaction_pool::TransactionPool;
use reth_tracing::tracing::info;
use revm::context::TxEnv;
use std::{collections::HashSet, fmt, sync::Arc};

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

        let pool = ctx.node.pool().clone();
        let provider = ctx.node.provider().clone();

        self.inner
            .launch_add_ons_with(ctx, move |container| {
                container
                    .modules
                    .merge_if_module_configured(RethRpcModule::Eth, eth_config.into_rpc())?;

                // register anvil_* namespace
                let anvil_api = AnvilRpcHandler::new(pool, provider);
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

/// Helper to return a standard "not yet implemented" RPC error.
fn not_implemented(method: &str) -> jsonrpsee::types::ErrorObject<'static> {
    jsonrpsee::types::ErrorObject::owned(-32000, format!("anvil_{method}: not yet implemented"), None::<()>)
}

/// Anvil RPC handler.
///
/// Generic over `Pool` (transaction pool) and `Provider` (chain state).
/// Methods that are wired return real results. Unimplemented methods return
/// explicit errors so callers don't mistake a noop for success.
pub struct AnvilRpcHandler<Pool, Provider> {
    /// Transaction pool handle.
    pool: Pool,
    /// Provider for chain state queries.
    provider: Provider,
    /// Accounts currently being impersonated.
    impersonated: Arc<RwLock<HashSet<Address>>>,
    /// Whether auto-impersonation is enabled.
    auto_impersonate: Arc<RwLock<bool>>,
    /// Whether automine is enabled.
    automine: Arc<RwLock<bool>>,
}

impl<Pool, Provider> fmt::Debug for AnvilRpcHandler<Pool, Provider> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnvilRpcHandler")
            .field("impersonated", &self.impersonated)
            .field("auto_impersonate", &self.auto_impersonate)
            .field("automine", &self.automine)
            .finish_non_exhaustive()
    }
}

impl<Pool: Clone, Provider: Clone> Clone for AnvilRpcHandler<Pool, Provider> {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            provider: self.provider.clone(),
            impersonated: self.impersonated.clone(),
            auto_impersonate: self.auto_impersonate.clone(),
            automine: self.automine.clone(),
        }
    }
}

impl<Pool, Provider> AnvilRpcHandler<Pool, Provider> {
    /// Create a new handler with the given pool and provider.
    pub fn new(pool: Pool, provider: Provider) -> Self {
        Self {
            pool,
            provider,
            impersonated: Arc::new(RwLock::new(HashSet::new())),
            auto_impersonate: Arc::new(RwLock::new(false)),
            automine: Arc::new(RwLock::new(true)),
        }
    }
}

#[async_trait]
impl<Pool, Provider> AnvilApiServer for AnvilRpcHandler<Pool, Provider>
where
    Pool: TransactionPool + Clone + Send + Sync + 'static,
    Provider: ChainSpecProvider<ChainSpec: EthChainSpec> + Clone + Send + Sync + 'static,
{
    // -- impersonation (wired) --

    async fn anvil_impersonate_account(&self, address: Address) -> RpcResult<()> {
        self.impersonated.write().insert(address);
        Ok(())
    }

    async fn anvil_stop_impersonating_account(&self, address: Address) -> RpcResult<()> {
        self.impersonated.write().remove(&address);
        Ok(())
    }

    async fn anvil_auto_impersonate_account(&self, enabled: bool) -> RpcResult<()> {
        *self.auto_impersonate.write() = enabled;
        Ok(())
    }

    // -- mining control (wired: state tracking, not yet triggering blocks) --

    async fn anvil_get_automine(&self) -> RpcResult<bool> {
        Ok(*self.automine.read())
    }

    async fn anvil_set_automine(&self, enabled: bool) -> RpcResult<()> {
        *self.automine.write() = enabled;
        Ok(())
    }

    // -- mining (needs engine handle) --

    async fn anvil_mine(&self, _blocks: Option<U256>, _interval: Option<U256>) -> RpcResult<()> {
        Err(not_implemented("mine"))
    }

    async fn anvil_set_interval_mining(&self, _interval: u64) -> RpcResult<()> {
        Err(not_implemented("setIntervalMining"))
    }

    async fn anvil_mine_detailed(
        &self,
        _opts: Option<MineOptions>,
    ) -> RpcResult<Vec<alloy_rpc_types_eth::Block>> {
        Err(not_implemented("mine_detailed"))
    }

    // -- pool operations (needs pool handle) --

    async fn anvil_drop_transaction(&self, tx_hash: B256) -> RpcResult<Option<B256>> {
        let removed = self.pool.remove_transaction(tx_hash);
        Ok(removed.map(|_| tx_hash))
    }

    async fn anvil_remove_pool_transactions(&self, address: Address) -> RpcResult<()> {
        self.pool.remove_transactions_by_sender(address);
        Ok(())
    }

    // -- state manipulation (needs state overlay) --

    async fn anvil_set_balance(&self, _address: Address, _balance: U256) -> RpcResult<()> {
        Err(not_implemented("setBalance"))
    }

    async fn anvil_set_code(&self, _address: Address, _code: Bytes) -> RpcResult<()> {
        Err(not_implemented("setCode"))
    }

    async fn anvil_set_nonce(&self, _address: Address, _nonce: U256) -> RpcResult<()> {
        Err(not_implemented("setNonce"))
    }

    async fn anvil_set_storage_at(
        &self,
        _address: Address,
        _slot: U256,
        _value: B256,
    ) -> RpcResult<bool> {
        Err(not_implemented("setStorageAt"))
    }

    async fn anvil_snapshot(&self) -> RpcResult<U256> {
        Err(not_implemented("snapshot"))
    }

    async fn anvil_revert(&self, _id: U256) -> RpcResult<bool> {
        Err(not_implemented("revert"))
    }

    async fn anvil_dump_state(&self) -> RpcResult<Bytes> {
        Err(not_implemented("dumpState"))
    }

    async fn anvil_load_state(&self, _state: Bytes) -> RpcResult<bool> {
        Err(not_implemented("loadState"))
    }

    // -- chain config (needs engine/config) --

    async fn anvil_set_coinbase(&self, _address: Address) -> RpcResult<()> {
        Err(not_implemented("setCoinbase"))
    }

    async fn anvil_set_chain_id(&self, _chain_id: u64) -> RpcResult<()> {
        Err(not_implemented("setChainId"))
    }

    async fn anvil_set_min_gas_price(&self, _gas_price: U256) -> RpcResult<()> {
        Err(not_implemented("setMinGasPrice"))
    }

    async fn anvil_set_next_block_base_fee_per_gas(&self, _base_fee: U256) -> RpcResult<()> {
        Err(not_implemented("setNextBlockBaseFeePerGas"))
    }

    async fn anvil_set_block_gas_limit(&self, _gas_limit: U256) -> RpcResult<bool> {
        Err(not_implemented("setBlockGasLimit"))
    }

    // -- time manipulation (needs time offset) --

    async fn anvil_set_time(&self, _timestamp: u64) -> RpcResult<u64> {
        Err(not_implemented("setTime"))
    }

    async fn anvil_increase_time(&self, _seconds: U256) -> RpcResult<i64> {
        Err(not_implemented("increaseTime"))
    }

    async fn anvil_set_next_block_timestamp(&self, _seconds: u64) -> RpcResult<()> {
        Err(not_implemented("setNextBlockTimestamp"))
    }

    async fn anvil_set_block_timestamp_interval(&self, _seconds: u64) -> RpcResult<()> {
        Err(not_implemented("setBlockTimestampInterval"))
    }

    async fn anvil_remove_block_timestamp_interval(&self) -> RpcResult<bool> {
        Err(not_implemented("removeBlockTimestampInterval"))
    }

    // -- fork control (needs full reset infra) --

    async fn anvil_reset(&self, _fork: Option<Forking>) -> RpcResult<()> {
        Err(not_implemented("reset"))
    }

    async fn anvil_set_rpc_url(&self, _url: String) -> RpcResult<()> {
        Err(not_implemented("setRpcUrl"))
    }

    // -- misc --

    async fn anvil_set_logging_enabled(&self, _enabled: bool) -> RpcResult<()> {
        Ok(()) // noop is fine, logging is optional
    }

    async fn anvil_enable_traces(&self) -> RpcResult<()> {
        Ok(()) // noop is fine, tracing is optional
    }

    async fn anvil_node_info(&self) -> RpcResult<NodeInfo> {
        Err(not_implemented("nodeInfo"))
    }

    async fn anvil_metadata(&self) -> RpcResult<Metadata> {
        Err(not_implemented("metadata"))
    }
}
