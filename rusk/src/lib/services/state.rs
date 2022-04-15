// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use crate::error::Error;
use crate::services::prover::RuskProver;
use crate::transaction::{SpentTransaction, TransferPayload};
use crate::{Result, Rusk, RuskState};

use canonical::{Canon, Sink};
use dusk_bls12_381::BlsScalar;
use dusk_bls12_381_sign::PublicKey;
use dusk_bytes::{DeserializableSlice, Serializable};
use dusk_pki::ViewKey;
use phoenix_core::Note;
use rusk_vm::GasMeter;
use tonic::{Request, Response, Status};
use tracing::info;

pub use rusk_schema::state_server::{State, StateServer};
pub use rusk_schema::{
    get_stake_response::Amount, EchoRequest, EchoResponse,
    ExecuteStateTransitionRequest, ExecuteStateTransitionResponse,
    ExecutedTransaction as ExecutedTransactionProto,
    FindExistingNullifiersRequest, FindExistingNullifiersResponse,
    GetAnchorRequest, GetAnchorResponse, GetNotesOwnedByRequest,
    GetNotesOwnedByResponse, GetOpeningRequest, GetOpeningResponse,
    GetProvisionersRequest, GetProvisionersResponse, GetStakeRequest,
    GetStakeResponse, GetStateRootRequest, GetStateRootResponse,
    PersistRequest, PersistResponse, PreverifyRequest, PreverifyResponse,
    Provisioner, RevertRequest, RevertResponse, Stake as StakeProto,
    StateTransitionRequest, StateTransitionResponse,
    Transaction as TransactionProto, VerifyStateTransitionRequest,
    VerifyStateTransitionResponse,
};

impl Rusk {
    fn verify(&self, tx: &TransferPayload) -> Result<(), Status> {
        if self.state()?.any_nullifier_exists(tx.inputs())? {
            return Err(Status::failed_precondition(
                "Nullifier(s) already exists in the state",
            ));
        }

        if !RuskProver::preverify(tx)? {
            return Err(Status::failed_precondition(
                "Proof verification failed",
            ));
        }

        Ok(())
    }

    fn accept_transactions(
        &self,
        block_height: u64,
        block_gas_limit: u64,
        transfer_txs: Vec<TransactionProto>,
        generator: PublicKey,
    ) -> Result<(Response<StateTransitionResponse>, RuskState), Status> {
        let mut state = self.state()?;
        let mut block_gas_left = block_gas_limit;

        let mut txs = Vec::with_capacity(transfer_txs.len());
        let mut dusk_spent = 0;

        for tx in transfer_txs {
            let tx = TransferPayload::from_slice(&tx.payload)
                .map_err(Error::Serialization)?;

            let gas_limit = tx.fee().gas_limit;

            let mut gas_meter = GasMeter::with_limit(gas_limit);

            let result =
                state.execute::<()>(block_height, tx.clone(), &mut gas_meter);

            dusk_spent += gas_meter.spent() * tx.fee().gas_price;

            block_gas_left = block_gas_left
                .checked_sub(gas_meter.spent())
                .ok_or_else(|| Status::invalid_argument("Out of gas"))?;

            let spent_tx = SpentTransaction(tx, gas_meter, result.err());
            txs.push(spent_tx.into());
        }

        state.push_coinbase(block_height, dusk_spent, &generator)?;
        let state_root = state.root().to_vec();

        Ok((
            Response::new(StateTransitionResponse { txs, state_root }),
            state,
        ))
    }
}

#[tonic::async_trait]
impl State for Rusk {
    async fn echo(
        &self,
        request: Request<EchoRequest>,
    ) -> Result<Response<EchoResponse>, Status> {
        info!("Received Echo request");

        let request = request.into_inner();

        Ok(Response::new(EchoResponse {
            message: request.message,
        }))
    }

    async fn preverify(
        &self,
        request: Request<PreverifyRequest>,
    ) -> Result<Response<PreverifyResponse>, Status> {
        info!("Received Preverify request");

        let request = request.into_inner();

        let tx_proto = request.tx.ok_or_else(|| {
            Status::invalid_argument("Transaction is required")
        })?;

        let tx = TransferPayload::from_slice(&tx_proto.payload)
            .map_err(Error::Serialization)?;

        let tx_hash = tx.hash();

        self.verify(&tx)?;

        Ok(Response::new(PreverifyResponse {
            tx_hash: tx_hash.to_bytes().to_vec(),
            fee: Some((tx.fee()).into()),
        }))
    }

    async fn execute_state_transition(
        &self,
        request: Request<ExecuteStateTransitionRequest>,
    ) -> Result<Response<ExecuteStateTransitionResponse>, Status> {
        info!("Received ExecuteStateTransition request");

        let mut state = self.state()?;

        let request = request.into_inner();

        let mut block_gas_left = request.block_gas_limit;

        let mut txs = Vec::with_capacity(request.txs.len() + 1);
        let mut dusk_spent = 0;

        // Here we discard transactions that:
        // - Fail parsing
        // - Spend more gas than the running `block_gas_left`
        for tx in request.txs {
            if let Ok(tx) = TransferPayload::from_slice(&tx.payload) {
                let mut forked_state = state.fork();
                let mut gas_meter = GasMeter::with_limit(tx.fee().gas_limit);

                // We do not care if the transaction fails or succeeds here
                let result = forked_state.execute::<()>(
                    request.block_height,
                    tx.clone(),
                    &mut gas_meter,
                );

                let gas_spent = gas_meter.spent();

                // If the transaction executes with more gas than is left in the
                // block reject it
                if gas_spent > block_gas_left {
                    continue;
                }

                block_gas_left -= gas_spent;
                dusk_spent += gas_spent * tx.fee().gas_price;

                state = forked_state;
                let spent_tx = SpentTransaction(tx, gas_meter, result.err());
                txs.push(spent_tx.into());

                // No need to keep executing if there is no gas left in the
                // block
                if block_gas_left == 0 {
                    break;
                }
            }
        }

        let generator = PublicKey::from_slice(&request.generator)
            .map_err(Error::Serialization)?;

        state.push_coinbase(request.block_height, dusk_spent, &generator)?;

        // Compute the new state root resulting from the state changes
        let state_root = state.root().to_vec();

        let success = true;

        Ok(Response::new(ExecuteStateTransitionResponse {
            success,
            txs,
            state_root,
        }))
    }

    async fn verify_state_transition(
        &self,
        request: Request<VerifyStateTransitionRequest>,
    ) -> Result<Response<VerifyStateTransitionResponse>, Status> {
        info!("Received VerifyStateTransition request");

        let request = request.into_inner();
        let generator = PublicKey::from_slice(&request.generator)
            .map_err(Error::Serialization)?;
        self.accept_transactions(
            request.block_height,
            request.block_gas_limit,
            request.txs,
            generator,
        )?;

        Ok(Response::new(VerifyStateTransitionResponse {}))
    }

    async fn accept(
        &self,
        request: Request<StateTransitionRequest>,
    ) -> Result<Response<StateTransitionResponse>, Status> {
        info!("Received Accept request");

        let request = request.into_inner();
        let generator = PublicKey::from_slice(&request.generator)
            .map_err(Error::Serialization)?;
        let (response, mut state) = self.accept_transactions(
            request.block_height,
            request.block_gas_limit,
            request.txs,
            generator,
        )?;

        state.accept();

        Ok(response)
    }

    async fn finalize(
        &self,
        request: Request<StateTransitionRequest>,
    ) -> Result<Response<StateTransitionResponse>, Status> {
        info!("Received Finalize request");

        let request = request.into_inner();
        let generator = PublicKey::from_slice(&request.generator)
            .map_err(Error::Serialization)?;
        let (response, mut state) = self.accept_transactions(
            request.block_height,
            request.block_gas_limit,
            request.txs,
            generator,
        )?;

        state.finalize();

        Ok(response)
    }

    async fn revert(
        &self,
        _request: Request<RevertRequest>,
    ) -> Result<Response<RevertResponse>, Status> {
        info!("Received Revert request");

        let mut state = self.state()?;
        state.revert();

        let state_root = state.root().to_vec();
        Ok(Response::new(RevertResponse { state_root }))
    }

    async fn persist(
        &self,
        request: Request<PersistRequest>,
    ) -> Result<Response<PersistResponse>, Status> {
        info!("Received Persist request");

        let request = request.into_inner();

        let mut state = self.state()?;
        let state_root = state.root();

        if request.state_root != state_root {
            return Err(Status::invalid_argument(format!(
                "state root mismatch. Expected {}, Got {}",
                hex::encode(state_root),
                hex::encode(request.state_root)
            )));
        }

        self.persist(&mut state)?;

        Ok(Response::new(PersistResponse {}))
    }

    async fn get_provisioners(
        &self,
        _request: Request<GetProvisionersRequest>,
    ) -> Result<Response<GetProvisionersResponse>, Status> {
        info!("Received GetProvisioners request");

        let state = self.state()?;
        let provisioners = state
            .get_provisioners()?
            .into_iter()
            .filter_map(|(key, stake)| {
                stake.amount().copied().map(|(value, eligibility)| {
                    let raw_public_key_bls = key.to_raw_bytes().to_vec();
                    let public_key_bls = key.to_bytes().to_vec();

                    let stake = StakeProto {
                        value,
                        eligibility,
                        reward: stake.reward(),
                        counter: stake.counter(),
                    };

                    Provisioner {
                        raw_public_key_bls,
                        public_key_bls,
                        stakes: vec![stake],
                    }
                })
            })
            .collect();

        Ok(Response::new(GetProvisionersResponse { provisioners }))
    }

    async fn get_state_root(
        &self,
        _request: Request<GetStateRootRequest>,
    ) -> Result<Response<GetStateRootResponse>, Status> {
        info!("Received GetEphemeralStateRoot request");

        let state_root = self.state()?.root().to_vec();
        Ok(Response::new(GetStateRootResponse { state_root }))
    }

    async fn get_notes_owned_by(
        &self,
        request: Request<GetNotesOwnedByRequest>,
    ) -> Result<Response<GetNotesOwnedByResponse>, Status> {
        info!("Received GetNotesOwnedBy request");

        let vk = ViewKey::from_slice(&request.get_ref().vk)
            .map_err(Error::Serialization)?;
        let block_height = request.get_ref().height;

        let state = self.state()?;

        let (notes, height) = state.fetch_notes(block_height, &vk)?;
        let notes = notes.iter().map(|note| note.to_bytes().to_vec()).collect();

        Ok(Response::new(GetNotesOwnedByResponse { notes, height }))
    }

    async fn get_anchor(
        &self,
        _request: Request<GetAnchorRequest>,
    ) -> Result<Response<GetAnchorResponse>, Status> {
        info!("Received GetAnchor request");

        let anchor = self.state()?.fetch_anchor()?.to_bytes().to_vec();
        Ok(Response::new(GetAnchorResponse { anchor }))
    }

    async fn get_opening(
        &self,
        request: Request<GetOpeningRequest>,
    ) -> Result<Response<GetOpeningResponse>, Status> {
        info!("Received GetOpening request");

        let note = Note::from_slice(&request.get_ref().note)
            .map_err(Error::Serialization)?;

        let branch = self.state()?.fetch_opening(&note)?;

        const PAGE_SIZE: usize = 1024 * 64;
        let mut bytes = [0u8; PAGE_SIZE];
        let mut sink = Sink::new(&mut bytes[..]);
        branch.encode(&mut sink);
        let len = branch.encoded_len();
        let branch = (&bytes[..len]).to_vec();

        Ok(Response::new(GetOpeningResponse { branch }))
    }

    async fn get_stake(
        &self,
        request: Request<GetStakeRequest>,
    ) -> Result<Response<GetStakeResponse>, Status> {
        info!("Received GetStake request");

        const ERR: Error = Error::Serialization(dusk_bytes::Error::InvalidData);

        let mut bytes = [0u8; PublicKey::SIZE];

        let pk = request.get_ref().pk.as_slice();

        if pk.len() < PublicKey::SIZE {
            return Err(ERR.into());
        }

        (&mut bytes[..PublicKey::SIZE]).copy_from_slice(&pk[..PublicKey::SIZE]);

        let pk = PublicKey::from_bytes(&bytes).map_err(|_| ERR)?;

        let stake = self.state()?.fetch_stake(&pk)?;
        let amount = stake
            .amount()
            .copied()
            .map(|(value, eligibility)| Amount { value, eligibility });

        Ok(Response::new(GetStakeResponse {
            amount,
            reward: stake.reward(),
            counter: stake.counter(),
        }))
    }

    async fn find_existing_nullifiers(
        &self,
        request: Request<FindExistingNullifiersRequest>,
    ) -> Result<Response<FindExistingNullifiersResponse>, Status> {
        info!("Received FindExistingNullifiers request");

        let nullifiers = &request.get_ref().nullifiers;

        let nullifiers = nullifiers
            .iter()
            .map(|n| BlsScalar::from_slice(n).map_err(Error::Serialization))
            .collect::<Result<Vec<_>, _>>()?;

        let nullifiers = self
            .state()?
            .transfer_contract()?
            .find_existing_nullifiers(&nullifiers)
            .map_err(|_| {
                Error::Serialization(dusk_bytes::Error::InvalidData)
            })?;

        let nullifiers =
            nullifiers.iter().map(|n| n.to_bytes().to_vec()).collect();

        Ok(Response::new(FindExistingNullifiersResponse { nullifiers }))
    }
}
