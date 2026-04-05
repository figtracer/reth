//! Anvil dev node built on reth's node builder.
//!
//! This crate provides [`AnvilNode`], a [`Node`] implementation that reuses reth's
//! ethereum components with anvil-specific RPC extensions for local development.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use reth_chainspec::ChainSpec;
use reth_ethereum_primitives::EthPrimitives;
use reth_node_builder::{
    components::BasicPayloadServiceBuilder, node::FullNodeTypes, Node, NodeAdapter, NodeTypes,
};
use reth_node_ethereum::{
    node::{
        EthereumConsensusBuilder, EthereumExecutorBuilder, EthereumNetworkBuilder,
        EthereumPoolBuilder,
    },
    EthEngineTypes, EthereumPayloadBuilder,
};
use reth_provider::EthStorage;

mod rpc;
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

impl NodeTypes for AnvilNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type Storage = EthStorage;
    type Payload = EthEngineTypes;
}

impl<N> Node<N> for AnvilNode
where
    N: FullNodeTypes<Types = Self>,
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
