use {
    log::{error, info},
    serde_json,
    mango_simulation::{
        cli,
        confirmation_strategies::confirmations_by_blocks,
        crank,
        helpers::{
            get_latest_blockhash, get_mango_market_perps_cache, start_blockhash_polling_service,
            to_sdk_pk, write_block_data_into_csv, write_transaction_data_into_csv,
        },
        keeper::start_keepers,
        mango::{AccountKeys, MangoConfig},
        market_markers::start_market_making_threads,
        states::{BlockData, PerpMarketCache, TransactionConfirmRecord, TransactionSendRecord},
        websocket_source::KeeperConfig,
    },
    solana_client::{
        connection_cache::ConnectionCache, rpc_client::RpcClient, tpu_client::TpuClient,
    },
    solana_metrics::datapoint_info,
    solana_program::pubkey::Pubkey,
    solana_sdk::commitment_config::CommitmentConfig,
    std::{
        fs,
        net::{IpAddr, Ipv4Addr},
        str::FromStr,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, RwLock,
        },
        thread::sleep,
        thread::{Builder, JoinHandle},
        time::Duration,
    },
};

#[derive(Default)]
struct MangoBencherStats {
    recv_limit: usize,
    num_confirmed_txs: usize,
    num_error_txs: usize,
    num_timeout_txs: usize,
}

impl MangoBencherStats {
    fn report(&self, name: &'static str) {
        datapoint_info!(
            name,
            ("recv_limit", self.recv_limit, i64),
            ("num_confirmed_txs", self.num_confirmed_txs, i64),
            ("num_error_txs", self.num_error_txs, i64),
            ("num_timeout_txs", self.num_timeout_txs, i64),
            (
                "percent_confirmed_txs",
                (self.num_confirmed_txs * 100) / self.recv_limit,
                i64
            ),
            (
                "percent_error_txs",
                (self.num_error_txs * 100) / self.recv_limit,
                i64
            ),
            (
                "percent_timeout_txs",
                (self.num_timeout_txs * 100) / self.recv_limit,
                i64
            ),
        );
    }
}

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
        priority_fees_proba,
        keeper_authority,
        number_of_markers_per_mm,
        ..
    } = &cli_config;
    let number_of_markers_per_mm = *number_of_markers_per_mm;

    let transaction_save_file = transaction_save_file.clone();
    let block_data_save_file = block_data_save_file.clone();

    info!(
        "Connecting to the cluster {}, {}",
        json_rpc_url, websocket_url
    );

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

    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        json_rpc_url.to_string(),
        CommitmentConfig::confirmed(),
    ));

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

    let tpu_client = Arc::new(
        TpuClient::new_with_connection_cache(
            rpc_client.clone(),
            &websocket_url,
            solana_client::tpu_client::TpuClientConfig::default(),
            quic_connection_cache.unwrap(),
        )
        .unwrap(),
    );

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
    let mango_program_pk = Pubkey::from_str(mango_group_config.mango_program_id.as_str()).unwrap();
    let perp_market_caches: Vec<PerpMarketCache> =
        get_mango_market_perps_cache(rpc_client.clone(), mango_group_config, &mango_program_pk);

    let quote_root_bank =
        Pubkey::from_str(mango_group_config.tokens.last().unwrap().root_key.as_str()).unwrap();
    let quote_node_banks = mango_group_config
        .tokens
        .last()
        .unwrap()
        .node_keys
        .iter()
        .map(|x| Pubkey::from_str(x.as_str()).unwrap())
        .collect();
    // start keeper if keeper authority is present
    let keepers_jl = if let Some(keeper_authority) = keeper_authority {
        let jl = start_keepers(
            exit_signal.clone(),
            tpu_client.clone(),
            perp_market_caches.clone(),
            blockhash.clone(),
            keeper_authority,
            quote_root_bank,
            quote_node_banks,
        );
        Some(jl)
    } else {
        None
    };

    let (tx_record_sx, tx_record_rx) = crossbeam_channel::unbounded();
    let from_slot = current_slot.load(Ordering::Relaxed);
    let keeper_config = KeeperConfig {
        program_id: to_sdk_pk(&mango_program_pk),
        rpc_url: json_rpc_url.clone(),
        websocket_url: websocket_url.clone(),
    };

    crank::start(
        keeper_config,
        tx_record_sx.clone(),
        exit_signal.clone(),
        blockhash.clone(),
        current_slot.clone(),
        tpu_client.clone(),
        mango_group_config,
        id,
    );

    let mm_threads: Vec<JoinHandle<()>> = start_market_making_threads(
        account_keys_parsed.clone(),
        perp_market_caches.clone(),
        tx_record_sx.clone(),
        exit_signal.clone(),
        blockhash.clone(),
        current_slot.clone(),
        tpu_client.clone(),
        &duration,
        *quotes_per_second,
        *txs_batch_size,
        *priority_fees_proba,
        number_of_markers_per_mm,
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
            let mut stats = MangoBencherStats::default();

            stats.recv_limit = account_keys_parsed.len()
                * number_of_markers_per_mm as usize
                * duration.as_secs() as usize
                * quotes_per_second as usize;

            //confirmation_by_querying_rpc(recv_limit, rpc_client.clone(), &tx_record_rx, tx_confirm_records.clone(), tx_timeout_records.clone());
            confirmations_by_blocks(
                rpc_client.clone(),
                stats.recv_limit,
                tx_record_rx,
                tx_confirm_records.clone(),
                tx_timeout_records.clone(),
                tx_block_data.clone(),
                from_slot,
            );

            let confirmed: Vec<TransactionConfirmRecord> = {
                let lock = tx_confirm_records.write().unwrap();
                (*lock).clone()
            };
            stats.num_confirmed_txs = confirmed.len();

            info!(
                "confirmed {} signatures of {} rate {}%",
                stats.num_confirmed_txs,
                stats.recv_limit,
                (stats.num_confirmed_txs * 100) / stats.recv_limit
            );
            stats.num_error_txs = confirmed.iter().filter(|tx| !tx.error.is_empty()).count();
            info!(
                "errors counted {} rate {}%",
                stats.num_error_txs,
                (stats.num_error_txs as usize * 100) / stats.recv_limit
            );
            let timeouts: Vec<TransactionSendRecord> = {
                let timeouts = tx_timeout_records.clone();
                let lock = timeouts.write().unwrap();
                (*lock).clone()
            };
            stats.num_timeout_txs = timeouts.len();
            info!(
                "timeouts counted {} rate {}%",
                stats.num_timeout_txs,
                (stats.num_timeout_txs * 100) / stats.recv_limit
            );
            stats.report("mango-bencher");
            // metrics are submitted every 10s,
            // it is necessary only because we do it once before the end of the execution
            sleep(Duration::from_secs(10));

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

    if let Some(keepers_jl) = keepers_jl {
        if let Err(err) = keepers_jl.join() {
            error!("keeper join failed with: {:?}", err);
        }
    }
}
