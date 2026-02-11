mod quic_utils;

use crate::quic_utils::{QuicError, create_client_config, create_client_endpoint};
use clap::Parser;
use quinn::Connection;
use solana_instruction::Instruction;
use solana_keypair::{EncodableKey, Keypair, Signature, Signer};
use solana_pubkey::{Pubkey, pubkey};
use solana_rpc_client::api::config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client::rpc_client::SerializableTransaction;
use solana_tls_utils::QuicClientCertificate;
use solana_tpu_client_next::connection_workers_scheduler::BindTarget;
use solana_transaction::{Message, Transaction};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

const SLOT_DELAY_BUFFER: usize = 2;
#[derive(Parser)]
struct Cli {
    #[clap(long)]
    rpc_url: String,
    #[clap(long)]
    target_leader: String,
    #[clap(long)]
    stake_keypair_path: Option<String>,
    #[clap(long)]
    signer_keypair_path: String,
}

#[tokio::main]
async fn main() {
    let args = Cli::parse();
    let stake_keypair = if let Some(kp) = args.stake_keypair_path {
        Keypair::read_from_file(&kp).unwrap()
    } else {
        Keypair::new()
    };

    let rpc = Arc::new(RpcClient::new_with_commitment(
        args.rpc_url.clone(),
        CommitmentConfig::processed(),
    ));

    let signer = Keypair::read_from_file(args.signer_keypair_path).unwrap();

    //get leader information
    let node_info = rpc.get_cluster_nodes().await.unwrap();
    let target_node_info = node_info
        .iter()
        .find(|info| info.pubkey == args.target_leader)
        .expect("cannot find target node in contact");
    println!("target node: {:?}", target_node_info);

    //get the leader schedule
    let mut current_slot = rpc.get_slot().await.unwrap();
    let leader_schedule = rpc.get_leader_schedule(Some(current_slot)).await.unwrap();
    let leader_schedule = leader_schedule.unwrap();
    let target_slots = leader_schedule
        .get(&args.target_leader)
        .expect("no target leader slots found");

    let next_leader_slot = *target_slots.first().unwrap();
    println!(
        "approx time to next target slot: {} mins",
        (next_leader_slot as f64 - current_slot as f64) * 0.4 / 60.0
    );

    //create the quic client
    let bind_socket: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let client_config = create_client_config(&QuicClientCertificate::new(Some(&stake_keypair)));
    let endpoint = create_client_endpoint(BindTarget::Address(bind_socket), client_config).unwrap();

    //connect to the validators quic port
    let conn = endpoint
        .connect(target_node_info.tpu_quic.unwrap(), "solana-tpu")
        .expect("cannot connect to target");
    let connection_res = timeout(Duration::from_secs(5), conn)
        .await
        .expect("handshake timeout");
    let connection = connection_res.expect("connection failed");

    println!("waiting for target slot");
    while next_leader_slot.saturating_sub(current_slot as usize) > SLOT_DELAY_BUFFER {
        tokio::time::sleep(Duration::from_millis(200)).await;
        current_slot = rpc.get_slot().await.unwrap();
        println!(
            "approx time to next target slot: {} mins",
            (next_leader_slot as f64 - current_slot as f64) * 0.4 / 60.0
        );
    }

    //send 10 transactions to the leader
    let (block_hash, _) = rpc
        .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
        .await
        .unwrap();
    let transactions = create_rakurai_virtual_bundle(&signer, &block_hash);

    //spam the transaction multiple times to makes sure one lands
    for _ in 0..10 {
        for transaction in &transactions {
            send_data_over_stream(&connection, transaction)
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn send_data_over_stream(connection: &Connection, data: &[u8]) -> Result<(), QuicError> {
    let mut send_stream = connection.open_uni().await?;
    send_stream.write_all(data).await.map_err(QuicError::from)?;
    Ok(())
}

pub fn create_rakurai_virtual_bundle(
    signer: &Keypair,
    block_hash: &solana_hash::Hash,
) -> Vec<Vec<u8>> {
    let source = create_dummy_source_transaction(signer, block_hash);
    let source_signature = source.get_signature();
    let backrun = create_backrun_transaction(signer, block_hash, source_signature);
    println!(
        "source signature: {:?}, backrun signature {:?}",
        source_signature,
        backrun.get_signature()
    );
    [source, backrun]
        .iter()
        .map(|txn| bincode::serialize(txn).unwrap())
        .collect::<Vec<_>>()
}

fn create_dummy_source_transaction(
    signer: &Keypair,
    block_hash: &solana_hash::Hash,
) -> Transaction {
    let compute_unit_ix =
        solana_compute_budget_interface::ComputeBudgetInstruction::set_compute_unit_limit(1000);
    let compute_price_ix =
        solana_compute_budget_interface::ComputeBudgetInstruction::set_compute_unit_price(10_000);
    let self_transfer_ix =
        solana_system_interface::instruction::transfer(&signer.pubkey(), &signer.pubkey(), 1000);
    let message = Message::new(
        &[compute_unit_ix, compute_price_ix, self_transfer_ix],
        Some(&signer.pubkey()),
    );
    Transaction::new(&[signer], message, block_hash.clone())
}

fn create_backrun_transaction(
    signer: &Keypair,
    block_hash: &solana_hash::Hash,
    source_signature: &Signature,
) -> Transaction {
    const RAKURAI_MEMO: Pubkey = pubkey!("3enQj1Awmf1WVGarKdL2NxoDWUto6XN1mX2Q3HNfghFW");
    const RAKURAI_TIP_ACCOUNT: Pubkey = pubkey!("68HZJtXe2JZebJayzr3S1c4GvkToDsbPqV5kEUicFzj7");

    let compute_limit =
        solana_compute_budget_interface::ComputeBudgetInstruction::set_compute_unit_limit(1000);
    let compute_price_ix =
        solana_compute_budget_interface::ComputeBudgetInstruction::set_compute_unit_price(10_000);
    let memo_instruction = Instruction::new_with_bytes(
        RAKURAI_MEMO,
        source_signature.as_ref(), // Pass signature as instruction data
        vec![],
    );
    let tip_transaction = solana_system_interface::instruction::transfer(
        &signer.pubkey(),
        &RAKURAI_TIP_ACCOUNT,
        10_000,
    );
    let message = Message::new(
        &[
            compute_limit,
            compute_price_ix,
            memo_instruction,
            tip_transaction,
        ],
        Some(&signer.pubkey()),
    );
    Transaction::new(&[signer], message, block_hash.clone())
}
