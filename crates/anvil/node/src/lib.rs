//! Anvil dev node built on reth's node builder.
//!
//! This crate provides [`AnvilNode`], a [`Node`] implementation that reuses reth's
//! ethereum components with anvil-specific RPC extensions for local development.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use reth_chainspec::ChainSpec;
use reth_engine_local::LocalPayloadAttributesBuilder;
use reth_ethereum_primitives::EthPrimitives;
use reth_node_api::{FullNodeComponents, PayloadAttributesBuilder};
use reth_node_builder::{
    components::BasicPayloadServiceBuilder, node::FullNodeTypes, DebugNode, Node, NodeAdapter,
    NodeTypes,
};
use reth_node_ethereum::{
    node::{
        EthereumConsensusBuilder, EthereumExecutorBuilder, EthereumNetworkBuilder,
        EthereumPoolBuilder,
    },
    EthEngineTypes, EthereumPayloadBuilder,
};
use reth_payload_primitives::PayloadTypes;
use reth_provider::EthStorage;
use std::sync::Arc;

mod payload;
mod rpc;
pub use payload::AnvilPayloadAttributesBuilder;
pub use rpc::*;

pub mod state;
pub use state::AnvilState;

/// Anvil dev node type.
///
/// Uses ethereum primitives and execution but adds anvil-specific RPC methods
/// (state manipulation, mining control, impersonation, snapshots, etc).
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct AnvilNode;

impl AnvilNode {
    /// Returns a [`ComponentsBuilder`] configured for an anvil dev node.
    pub fn components<Node>() -> reth_node_builder::components::ComponentsBuilder<
        Node,
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        EthereumNetworkBuilder,
        EthereumExecutorBuilder,
        EthereumConsensusBuilder,
    >
    where
        Node: FullNodeTypes<Types = Self>,
    {
        reth_node_builder::components::ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(EthereumPoolBuilder::default())
            .executor(EthereumExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::default())
            .network(EthereumNetworkBuilder::default())
            .consensus(EthereumConsensusBuilder::default())
    }
}

impl NodeTypes for AnvilNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type Storage = EthStorage;
    type Payload = EthEngineTypes;
}

impl<N> Node<N> for AnvilNode
where
    N: FullNodeTypes<
        Types = Self,
        Provider: reth_storage_api::AccountReader
            + reth_storage_api::DatabaseProviderFactory<
                ProviderRW: reth_storage_api::StateWriter + reth_storage_api::DBProvider,
            >,
    >,
{
    type ComponentsBuilder = reth_node_builder::components::ComponentsBuilder<
        N,
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        EthereumNetworkBuilder,
        EthereumExecutorBuilder,
        EthereumConsensusBuilder,
    >;

    type AddOns = AnvilAddOns<NodeAdapter<N>>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        reth_node_builder::components::ComponentsBuilder::default()
            .node_types::<N>()
            .pool(EthereumPoolBuilder::default())
            .executor(EthereumExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::default())
            .network(EthereumNetworkBuilder::default())
            .consensus(EthereumConsensusBuilder::default())
    }

    fn add_ons(&self) -> Self::AddOns {
        AnvilAddOns::default()
    }
}

impl<N> DebugNode<N> for AnvilNode
where
    N: FullNodeComponents<
        Types = Self,
        Provider: reth_storage_api::AccountReader
            + reth_storage_api::DatabaseProviderFactory<
                ProviderRW: reth_storage_api::StateWriter + reth_storage_api::DBProvider,
            >,
    >,
{
    type RpcBlock = alloy_rpc_types_eth::Block;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_ethereum_primitives::Block {
        rpc_block.into_consensus().convert_transactions()
    }

    fn local_payload_attributes_builder(
        chain_spec: &Self::ChainSpec,
    ) -> impl PayloadAttributesBuilder<
        <Self::Payload as PayloadTypes>::PayloadAttributes,
        reth_node_api::HeaderTy<Self>,
    > {
        LocalPayloadAttributesBuilder::new(Arc::new(chain_spec.clone()))
    }
}
