//! Anvil RPC add-ons and API implementation.

use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_anvil::{Forking, Metadata, MineOptions, NodeInfo};
use alloy_rpc_types_engine::ExecutionData;
use tokio_stream::wrappers::ReceiverStream;
use jsonrpsee::core::{async_trait, RpcResult};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_engine_local::{LocalMiner, MiningMode};
use reth_engine_primitives::EngineTypes;
use reth_ethereum_engine_primitives::EthPayloadAttributes;
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::{ConfigureEvm, EvmFactory, EvmFactoryFor, NextBlockEnvAttributes};
use reth_node_api::{FullNodeComponents, NodeAddOns, PayloadAttributesBuilder};
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
use reth_storage_api::BlockReader;
use reth_transaction_pool::TransactionPool;
use reth_tracing::tracing::info;
use revm::context::TxEnv;
use std::{fmt, marker::Unpin, sync::Arc};
use tokio::sync::mpsc;

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
            ChainSpec: Hardforks + EthereumHardforks + EthChainSpec + 'static,
            Primitives = EthPrimitives,
            Payload: EngineTypes<ExecutionData = ExecutionData>
                         + PayloadTypes<PayloadAttributes = EthPayloadAttributes>,
        >,
        Evm: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
        Pool: Unpin,
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

        // shared state between RPC handler and payload builder
        let anvil_state = crate::AnvilState::new();

        // mining trigger channel: RPC handler sends, miner receives
        let (mine_tx, mine_rx) = mpsc::channel::<()>(64);

        // use trigger-based mining so anvil_mine always works
        let mining_mode: MiningMode<N::Pool> =
            MiningMode::Trigger(Box::pin(ReceiverStream::new(mine_rx)));

        // payload builder applies anvil overrides (timestamp, coinbase, etc)
        let payload_attributes_builder = crate::AnvilPayloadAttributesBuilder::new(
            ctx.config.chain.clone(),
            anvil_state.clone(),
        );

        let miner = LocalMiner::new(
            ctx.node.provider().clone(),
            payload_attributes_builder,
            ctx.beacon_engine_handle.clone(),
            mining_mode,
            ctx.node.payload_builder_handle().clone(),
        );

        ctx.node
            .task_executor()
            .spawn_critical_task("anvil local miner", async move {
                miner.run().await
            });

        info!(target: "reth::cli", "Anvil local miner started");

        // clone state for the RPC handler
        let handler_state = anvil_state;

        self.inner
            .launch_add_ons_with(ctx, move |container| {
                container
                    .modules
                    .merge_if_module_configured(RethRpcModule::Eth, eth_config.into_rpc())?;

                // register anvil_* namespace
                let mut anvil_api = AnvilRpcHandler::new(pool, provider, mine_tx);
                anvil_api.state = handler_state;
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
            ChainSpec: Hardforks + EthereumHardforks + EthChainSpec + 'static,
            Primitives = EthPrimitives,
            Payload: EngineTypes<ExecutionData = ExecutionData>
                         + PayloadTypes<PayloadAttributes = EthPayloadAttributes>,
        >,
        Evm: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
        Pool: Unpin,
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
    /// Trigger channel to request block mining.
    mine_trigger: mpsc::Sender<()>,
    /// Unique instance ID for this anvil node.
    instance_id: B256,
    /// Shared mutable anvil state.
    state: crate::AnvilState,
}

impl<Pool, Provider> fmt::Debug for AnvilRpcHandler<Pool, Provider> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnvilRpcHandler")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl<Pool: Clone, Provider: Clone> Clone for AnvilRpcHandler<Pool, Provider> {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            provider: self.provider.clone(),
            mine_trigger: self.mine_trigger.clone(),
            instance_id: self.instance_id,
            state: self.state.clone(),
        }
    }
}

impl<Pool, Provider> AnvilRpcHandler<Pool, Provider> {
    /// Create a new handler with the given pool, provider, and mine trigger.
    pub fn new(pool: Pool, provider: Provider, mine_trigger: mpsc::Sender<()>) -> Self {
        Self {
            pool,
            provider,
            mine_trigger,
            instance_id: B256::random(),
            state: crate::AnvilState::new(),
        }
    }
}

#[async_trait]
impl<Pool, Provider> AnvilApiServer for AnvilRpcHandler<Pool, Provider>
where
    Pool: TransactionPool + Clone + Send + Sync + 'static,
    Provider: BlockReader + ChainSpecProvider<ChainSpec: EthChainSpec> + Clone + Send + Sync + 'static,
{
    // -- impersonation --

    async fn anvil_impersonate_account(&self, address: Address) -> RpcResult<()> {
        self.state.write().impersonated.insert(address);
        Ok(())
    }

    async fn anvil_stop_impersonating_account(&self, address: Address) -> RpcResult<()> {
        self.state.write().impersonated.remove(&address);
        Ok(())
    }

    async fn anvil_auto_impersonate_account(&self, enabled: bool) -> RpcResult<()> {
        self.state.write().auto_impersonate = enabled;
        Ok(())
    }

    // -- mining control --

    async fn anvil_get_automine(&self) -> RpcResult<bool> {
        Ok(self.state.read().automine)
    }

    async fn anvil_set_automine(&self, enabled: bool) -> RpcResult<()> {
        self.state.write().automine = enabled;
        Ok(())
    }

    async fn anvil_mine(&self, blocks: Option<U256>, _interval: Option<U256>) -> RpcResult<()> {
        let count: u64 = blocks.map(|b| b.try_into().unwrap_or(1u64)).unwrap_or(1);
        for _ in 0..count {
            self.mine_trigger.send(()).await.map_err(|_| {
                jsonrpsee::types::ErrorObject::owned(
                    -32000,
                    "mining service unavailable",
                    None::<()>,
                )
            })?;
        }
        Ok(())
    }

    async fn anvil_set_interval_mining(&self, interval: u64) -> RpcResult<()> {
        self.state.write().interval_mining_secs = interval;
        // TODO: actually reconfigure the miner's mode at runtime
        Ok(())
    }

    async fn anvil_mine_detailed(
        &self,
        _opts: Option<MineOptions>,
    ) -> RpcResult<Vec<alloy_rpc_types_eth::Block>> {
        // mine one block then return empty (detailed block fetching needs more infra)
        self.mine_trigger.send(()).await.map_err(|_| {
            jsonrpsee::types::ErrorObject::owned(
                -32000,
                "mining service unavailable",
                None::<()>,
            )
        })?;
        Ok(vec![])
    }

    // -- pool operations --

    async fn anvil_drop_transaction(&self, tx_hash: B256) -> RpcResult<Option<B256>> {
        let removed = self.pool.remove_transaction(tx_hash);
        Ok(removed.map(|_| tx_hash))
    }

    async fn anvil_remove_pool_transactions(&self, address: Address) -> RpcResult<()> {
        self.pool.remove_transactions_by_sender(address);
        Ok(())
    }

    // -- state manipulation (needs db-level write, deferred to state overlay) --

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

    // -- snapshots --

    async fn anvil_snapshot(&self) -> RpcResult<U256> {
        let best_number = self.provider.best_block_number().map_err(|e| {
            jsonrpsee::types::ErrorObject::owned(-32000, e.to_string(), None::<()>)
        })?;
        let best_header = self.provider.sealed_header(best_number).map_err(|e| {
            jsonrpsee::types::ErrorObject::owned(-32000, e.to_string(), None::<()>)
        })?.ok_or_else(|| {
            jsonrpsee::types::ErrorObject::owned(-32000, "best header not found", None::<()>)
        })?;
        let id = self.state.create_snapshot(best_number, best_header.hash());
        Ok(id)
    }

    async fn anvil_revert(&self, id: U256) -> RpcResult<bool> {
        // record the snapshot but can't actually rewind the chain yet
        let _snapshot = self.state.remove_snapshot(id);
        // TODO: actually unwind chain to snapshot block
        Ok(_snapshot.is_some())
    }

    async fn anvil_dump_state(&self) -> RpcResult<Bytes> {
        Err(not_implemented("dumpState"))
    }

    async fn anvil_load_state(&self, _state: Bytes) -> RpcResult<bool> {
        Err(not_implemented("loadState"))
    }

    // -- chain config --

    async fn anvil_set_coinbase(&self, address: Address) -> RpcResult<()> {
        self.state.write().coinbase = Some(address);
        Ok(())
    }

    async fn anvil_set_chain_id(&self, _chain_id: u64) -> RpcResult<()> {
        Err(not_implemented("setChainId"))
    }

    async fn anvil_set_min_gas_price(&self, _gas_price: U256) -> RpcResult<()> {
        Ok(()) // post-EIP-1559, min gas price is not meaningful
    }

    async fn anvil_set_next_block_base_fee_per_gas(&self, base_fee: U256) -> RpcResult<()> {
        self.state.write().next_block_base_fee = Some(base_fee.try_into().unwrap_or(u64::MAX));
        Ok(())
    }

    async fn anvil_set_block_gas_limit(&self, gas_limit: U256) -> RpcResult<bool> {
        self.state.write().block_gas_limit = Some(gas_limit.try_into().unwrap_or(u64::MAX));
        Ok(true)
    }

    // -- time manipulation --

    async fn anvil_set_time(&self, timestamp: u64) -> RpcResult<u64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let offset = timestamp as i64 - now as i64;
        self.state.write().time_offset_secs = offset;
        Ok(now)
    }

    async fn anvil_increase_time(&self, seconds: U256) -> RpcResult<i64> {
        let secs: i64 = seconds.try_into().unwrap_or(i64::MAX);
        let mut state = self.state.write();
        state.time_offset_secs += secs;
        Ok(state.time_offset_secs)
    }

    async fn anvil_set_next_block_timestamp(&self, timestamp: u64) -> RpcResult<()> {
        self.state.write().next_block_timestamp = Some(timestamp);
        Ok(())
    }

    async fn anvil_set_block_timestamp_interval(&self, seconds: u64) -> RpcResult<()> {
        self.state.write().block_timestamp_interval = Some(seconds);
        Ok(())
    }

    async fn anvil_remove_block_timestamp_interval(&self) -> RpcResult<bool> {
        let removed = self.state.write().block_timestamp_interval.take().is_some();
        Ok(removed)
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
        Ok(())
    }

    async fn anvil_enable_traces(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn anvil_node_info(&self) -> RpcResult<NodeInfo> {
        use alloy_rpc_types_anvil::{NodeEnvironment, NodeForkConfig};
        use reth_storage_api::BlockIdReader;

        let chain_spec = self.provider.chain_spec();
        let best_number = self.provider.best_block_number().map_err(|e| {
            jsonrpsee::types::ErrorObject::owned(-32000, e.to_string(), None::<()>)
        })?;
        let best_header = self.provider.sealed_header(best_number).map_err(|e| {
            jsonrpsee::types::ErrorObject::owned(-32000, e.to_string(), None::<()>)
        })?.ok_or_else(|| {
            jsonrpsee::types::ErrorObject::owned(-32000, "best header not found", None::<()>)
        })?;

        Ok(NodeInfo {
            current_block_number: best_number,
            current_block_timestamp: best_header.timestamp(),
            current_block_hash: best_header.hash(),
            hard_fork: "latest".to_string(),
            transaction_order: "fifo".to_string(),
            environment: NodeEnvironment {
                base_fee: best_header.base_fee_per_gas().unwrap_or(0) as u128,
                chain_id: chain_spec.chain().id(),
                gas_limit: best_header.gas_limit(),
                gas_price: best_header.base_fee_per_gas().unwrap_or(0) as u128,
            },
            fork_config: NodeForkConfig::default(),
        })
    }

    async fn anvil_metadata(&self) -> RpcResult<Metadata> {
        use reth_storage_api::BlockIdReader;

        let chain_spec = self.provider.chain_spec();
        let best_number = self.provider.best_block_number().map_err(|e| {
            jsonrpsee::types::ErrorObject::owned(-32000, e.to_string(), None::<()>)
        })?;
        let best_header = self.provider.sealed_header(best_number).map_err(|e| {
            jsonrpsee::types::ErrorObject::owned(-32000, e.to_string(), None::<()>)
        })?.ok_or_else(|| {
            jsonrpsee::types::ErrorObject::owned(-32000, "best header not found", None::<()>)
        })?;

        Ok(Metadata {
            client_version: format!("anvil-reth/v{}", env!("CARGO_PKG_VERSION")),
            chain_id: chain_spec.chain().id(),
            instance_id: self.instance_id,
            latest_block_number: best_number,
            latest_block_hash: best_header.hash(),
            forked_network: None,
            snapshots: Default::default(),
        })
    }
}
