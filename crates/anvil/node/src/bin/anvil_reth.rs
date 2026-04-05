use alloy_genesis::Genesis;
use alloy_primitives::U256;
use alloy_signer_local::{coins_bip39::English, MnemonicBuilder, PrivateKeySigner};
use clap::Parser;
use reth_chainspec::ChainSpec;
use reth_node_anvil::{AnvilAddOns, AnvilNode};
use reth_node_builder::{NodeBuilder, NodeConfig, NodeHandle};
use reth_node_core::args::RpcServerArgs;
use reth_provider::providers::BlockchainProvider;
use reth_rpc_server_types::RpcModuleSelection;
use std::sync::Arc;

const DEV_MNEMONIC: &str = "test test test test test test test test test test test junk";

#[derive(Parser, Debug)]
#[command(name = "anvil-reth", about = "Anvil dev node built on reth")]
struct Args {
    /// Port to listen on.
    #[arg(short, long, default_value = "8545")]
    port: u16,

    /// Chain ID.
    #[arg(long, default_value = "31337")]
    chain_id: u64,

    /// Number of dev accounts to generate.
    #[arg(short, long, default_value = "10")]
    accounts: usize,

    /// Balance of each dev account in ETH.
    #[arg(long, default_value = "10000")]
    balance: u64,
}

fn dev_accounts(num: usize) -> Vec<PrivateKeySigner> {
    (0..num)
        .map(|i| {
            MnemonicBuilder::<English>::default()
                .phrase(DEV_MNEMONIC)
                .derivation_path(format!("m/44'/60'/0'/0/{i}"))
                .unwrap()
                .build()
                .expect("valid mnemonic")
        })
        .collect()
}

fn dev_genesis(chain_id: u64, signers: &[PrivateKeySigner], balance_eth: u64) -> Arc<ChainSpec> {
    let balance = U256::from(balance_eth) * U256::from(10u64.pow(18));
    let alloc = signers
        .iter()
        .map(|s| {
            (
                s.address(),
                alloy_genesis::GenesisAccount { balance, ..Default::default() },
            )
        })
        .collect();

    let genesis = Genesis {
        gas_limit: 30_000_000,
        alloc,
        config: alloy_genesis::ChainConfig {
            chain_id,
            homestead_block: Some(0),
            eip150_block: Some(0),
            eip155_block: Some(0),
            eip158_block: Some(0),
            byzantium_block: Some(0),
            constantinople_block: Some(0),
            petersburg_block: Some(0),
            istanbul_block: Some(0),
            berlin_block: Some(0),
            london_block: Some(0),
            terminal_total_difficulty: Some(U256::ZERO),
            terminal_total_difficulty_passed: true,
            shanghai_time: Some(0),
            cancun_time: Some(0),
            prague_time: Some(0),
            ..Default::default()
        },
        ..Default::default()
    };

    Arc::new(genesis.into())
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = Args::parse();

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let signers = dev_accounts(args.accounts);
    let chain_spec = dev_genesis(args.chain_id, &signers, args.balance);

    let mut rpc = RpcServerArgs::default()
        .with_http()
        .with_http_api(RpcModuleSelection::All)
        .with_ws()
        .with_ws_api(RpcModuleSelection::All);
    rpc.http_port = args.port;
    rpc.ws_port = args.port + 1;

    let node_config = NodeConfig::test().with_chain(chain_spec).with_rpc(rpc);
    let runtime = reth_tasks::Runtime::test();

    println!();
    println!("                       _  _              _   _     ");
    println!("       __ _ _ ____ __ (_)| |    _ _ ___ | |_| |_   ");
    println!("      / _` | '_ \\ V /| || |___| '_/ -_)|  _| ' \\  ");
    println!("      \\__,_|_| |_|\\_/ |_||_____|_| \\___| \\__|_||_| ");
    println!();

    println!("Available Accounts");
    println!("==================");
    for (i, signer) in signers.iter().enumerate() {
        println!("({i}) {:?} ({} ETH)", signer.address(), args.balance);
    }

    println!();
    println!("Private Keys");
    println!("==================");
    for (i, signer) in signers.iter().enumerate() {
        println!(
            "({i}) 0x{}",
            alloy_primitives::hex::encode(signer.credential().to_bytes())
        );
    }

    println!();
    println!("Chain ID:        {}", args.chain_id);
    println!("Gas Limit:       30000000");
    println!("Base Fee:        1 gwei");
    println!();

    let NodeHandle { node, .. } = NodeBuilder::new(node_config)
        .testing_node(runtime)
        .with_types_and_provider::<AnvilNode, BlockchainProvider<_>>()
        .with_components(AnvilNode::components())
        .with_add_ons(AnvilAddOns::default())
        .launch()
        .await?;

    let rpc_handle = node.rpc_server_handle();

    if let Some(url) = rpc_handle.http_url() {
        println!("Listening on {url}");
    }
    if let Some(url) = rpc_handle.ws_url() {
        println!("WS on {url}");
    }
    println!();

    // wait forever
    futures::future::pending::<()>().await;

    Ok(())
}
