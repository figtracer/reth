//! Anvil payload attributes builder.
//!
//! Wraps reth's [`LocalPayloadAttributesBuilder`] and applies anvil-specific
//! overrides (timestamp, base fee, coinbase) from [`AnvilState`].

use crate::AnvilState;
use alloy_consensus::BlockHeader;
use alloy_primitives::B256;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_engine_local::LocalPayloadAttributesBuilder;
use reth_ethereum_engine_primitives::EthPayloadAttributes;
use reth_node_api::PayloadAttributesBuilder;
use reth_primitives_traits::SealedHeader;
use std::sync::Arc;

/// Payload attributes builder that applies anvil overrides.
#[derive(Debug)]
pub struct AnvilPayloadAttributesBuilder<ChainSpec> {
    inner: LocalPayloadAttributesBuilder<ChainSpec>,
    state: AnvilState,
}

impl<ChainSpec> AnvilPayloadAttributesBuilder<ChainSpec> {
    /// Create a new builder wrapping the given chain spec and anvil state.
    pub fn new(chain_spec: Arc<ChainSpec>, state: AnvilState) -> Self {
        Self {
            inner: LocalPayloadAttributesBuilder::new(chain_spec),
            state,
        }
    }
}

impl<ChainSpec> PayloadAttributesBuilder<EthPayloadAttributes, ChainSpec::Header>
    for AnvilPayloadAttributesBuilder<ChainSpec>
where
    ChainSpec: EthChainSpec + EthereumHardforks + 'static,
{
    fn build(&self, parent: &SealedHeader<ChainSpec::Header>) -> EthPayloadAttributes {
        let mut attrs = self.inner.build(parent);

        // apply timestamp override
        if let Some(ts) = self.state.take_next_block_timestamp(parent.timestamp()) {
            attrs.timestamp = ts;
        } else {
            // apply time offset
            let offset = self.state.read().time_offset_secs;
            if offset != 0 {
                if offset > 0 {
                    attrs.timestamp = attrs.timestamp.saturating_add(offset as u64);
                } else {
                    attrs.timestamp = attrs.timestamp.saturating_sub((-offset) as u64);
                }
            }
        }

        // ensure timestamp is after parent
        if attrs.timestamp <= parent.timestamp() {
            attrs.timestamp = parent.timestamp() + 1;
        }

        // apply coinbase override
        if let Some(coinbase) = self.state.coinbase() {
            attrs.suggested_fee_recipient = coinbase;
        }

        attrs
    }
}
