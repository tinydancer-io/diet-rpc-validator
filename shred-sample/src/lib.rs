//! Shred sample service
//! new - setup
//! join - join the thread

use std::{
    cmp::Reverse,
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    thread::{self, Builder, JoinHandle},
    time::{Duration, Instant},
};

use crossbeam_channel::unbounded;
use log::{info, trace};
use solana_core::{
    repair_service::REPAIR_MS,
    result::{self, Error, Result},
};
use solana_gossip::{
    cluster_info::ClusterInfo,
    contact_info::ContactInfo,
    ping_pong::{self, Ping, PingCache},
};
use solana_ledger::{blockstore::Blockstore, shred::SIZE_OF_NONCE};
use solana_perf::{
    data_budget::DataBudget,
    packet::{PacketBatchRecycler, PACKET_DATA_SIZE},
    recycler::Recycler,
};
use solana_runtime::bank_forks::BankForks;
use solana_sdk::{
    clock::Slot,
    genesis_config::ClusterType,
    hash::{Hash, HASH_BYTES},
    pubkey::{Pubkey, PUBKEY_BYTES},
    signature::{Keypair, SIGNATURE_BYTES},
    signer::Signer,
};
use solana_streamer::{
    packet::PacketBatch,
    socket::SocketAddrSpace,
    streamer::{self, PacketBatchReceiver, PacketBatchSender, StreamerReceiveStats},
};
type SlotHash = (Slot, Hash);
// the number of slots to respond with when responding to `Orphan` requests
pub const MAX_ORPHAN_REPAIR_RESPONSES: usize = 11;
// Number of slots to cache their respective repair peers and sampling weights.
pub(crate) const REPAIR_PEERS_CACHE_CAPACITY: usize = 128;
// Limit cache entries ttl in order to avoid re-using outdated data.
const REPAIR_PEERS_CACHE_TTL: Duration = Duration::from_secs(10);
pub const MAX_ANCESTOR_BYTES_IN_PACKET: usize =
    PACKET_DATA_SIZE -
    SIZE_OF_NONCE -
    4 /*(response version enum discriminator)*/ -
    4 /*slot_hash length*/;
pub const MAX_ANCESTOR_RESPONSES: usize =
    MAX_ANCESTOR_BYTES_IN_PACKET / std::mem::size_of::<SlotHash>();
/// Number of bytes in the randomly generated token sent with ping messages.
pub(crate) const REPAIR_PING_TOKEN_SIZE: usize = HASH_BYTES;
pub const REPAIR_PING_CACHE_CAPACITY: usize = 65536;
pub const REPAIR_PING_CACHE_TTL: Duration = Duration::from_secs(1280);
const REPAIR_PING_CACHE_RATE_LIMIT_DELAY: Duration = Duration::from_secs(2);
pub(crate) const REPAIR_RESPONSE_SERIALIZED_PING_BYTES: usize =
    4 /*enum discriminator*/ + PUBKEY_BYTES + REPAIR_PING_TOKEN_SIZE + SIGNATURE_BYTES;
const SIGNED_REPAIR_TIME_WINDOW: Duration = Duration::from_secs(60 * 10); // 10 min
use serde_derive::{Deserialize, Serialize};
pub(crate) struct ShredSampleService {
    thread_hdls: Vec<JoinHandle<()>>,
}

#[derive(Default)]
struct ServeSamplesStats {
    total_requests: usize,
    unsigned_requests: usize,
    dropped_requests_outbound_bandwidth: usize,
    dropped_requests_load_shed: usize,
    dropped_requests_low_stake: usize,
    whitelisted_requests: usize,
    total_dropped_response_packets: usize,
    total_response_packets: usize,
    total_response_bytes_staked: usize,
    total_response_bytes_unstaked: usize,
    handle_requests_staked: usize,
    handle_requests_unstaked: usize,
    processed: usize,
    self_repair: usize,
    window_index: usize,
    highest_window_index: usize,
    orphan: usize,
    pong: usize,
    ancestor_hashes: usize,
    ping_cache_check_failed: usize,
    pings_sent: usize,
    decode_time_us: u64,
    err_time_skew: usize,
    err_malformed: usize,
    err_sig_verify: usize,
    err_unsigned: usize,
    err_id_mismatch: usize,
}

struct SampleRequestWithMeta {
    request: SampleProtocol,
    from_addr: SocketAddr,
    // stake: u64,
    // whitelisted: bool, // @TODO
}
pub struct ServeSamples {
    cluster_info: Arc<ClusterInfo>,
    bank_forks: Arc<RwLock<BankForks>>,
    // sample_whitelist: Arc<RwLock<HashSet<Pubkey>>>, //@TODO whitelist when we add the crds
}

#[derive(Serialize, Deserialize, Debug)]
pub enum SampleProtocol {
    RandomSamplesForSlot(ContactInfo, Slot, u64, Vec<u8>),
    Pong(ping_pong::Pong),
}
impl SampleProtocol {
    fn supports_signature(&self) -> bool {
        match self {
            Self::RandomSamplesForSlot(_, _, _, _) => false,
            Self::Pong(_) => true,
        }
    }
    fn sender(&self) -> &Pubkey {
        match self {
            Self::RandomSamplesForSlot(ci, _, _, _) => &ci.id,
            Self::Pong(pong) => pong.from(),
        }
    }
}
impl ServeSamples {
    fn handle_packets(
        &self,
        ping_cache: &mut PingCache,
        recycler: &PacketBatchRecycler,
        blockstore: &Blockstore,
        requests: Vec<SampleRequestWithMeta>,
        response_sender: &PacketBatchSender,
        stats: &mut ServeSamplesStats,
        data_budget: &DataBudget,
    ) {
        let identity_keypair = self.cluster_info.keypair().clone();
        let mut pending_pings = Vec::default();

        let requests_len = requests.len();
        for (
            i,
            SampleRequestWithMeta {
                request,
                from_addr,
                // stake,
                // ..
            },
        ) in requests.into_iter().enumerate()
        {
            if !matches!(&request, SampleProtocol::Pong(_)) {
                let (check, ping_pkt) =
                    Self::check_ping_cache(ping_cache, &request, &from_addr, &identity_keypair);
                if let Some(ping_pkt) = ping_pkt {
                    pending_pings.push(ping_pkt);
                }
                if !check {
                    // collect stats for ping/pong verification
                    stats.ping_cache_check_failed += 1;
                }
            }
            stats.processed += 1;
            let rsp = match Self::handle_sampling(
                recycler, &from_addr, blockstore, request, stats, ping_cache,
            ) {
                None => continue,
                Some(rsp) => rsp,
            };
            let num_response_packets = rsp.len();
            let num_response_bytes = rsp.iter().map(|p| p.meta().size).sum();
            if data_budget.take(num_response_bytes) && response_sender.send(rsp).is_ok() {
                stats.total_response_packets += num_response_packets;
                // match stake > 0 {
                //     true => stats.total_response_bytes_staked += num_response_bytes,
                //     false => stats.total_response_bytes_unstaked += num_response_bytes,
                // }
            } else {
                stats.dropped_requests_outbound_bandwidth += requests_len - i;
                stats.total_dropped_response_packets += num_response_packets;
                break;
            }
        }

        if !pending_pings.is_empty() {
            stats.pings_sent += pending_pings.len();
            let batch = PacketBatch::new(pending_pings);
            let _ignore = response_sender.send(batch);
        }
    }
    fn handle_sampling(
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        blockstore: &Blockstore,
        request: SampleProtocol,
        stats: &mut ServeSamplesStats,
        ping_cache: &mut PingCache,
    ) -> Option<PacketBatch> {
        let now = Instant::now();
        let (res, label) = {
            match &request {
                SampleProtocol::RandomSamplesForSlot(ci, slot, i, shred_indices) => (None, "Pong"),
                SampleProtocol::Pong(pong) => {
                    stats.pong += 1;
                    ping_cache.add(pong, *from_addr, Instant::now());
                    (None, "Pong")
                }
            }
        };
        // Self::report_time_spent(label, &now.elapsed(), "");
        res
    }
    fn run_listen(
        &self,
        ping_cache: &mut PingCache,
        recycler: &PacketBatchRecycler,
        blockstore: &Blockstore,
        requests_receiver: &PacketBatchReceiver,
        response_sender: &PacketBatchSender,
        stats: &mut ServeSamplesStats,
        data_budget: &DataBudget,
    ) -> Result<()> {
        //TODO cache connections
        let timeout = Duration::new(1, 0);
        let mut reqs_v = vec![requests_receiver.recv_timeout(timeout)?];
        const MAX_REQUESTS_PER_ITERATION: usize = 1024;
        let mut total_requests = reqs_v[0].len();

        let socket_addr_space = *self.cluster_info.socket_addr_space();
        let root_bank = self.bank_forks.read().unwrap().root_bank();
        let epoch_staked_nodes = root_bank.epoch_staked_nodes(root_bank.epoch());
        let identity_keypair = self.cluster_info.keypair().clone();
        let my_id = identity_keypair.pubkey();

        let max_buffered_packets = if root_bank.cluster_type() != ClusterType::MainnetBeta {
            // if self.repair_whitelist.read().unwrap().len() > 0 { //@TODO disabling until we add crds
            4 * MAX_REQUESTS_PER_ITERATION
            // } else {
            // 2 * MAX_REQUESTS_PER_ITERATION
            // }
        } else {
            MAX_REQUESTS_PER_ITERATION
        };

        let mut dropped_requests = 0;
        while let Ok(more) = requests_receiver.try_recv() {
            total_requests += more.len();
            if total_requests > max_buffered_packets {
                dropped_requests += more.len();
            } else {
                reqs_v.push(more);
            }
        }

        stats.dropped_requests_load_shed += dropped_requests;
        stats.total_requests += total_requests;

        let decode_start = Instant::now();
        let mut decoded_requests = Vec::default();
        let mut whitelisted_request_count: usize = 0;
        {
            // let whitelist = self.repair_whitelist.read().unwrap(); // @TODO
            for packet in reqs_v.iter().flatten() {
                let request: SampleProtocol = match packet.deserialize_slice(..) {
                    Ok(request) => request,
                    Err(_) => {
                        stats.err_malformed += 1;
                        continue;
                    }
                };

                let from_addr = packet.meta().socket_addr();
                if !ContactInfo::is_valid_address(&from_addr, &socket_addr_space) {
                    stats.err_malformed += 1;
                    continue;
                }

                // if request.supports_signature() { // @TODO disabling until we add crds
                //     // collect stats for signature verification
                //     Self::verify_signed_packet(&my_id, packet, &request, stats);
                // } else {
                stats.unsigned_requests += 1;
                // }

                if request.sender() == &my_id {
                    stats.self_repair += 1;
                    continue;
                }

                // let stake = epoch_staked_nodes
                //     .as_ref()
                //     .and_then(|stakes| stakes.get(request.sender()))
                //     .unwrap_or(&0);
                // if *stake == 0 {
                //     stats.handle_requests_unstaked += 1;
                // } else {
                //     stats.handle_requests_staked += 1;
                // }

                // let whitelisted = whitelist.contains(request.sender()); // @TODO disabling until we add crds
                // if whitelisted {
                //     whitelisted_request_count += 1;
                // }

                decoded_requests.push(SampleRequestWithMeta {
                    request,
                    from_addr,
                    // whitelisted, // @TODO
                });
            }
        }
        stats.decode_time_us += decode_start.elapsed().as_micros() as u64;
        stats.whitelisted_requests += whitelisted_request_count.min(MAX_REQUESTS_PER_ITERATION);

        if decoded_requests.len() > MAX_REQUESTS_PER_ITERATION {
            stats.dropped_requests_low_stake += decoded_requests.len() - MAX_REQUESTS_PER_ITERATION;
            // decoded_requests.sort_unstable_by_key(|r| Reverse((r.whitelisted, r.stake))); // @TODO
            decoded_requests.truncate(MAX_REQUESTS_PER_ITERATION);
        }

        self.handle_packets(
            ping_cache,
            recycler,
            blockstore,
            decoded_requests,
            response_sender,
            stats,
            data_budget,
        );

        Ok(())
    }
    pub fn listen(
        self,
        blockstore: Arc<Blockstore>,
        requests_receiver: PacketBatchReceiver,
        response_sender: PacketBatchSender,
        exit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        const INTERVAL_MS: u64 = 1000;
        const MAX_BYTES_PER_SECOND: usize = 12_000_000;
        const MAX_BYTES_PER_INTERVAL: usize = MAX_BYTES_PER_SECOND * INTERVAL_MS as usize / 1000;

        // rate limit delay should be greater than the repair request iteration delay
        assert!(REPAIR_PING_CACHE_RATE_LIMIT_DELAY > Duration::from_millis(REPAIR_MS));

        let mut ping_cache = PingCache::new(
            REPAIR_PING_CACHE_TTL,
            REPAIR_PING_CACHE_RATE_LIMIT_DELAY,
            REPAIR_PING_CACHE_CAPACITY,
        );

        let recycler = PacketBatchRecycler::default();
        Builder::new()
            .name("solRepairListen".to_string())
            .spawn(move || {
                let mut last_print = Instant::now();
                let mut stats = ServeSamplesStats::default();
                let data_budget = DataBudget::default();
                loop {
                    let result = self.run_listen(
                        &mut ping_cache,
                        &recycler,
                        &blockstore,
                        &requests_receiver,
                        &response_sender,
                        &mut stats,
                        &data_budget,
                    );
                    match result {
                        Err(Error::RecvTimeout(_)) | Ok(_) => {}
                        Err(err) => info!("repair listener error: {:?}", err),
                    };
                    if exit.load(Ordering::Relaxed) {
                        return;
                    }
                    // if last_print.elapsed().as_secs() > 2 {
                    //     self.report_reset_stats(&mut stats);
                    //     last_print = Instant::now();
                    // }
                    data_budget.update(INTERVAL_MS, |_bytes| MAX_BYTES_PER_INTERVAL);
                }
            })
            .unwrap()
    }
}

impl ShredSampleService {
    pub(crate) fn new(
        serve_samples: ServeSamples,
        socket: UdpSocket,
        blockstore: Arc<Blockstore>,
        socket_addr_space: SocketAddrSpace,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let (request_sender, request_receiver) = unbounded();
        let serve_samples_socket = Arc::new(socket);
        trace!(
            "ServeSampleService listening on: {:?}",
            serve_samples_socket.local_addr().unwrap()
        );
        let t_receiver = streamer::receiver(
            serve_samples_socket.clone(),
            exit.clone(),
            request_sender,
            Recycler::default(),
            Arc::new(StreamerReceiveStats::new("serve_repair_receiver")),
            1,
            false,
            None,
        );
        let (response_sender, response_receiver) = unbounded();
        let t_responder = streamer::responder(
            "Repair",
            serve_samples_socket,
            response_receiver,
            socket_addr_space,
            None,
        );
        let t_listen = serve_samples.listen(blockstore, request_receiver, response_sender, exit);

        let thread_hdls = vec![t_receiver, t_responder, t_listen];
        Self { thread_hdls }
    }
    pub(crate) fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls {
            thread_hdl.join()?;
        }
        Ok(())
    }
}

fn check_ping_cache(
    ping_cache: &mut PingCache,
    request: &SampleProtocol,
    from_addr: &SocketAddr,
    identity_keypair: &Keypair,
) -> (bool, Option<Packet>) {
    let mut rng = rand::thread_rng();
    let mut pingf = move || Ping::new_rand(&mut rng, identity_keypair).ok();
    let (check, ping) =
        ping_cache.check(Instant::now(), (*request.sender(), *from_addr), &mut pingf);
    let ping_pkt = if let Some(ping) = ping {
        match request {
            SampleProtocol::RandomSamplesForSlot(_, _, _, _) => {}
            SampleProtocol::Pong(_) => None,
        }
    } else {
        None
    };
    (check, ping_pkt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        // let result = add(2, 2);
        // assert_eq!(result, 4);
    }
}
