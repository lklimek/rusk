// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use crate::commons::RoundUpdate;
use node_data::bls::PublicKeyBytes;
use node_data::ledger::{Certificate, IterationInfo, Signature, StepVotes};
use node_data::message::payload::Vote;
use node_data::message::{payload, ConsensusMessage, Message};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error};

pub(crate) enum SvType {
    Validation,
    Ratification,
}

#[derive(Default, Clone)]
struct CertificateInfo {
    vote: Vote,
    cert: Certificate,

    quorum_reached_validation: bool,
    quorum_reached_ratification: bool,
}

impl fmt::Display for CertificateInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cert_info: {}, validation: ({:?},{:?}), ratification: ({:?},{:?}) ",
            self.vote,
            self.cert.validation,
            self.quorum_reached_validation,
            self.cert.ratification,
            self.quorum_reached_ratification
        )
    }
}

impl CertificateInfo {
    pub(crate) fn add_sv(
        &mut self,
        iter: u8,
        sv: StepVotes,
        svt: SvType,
        quorum_reached: bool,
    ) -> bool {
        match svt {
            SvType::Validation => {
                self.cert.validation = sv;

                if quorum_reached {
                    self.quorum_reached_validation = quorum_reached;
                }
            }
            SvType::Ratification => {
                self.cert.ratification = sv;

                if quorum_reached {
                    self.quorum_reached_ratification = quorum_reached;
                }
            }
        }

        debug!(
            event = "add_sv",
            iter,
            data = format!("{}", self),
            quorum_reached
        );

        self.is_ready()
    }

    /// Returns `true` if all fields are non-empty and quorum is reached for
    /// both validation and ratification
    fn is_ready(&self) -> bool {
        self.has_votes()
            && self.quorum_reached_validation
            && self.quorum_reached_ratification
    }

    /// Returns `true` if the certificate contains at least one vote
    fn has_votes(&self) -> bool {
        !self.cert.validation.is_empty() && !self.cert.ratification.is_empty()
    }
}

pub type SafeCertificateInfoRegistry = Arc<Mutex<CertInfoRegistry>>;

#[derive(Clone)]
struct IterationCerts {
    valid: Option<CertificateInfo>,
    nil: CertificateInfo,
    generator: PublicKeyBytes,
}

impl IterationCerts {
    fn new(generator: PublicKeyBytes) -> Self {
        Self {
            valid: None,
            nil: CertificateInfo::default(),
            generator,
        }
    }

    fn for_vote(&mut self, vote: &Vote) -> Option<&mut CertificateInfo> {
        if vote == &Vote::NoCandidate {
            return Some(&mut self.nil);
        }
        let cert = self.valid.get_or_insert_with(|| CertificateInfo {
            vote: vote.clone(),
            ..Default::default()
        });
        match &cert.vote == vote {
            true => Some(cert),
            false => {
                error!("Cannot add step votes for vote {vote:?}");
                None
            }
        }
    }
}

pub struct CertInfoRegistry {
    ru: RoundUpdate,

    /// Iterations certificates for current round keyed by iteration
    cert_list: HashMap<u8, IterationCerts>,
}

impl CertInfoRegistry {
    pub(crate) fn new(ru: RoundUpdate) -> Self {
        Self {
            ru,
            cert_list: HashMap::new(),
        }
    }

    /// Adds step votes per iteration
    /// Returns a quorum if both validation and ratification for an iteration
    /// exist
    pub(crate) fn add_step_votes(
        &mut self,
        iteration: u8,
        vote: &Vote,
        sv: StepVotes,
        svt: SvType,
        quorum_reached: bool,
        generator: &PublicKeyBytes,
    ) -> Option<Message> {
        let cert = self
            .cert_list
            .entry(iteration)
            .or_insert_with(|| IterationCerts::new(*generator));

        cert.for_vote(vote).and_then(|cert| {
            cert.add_sv(iteration, sv, svt, quorum_reached).then(|| {
                Self::build_quorum_msg(self.ru.clone(), iteration, cert.clone())
            })
        })
    }

    fn build_quorum_msg(
        ru: RoundUpdate,
        iteration: u8,
        result: CertificateInfo,
    ) -> Message {
        let header = node_data::message::ConsensusHeader {
            pubkey_bls: ru.pubkey_bls.clone(),
            prev_block_hash: ru.hash(),
            round: ru.round,
            iteration,
            msg_type: node_data::message::ConsensusMsgType::Quorum,
            signature: Signature::default(),
        };

        let mut payload = payload::Quorum {
            header,
            vote: result.vote,
            validation: result.cert.validation,
            ratification: result.cert.ratification,
        };
        payload.sign(&ru.secret_key, ru.pubkey_bls.inner());

        Message::new_quorum(payload)
    }

    pub(crate) fn get_nil_certificates(
        &mut self,
        to: u8,
    ) -> Vec<Option<IterationInfo>> {
        let mut res = Vec::with_capacity(to as usize);

        for iteration in 0u8..to {
            res.push(
                self.cert_list
                    .get(&iteration)
                    .map(|c| (&c.nil, c.generator))
                    .filter(|(ci, _)| ci.is_ready())
                    .map(|(ci, pk)| (ci.cert, pk)),
            );
        }

        res
    }
}
