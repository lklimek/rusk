// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use super::acceptor::{Acceptor, RevertTarget};
use super::stall_chain_fsm::{self, StalledChainFSM};
use crate::chain::fallback;
use crate::database;
use crate::{vm, Network};

use crate::database::{Candidate, Ledger};
use metrics::counter;
use node_data::ledger::{to_str, Attestation, Block};
use node_data::message::payload::{
    GetBlocks, GetResource, Inv, RatificationResult, Vote,
};

use node_data::message::{payload, Message, Metadata};
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::ops::Deref;
use std::time::Duration;
use std::{sync::Arc, time::SystemTime};
use tokio::sync::RwLock;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

const MAX_BLOCKS_TO_REQUEST: i16 = 50;
const EXPIRY_TIMEOUT_MILLIS: i16 = 5000;
const DEFAULT_ATT_CACHE_EXPIRY: Duration = Duration::from_secs(60);

/// Maximum number of hops between the requester and the node that contains the
/// requested resource
const DEFAULT_HOPS_LIMIT: u16 = 16;

type SharedHashSet = Arc<RwLock<HashSet<[u8; 32]>>>;

#[derive(Clone)]
struct PresyncInfo {
    peer_addr: SocketAddr,
    start_height: u64,
    target_blk: Block,
    expiry: Instant,
}

impl PresyncInfo {
    const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
    fn new(
        peer_addr: SocketAddr,
        target_blk: Block,
        start_height: u64,
    ) -> Self {
        Self {
            peer_addr,
            target_blk,
            expiry: Instant::now().checked_add(Self::DEFAULT_TIMEOUT).unwrap(),
            start_height,
        }
    }

    fn start_height(&self) -> u64 {
        self.start_height
    }
}

enum State<N: Network, DB: database::DB, VM: vm::VMExecution> {
    InSync(InSyncImpl<DB, VM, N>),
    OutOfSync(OutOfSyncImpl<DB, VM, N>),
}

/// Implements a finite-state-machine to manage InSync and OutOfSync
pub(crate) struct SimpleFSM<N: Network, DB: database::DB, VM: vm::VMExecution> {
    curr: State<N, DB, VM>,
    acc: Arc<RwLock<Acceptor<N, DB, VM>>>,
    network: Arc<RwLock<N>>,

    blacklisted_blocks: SharedHashSet,

    /// Attestations cached from received Quorum messages
    attestations_cache: HashMap<[u8; 32], (Attestation, Instant)>,

    /// State machine to detect a stalled state of the chain
    stalled_sm: StalledChainFSM<DB, N, VM>,
}

impl<N: Network, DB: database::DB, VM: vm::VMExecution> SimpleFSM<N, DB, VM> {
    pub async fn new(
        acc: Arc<RwLock<Acceptor<N, DB, VM>>>,
        network: Arc<RwLock<N>>,
    ) -> Self {
        let blacklisted_blocks = Arc::new(RwLock::new(HashSet::new()));
        let stalled_sm = StalledChainFSM::new_with_acc(acc.clone()).await;
        let curr = State::InSync(InSyncImpl::<DB, VM, N>::new(
            acc.clone(),
            network.clone(),
            blacklisted_blocks.clone(),
        ));

        Self {
            curr,
            acc,
            network: network.clone(),
            blacklisted_blocks,
            attestations_cache: Default::default(),
            stalled_sm,
        }
    }

    pub async fn on_failed_consensus(&mut self) {
        self.acc.write().await.restart_consensus().await;
    }

    /// Handles an event of a block occurrence.
    ///
    /// A block event could originate from either local consensus execution, a
    /// wire Block message (topics::Block), or a wire Quorum message
    /// (topics::Quorum).
    ///
    /// If the block is accepted, it returns the block itself
    pub async fn on_block_event(
        &mut self,
        blk: Block,
        metadata: Option<Metadata>,
    ) -> anyhow::Result<Option<Block>> {
        let block_hash = &blk.header().hash;

        // Filter out blocks that have already been marked as
        // blacklisted upon successful fallback execution.
        if self.blacklisted_blocks.read().await.contains(block_hash) {
            info!(
                event = "block discarded",
                reason = "blacklisted",
                hash = to_str(&blk.header().hash),
                height = blk.header().height,
                iter = blk.header().iteration,
            );
            // block discarded, should we clean up attestation cache (if any)?
            return Ok(None);
        }

        let blk = self.attach_att_if_needed(blk);
        if let Some(blk) = blk.as_ref() {
            let fsm_res = match &mut self.curr {
                State::InSync(ref mut curr) => {
                    if let Some((b, peer_addr)) =
                        curr.on_block_event(blk, metadata).await?
                    {
                        // Transition from InSync to OutOfSync state
                        curr.on_exiting().await;

                        // Enter new state
                        let mut next = OutOfSyncImpl::new(
                            self.acc.clone(),
                            self.network.clone(),
                        );
                        next.on_entering(b, peer_addr).await;
                        self.curr = State::OutOfSync(next);
                    }
                    anyhow::Ok(())
                }
                State::OutOfSync(ref mut curr) => {
                    if curr.on_block_event(blk, metadata).await? {
                        // Transition from OutOfSync to InSync state
                        curr.on_exiting().await;

                        // Enter new state
                        let mut next = InSyncImpl::new(
                            self.acc.clone(),
                            self.network.clone(),
                            self.blacklisted_blocks.clone(),
                        );
                        next.on_entering(blk).await.map_err(|e| {
                            error!("Unable to enter in_sync state: {e}");
                            e
                        })?;
                        self.curr = State::InSync(next);
                    }
                    anyhow::Ok(())
                }
            };

            // Try to detect a stalled chain
            // Generally speaking, if a node is receiving future blocks from the
            // network but it cannot accept a new block for long time, then
            // it might be a sign of a getting stalled on non-main branch.

            let res = self.stalled_sm.on_block_received(blk).await.clone();
            match res {
                stall_chain_fsm::State::StalledOnFork(
                    local_hash_at_fork,
                    remote_blk,
                ) => {
                    info!(
                        event = "stalled on fork",
                        local_hash = to_str(&local_hash_at_fork),
                        remote_hash = to_str(&remote_blk.header().hash),
                        remote_height = remote_blk.header().height,
                    );
                    let mut acc = self.acc.write().await;

                    let prev_local_state_root =
                        acc.db.read().await.view(|t| {
                            let local_blk = t
                                .fetch_block_header(&local_hash_at_fork)?
                                .expect("local hash should exist");

                            let prev_blk = t
                                .fetch_block_header(&local_blk.prev_block_hash)?
                                .expect("prev block hash should exist");

                            anyhow::Ok(prev_blk.state_hash)
                        })?;

                    match acc
                        .try_revert(RevertTarget::Commit(prev_local_state_root))
                        .await
                    {
                        Ok(_) => {
                            counter!("dusk_revert_count").increment(1);
                            info!(event = "reverted to last finalized");

                            info!(
                                event = "recovery block",
                                height = remote_blk.header().height,
                                hash = to_str(&remote_blk.header().hash),
                            );

                            acc.try_accept_block(&remote_blk, true).await?;

                            // Black list the block hash to avoid accepting it
                            // again due to fallback execution
                            self.blacklisted_blocks
                                .write()
                                .await
                                .insert(local_hash_at_fork);

                            // Try to reset the stalled chain FSM to `running`
                            // state
                            if let Err(err) =
                                self.stalled_sm.reset(remote_blk.header())
                            {
                                info!(
                                    event = "revert failed",
                                    err = format!("{:?}", err)
                                );
                            }
                        }
                        Err(e) => {
                            error!(
                                event = "revert failed",
                                err = format!("{:?}", e)
                            );
                            return Ok(None);
                        }
                    }
                }
                stall_chain_fsm::State::Stalled(_) => {
                    self.blacklisted_blocks.write().await.clear();
                }
                _ => {}
            }

            // Ensure that an error in FSM does not affect the stalled_sm
            fsm_res?;
        }

        Ok(blk)
    }

    async fn flood_request_block(&mut self, hash: [u8; 32], att: Attestation) {
        if self.attestations_cache.contains_key(&hash) {
            return;
        }

        // Save attestation in case only candidate block is received
        let expiry = Instant::now()
            .checked_add(DEFAULT_ATT_CACHE_EXPIRY)
            .unwrap();
        self.attestations_cache.insert(hash, (att, expiry));

        let mut inv = Inv::new(1);
        inv.add_candidate_from_hash(hash);

        flood_request(&self.network, &inv).await;
    }

    /// Handles a Quorum message that is received from either the network
    /// or internal consensus execution.
    ///
    /// The winner block is built from the quorum attestation and candidate
    /// block. If the candidate is not found in local storage then the
    /// block/candidate is requested from the network.
    ///
    /// It returns the corresponding winner block if it gets accepted
    pub(crate) async fn on_quorum_msg(
        &mut self,
        quorum: &payload::Quorum,
        msg: &Message,
    ) -> anyhow::Result<Option<Block>> {
        // Clean up attestation cache
        let now = Instant::now();
        self.attestations_cache
            .retain(|_, (_, expiry)| *expiry > now);

        // FIXME: We should return the whole outcome for this quorum
        // Basically we need to inform the upper layer if the received quorum is
        // valid (even if it's a FailedQuorum)
        // This will be usefull in order to:
        // - Reset the idle timer if the current iteration reached a quorum
        // - Move to next iteration if the quorum is a Failed one
        // - Remove the FIXME in fsm::on_block_event
        let res = match quorum.att.result {
            RatificationResult::Success(Vote::Valid(hash)) => {
                let local_header = self.acc.read().await.tip_header().await;
                let db = self.acc.read().await.db.clone();
                let remote_height = msg.header.round;

                // Quorum from future
                if remote_height > local_header.height + 1 {
                    debug!(
                        event = "Quorum from future",
                        hash = to_str(&hash),
                        height = remote_height,
                    );

                    self.flood_request_block(hash, quorum.att).await;

                    Ok(None)
                } else {
                    // If the quorum msg belongs to the next block,
                    // if the quorum msg belongs to a block of current round
                    // with different hash:
                    // Then try to fetch the corresponding candidate and
                    // redirect to on_block_event
                    if (remote_height == local_header.height + 1)
                        || (remote_height == local_header.height
                            && local_header.hash != hash)
                    {
                        let res = db
                            .read()
                            .await
                            .view(|t| t.fetch_candidate_block(&hash));

                        match res {
                            Ok(b) => Ok(b),
                            Err(err) => {
                                error!(
                                    event = "Candidate not found",
                                    hash = to_str(&hash),
                                    height = remote_height,
                                    err = ?err,
                                );

                                // Candidate block is not found from local
                                // storage.  Cache the attestation and request
                                // candidate block only.
                                self.flood_request_block(hash, quorum.att)
                                    .await;
                                Err(err)
                            }
                        }
                    } else {
                        Ok(None)
                    }
                }
            }
            _ => Ok(None),
        }?;

        if let Some(mut block) = res {
            info!(
                event = "block received",
                src = "quorum_msg",
                blk_height = block.header().height,
                blk_hash = to_str(&block.header().hash),
            );

            block.set_attestation(quorum.att);
            if let Some(block) =
                self.on_block_event(block, msg.metadata.clone()).await?
            {
                return Ok(Some(block));
            }
        }

        Ok(None)
    }

    pub(crate) async fn on_heartbeat_event(&mut self) -> anyhow::Result<()> {
        self.stalled_sm.on_heartbeat_event().await;

        match &mut self.curr {
            State::InSync(ref mut curr) => {
                if curr.on_heartbeat().await? {
                    // Transition from InSync to OutOfSync state
                    curr.on_exiting().await;

                    // Enter new state
                    let next = OutOfSyncImpl::new(
                        self.acc.clone(),
                        self.network.clone(),
                    );
                    self.curr = State::OutOfSync(next);
                }
            }
            State::OutOfSync(ref mut curr) => {
                if curr.on_heartbeat().await? {
                    // Transition from OutOfSync to InSync state
                    curr.on_exiting().await;

                    // Enter new state
                    let next = InSyncImpl::new(
                        self.acc.clone(),
                        self.network.clone(),
                        self.blacklisted_blocks.clone(),
                    );
                    self.curr = State::InSync(next);
                }
            }
        };

        Ok(())
    }

    /// Try to attach the attestation to a block that misses it
    ///
    /// Return None if it's not able to attach the attestation
    fn attach_att_if_needed(&mut self, mut blk: Block) -> Option<Block> {
        let block_hash = blk.header().hash;

        let block_with_att = if blk.header().att == Attestation::default() {
            // The default att means the block was retrieved from Candidate
            // CF thus missing the attestation. If so, we try to set the valid
            // attestation from the cache attestations.
            if let Some((att, _)) =
                self.attestations_cache.get(&blk.header().hash)
            {
                blk.set_attestation(*att);
                Some(blk)
            } else {
                error!("att not found for {}", hex::encode(blk.header().hash));
                None
            }
        } else {
            Some(blk)
        };

        // Clean up attestation cache
        let now = Instant::now();
        self.attestations_cache
            .retain(|_, (_, expiry)| *expiry > now);
        self.attestations_cache.remove(&block_hash);

        block_with_att
    }
}

struct InSyncImpl<DB: database::DB, VM: vm::VMExecution, N: Network> {
    acc: Arc<RwLock<Acceptor<N, DB, VM>>>,
    network: Arc<RwLock<N>>,

    blacklisted_blocks: SharedHashSet,
    presync: Option<PresyncInfo>,
}

impl<DB: database::DB, VM: vm::VMExecution, N: Network> InSyncImpl<DB, VM, N> {
    fn new(
        acc: Arc<RwLock<Acceptor<N, DB, VM>>>,
        network: Arc<RwLock<N>>,
        blacklisted_blocks: SharedHashSet,
    ) -> Self {
        Self {
            acc,
            network,
            blacklisted_blocks,
            presync: None,
        }
    }

    /// performed when entering the state
    async fn on_entering(&mut self, blk: &Block) -> anyhow::Result<()> {
        let mut acc = self.acc.write().await;
        let curr_h = acc.get_curr_height().await;

        if blk.header().height == curr_h + 1 {
            acc.try_accept_block(blk, true).await?;
        }

        info!(event = "entering in-sync", height = curr_h);

        Ok(())
    }

    /// performed when exiting the state
    async fn on_exiting(&mut self) {}

    /// Return Some if there is the need to switch to OutOfSync mode.
    /// This way the sync-up procedure to download all missing blocks from the
    /// main chain will be triggered
    async fn on_block_event(
        &mut self,
        remote_blk: &Block,
        metadata: Option<Metadata>,
    ) -> anyhow::Result<Option<(Block, SocketAddr)>> {
        let mut acc = self.acc.write().await;
        let local_header = acc.tip_header().await;
        let remote_height = remote_blk.header().height;

        if remote_height < local_header.height {
            // Ensure that the block does not exist in the local state
            let exists = acc
                .db
                .read()
                .await
                .view(|t| t.get_block_exists(&remote_blk.header().hash))?;

            if exists {
                // Already exists in local state
                return Ok(None);
            }

            // Ensure that the block height is higher than the last finalized
            if remote_height
                <= acc.get_latest_final_block().await?.header().height
            {
                return Ok(None);
            }

            // If our local chain has a block L_B with ConsensusState not Final,
            // and we receive a block R_B such that:
            //
            // R_B.PrevBlock == L_B.PrevBlock
            // R_B.Iteration < L_B.Iteration
            //
            // Then we fallback to N_B.PrevBlock and accept N_B
            let result = acc.db.read().await.view(|t| {
                if let Some(prev_header) =
                    t.fetch_block_header(&remote_blk.header().prev_block_hash)?
                {
                    let local_height = prev_header.height + 1;
                    if let Some(l_b) = t.fetch_block_by_height(local_height)? {
                        if remote_blk.header().iteration
                            < l_b.header().iteration
                        {
                            return Ok(Some((
                                l_b.header().clone(),
                                prev_header.state_hash,
                            )));
                        }
                    }
                }

                anyhow::Ok(None)
            })?;

            if let Some((local_header, state_hash)) = result {
                match fallback::WithContext::new(acc.deref())
                    .try_revert(
                        &local_header,
                        remote_blk.header(),
                        RevertTarget::Commit(state_hash),
                    )
                    .await
                {
                    Ok(_) => {
                        counter!("dusk_fallback_count").increment(1);
                        if remote_height == acc.get_curr_height().await + 1 {
                            acc.try_accept_block(remote_blk, true).await?;
                            return Ok(None);
                        }
                    }
                    Err(e) => {
                        error!(
                            event = "fallback failed",
                            height = local_header.height,
                            remote_height,
                            err = format!("{:?}", e)
                        );
                        return Ok(None);
                    }
                }
            }

            return Ok(None);
        }

        if remote_height == local_header.height {
            if remote_blk.header().hash == local_header.hash {
                // Duplicated block.
                // Node has already accepted it.
                return Ok(None);
            }

            info!(
                event = "entering fallback",
                height = local_header.height,
                iter = local_header.iteration,
                new_iter = remote_blk.header().iteration,
            );

            let state_hash = acc
                .db
                .read()
                .await
                .view(|t| {
                    let res = t
                        .fetch_block_header(
                            &remote_blk.header().prev_block_hash,
                        )?
                        .map(|prev| prev.state_hash);

                    anyhow::Ok(res)
                })?
                .ok_or_else(|| {
                    anyhow::anyhow!("could not retrieve state_hash")
                })?;

            match fallback::WithContext::new(acc.deref())
                .try_revert(
                    &local_header,
                    remote_blk.header(),
                    RevertTarget::Commit(state_hash),
                )
                .await
            {
                Err(e) => {
                    // Fallback execution has failed. The block is ignored and
                    // Node remains in InSync state.
                    error!(event = "fallback failed", err = format!("{:?}", e));
                    return Ok(None);
                }
                Ok(_) => {
                    // Fallback has completed successfully. Node has managed to
                    // fallback to the most recent finalized block and state.

                    // Blacklist the old-block hash so that if it's again
                    // sent then this node does not try to accept it.
                    self.blacklisted_blocks
                        .write()
                        .await
                        .insert(local_header.hash);

                    if remote_height == acc.get_curr_height().await + 1 {
                        // If we have fallback-ed to previous block only, then
                        // accepting the new block would be enough to continue
                        // in in_Sync mode instead of switching to Out-Of-Sync
                        // mode.

                        acc.try_accept_block(remote_blk, true).await?;
                        return Ok(None);
                    }

                    // By switching to OutOfSync mode, we trigger the
                    // sync-up procedure to download all missing blocks from the
                    // main chain.
                    if let Some(metadata) = &metadata {
                        let res = (remote_blk.clone(), metadata.src_addr);
                        return Ok(Some(res));
                    } else {
                        return Ok(None);
                    };
                }
            }
        }

        // Try accepting consecutive block
        if remote_height == local_header.height + 1 {
            let finalized = acc.try_accept_block(remote_blk, true).await?;

            // On first final block accepted while we're inSync, clear
            // blacklisted blocks
            if finalized {
                self.blacklisted_blocks.write().await.clear();
            }

            // If the accepted block is the one requested to presync peer,
            // switch to OutOfSync/Syncing mode
            if let Some(metadata) = &metadata {
                if let Some(presync) = &mut self.presync {
                    if metadata.src_addr == presync.peer_addr
                        && remote_height == presync.start_height() + 1
                    {
                        let res =
                            (presync.target_blk.clone(), presync.peer_addr);
                        self.presync = None;
                        return Ok(Some(res));
                    }
                }
            }

            return Ok(None);
        }

        // Block with height higher than (tip + 1) is received
        // Before switching to outOfSync mode and download missing blocks,
        // ensure that the Peer does know next valid block
        if let Some(metadata) = &metadata {
            if self.presync.is_none() {
                self.presync = Some(PresyncInfo::new(
                    metadata.src_addr,
                    remote_blk.clone(),
                    local_header.height,
                ));
            }

            Self::request_block_by_height(
                &self.network,
                local_header.height + 1,
                metadata.src_addr,
            )
            .await;
        }

        Ok(None)
    }

    /// Requests a block by height from a `peer_addr`
    async fn request_block_by_height(
        network: &Arc<RwLock<N>>,
        height: u64,
        peer_addr: SocketAddr,
    ) {
        let mut inv = Inv::new(1);
        inv.add_block_from_height(height);
        let this_peer = *network.read().await.public_addr();
        let req = GetResource::new(inv, Some(this_peer), u64::MAX, 1);
        debug!(event = "request block by height", ?req, ?peer_addr);

        if let Err(err) = network
            .read()
            .await
            .send_to_peer(req.into(), peer_addr)
            .await
        {
            warn!("could not request block {err}")
        }
    }

    async fn on_heartbeat(&mut self) -> anyhow::Result<bool> {
        if let Some(pre_sync) = &mut self.presync {
            if pre_sync.expiry <= Instant::now() {
                // Reset presync if it timed out
                self.presync = None;
            }
        }

        Ok(false)
    }
}

struct OutOfSyncImpl<DB: database::DB, VM: vm::VMExecution, N: Network> {
    range: (u64, u64),
    start_time: SystemTime,
    pool: HashMap<u64, Block>,
    peer_addr: SocketAddr,
    attempts: u8,

    acc: Arc<RwLock<Acceptor<N, DB, VM>>>,
    network: Arc<RwLock<N>>,
}

impl<DB: database::DB, VM: vm::VMExecution, N: Network>
    OutOfSyncImpl<DB, VM, N>
{
    fn new(
        acc: Arc<RwLock<Acceptor<N, DB, VM>>>,
        network: Arc<RwLock<N>>,
    ) -> Self {
        Self {
            start_time: SystemTime::now(),
            range: (0, 0),
            pool: HashMap::new(),
            acc,
            network,
            peer_addr: SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(127, 0, 0, 1),
                8000,
            )),
            attempts: 3,
        }
    }
    /// performed when entering the OutOfSync state
    async fn on_entering(&mut self, blk: Block, peer_addr: SocketAddr) {
        let (curr_height, locator) = {
            let acc = self.acc.read().await;
            (acc.get_curr_height().await, acc.get_curr_hash().await)
        };

        self.range = (
            curr_height,
            std::cmp::min(
                curr_height + MAX_BLOCKS_TO_REQUEST as u64,
                blk.header().height,
            ),
        );

        // Request missing blocks from source peer
        let gb_msg = GetBlocks::new(locator).into();

        if let Err(e) = self
            .network
            .read()
            .await
            .send_to_peer(gb_msg, peer_addr)
            .await
        {
            warn!("Unable to send GetBlocks: {e}")
        };

        // add to the pool
        let key = blk.header().height;
        self.pool.clear();
        self.pool.insert(key, blk);
        self.peer_addr = peer_addr;

        let (from, to) = &self.range;
        info!(event = "entering out-of-sync", from, to, ?peer_addr);
    }

    /// performed when exiting the state
    async fn on_exiting(&mut self) {
        self.pool.clear();
    }

    /// Return true if a transit back to InSync mode is needed
    pub async fn on_block_event(
        &mut self,
        blk: &Block,
        metadata: Option<Metadata>,
    ) -> anyhow::Result<bool> {
        let mut acc = self.acc.write().await;
        let h = blk.header().height;

        if self
            .start_time
            .checked_add(Duration::from_millis(EXPIRY_TIMEOUT_MILLIS as u64))
            .unwrap()
            <= SystemTime::now()
        {
            acc.restart_consensus().await;
            // Timeout-ed sync-up
            // Transit back to InSync mode
            return Ok(true);
        }

        if h <= acc.get_curr_height().await {
            return Ok(false);
        }

        // Try accepting consecutive block
        if h == acc.get_curr_height().await + 1 {
            acc.try_accept_block(blk, false).await?;

            if let Some(metadata) = &metadata {
                if metadata.src_addr == self.peer_addr {
                    // reset expiry_time only if we receive a valid block from
                    // the syncing peer.
                    self.start_time = SystemTime::now();
                }
            }

            // Try to accept other consecutive blocks from the pool, if
            // available
            for height in (h + 1)..(self.range.1 + 1) {
                if let Some(blk) = self.pool.get(&height) {
                    acc.try_accept_block(blk, false).await?;
                } else {
                    break;
                }
            }

            let tip = acc.get_curr_height().await;
            // Check target height is reached
            if tip >= self.range.1 {
                debug!(event = "sync target reached", height = tip);

                // Block sync-up procedure manages to download all requested
                acc.restart_consensus().await;

                // Transit to InSync mode
                return Ok(true);
            }

            return Ok(false);
        }

        // add block to the pool
        if self.pool.len() < MAX_BLOCKS_TO_REQUEST as usize {
            let key = blk.header().height;
            self.pool.insert(key, blk.clone());
        }

        debug!(event = "block saved", len = self.pool.len());

        Ok(false)
    }

    async fn on_heartbeat(&mut self) -> anyhow::Result<bool> {
        if self
            .start_time
            .checked_add(Duration::from_millis(EXPIRY_TIMEOUT_MILLIS as u64))
            .unwrap()
            <= SystemTime::now()
        {
            if self.attempts == 0 {
                debug!(
                    event = format!(
                        "out_of_sync timer expired for {} attempts",
                        self.attempts
                    )
                );
                // sync-up has timed out, recover consensus task
                self.acc.write().await.restart_consensus().await;

                // sync-up timed out for N attempts
                // Transit back to InSync mode as a fail-over
                return Ok(true);
            }

            // Request missing from local_pool blocks
            let mut inv = Inv::new(0);
            let from = self.range.0 + 1;
            let to = self.range.1 + 1;

            for height in from..=to {
                if self.pool.contains_key(&height) {
                    // already received
                    continue;
                }
                inv.add_block_from_height(height);
            }

            let network = self.acc.read().await.network.clone();
            if !inv.inv_list.is_empty() {
                if let Err(e) =
                    network.read().await.flood_request(&inv, None, 8).await
                {
                    warn!("Unable to request missing blocks {e}");
                }
            }

            self.start_time = SystemTime::now();
            self.attempts -= 1;
        }

        Ok(false)
    }
}

/// Requests a block by height/hash from the network with so-called
/// Flood-request approach.
async fn flood_request<N: Network>(network: &Arc<RwLock<N>>, inv: &Inv) {
    debug!(event = "flood_request", ?inv);

    if let Err(err) = network
        .read()
        .await
        .flood_request(inv, None, DEFAULT_HOPS_LIMIT)
        .await
    {
        warn!("could not request block {err}")
    };
}
