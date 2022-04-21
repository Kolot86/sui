// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

#![deny(warnings)]

use anyhow::Error;
use bytes::{Bytes, BytesMut};
use futures::channel::mpsc::{channel as MpscChannel, Receiver, Sender as MpscSender};
use futures::stream::StreamExt;
use futures::SinkExt;

use rayon::prelude::*;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};
use sui_core::authority::*;
use sui_core::authority_server::AuthorityServer;
use sui_network::network::{NetworkClient, NetworkServer};
use sui_network::transport;
use sui_types::{messages::*, serialize::*};
use tokio::sync::Notify;
use tokio::time;
use tracing::{error, info};

pub fn check_transaction_response(reply_message: Result<SerializedMessage, Error>) {
    match reply_message {
        Ok(SerializedMessage::TransactionResp(res)) => {
            if let Some(e) = res.signed_effects {
                if matches!(e.effects.status, ExecutionStatus::Failure { .. }) {
                    info!("Execution Error {:?}", e.effects.status);
                }
            }
        }
        Err(err) => {
            error!("Received Error {:?}", err);
        }
        Ok(q) => error!("Received invalid response {:?}", q),
    };
}

pub async fn send_tx_chunks(
    tx_chunks: Vec<Bytes>,
    net_client: NetworkClient,
    conn: usize,
) -> (u128, Vec<Result<BytesMut, io::Error>>) {
    let time_start = Instant::now();

    let tx_resp = net_client
        .batch_send(tx_chunks, conn, 0)
        .map(|x| x.unwrap())
        .concat()
        .await;

    let elapsed = time_start.elapsed().as_micros();

    (elapsed, tx_resp)
}

/// TODO: Add support for stake
async fn send_tx_for_quorum(
    notif: Arc<Notify>,
    order_chunk: Vec<Bytes>,
    conf_chunk: Vec<Bytes>,

    result_chann_tx: &mut MpscSender<u128>,
    net_clients: Vec<NetworkClient>,
    conn: usize,
) {
    let num_validators = net_clients.len();
    // For receiving info back from the subtasks
    let (order_chann_tx, mut order_chann_rx) = MpscChannel(net_clients.len() * 2);

    // Send intent orders to 3f+1
    let order_start_notifier = Arc::new(Notify::new());
    for n in net_clients.clone() {
        // This is for sending a start signal to the subtasks
        let notif = order_start_notifier.clone();
        // This is for getting the elapsed time
        let mut ch_tx = order_chann_tx.clone();
        // Chunk to send for order_
        let chunk = order_chunk.clone();

        tokio::spawn(async move {
            send_tx_chunks_notif(notif, chunk, &mut ch_tx, n.clone(), conn).await;
            println!("Spawn for order {:?}", n);
        });
    }
    drop(order_chann_tx);

    // Wait for tick
    notif.notified().await;
    // Notify all the subtasks
    order_start_notifier.notify_waiters();
    let time_start = Instant::now();

    // Wait for 2f+1
    let mut count = 0;

    while time::timeout(Duration::from_secs(10), order_chann_rx.next())
        .await
        .unwrap_or(None)
        .is_some()
    {
        count += 1;

        if count > 2 * (num_validators - 1) / 3 {
            break;
        }
    }
    println!("order {}", count);
    // Confirmation step
    let (conf_chann_tx, mut conf_chann_rx) = MpscChannel(net_clients.len() * 2);

    // Send the confs
    let mut handles = vec![];
    for n in net_clients {
        let chunk = conf_chunk.clone();
        let mut chann_tx = conf_chann_tx.clone();
        handles.push(tokio::spawn(async move {
            let r = send_tx_chunks(chunk, n.clone(), conn).await;
            println!("Spawn for conf {:?}", n);
            match chann_tx.send(r.0).await {
                Ok(_) => (),
                Err(e) => if !e.is_disconnected() {
                    panic!("Send failed! {:?}", n)
                }
            }

            let _: Vec<_> =
                r.1.par_iter()
                    .map(|q| {
                        check_transaction_response(deserialize_message(&(q.as_ref().unwrap())[..]))
                    })
                    .collect();
        }));
    }
    drop(conf_chann_tx);

    // Reset counter
    count = 0;
    while time::timeout(Duration::from_secs(10), conf_chann_rx.next())
        .await
        .unwrap_or(None)
        .is_some()
    {
        count += 1;

        if count > 2 * (num_validators - 1) / 3 {
            break;
        }
    }
    println!("conf {}", count);

    let elapsed = time_start.elapsed().as_micros();

    // Send the total time over
    result_chann_tx.send(elapsed).await.unwrap();
}

async fn send_tx_chunks_notif(
    notif: Arc<Notify>,
    tx_chunk: Vec<Bytes>,
    result_chann_tx: &mut MpscSender<u128>,
    net_client: NetworkClient,
    conn: usize,
) {
    notif.notified().await;
    let r = send_tx_chunks(tx_chunk, net_client.clone(), conn).await;
    match result_chann_tx.send(r.0).await {
        Ok(_) => (),
        Err(e) => if !e.is_disconnected() {
            panic!("Send failed! {:?}", net_client)
        }
    }

    let _: Vec<_> =
        r.1.par_iter()
            .map(|q| check_transaction_response(deserialize_message(&(q.as_ref().unwrap())[..])))
            .collect();
}

pub struct FixedRateLoadGenerator {
    /// The time between sending transactions chunks
    /// Anything below 10ms causes degradation in resolution
    pub period_us: u64,
    /// The network client to send transactions on
    pub network_clients: Vec<NetworkClient>,

    pub tick_notifier: Arc<Notify>,

    /// Number of TCP connections to open
    pub connections: usize,

    pub transactions: Vec<Bytes>,

    pub results_chann_rx: Receiver<u128>,

    /// This is the chunk size actually assigned for each tick per task
    /// It is 2*chunk_size due to order and confirmation steps
    pub chunk_size_per_task: usize,
}

// new -> ready -> start

impl FixedRateLoadGenerator {
    pub async fn new_for_multi_validator(
        transactions: Vec<Bytes>,
        period_us: u64,
        network_clients: Vec<NetworkClient>,
        connections: usize,
    ) -> Self {
        let mut handles = vec![];
        let tick_notifier = Arc::new(Notify::new());

        let (result_chann_tx, results_chann_rx) = MpscChannel(transactions.len() * 2);

        let conn = connections;
        // Spin up a bunch of worker tasks
        // Give each task
        // Step by 2*conn due to order+confirmation, with `conn` tcp connections
        // Take up to 2*conn for each task
        let num_chunks_per_task = conn * 2;
        for tx_chunk in transactions[..].chunks(num_chunks_per_task) {
            let notif = tick_notifier.clone();
            let mut result_chann_tx = result_chann_tx.clone();
            let tx_chunk = tx_chunk.to_vec();
            let clients = network_clients.clone();

            let mut order_chunk = vec![];
            let mut conf_chunk = vec![];

            for ch in tx_chunk[..].chunks(2) {
                order_chunk.push(ch[0].clone());
                conf_chunk.push(ch[1].clone());
            }

            handles.push(tokio::spawn(async move {
                send_tx_for_quorum(
                    notif,
                    order_chunk,
                    conf_chunk,
                    &mut result_chann_tx,
                    clients,
                    conn,
                )
                .await;
            }));
        }

        drop(result_chann_tx);

        Self {
            period_us,
            network_clients,
            transactions,
            connections,
            results_chann_rx,
            tick_notifier,
            chunk_size_per_task: num_chunks_per_task,
        }
    }

    pub async fn new(
        transactions: Vec<Bytes>,
        period_us: u64,
        network_client: NetworkClient,
        connections: usize,
    ) -> Self {
        let mut handles = vec![];
        let tick_notifier = Arc::new(Notify::new());

        let (result_chann_tx, results_chann_rx) = MpscChannel(transactions.len() * 2);

        let conn = connections;
        // Spin up a bunch of worker tasks
        // Give each task
        // Step by 2*conn due to order+confirmation, with `conn` tcp connections
        // Take up to 2*conn for each task
        let num_chunks_per_task = conn * 2;
        for tx_chunk in transactions[..].chunks(num_chunks_per_task) {
            let notif = tick_notifier.clone();
            let mut result_chann_tx = result_chann_tx.clone();
            let tx_chunk = tx_chunk.to_vec();
            let client = network_client.clone();

            handles.push(tokio::spawn(async move {
                send_tx_chunks_notif(notif, tx_chunk, &mut result_chann_tx, client, conn).await;
            }));
        }

        drop(result_chann_tx);

        Self {
            period_us,
            network_clients: vec![network_client],
            transactions,
            connections,
            results_chann_rx,
            tick_notifier,
            chunk_size_per_task: num_chunks_per_task,
        }
    }

    pub async fn start(&mut self) -> Vec<u128> {
        let mut interval = time::interval(Duration::from_micros(self.period_us));
        let mut count = 0;
        loop {
            tokio::select! {
                _  = interval.tick() => {
                    self.tick_notifier.notify_one();
                    count += self.chunk_size_per_task;
                    if count >= self.transactions.len() {
                        break;
                    }
                }
            }
        }
        let mut times = Vec::new();
        while let Some(v) = time::timeout(Duration::from_secs(10), self.results_chann_rx.next())
            .await
            .unwrap_or(None)
        {
            times.push(v);
        }

        times
    }
}

pub async fn spawn_authority_server(
    network_server: NetworkServer,
    state: AuthorityState,
) -> transport::SpawnedServer<AuthorityServer> {
    let server = AuthorityServer::new(
        network_server.base_address,
        network_server.base_port,
        network_server.buffer_size,
        state,
    );
    server.spawn().await.unwrap()
}

pub fn calculate_throughput(num_items: usize, elapsed_time_us: u128) -> f64 {
    1_000_000.0 * num_items as f64 / elapsed_time_us as f64
}
