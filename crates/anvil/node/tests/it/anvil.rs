use alloy_genesis::Genesis;
use alloy_primitives::U256;
use reth_chainspec::ChainSpec;
use reth_node_anvil::AnvilNode;
use reth_node_builder::{NodeBuilder, NodeConfig, NodeHandle};
use reth_node_core::args::RpcServerArgs;
use reth_provider::providers::BlockchainProvider;
use reth_rpc_api::anvil::AnvilApiClient;
use reth_rpc_server_types::RpcModuleSelection;
use reth_tasks::Runtime;
use std::sync::Arc;

fn custom_chain() -> Arc<ChainSpec> {
    let custom_genesis = r#"
{
    "nonce": "0x42",
    "timestamp": "0x0",
    "extraData": "0x5343",
    "gasLimit": "0x1c9c380",
    "difficulty": "0x400000000",
    "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "coinbase": "0x0000000000000000000000000000000000000000",
    "alloc": {
        "0x6Be02d1d3665660d22FF9624b7BE0551ee1Ac91b": {
            "balance": "0x4a47e3c12448f4ad000000"
        }
    },
    "number": "0x0",
    "gasUsed": "0x0",
    "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "config": {
        "ethash": {},
        "chainId": 31337,
        "homesteadBlock": 0,
        "eip150Block": 0,
        "eip155Block": 0,
        "eip158Block": 0,
        "byzantiumBlock": 0,
        "constantinopleBlock": 0,
        "petersburgBlock": 0,
        "istanbulBlock": 0,
        "berlinBlock": 0,
        "londonBlock": 0,
        "terminalTotalDifficulty": 0,
        "terminalTotalDifficultyPassed": true,
        "shanghaiTime": 0
    }
}
"#;
    let genesis: Genesis = serde_json::from_str(custom_genesis).unwrap();
    Arc::new(genesis.into())
}

#[tokio::test]
async fn can_launch_anvil_node() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let runtime = Runtime::test();

    let rpc = RpcServerArgs::default()
        .with_http()
        .with_http_api(RpcModuleSelection::All)
        .with_unused_ports();

    let node_config = NodeConfig::test().with_chain(custom_chain()).with_rpc(rpc);

    let NodeHandle { node, .. } = NodeBuilder::new(node_config)
        .testing_node(runtime)
        .with_types_and_provider::<AnvilNode, BlockchainProvider<_>>()
        .with_components(AnvilNode::components())
        .with_add_ons(reth_node_anvil::AnvilAddOns::default())
        .launch()
        .await?;

    // get HTTP URL from the rpc server handle
    let rpc_handle = node.rpc_server_handle();
    let http_url = rpc_handle.http_url().expect("HTTP RPC should be enabled");

    let client = jsonrpsee::http_client::HttpClientBuilder::default().build(&http_url)?;

    // test anvil_getAutomine
    let automine = AnvilApiClient::anvil_get_automine(&client).await?;
    assert!(automine, "automine should be true by default");

    // test anvil_metadata
    let metadata = AnvilApiClient::anvil_metadata(&client).await?;
    assert_eq!(metadata.chain_id, 31337);
    assert!(
        metadata.client_version.starts_with("anvil-reth/"),
        "unexpected client version: {}",
        metadata.client_version
    );

    // test anvil_mine
    let block_before = metadata.latest_block_number;
    AnvilApiClient::anvil_mine(&client, Some(U256::from(1)), None).await?;

    // give the miner a moment to process the trigger
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    let metadata_after = AnvilApiClient::anvil_metadata(&client).await?;
    assert!(
        metadata_after.latest_block_number > block_before,
        "block number should increase after mining: {} vs {}",
        metadata_after.latest_block_number,
        block_before
    );

    // test anvil_nodeInfo
    let node_info = AnvilApiClient::anvil_node_info(&client).await?;
    assert_eq!(node_info.environment.chain_id, 31337);

    // test anvil_impersonateAccount
    let addr = alloy_primitives::address!("0x1234567890abcdef1234567890abcdef12345678");
    AnvilApiClient::anvil_impersonate_account(&client, addr).await?;
    AnvilApiClient::anvil_stop_impersonating_account(&client, addr).await?;

    // test anvil_snapshot
    let snapshot_id = AnvilApiClient::anvil_snapshot(&client).await?;
    assert!(snapshot_id > U256::ZERO, "snapshot id should be positive");

    // test anvil_setNextBlockTimestamp
    AnvilApiClient::anvil_set_next_block_timestamp(&client, 9999999999).await?;

    // test anvil_setCoinbase
    AnvilApiClient::anvil_set_coinbase(&client, addr).await?;

    // test anvil_setNextBlockBaseFeePerGas
    AnvilApiClient::anvil_set_next_block_base_fee_per_gas(&client, U256::from(1_000_000_000u64))
        .await?;

    println!("anvil node test passed!");
    println!("  chain_id: {}", metadata.chain_id);
    println!("  client: {}", metadata.client_version);
    println!(
        "  blocks mined: {} -> {}",
        block_before, metadata_after.latest_block_number
    );

    Ok(())
}
