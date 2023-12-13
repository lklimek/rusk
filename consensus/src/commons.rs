// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use crate::contract_state::Operations;
// TODO: use crate::messages::payload::StepVotes;
// RoundUpdate carries the data about the new Round, such as the active
// Provisioners, the BidList, the Seed and the Hash.

use node_data::ledger::*;
use node_data::message;
use node_data::message::{Payload, Topics};
use tracing::Instrument;

use crate::contract_state::CallParams;
use dusk_bls12_381_sign::SecretKey;
use dusk_bytes::DeserializableSlice;
use node_data::bls::PublicKey;

use crate::config;
use node_data::message::{AsyncQueue, Message};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tracing::{debug, error};

#[derive(Clone, Default, Debug)]
pub struct RoundUpdate {
    // Current round number of the ongoing consensus
    pub round: u64,

    // This provisioner consensus keys
    pub pubkey_bls: PublicKey,
    pub secret_key: SecretKey,

    seed: Seed,
    hash: [u8; 32],
    timestamp: i64,
    cert: Certificate,
}

impl RoundUpdate {
    pub fn new(
        pubkey_bls: PublicKey,
        secret_key: SecretKey,
        mrb_block: &Block,
    ) -> Self {
        let round = mrb_block.header().height + 1;
        RoundUpdate {
            round,
            pubkey_bls,
            secret_key,
            cert: mrb_block.header().cert,
            hash: mrb_block.header().hash,
            seed: mrb_block.header().seed,
            timestamp: mrb_block.header().timestamp,
        }
    }

    pub fn seed(&self) -> Seed {
        self.seed
    }

    pub fn hash(&self) -> [u8; 32] {
        self.hash
    }

    pub fn timestamp(&self) -> i64 {
        self.timestamp
    }

    pub fn cert(&self) -> &Certificate {
        &self.cert
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ConsensusError {
    InvalidBlock,
    InvalidBlockHash,
    InvalidSignature,
    InvalidMsgType,
    FutureEvent,
    PastEvent,
    NotCommitteeMember,
    NotImplemented,
    NotReady,
    MaxIterationReached,
    ChildTaskTerminated,
    Canceled,
}
/// Makes an attempt to cast a vote for a specified candidate block if VST call
/// passes. If candidate block is default, it casts a NIL vote, without calling
/// VST API
#[allow(clippy::too_many_arguments)]
pub fn spawn_cast_vote<T: Operations + 'static>(
    join_set: &mut JoinSet<()>,
    verified_hash: Arc<Mutex<[u8; 32]>>,
    candidate: Block,
    pubkey: PublicKey,
    ru: RoundUpdate,
    step: u8,
    outbound: AsyncQueue<Message>,
    inbound: AsyncQueue<Message>,
    executor: Arc<Mutex<T>>,
    topic: Topics,
) {
    let hash = to_str(&candidate.header().hash);

    join_set.spawn(
        async move {
            let hash = candidate.header().hash;
            let already_verified = *verified_hash.lock().await == hash;

            if !already_verified && hash != [0u8; 32] {
                let pubkey = &candidate.header().generator_bls_pubkey.0;
                let generator =
                    match dusk_bls12_381_sign::PublicKey::from_slice(pubkey) {
                        Ok(pubkey) => pubkey,
                        Err(e) => {
                            error!(
                        "unable to decode generator BLS Pubkey {}, err: {:?}",
                        hex::encode(pubkey),
                        e,
                    );
                            return;
                        }
                    };

                match executor
                    .lock()
                    .await
                    .verify_state_transition(
                        CallParams {
                            round: ru.round,
                            block_gas_limit: candidate.header().gas_limit,
                            generator_pubkey: PublicKey::new(generator),
                        },
                        candidate.txs().clone(),
                    )
                    .await
                {
                    Ok(verification_output) => {
                        // Ensure the `event_hash` and `state_root` returned
                        // from the VST call are the
                        // ones we expect to have with the
                        // current candidate block.
                        if verification_output.event_hash
                            != candidate.header().event_hash
                        {
                            error!(
                                desc = "event hash mismatch",
                                event_hash =
                                    hex::encode(verification_output.event_hash),
                                candidate_event_hash =
                                    hex::encode(candidate.header().event_hash),
                            );
                            return;
                        }

                        if verification_output.state_root
                            != candidate.header().state_hash
                        {
                            error!(
                                desc = "state hash mismatch",
                                vst_state_hash =
                                    hex::encode(verification_output.state_root),
                                state_hash =
                                    hex::encode(candidate.header().state_hash),
                            );
                            return;
                        }
                    }
                    Err(e) => {
                        error!("VST failed with err: {:?}", e);
                        return;
                    }
                };
            }

            if already_verified && hash != [0u8; 32] {
                debug!(event = "vst call skipped", reason = "already_verified",)
            }

            {
                let mut guard = verified_hash.lock().await;
                *guard = hash;
            }

            let hdr = message::Header {
                pubkey_bls: pubkey,
                round: ru.round,
                step,
                block_hash: hash,
                topic: topic.into(),
            };

            let signature = hdr.sign(&ru.secret_key, ru.pubkey_bls.inner());

            // Sign and construct reduction message
            let msg = message::Message::new_validation(
                hdr,
                message::payload::Validation { signature },
            );

            //   publish
            outbound.send(msg.clone()).await.unwrap_or_else(|err| {
                error!("unable to publish reduction msg {:?}", err)
            });

            // Register my vote locally
            inbound.send(msg).await.unwrap_or_else(|err| {
                error!("unable to register reduction msg {:?}", err)
            });
        }
        .instrument(tracing::info_span!("voting", hash)),
    );
}
#[async_trait::async_trait]
pub trait Database: Send + Sync {
    fn store_candidate_block(&mut self, b: Block);
    async fn get_candidate_block_by_hash(
        &self,
        h: &Hash,
    ) -> anyhow::Result<Block>;
    fn delete_candidate_blocks(&mut self);
}

pub enum StepName {
    Proposal = 0,
    Validation = 1,
    Ratification = 2,
}

pub trait IterCounter {
    /// Count of all steps per a single iteration
    const STEP_NUM: u8 = 3;
    type Step;
    fn next(&mut self) -> Result<Self, ConsensusError>
    where
        Self: Sized;
    fn from_step(step_num: Self::Step) -> Self;
    fn step_from_name(&self, st: StepName) -> Self::Step;
    fn step_from_pos(&self, pos: usize) -> Self::Step;
    fn to_step_name(&self) -> StepName;
}

impl IterCounter for u8 {
    type Step = u8;

    fn next(&mut self) -> Result<Self, ConsensusError> {
        let next = *self + 1;
        if next >= config::CONSENSUS_MAX_ITER {
            return Err(ConsensusError::MaxIterationReached);
        }

        *self = next;
        Ok(next)
    }

    fn from_step(step: Self::Step) -> Self {
        step / Self::STEP_NUM
    }

    fn step_from_name(&self, st: StepName) -> Self::Step {
        let iteration_step = self * Self::STEP_NUM;
        let relative_step = st as u8;
        iteration_step + relative_step
    }

    fn step_from_pos(&self, pos: usize) -> Self::Step {
        self * Self::STEP_NUM + pos as u8
    }

    fn to_step_name(&self) -> StepName {
        match self % Self::STEP_NUM {
            0 => StepName::Proposal,
            1 => StepName::Validation,
            2 => StepName::Ratification,
            _ => panic!("STEP_NUM>3"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct QuorumMsgSender {
    queue: AsyncQueue<Message>,
}

impl QuorumMsgSender {
    pub(crate) fn new(queue: AsyncQueue<Message>) -> Self {
        Self { queue }
    }

    /// Sends an quorum (internally) to the quorum loop.
    pub(crate) async fn send(&self, msg: Message) -> bool {
        if let Payload::Quorum(q) = &msg.payload {
            if q.signature == [0u8; 48]
                || q.validation.is_empty()
                || q.ratification.is_empty()
                || msg.header.block_hash == [0; 32]
            {
                return false;
            }

            tracing::debug!(
                event = "send quorum_msg",
                hash = to_str(&msg.header.block_hash),
                round = msg.header.round,
                step = msg.header.step,
                validation = format!("{:#?}", q.validation),
                ratification = format!("{:#?}", q.ratification),
                signature = to_str(&q.signature),
            );

            let _ = self
                .queue
                .send(msg.clone())
                .await
                .map_err(|e| error!("send quorum_msg failed with {:?}", e));

            return true;
        }

        false
    }
}
