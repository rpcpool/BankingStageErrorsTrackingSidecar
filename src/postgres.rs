use std::{
    collections::BTreeMap,
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};

use anyhow::Context;
use base64::Engine;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use futures::pin_mut;
use itertools::Itertools;
use log::{debug, error, info};
use native_tls::{Certificate, Identity, TlsConnector};
use postgres_native_tls::MakeTlsConnector;
use serde::Serialize;
use solana_sdk::transaction::TransactionError;
use tokio_postgres::{
    binary_copy::BinaryCopyInWriter,
    config::SslMode,
    tls::MakeTlsConnect,
    types::{ToSql, Type},
    Client, CopyInSink, NoTls, Socket,
};

use crate::{block_info::BlockInfo, transaction_info::TransactionInfo};

pub struct PostgresSession {
    client: Client,
}

impl PostgresSession {
    pub async fn new() -> anyhow::Result<Self> {
        let pg_config = std::env::var("PG_CONFIG").context("env PG_CONFIG not found")?;
        let pg_config = pg_config.parse::<tokio_postgres::Config>()?;

        let client = if let SslMode::Disable = pg_config.get_ssl_mode() {
            Self::spawn_connection(pg_config, NoTls).await?
        } else {
            let ca_pem_b64 = std::env::var("CA_PEM_B64").context("env CA_PEM_B64 not found")?;
            let client_pks_b64 =
                std::env::var("CLIENT_PKS_B64").context("env CLIENT_PKS_B64 not found")?;
            let client_pks_password =
                std::env::var("CLIENT_PKS_PASS").context("env CLIENT_PKS_PASS not found")?;

            let ca_pem = base64::engine::general_purpose::STANDARD
                .decode(ca_pem_b64)
                .context("ca pem decode")?;
            let client_pks = base64::engine::general_purpose::STANDARD
                .decode(client_pks_b64)
                .context("client pks decode")?;

            let connector = TlsConnector::builder()
                .add_root_certificate(Certificate::from_pem(&ca_pem)?)
                .identity(
                    Identity::from_pkcs12(&client_pks, &client_pks_password).context("Identity")?,
                )
                .danger_accept_invalid_hostnames(true)
                .danger_accept_invalid_certs(true)
                .build()?;

            Self::spawn_connection(pg_config, MakeTlsConnector::new(connector)).await?
        };

        Ok(Self { client })
    }

    async fn spawn_connection<T>(
        pg_config: tokio_postgres::Config,
        connector: T,
    ) -> anyhow::Result<Client>
    where
        T: MakeTlsConnect<Socket> + Send + 'static,
        <T as MakeTlsConnect<Socket>>::Stream: Send,
    {
        let (client, connection) = pg_config
            .connect(connector)
            .await
            .context("Connecting to Postgres failed")?;

        tokio::spawn(async move {
            info!("Connecting to Postgres");

            if let Err(err) = connection.await {
                error!("Connection to Postgres broke {err:?}");
                // should restart the side car / currently no way around it
                panic!("Connection to Postgres broke {err:?}");
            }
            unreachable!("Postgres thread returned")
        });

        Ok(client)
    }

    pub fn _multiline_query(query: &mut String, args: usize, rows: usize, types: &[&str]) {
        let mut arg_index = 1usize;
        for row in 0..rows {
            query.push('(');

            for i in 0..args {
                if row == 0 && !types.is_empty() {
                    query.push_str(&format!("(${arg_index})::{}", types[i]));
                } else {
                    query.push_str(&format!("${arg_index}"));
                }
                arg_index += 1;
                if i != (args - 1) {
                    query.push(',');
                }
            }

            query.push(')');

            if row != (rows - 1) {
                query.push(',');
            }
        }
    }

    pub async fn save_banking_transaction_results(
        &self,
        txs: Vec<TransactionInfo>,
    ) -> anyhow::Result<()> {
        if txs.is_empty() {
            return Ok(());
        }
        const NUMBER_OF_ARGS: usize = 10;

        let txs: Vec<PostgresTransactionInfo> =
            txs.iter().map(PostgresTransactionInfo::from).collect();

        let statement = r#"
                COPY banking_stage_results.transaction_infos(
                    signature, errors, is_executed, is_confirmed, first_notification_slot, cu_requested, prioritization_fees, utc_timestamp, accounts_used, processed_slot
                ) FROM STDIN BINARY
            "#;
        let sink: CopyInSink<bytes::Bytes> = self.copy_in(statement).await.unwrap();
        let writer = BinaryCopyInWriter::new(
            sink,
            &[
                Type::TEXT,
                Type::TEXT,
                Type::BOOL,
                Type::BOOL,
                Type::INT8,
                Type::INT8,
                Type::INT8,
                Type::TIMESTAMPTZ,
                Type::TEXT,
                Type::INT8,
            ],
        );
        pin_mut!(writer);
        for tx in txs.iter() {
            let mut args: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(NUMBER_OF_ARGS);
            args.push(&tx.signature);
            args.push(&tx.errors);
            args.push(&tx.is_executed);
            args.push(&tx.is_confirmed);
            args.push(&tx.first_notification_slot);
            args.push(&tx.cu_requested);
            args.push(&tx.prioritization_fees);
            args.push(&tx.utc_timestamp);
            args.push(&tx.accounts_used);
            args.push(&tx.processed_slot);

            writer.as_mut().write(&args).await.unwrap();
        }
        writer.finish().await.unwrap();
        Ok(())
    }

    pub async fn copy_in(
        &self,
        statement: &str,
    ) -> Result<CopyInSink<bytes::Bytes>, tokio_postgres::error::Error> {
        // BinaryCopyInWriter
        // https://github.com/sfackler/rust-postgres/blob/master/tokio-postgres/tests/test/binary_copy.rs
        self.client.copy_in(statement).await
    }

    pub async fn save_block(&self, block_info: BlockInfo) -> anyhow::Result<()> {
        const NUMBER_OF_ARGS: usize = 11;
        let mut args: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(NUMBER_OF_ARGS);
        args.push(&block_info.block_hash);
        args.push(&block_info.slot);
        args.push(&block_info.leader_identity);
        args.push(&block_info.successful_transactions);
        args.push(&block_info.banking_stage_errors);
        args.push(&block_info.processed_transactions);
        args.push(&block_info.total_cu_used);
        args.push(&block_info.total_cu_requested);
        let heavily_writelocked_accounts =
            serde_json::to_string(&block_info.heavily_writelocked_accounts).unwrap_or_default();
        let heavily_readlocked_accounts =
            serde_json::to_string(&block_info.heavily_readlocked_accounts).unwrap_or_default();
        args.push(&heavily_writelocked_accounts);
        args.push(&heavily_readlocked_accounts);

        let supp_infos = serde_json::to_string(&block_info.sup_info).unwrap_or_default();
        args.push(&supp_infos);

        let statement = r#"
                COPY banking_stage_results.blocks(
                    block_hash, slot, leader_identity, successful_transactions, banking_stage_errors, processed_transactions, total_cu_used, total_cu_requested, heavily_writelocked_accounts, heavily_readlocked_accounts, supp_infos
                ) FROM STDIN BINARY
            "#;
        let sink: CopyInSink<bytes::Bytes> = self.copy_in(statement).await.unwrap();
        let writer = BinaryCopyInWriter::new(
            sink,
            &[
                Type::TEXT,
                Type::INT8,
                Type::TEXT,
                Type::INT8,
                Type::INT8,
                Type::INT8,
                Type::INT8,
                Type::INT8,
                Type::TEXT,
                Type::TEXT,
                Type::TEXT,
            ],
        );
        pin_mut!(writer);
        writer.as_mut().write(&args).await.unwrap();
        writer.finish().await.unwrap();
        Ok(())
    }
}

#[derive(Clone)]
pub struct Postgres {
    session: Arc<PostgresSession>,
}

impl Postgres {
    pub async fn new() -> Self {
        let session = PostgresSession::new().await.unwrap();
        Self {
            session: Arc::new(session),
        }
    }

    pub fn spawn_transaction_infos_saver(
        &self,
        map_of_transaction: Arc<DashMap<String, BTreeMap<u64, TransactionInfo>>>,
        slot: Arc<AtomicU64>,
    ) {
        let session = self.session.clone();
        tokio::task::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let slot = slot.load(std::sync::atomic::Ordering::Relaxed);
                let mut txs_to_store = vec![];
                for tx in map_of_transaction.iter() {
                    let slot_map = tx.value();
                    let first_slot = slot_map.keys().next().cloned().unwrap_or_default();
                    if slot > first_slot + 300 {
                        txs_to_store.push(tx.key().clone());
                    }
                }

                if !txs_to_store.is_empty() {
                    debug!("saving transaction infos for {}", txs_to_store.len());
                    let data = txs_to_store
                        .iter()
                        .filter_map(|key| map_of_transaction.remove(key))
                        .map(|(_, tree)| tree.iter().map(|(_, info)| info).cloned().collect_vec())
                        .flatten()
                        .collect_vec();
                    let batches = data.chunks(8).collect_vec();
                    for batch in batches {
                        session
                            .save_banking_transaction_results(batch.to_vec())
                            .await
                            .unwrap();
                    }
                }
            }
        });
    }

    pub async fn save_block_info(&self, block: BlockInfo) -> anyhow::Result<()> {
        self.session.save_block(block).await
    }
}

pub struct PostgresTransactionInfo {
    pub signature: String,
    pub errors: String,
    pub is_executed: bool,
    pub is_confirmed: bool,
    pub first_notification_slot: i64,
    pub cu_requested: Option<i64>,
    pub prioritization_fees: Option<i64>,
    pub utc_timestamp: DateTime<Utc>,
    pub accounts_used: String,
    pub processed_slot: Option<i64>,
}

#[derive(Serialize, Clone)]
pub struct TransactionErrorData {
    error: TransactionError,
    slot: u64,
    count: usize,
}

#[derive(Serialize, Clone)]
pub struct AccountUsed {
    key: String,
    writable: bool,
}

impl From<&TransactionInfo> for PostgresTransactionInfo {
    fn from(value: &TransactionInfo) -> Self {
        let errors = value
            .errors
            .iter()
            .map(|(key, count)| TransactionErrorData {
                error: key.error.clone(),
                slot: key.slot,
                count: *count,
            })
            .collect_vec();
        let accounts_used = value
            .account_used
            .iter()
            .map(|(key, writable)| AccountUsed {
                key: key.to_string(),
                writable: *writable,
            })
            .collect_vec();
        Self {
            signature: value.signature.clone(),
            errors: serde_json::to_string(&errors).unwrap_or_default(),
            is_executed: value.is_executed,
            is_confirmed: value.is_confirmed,
            cu_requested: value.cu_requested.map(|x| x as i64),
            first_notification_slot: value.first_notification_slot as i64,
            prioritization_fees: value.prioritization_fees.map(|x| x as i64),
            utc_timestamp: value.utc_timestamp,
            accounts_used: serde_json::to_string(&accounts_used).unwrap_or_default(),
            processed_slot: value.processed_slot.map(|x| x as i64),
        }
    }
}
