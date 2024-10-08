// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

pub mod conf;

use crate::database::{Ledger, Mempool};
use crate::mempool::conf::Params;
use crate::{database, vm, LongLivedService, Message, Network};
use async_trait::async_trait;
use conf::{
    DEFAULT_DOWNLOAD_REDUNDANCY, DEFAULT_EXPIRY_TIME, DEFAULT_IDLE_INTERVAL,
};
use node_data::events::{Event, TransactionEvent};
use node_data::get_current_timestamp;
use node_data::ledger::Transaction;
use node_data::message::{payload, AsyncQueue, Payload, Topics};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc::Sender;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

const TOPICS: &[u8] = &[Topics::Tx as u8];

#[derive(Debug, Error)]
enum TxAcceptanceError {
    #[error("this transaction exists in the mempool")]
    AlreadyExistsInMempool,
    #[error("this transaction exists in the ledger")]
    AlreadyExistsInLedger,
    #[error("this transaction's spendId exists in the mempool")]
    SpendIdExistsInMempool,
    #[error("this transaction is invalid {0}")]
    VerificationFailed(String),
    #[error("Maximum count of transactions exceeded {0}")]
    MaxTxnCountExceeded(usize),
    #[error("A generic error occurred {0}")]
    Generic(anyhow::Error),
}

impl From<anyhow::Error> for TxAcceptanceError {
    fn from(err: anyhow::Error) -> Self {
        Self::Generic(err)
    }
}

pub struct MempoolSrv {
    inbound: AsyncQueue<Message>,
    conf: Params,
    event_sender: Sender<Event>,
}

impl MempoolSrv {
    pub fn new(conf: Params, event_sender: Sender<Event>) -> Self {
        info!("MempoolSrv::new with conf {}", conf);
        Self {
            inbound: AsyncQueue::bounded(
                conf.max_queue_size,
                "mempool_inbound",
            ),
            conf,
            event_sender,
        }
    }
}

#[async_trait]
impl<N: Network, DB: database::DB, VM: vm::VMExecution>
    LongLivedService<N, DB, VM> for MempoolSrv
{
    async fn execute(
        &mut self,
        network: Arc<RwLock<N>>,
        db: Arc<RwLock<DB>>,
        vm: Arc<RwLock<VM>>,
    ) -> anyhow::Result<usize> {
        LongLivedService::<N, DB, VM>::add_routes(
            self,
            TOPICS,
            self.inbound.clone(),
            &network,
        )
        .await?;

        // Request mempool update from N alive peers
        self.request_mempool(&network).await;

        let idle_interval =
            self.conf.idle_interval.unwrap_or(DEFAULT_IDLE_INTERVAL);

        let mempool_expiry = self
            .conf
            .mempool_expiry
            .unwrap_or(DEFAULT_EXPIRY_TIME)
            .as_secs();

        // Mempool service loop
        let mut on_idle_event = tokio::time::interval(idle_interval);
        loop {
            tokio::select! {
                biased;
                _ = on_idle_event.tick() => {
                    info!(event = "mempool_idle", interval = ?idle_interval);

                    let expiration_time = get_current_timestamp()
                        .checked_sub(mempool_expiry)
                        .expect("valid duration");

                    // Remove expired transactions from the mempool
                    db.read().await.update(|db| {
                        let expired_txs = db.get_expired_txs(expiration_time)?;
                        for tx_id in expired_txs {
                            info!(event = "expired_tx", hash = hex::encode(tx_id));
                            if db.delete_tx(tx_id)? {
                                let event = TransactionEvent::Removed(tx_id);
                                if let Err(e) = self.event_sender.try_send(event.into()) {
                                    warn!("cannot notify mempool removed transaction {e}")
                                };
                            }
                        }
                        Ok(())
                    })?;

                },
                msg = self.inbound.recv() => {
                    if let Ok(msg) = msg {
                        match &msg.payload {
                            Payload::Transaction(tx) => {
                                let accept = self.accept_tx::<DB, VM>(&db, &vm, tx);
                                if let Err(e) = accept.await {
                                    error!("{}", e);
                                    continue;
                                }

                                let network = network.read().await;
                                if let Err(e) = network.broadcast(&msg).await {
                                    warn!("Unable to broadcast accepted tx: {e}")
                                };
                            }
                            _ => error!("invalid inbound message payload"),
                        }
                    }
                }
            }
        }
    }

    /// Returns service name.
    fn name(&self) -> &'static str {
        "mempool"
    }
}

impl MempoolSrv {
    async fn accept_tx<DB: database::DB, VM: vm::VMExecution>(
        &mut self,
        db: &Arc<RwLock<DB>>,
        vm: &Arc<RwLock<VM>>,
        tx: &Transaction,
    ) -> Result<(), TxAcceptanceError> {
        let tx_id = tx.id();

        // Perform basic checks on the transaction
        db.read().await.view(|view| {
            let count = view.txs_count();
            if count >= self.conf.max_mempool_txn_count {
                return Err(TxAcceptanceError::MaxTxnCountExceeded(count));
            }

            // ensure transaction does not exist in the mempool
            if view.get_tx_exists(tx_id)? {
                return Err(TxAcceptanceError::AlreadyExistsInMempool);
            }

            // ensure transaction does not exist in the blockchain
            if view.get_ledger_tx_exists(&tx_id)? {
                return Err(TxAcceptanceError::AlreadyExistsInLedger);
            }

            Ok(())
        })?;

        // VM Preverify call
        if let Err(e) = vm.read().await.preverify(tx) {
            Err(TxAcceptanceError::VerificationFailed(format!("{e:?}")))?;
        }

        let mut events = vec![];

        // Try to add the transaction to the mempool
        db.read().await.update(|db| {
            let spend_ids = tx.to_spend_ids();

            // ensure spend_ids do not exist in the mempool
            for m_tx_id in db.get_txs_by_spendable_ids(&spend_ids) {
                if let Some(m_tx) = db.get_tx(m_tx_id)? {
                    if m_tx.inner.gas_price() < tx.inner.gas_price() {
                        if db.delete_tx(m_tx_id)? {
                            events.push(TransactionEvent::Removed(m_tx_id));
                        };
                    } else {
                        return Err(
                            TxAcceptanceError::SpendIdExistsInMempool.into()
                        );
                    }
                }
            }

            events.push(TransactionEvent::Included(tx));
            // Persist transaction in mempool storage

            let now = get_current_timestamp();

            db.add_tx(tx, now)
        })?;

        tracing::info!(
            event = "transaction accepted",
            hash = hex::encode(tx_id)
        );

        for tx_event in events {
            let node_event = tx_event.into();
            if let Err(e) = self.event_sender.try_send(node_event) {
                warn!("cannot notify mempool accepted transaction {e}")
            };
        }

        Ok(())
    }

    /// Requests full mempool data from N alive peers
    ///
    /// Message flow:
    /// GetMempool -> Inv -> GetResource -> Tx
    async fn request_mempool<N: Network>(&self, network: &Arc<RwLock<N>>) {
        let max_peers = self
            .conf
            .mempool_download_redundancy
            .unwrap_or(DEFAULT_DOWNLOAD_REDUNDANCY);
        let msg = payload::GetMempool::default().into();
        if let Err(err) = network
            .read()
            .await
            .send_to_alive_peers(msg, max_peers)
            .await
        {
            error!("could not request mempool from network: {err}");
        }
    }
}
