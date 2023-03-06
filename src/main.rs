use log::{error, info};
use serde_json;
use solana_bench_mango::{
    cli,
    confirmation_strategies::confirmations_by_blocks,
    helpers::{
        get_latest_blockhash, get_mango_market_perps_cache, start_blockhash_polling_service,
        write_block_data_into_csv, write_transaction_data_into_csv,
    },
    mango::{AccountKeys, MangoConfig},
    market_markers::start_market_making_threads,
    rotating_queue::RotatingQueue,
    states::{BlockData, PerpMarketCache, TransactionConfirmRecord, TransactionSendRecord}, crank, account_write_filter,
};
use solana_client::{
    rpc_client::RpcClient, tpu_client::TpuClient, connection_cache::ConnectionCache,
};
use solana_quic_client::{QuicPool, QuicConnectionManager, QuicConfig};
use solana_sdk::commitment_config::CommitmentConfig;

use std::{
    fs,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, RwLock,
    },
    thread::{Builder, JoinHandle}, net::{IpAddr, Ipv4Addr},
};

#[tokio::main]
async fn main() {
    solana_logger::setup_with_default("solana=info");
    solana_metrics::set_panic_hook("bench-mango", /*version:*/ None);

    let matches = cli::build_args(solana_version::version!()).get_matches();
    let cli_config = cli::extract_args(&matches);

    let cli::Config {
        json_rpc_url,
        websocket_url,
        id,
        account_keys,
        mango_keys,
        duration,
        quotes_per_second,
        transaction_save_file,
        block_data_save_file,
        mango_cluster,
        txs_batch_size,
        ..
    } = &cli_config;

    let transaction_save_file = transaction_save_file.clone();
    let block_data_save_file = block_data_save_file.clone();

    info!("Connecting to the cluster");

    let account_keys_json = fs::read_to_string(account_keys).expect("unable to read accounts file");
    let account_keys_parsed: Vec<AccountKeys> =
        serde_json::from_str(&account_keys_json).expect("accounts JSON was not well-formatted");

    let mango_keys_json = fs::read_to_string(mango_keys).expect("unable to read mango keys file");
    let mango_keys_parsed: MangoConfig =
        serde_json::from_str(&mango_keys_json).expect("mango JSON was not well-formatted");

    let mango_group_id = mango_cluster;
    let mango_group_config = mango_keys_parsed
        .groups
        .iter()
        .find(|g| g.name == *mango_group_id)
        .unwrap();

    let number_of_tpu_clients: usize = 1;
    let rpc_clients = RotatingQueue::<Arc<RpcClient>>::new(number_of_tpu_clients, || {
        Arc::new(RpcClient::new_with_commitment(
            json_rpc_url.to_string(),
            CommitmentConfig::confirmed(),
        ))
    });

    let connection_cache = ConnectionCache::new_with_client_options(
        4,
        None,
        Some((id, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)))),
        None,
    );
    let quic_connection_cache = if let ConnectionCache::Quic(connection_cache) = connection_cache {
        Some(connection_cache)
    } else {
        None
    };

    let tpu_client_pool = Arc::new(RotatingQueue::<Arc<TpuClient<QuicPool, QuicConnectionManager, QuicConfig>>>::new(
        number_of_tpu_clients,
        || {
            let quic_connection_cache = quic_connection_cache.clone();
            Arc::new(
                TpuClient::new_with_connection_cache(
                    rpc_clients.get().clone(),
                    &websocket_url,
                    solana_client::tpu_client::TpuClientConfig::default(),
                    quic_connection_cache.unwrap(),
                )
                .unwrap(),
            )
        },
    ));

    info!(
        "accounts:{:?} markets:{:?} quotes_per_second:{:?} expected_tps:{:?} duration:{:?}",
        account_keys_parsed.len(),
        mango_group_config.perp_markets.len(),
        quotes_per_second,
        account_keys_parsed.len()
            * mango_group_config.perp_markets.len()
            * quotes_per_second.clone() as usize,
        duration
    );

    // continuosly fetch blockhash
    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        json_rpc_url.to_string(),
        CommitmentConfig::confirmed(),
    ));
    let exit_signal = Arc::new(AtomicBool::new(false));
    let blockhash = Arc::new(RwLock::new(get_latest_blockhash(&rpc_client.clone())));
    let current_slot = Arc::new(AtomicU64::new(0));
    let blockhash_thread = start_blockhash_polling_service(
        exit_signal.clone(),
        blockhash.clone(),
        current_slot.clone(),
        rpc_client.clone(),
    );

    let perp_market_caches: Vec<PerpMarketCache> =
        get_mango_market_perps_cache(rpc_client.clone(), mango_group_config);

    let (tx_record_sx, tx_record_rx) = crossbeam_channel::unbounded();

    crank::start(
        tx_record_sx.clone(),
        exit_signal.clone(),
        blockhash.clone(),
        current_slot.clone(),
        tpu_client_pool.clone(),
        mango_group_config,
        id
    );

    let mm_threads: Vec<JoinHandle<()>> = start_market_making_threads(
        account_keys_parsed.clone(),
        perp_market_caches.clone(),
        tx_record_sx.clone(),
        exit_signal.clone(),
        blockhash.clone(),
        current_slot.clone(),
        tpu_client_pool.clone(),
        &duration,
        *quotes_per_second,
        *txs_batch_size,
    );



    let duration = duration.clone();
    let quotes_per_second = quotes_per_second.clone();
    let account_keys_parsed = account_keys_parsed.clone();
    let tx_confirm_records: Arc<RwLock<Vec<TransactionConfirmRecord>>> =
        Arc::new(RwLock::new(Vec::new()));
    let tx_timeout_records: Arc<RwLock<Vec<TransactionSendRecord>>> =
        Arc::new(RwLock::new(Vec::new()));

    let tx_block_data = Arc::new(RwLock::new(Vec::<BlockData>::new()));

    let confirmation_thread = Builder::new()
        .name("solana-client-sender".to_string())
        .spawn(move || {
            let recv_limit = account_keys_parsed.len()
                * perp_market_caches.len()
                * duration.as_secs() as usize
                * quotes_per_second as usize;

            //confirmation_by_querying_rpc(recv_limit, rpc_client.clone(), &tx_record_rx, tx_confirm_records.clone(), tx_timeout_records.clone());
            confirmations_by_blocks(
                rpc_clients,
                &current_slot,
                recv_limit,
                tx_record_rx,
                tx_confirm_records.clone(),
                tx_timeout_records.clone(),
                tx_block_data.clone(),
            );

            let confirmed: Vec<TransactionConfirmRecord> = {
                let lock = tx_confirm_records.write().unwrap();
                (*lock).clone()
            };
            let total_signed = account_keys_parsed.len()
                * perp_market_caches.len()
                * duration.as_secs() as usize
                * quotes_per_second as usize;
            info!(
                "confirmed {} signatures of {} rate {}%",
                confirmed.len(),
                total_signed,
                (confirmed.len() * 100) / total_signed
            );
            let error_count = confirmed.iter().filter(|tx| !tx.error.is_empty()).count();
            info!(
                "errors counted {} rate {}%",
                error_count,
                (error_count as usize * 100) / total_signed
            );
            let timeouts: Vec<TransactionSendRecord> = {
                let timeouts = tx_timeout_records.clone();
                let lock = timeouts.write().unwrap();
                (*lock).clone()
            };
            info!(
                "timeouts counted {} rate {}%",
                timeouts.len(),
                (timeouts.len() * 100) / total_signed
            );

            // let mut confirmation_times = confirmed
            //     .iter()
            //     .map(|r| {
            //         r.confirmed_at
            //             .signed_duration_since(r.sent_at)
            //             .num_milliseconds()
            //     })
            //     .collect::<Vec<_>>();
            // confirmation_times.sort();
            // info!(
            //     "confirmation times min={} max={} median={}",
            //     confirmation_times.first().unwrap(),
            //     confirmation_times.last().unwrap(),
            //     confirmation_times[confirmation_times.len() / 2]
            // );

            write_transaction_data_into_csv(
                transaction_save_file,
                tx_confirm_records,
                tx_timeout_records,
            );

            write_block_data_into_csv(block_data_save_file, tx_block_data);
        })
        .unwrap();

    for t in mm_threads {
        if let Err(err) = t.join() {
            error!("mm join failed with: {:?}", err);
        }
    }

    info!("joined all mm_threads");

    if let Err(err) = confirmation_thread.join() {
        error!("confirmation join fialed with: {:?}", err);
    }

    info!("joined confirmation thread");

    exit_signal.store(true, Ordering::Relaxed);

    if let Err(err) = blockhash_thread.join() {
        error!("blockhash join failed with: {:?}", err);
    }
}
