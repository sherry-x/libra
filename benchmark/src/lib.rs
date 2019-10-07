// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use admission_control_proto::proto::{
    admission_control::AdmissionControlClient,
    admission_control::SubmitTransactionResponse as ProtoSubmitTransactionResponse,
};
use client::{AccountData, AccountStatus};
use crypto::{ed25519::*, test_utils::KeyPair};
use generate_keypair::load_key_from_file;
use lazy_static::lazy_static;
use libra_types::{account_address::AccountAddress, account_config::association_address};
use logger::prelude::*;
use metrics::OpMetrics;
use rand::Rng;
use std::{collections::HashMap, convert::TryInto, sync::Arc, thread, time};

pub mod bin_utils;
pub mod cli_opt;
pub mod grpc_helpers;
pub mod load_generator;
pub mod submit_rate;

use grpc_helpers::{
    divide_items, get_account_states, submit_and_wait_requests, sync_account_sequence_number,
};
use load_generator::Request;

lazy_static! {
    pub static ref OP_COUNTER: OpMetrics = OpMetrics::new_and_registered("benchmark");
}

/// Benchmark library for Libra Blockchain.
///
/// Benchmarker aims to automate the process of submitting requests to admission control
/// in a configurable mechanism, and wait for accepted TXNs comitted or timeout.
/// Current main usage is measuring TXN throughput.
/// How to run a benchmarker (see RuBen in bin/ruben.rs):
/// 1. Create a benchmarker with AdmissionControlClient(s).
/// 2. Generate accounts (using load_generator module) and mint them: mint_accounts.
/// 3. Play transactions offline-generated by load_generator module:
///    submit_requests_and_wait_txns_committed, measure_txn_throughput.
/// Metrics reported include:
/// * Counters related to:
///   * TXN generation: create_txn_request.(success|failure)
///   * Submission to AC and AC response: submit_requests (used to measure submission rate),
///     submit_txns.failure.ac.{ac_status_code}, submit_txns.failure.mempool.{mempool_status_code},
///     submit_txns.failure.vm..{vm_status}, submit_txns.{grpc_error}, submit_read_requests.{error};
///   * Final status within epoch: committed_txns, timedout_txns;
/// * Gauges: request_duration_ms, running_duration_ms, request_throughput, txns_throughput.
/// * Histograms: read_requests.response_bytes.
pub struct Benchmarker {
    /// Using multiple clients can help improve the request speed.
    clients: Vec<Arc<AdmissionControlClient>>,
    /// Upper bound duration to stagger the clients before submitting requests.
    stagger_range_ms: u16,
    /// Persisted sequence numbers for generated accounts and faucet account
    /// BEFORE playing new round of requests.
    prev_sequence_numbers: HashMap<AccountAddress, u64>,
    /// Submit requests with specified rate. Minting opearation always floods requests.
    submit_rate: u64,
}

/// Summary of the results of playing TXNs with Benchmarker.
#[derive(Debug)]
pub struct BenchSummary {
    /// Number of TXNs submitted to AC.
    num_submitted: usize,
    /// Number of TXNs accepted by AC.
    num_accepted: usize,
    /// Number of TXNs committed in Libra.
    num_committed: usize,
    /// Duration to submit TXNs.
    submit_duration_ms: u128,
    /// Duration to wait TXNs committed.
    wait_duration_ms: u128,
}

impl BenchSummary {
    /// Calcuate average number of accepted TXNs per second.
    pub fn req_throughput(&self) -> f64 {
        assert!(self.submit_duration_ms > 0);
        let throughput = self.num_accepted as f64 * 1000f64 / self.submit_duration_ms as f64;
        debug!(
            "req_throughput est = {} txns / {} ms = {:.2} rps.",
            self.num_accepted, self.submit_duration_ms, throughput,
        );
        throughput
    }

    /// Calcuate average number of committed TXNs per second.
    pub fn txn_throughput(&self) -> f64 {
        assert!(self.submit_duration_ms > 0 || self.wait_duration_ms > 0);
        let running_duration_ms = self.running_duration_ms();
        let throughput = self.num_committed as f64 * 1000f64 / running_duration_ms;
        debug!(
            "txn_throughput est = {} txns / {} ms = {:.2} rps.",
            self.num_committed, running_duration_ms, throughput,
        );
        throughput
    }

    pub fn running_duration_ms(&self) -> f64 {
        (self.submit_duration_ms + self.wait_duration_ms) as f64
    }

    pub fn has_rejected_txns(&self) -> bool {
        self.num_submitted - self.num_accepted > 0
    }

    pub fn has_uncommitted_txns(&self) -> bool {
        self.num_accepted - self.num_committed > 0
    }
}

impl Benchmarker {
    /// Construct Benchmarker with a vector of AC clients, stagger time range, and submission rate.
    pub fn new(
        clients: Vec<AdmissionControlClient>,
        stagger_range_ms: u16,
        submit_rate: u64,
    ) -> Self {
        if clients.is_empty() {
            panic!("failed to create benchmarker without any AdmissionControlClient");
        }
        let arc_clients = clients.into_iter().map(Arc::new).collect();
        let prev_sequence_numbers = HashMap::new();
        Benchmarker {
            clients: arc_clients,
            stagger_range_ms,
            prev_sequence_numbers,
            submit_rate,
        }
    }

    /// -------------------------------------------------------------------- ///
    ///  Benchmark setup: Load faucet account and minting APIs and helpers.  ///
    /// -------------------------------------------------------------------- ///

    /// Load keypair from given faucet_account_path,
    /// then try to sync with a validator to get up-to-date faucet account's sequence number.
    /// Why restore faucet account: Benchmarker as a client can be stopped/restarted repeatedly
    /// while the libra swarm as a server keeping running.
    pub fn load_faucet_account(&mut self, faucet_account_path: &str) -> AccountData {
        let faucet_account_keypair: KeyPair<Ed25519PrivateKey, Ed25519PublicKey> =
            load_key_from_file(faucet_account_path).expect("invalid faucet keypair file");
        let address = association_address();
        // Request and wait for account's (sequence_number, account_status) from a validator.
        // Assume Benchmarker is the ONLY active client in the libra network.
        let client = self
            .clients
            .get(0)
            .expect("no available AdmissionControlClient");
        let states = get_account_states(client, &[address]);
        let (sequence_number, status) = states
            .get(&address)
            .expect("failed to get faucet account from validator");
        assert_eq!(status, &AccountStatus::Persisted);
        self.prev_sequence_numbers.insert(address, *sequence_number);
        AccountData {
            address,
            key_pair: Some(faucet_account_keypair),
            sequence_number: *sequence_number,
            status: status.clone(),
        }
    }

    /// Initialize the sequence numbers for testing accounts.
    pub fn register_accounts(&mut self, accounts: &[AccountData]) {
        for account in accounts.iter() {
            self.prev_sequence_numbers
                .insert(account.address, account.sequence_number);
        }
    }

    /// Minting given accounts using self's AC client(s).
    /// Mint TXNs must be 100% successful in order to continue benchmark.
    /// Therefore mint_accounts() will panic when any mint TXN is not accepted or fails.
    /// Known issue: Minting opereations from two different Benchmarker instances
    /// will fail because they are sharing the same faucet account.
    pub fn mint_accounts(&mut self, mint_requests: &[Request], faucet_account: &mut AccountData) {
        // Disable client staggering for mint operations.
        let stagger_range_ms = self.stagger_range_ms;
        self.stagger_range_ms = 1;
        let result = self.submit_requests_and_wait_txns_committed(
            mint_requests,
            std::slice::from_mut(faucet_account),
            Some(std::u64::MAX), /* Flood minting TXNs. */
        );
        self.stagger_range_ms = stagger_range_ms;
        // We stop immediately if any minting fails.
        if result.has_rejected_txns() || result.has_uncommitted_txns() {
            panic!(
                "{} of {} mint transaction(s) accepted, and {} failed",
                result.num_accepted,
                mint_requests.len(),
                result.num_accepted - result.num_committed,
            )
        }
    }

    /// ----------------------------------------------------------------- ///
    ///  Transaction submission and waiting for commit APIs and helpers.  ///
    /// ----------------------------------------------------------------- ///

    /// Put client to sleep for a random duration before submitting requests.
    /// Return how long the client is scheduled to be delayed.
    fn stagger_client(stagger_range_ms: u16) -> u16 {
        let mut rng = rand::thread_rng();
        // Double check the upper bound value to be no less than 1.
        let duration = rng.gen_range(0, std::cmp::max(1, stagger_range_ms));
        thread::sleep(time::Duration::from_millis(u64::from(duration)));
        duration
    }

    /// Send both TXNs and read requests to AC async, wait for TXNs' responses from AC.
    /// Read requests are handled in a separate thread.
    /// Return #accepted TXNs and submission duration.
    pub fn submit_requests(&mut self, requests: &[Request], submit_rate: u64) -> (usize, u128) {
        let req_chunks = divide_items(requests, self.clients.len());
        let now = time::Instant::now();
        // Zip req_chunks with clients: when first iter returns none,
        // zip will short-circuit and next will not be called on the second iter.
        let children: Vec<thread::JoinHandle<_>> = req_chunks
            .zip(self.clients.iter().cycle())
            .map(|(chunk, client)| {
                let local_chunk = Vec::from(chunk);
                let local_client = Arc::clone(client);
                let stagger_range_ms = self.stagger_range_ms;
                // Spawn threads with corresponding client.
                thread::spawn(
                    // Dispatch requests to client and submit, return the list of responses
                    // that are accepted by AC, and how long the client is delayed.
                    move || -> (Vec<ProtoSubmitTransactionResponse>, u16) {
                        let delay_duration_ms = Self::stagger_client(stagger_range_ms);
                        debug!(
                            "Dispatch {} requests to client after staggered {} ms.",
                            local_chunk.len(),
                            delay_duration_ms,
                        );
                        (
                            submit_and_wait_requests(&local_client, local_chunk, submit_rate),
                            delay_duration_ms,
                        )
                    },
                )
            })
            .collect();
        // Wait for threads and gather reponses.
        // TODO: Group response by error type and report staticstics.
        let mut txn_resps: Vec<ProtoSubmitTransactionResponse> = vec![];
        let mut delay_duration_ms = self.stagger_range_ms;
        for child in children {
            let resp_tuple = child.join().expect("failed to join a request thread");
            txn_resps.extend(resp_tuple.0.into_iter());
            // Start counting time as soon as the first client starts to submit requests.
            delay_duration_ms = std::cmp::min(delay_duration_ms, resp_tuple.1);
        }
        let mut request_duration_ms = now.elapsed().as_millis();
        // Calling stagger_client() should ensure delay duration strictly < self.stagger_range_ms.
        if delay_duration_ms < self.stagger_range_ms {
            request_duration_ms -= u128::from(delay_duration_ms);
        }
        info!(
            "Submitted and accepted {} TXNs within {} ms.",
            txn_resps.len(),
            request_duration_ms,
        );
        (txn_resps.len(), request_duration_ms)
    }

    /// Wait for accepted TXNs to commit or time out: for any account, if its sequence number
    /// (bumpped during TXN generation) equals the one synchronized from validator,
    /// denoted as sync sequence number, then all its TXNs are committed.
    /// Return senders' most up-to-date sync sequence numbers and how long we have waited.
    pub fn wait_txns_committed(
        &self,
        senders: &[AccountData],
    ) -> (HashMap<AccountAddress, u64>, u128) {
        let account_chunks = divide_items(senders, self.clients.len());
        let now = time::Instant::now();
        let children: Vec<thread::JoinHandle<HashMap<_, _>>> = account_chunks
            .zip(self.clients.iter().cycle())
            .map(|(chunk, client)| {
                let local_chunk: Vec<(AccountAddress, u64)> = chunk
                    .iter()
                    .map(|sender| (sender.address, sender.sequence_number))
                    .collect();
                let local_client = Arc::clone(client);
                debug!(
                    "Dispatch a chunk of {} accounts to client.",
                    local_chunk.len()
                );
                thread::spawn(move || -> HashMap<AccountAddress, u64> {
                    sync_account_sequence_number(&local_client, &local_chunk)
                })
            })
            .collect();
        let mut sequence_numbers: HashMap<AccountAddress, u64> = HashMap::new();
        for child in children {
            let sequence_number_chunk = child.join().expect("failed to join a wait thread");
            sequence_numbers.extend(sequence_number_chunk);
        }
        let wait_duration_ms = now.elapsed().as_millis();
        info!("Waited for TXNs for {} ms", wait_duration_ms);
        (sequence_numbers, wait_duration_ms)
    }

    /// -------------------------------------------------- ///
    ///  Transaction playing, throughput measureing APIs.  ///
    /// -------------------------------------------------- ///

    /// With the previous stored sequence number (e.g. self.prev_sequence_numbers)
    /// and the synchronized sequence number from validator, calculate how many TXNs are committed.
    /// Update both senders sequence numbers and self.prev_sequence_numbers to the just-queried
    /// synchrnized sequence numbers. Return (#committed, #uncommitted) TXNs.
    /// Reason to backtrace sender's sequence number:
    /// If some of sender's TXNs are not committed because they are rejected by AC,
    /// we should use the synchronized sequence number in future TXN generation.
    /// On the other hand, if sender's TXNs are accepted but just waiting to be committed,
    /// part of the newly generated TXNs will be rejected by AC due to old sequence number,
    /// but eventually local account's sequence number will be new enough to get accepted.
    fn check_txn_results(
        &mut self,
        senders: &mut [AccountData],
        sync_sequence_numbers: &HashMap<AccountAddress, u64>,
    ) -> (usize, usize) {
        let mut committed_txns = 0;
        let mut uncommitted_txns = 0;
        // Invariant for any account X in Benchmarker:
        // 1) X's current persisted sequence number (X.sequence_number) >=
        //    X's synchronized sequence number (sync_sequence_number[X])
        // 2) X's current persisted sequence number (X.sequence_number) >=
        //    X's previous persisted sequence number (self.prev_sequence_numbers[X])
        for sender in senders.iter_mut() {
            let prev_sequence_number = self
                .prev_sequence_numbers
                .get_mut(&sender.address)
                .expect("Sender doesn't exist in Benchmark environment");
            let sync_sequence_number = sync_sequence_numbers
                .get(&sender.address)
                .expect("Sender doesn't exist in validators");
            assert!(sender.sequence_number >= *sync_sequence_number);
            assert!(*sync_sequence_number >= *prev_sequence_number);
            if sender.sequence_number > *sync_sequence_number {
                error!(
                    "Account {:?} has {} uncommitted TXNs",
                    sender.address,
                    sender.sequence_number - *sync_sequence_number
                );
            }
            committed_txns += *sync_sequence_number - *prev_sequence_number;
            uncommitted_txns += sender.sequence_number - *sync_sequence_number;
            *prev_sequence_number = *sync_sequence_number;
            sender.sequence_number = *sync_sequence_number;
        }
        info!(
            "#committed TXNs = {}, #uncommitted TXNs = {}",
            committed_txns, uncommitted_txns
        );
        let committed_txns_usize = committed_txns
            .try_into()
            .expect("Unable to convert u64 to usize");
        let uncommitted_txns_usize = uncommitted_txns
            .try_into()
            .expect("Unable to convert u64 to usize");
        OP_COUNTER.inc_by("committed_txns", committed_txns_usize);
        OP_COUNTER.inc_by("timedout_txns", uncommitted_txns_usize);
        (committed_txns_usize, uncommitted_txns_usize)
    }

    /// Implement the general way to submit requests to Libra and then
    /// wait for all accepted TXNs to become committed.
    /// Return (#accepted TXNs, #committed TXNs, submit duration, wait duration).
    pub fn submit_requests_and_wait_txns_committed(
        &mut self,
        requests: &[Request],
        senders: &mut [AccountData],
        submit_rate: Option<u64>,
    ) -> BenchSummary {
        let rate = submit_rate.unwrap_or(self.submit_rate);
        let (num_accepted, submit_duration_ms) = self.submit_requests(requests, rate);
        let (sync_sequence_numbers, wait_duration_ms) = self.wait_txns_committed(senders);
        let (num_committed, _) = self.check_txn_results(senders, &sync_sequence_numbers);
        BenchSummary {
            num_submitted: requests.len(),
            num_accepted,
            num_committed,
            submit_duration_ms,
            wait_duration_ms,
        }
    }

    /// Similar to submit_requests_and_wait_txns_committed but with timing.
    /// How given TXNs are played and how time durations (submission, commit and running)
    /// are defined are illustrated as follows:
    ///                t_submit                AC responds all requests
    /// |==============================================>|
    ///                t_commit (unable to measure)     Storage stores all committed TXNs
    ///    |========================================================>|
    ///                t_run                            1 epoch of measuring finishes.
    /// |===========================================================>|
    /// Estimated TXN throughput from user perspective = #TXN / t_run.
    /// Estimated request throughput = #TXN / t_submit.
    /// Estimated TXN throughput internal to libra = #TXN / t_commit, not measured by this API.
    /// Return request througnhput and TXN throughput.
    pub fn measure_txn_throughput(
        &mut self,
        requests: &[Request],
        senders: &mut [AccountData],
        submit_rate: Option<u64>,
    ) -> BenchSummary {
        let result = self.submit_requests_and_wait_txns_committed(requests, senders, submit_rate);
        OP_COUNTER.set("submit_duration_ms", result.submit_duration_ms as usize);
        OP_COUNTER.set("wait_duration_ms", result.wait_duration_ms as usize);
        OP_COUNTER.set("running_duration_ms", result.running_duration_ms() as usize);
        OP_COUNTER.set("request_throughput", result.req_throughput() as usize);
        OP_COUNTER.set("txn_throughput", result.txn_throughput() as usize);
        result
    }
}
