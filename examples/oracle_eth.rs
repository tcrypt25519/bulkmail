use alloy::primitives::{Address, U256};
use alloy_node_bindings::Anvil;
use bulkmail::{
    Chain, Eth, EthClient, EthFeeManager, EthReplayProtection, EthRetryStrategy, Message, Sender,
};
use log::{error, info, LevelFilter};
use simple_logger::SimpleLogger;
use std::{sync::Arc, time::Duration};
use thiserror::Error;
use tokio::time::sleep;

#[derive(Error, Debug)]
pub enum Error {
    #[error("transaction manager error: {0}")]
    TM(#[from] bulkmail::Error),
    #[error("chain error: {0}")]
    Chain(#[from] bulkmail::chain::Error),
}

const CHAIN_ID: u64 = 1337;
const BLOCK_TIME: u64 = 1;

#[tokio::main]
async fn main() -> Result<(), Error> {
    SimpleLogger::new()
        .with_level(LevelFilter::Info)
        .init()
        .expect("Failed to initialize logger");

    // Start Anvil server
    let anvil = match Anvil::new()
        .block_time(BLOCK_TIME)
        .chain_id(CHAIN_ID)
        .try_spawn()
    {
        Ok(a) => a,
        Err(e) => {
            error!("Anvil error: {}", e);
            return Ok(());
        }
    };
    info!("Anvil running at {}", anvil.ws_endpoint());

    // Get sender key and address
    let keys = anvil.keys();
    let sender_key = keys[0].clone();
    let sender_addr = Address::from_private_key(&sender_key.clone().into());

    // Start services
    let chain = Chain::new(&anvil.ws_endpoint(), sender_key.clone().into(), CHAIN_ID).await?;
    let chain_arc: Arc<dyn bulkmail::LegacyChainClient> = Arc::new(chain.clone());
    let client = Arc::new(EthClient::new(chain_arc.clone()));
    let fees = Arc::new(EthFeeManager::new());
    let replay = Arc::new(EthReplayProtection::new(chain_arc, sender_addr).await?);
    let retry = Arc::new(EthRetryStrategy::new());
    let sender = Arc::new(Sender::<Eth>::new(client, fees, replay, retry));

    // Spawn a new task for continuous message sending
    let sender_clone = sender.clone();
    tokio::spawn(async move {
        if let Err(e) = continuous_send(sender_addr, sender_clone, chain).await {
            error!("Error in continuous send: {}", e);
        }
    });

    sender.run().await?;

    Ok(())
}

async fn continuous_send(
    addr: Address,
    sender: Arc<Sender<Eth>>,
    chain: Chain,
) -> Result<(), Error> {
    loop {
        // Send a new message
        let mut msg = Message::default();
        msg.to = Some(Address::default());
        msg.gas = 21_000u64;
        msg.value = U256::from(1_000_000u64); // 1 gwei
        sender.add_message(msg).await;

        // Generate a random delay between 500ms and 1s
        let delay = rand::random_range(20..200);
        sleep(Duration::from_millis(delay)).await;

        // Print some status info
        let block_number = chain.get_block_number().await?;
        let nonce = chain.get_account_nonce(addr).await?;
        info!("Block number: {}, nonce: {}", block_number, nonce);
    }
}
