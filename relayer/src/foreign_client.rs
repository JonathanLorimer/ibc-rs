use std::{thread, time::Duration};

use prost_types::Any;
use thiserror::Error;
use tracing::{error, info};

use ibc::downcast;
use ibc::events::{IbcEvent, IbcEventType};
use ibc::ics02_client::client_consensus::{ConsensusState, QueryClientEventRequest};
use ibc::ics02_client::client_misbehaviour::AnyMisbehaviour;
use ibc::ics02_client::client_state::ClientState;
use ibc::ics02_client::events::UpdateClient;
use ibc::ics02_client::header::Header;
use ibc::ics02_client::msgs::create_client::MsgCreateAnyClient;
use ibc::ics02_client::msgs::misbehavior::MsgSubmitAnyMisbehaviour;
use ibc::ics02_client::msgs::update_client::MsgUpdateAnyClient;
use ibc::ics24_host::identifier::{ChainId, ClientId};
use ibc::query::QueryTxRequest;
use ibc::tx_msg::Msg;
use ibc::Height;
use ibc_proto::ibc::core::client::v1::QueryConsensusStatesRequest;

use crate::chain::handle::ChainHandle;

#[derive(Debug, Error)]
pub enum ForeignClientError {
    #[error("error raised while creating client: {0}")]
    ClientCreate(String),

    #[error("error raised while updating client: {0}")]
    ClientUpdate(String),

    #[error("failed while querying for client {0} on chain id: {1} with error: {2}")]
    ClientQuery(ClientId, ChainId, String),

    #[error("failed while finding client {0}: expected chain_id in client state: {1}; actual chain_id: {2}")]
    ClientFind(ClientId, ChainId, ChainId),

    #[error("error raised while submitting the misbehaviour evidence: {0}")]
    Misbehaviour(String),
}

#[derive(Clone, Debug)]
pub struct ForeignClient {
    /// The identifier of this client. The host chain determines this id upon client creation,
    /// so we may be using the default value temporarily.
    pub id: ClientId,

    /// A handle to the chain hosting this client, i.e., destination chain.
    pub dst_chain: Box<dyn ChainHandle>,

    /// A handle to the chain whose headers this client is verifying, aka the source chain.
    pub src_chain: Box<dyn ChainHandle>,
}

impl ForeignClient {
    /// Creates a new foreign client on `dst_chain`. Blocks until the client is created, or
    /// an error occurs.
    /// Post-condition: `dst_chain` hosts an IBC client for `src_chain`.
    pub fn new(
        dst_chain: Box<dyn ChainHandle>,
        src_chain: Box<dyn ChainHandle>,
    ) -> Result<ForeignClient, ForeignClientError> {
        // Sanity check
        if src_chain.id().eq(&dst_chain.id()) {
            return Err(ForeignClientError::ClientCreate(format!(
                "the source ({}) and destination ({}) chains must be different",
                src_chain.id(),
                dst_chain.id(),
            )));
        }

        let mut client = ForeignClient {
            id: ClientId::default(),
            dst_chain: dst_chain.clone(),
            src_chain: src_chain.clone(),
        };

        client.create()?;

        Ok(client)
    }

    pub fn restore_client(
        dst_chain: Box<dyn ChainHandle>,
        src_chain: Box<dyn ChainHandle>,
        client_id: &ClientId,
    ) -> ForeignClient {
        ForeignClient {
            id: client_id.clone(),
            dst_chain: dst_chain.clone(),
            src_chain: src_chain.clone(),
        }
    }

    /// Queries `host_chain` to verify that a client with identifier `client_id` exists.
    /// If the client does not exist, returns an error. If the client exists, cross-checks that the
    /// identifier for the target chain of this client (i.e., the chain whose headers this client is
    /// verifying) is consistent with `expected_target_chain`, and if so, return a new
    /// `ForeignClient` representing this client.
    pub fn find(
        expected_target_chain: Box<dyn ChainHandle>,
        host_chain: Box<dyn ChainHandle>,
        client_id: &ClientId,
    ) -> Result<ForeignClient, ForeignClientError> {
        let height = Height::new(expected_target_chain.id().version(), 0);

        match host_chain.query_client_state(&client_id, height) {
            Ok(cs) => {
                if cs.chain_id() != expected_target_chain.id() {
                    Err(ForeignClientError::ClientFind(
                        client_id.clone(),
                        expected_target_chain.id(),
                        cs.chain_id(),
                    ))
                } else {
                    // TODO: Any additional checks?
                    Ok(ForeignClient {
                        id: client_id.clone(),
                        dst_chain: host_chain.clone(),
                        src_chain: expected_target_chain.clone(),
                    })
                }
            }
            Err(e) => Err(ForeignClientError::ClientQuery(
                client_id.clone(),
                host_chain.id(),
                format!("{}", e),
            )),
        }
    }

    /// Returns a handle to the chain hosting this client.
    pub fn dst_chain(&self) -> Box<dyn ChainHandle> {
        self.dst_chain.clone()
    }

    /// Returns a handle to the chain whose headers this client is sourcing (the source chain).
    pub fn src_chain(&self) -> Box<dyn ChainHandle> {
        self.src_chain.clone()
    }

    pub fn id(&self) -> &ClientId {
        &self.id
    }

    /// Lower-level interface for preparing a message to create a client.
    pub fn build_create_client(&self) -> Result<MsgCreateAnyClient, ForeignClientError> {
        // Get signer
        let signer = self.dst_chain.get_signer().map_err(|e| {
            ForeignClientError::ClientCreate(format!(
                "failed while fetching the destination chain ({}) signer: {}",
                self.dst_chain.id(),
                e
            ))
        })?;

        // Build client create message with the data from source chain at latest height.
        let latest_height = self.src_chain.query_latest_height().map_err(|e| {
            ForeignClientError::ClientCreate(format!(
                "failed while querying src chain ({}) for latest height: {}",
                self.src_chain.id(),
                e
            ))
        })?;

        let client_state = self
            .src_chain
            .build_client_state(latest_height)
            .map_err(|e| {
                ForeignClientError::ClientCreate(format!(
                    "failed while building client state from src chain ({}) with error: {}",
                    self.src_chain.id(),
                    e
                ))
            })?
            .wrap_any();

        let consensus_state = self.src_chain
            .build_consensus_state(latest_height)
            .map_err(|e| ForeignClientError::ClientCreate(format!("failed while building client consensus state from src chain ({}) with error: {}", self.src_chain.id(), e)))?
            .wrap_any();

        //TODO Get acct_prefix
        let msg = MsgCreateAnyClient::new(client_state, consensus_state, signer).map_err(|e| {
            ForeignClientError::ClientCreate(format!(
                "failed while building the create client message: {}",
                e
            ))
        })?;

        Ok(msg)
    }

    /// Returns the identifier of the newly created client.
    pub fn build_create_client_and_send(&self) -> Result<IbcEvent, ForeignClientError> {
        let new_msg = self.build_create_client()?;

        let res = self
            .dst_chain
            .send_msgs(vec![new_msg.to_any()])
            .map_err(|e| {
                ForeignClientError::ClientCreate(format!(
                    "failed sending message to dst chain ({}) with err: {}",
                    self.dst_chain.id(),
                    e
                ))
            })?;

        assert!(!res.is_empty());
        Ok(res[0].clone())
    }

    /// Sends the client creation transaction & subsequently sets the id of this ForeignClient
    fn create(&mut self) -> Result<(), ForeignClientError> {
        let done = '\u{1F36D}';

        match self.build_create_client_and_send() {
            Err(e) => {
                error!("Failed CreateClient {:?}: {}", self.dst_chain.id(), e);
                return Err(ForeignClientError::ClientCreate(format!(
                    "Create client failed ({:?})",
                    e
                )));
            }
            Ok(event) => {
                self.id = extract_client_id(&event)?.clone();
                println!("{}  {} => {:?}\n", done, self.dst_chain.id(), event);
            }
        }
        Ok(())
    }

    pub fn build_update_client(
        &self,
        target_height: Height,
    ) -> Result<Vec<Any>, ForeignClientError> {
        // Wait for source chain to reach `target_height`
        while self.src_chain().query_latest_height().map_err(|e| {
            ForeignClientError::ClientUpdate(format!(
                "failed fetching src chain latest height with error: {}",
                e
            ))
        })? < target_height
        {
            thread::sleep(Duration::from_millis(100))
        }

        // Get the latest trusted height from the client state on destination.
        let trusted_height = self
            .dst_chain()
            .query_client_state(&self.id, Height::default())
            .map_err(|e| {
                ForeignClientError::ClientUpdate(format!(
                    "failed querying client state on dst chain {} with error: {}",
                    self.id, e
                ))
            })?
            .latest_height();

        let header = self
            .src_chain()
            .build_header(trusted_height, target_height)
            .map_err(|e| {
                ForeignClientError::ClientUpdate(format!(
                    "failed building header with error: {}",
                    e
                ))
            })?
            .wrap_any();

        let signer = self.dst_chain().get_signer().map_err(|e| {
            ForeignClientError::ClientUpdate(format!(
                "failed getting signer for dst chain ({}) with error: {}",
                self.dst_chain.id(),
                e
            ))
        })?;
        let new_msg = MsgUpdateAnyClient {
            client_id: self.id.clone(),
            header,
            signer,
        };

        Ok(vec![new_msg.to_any()])
    }

    pub fn build_update_client_and_send(&self) -> Result<IbcEvent, ForeignClientError> {
        let h = self.src_chain.query_latest_height().map_err(|e| {
            ForeignClientError::ClientUpdate(format!(
                "failed while querying src chain ({}) for latest height: {}",
                self.src_chain.id(),
                e
            ))
        })?;
        let new_msgs = self.build_update_client(h)?;

        let mut events = self.dst_chain().send_msgs(new_msgs).map_err(|e| {
            ForeignClientError::ClientUpdate(format!(
                "failed sending message to dst chain ({}) with err: {}",
                self.dst_chain.id(),
                e
            ))
        })?;

        assert!(!events.is_empty());
        Ok(events.pop().unwrap())
    }

    /// Attempts to update a client using header from the latest height of its source chain.
    pub fn update(&self) -> Result<(), ForeignClientError> {
        let res = self.build_update_client_and_send().map_err(|e| {
            ForeignClientError::ClientUpdate(format!("build_create_client_and_send {:?}", e))
        })?;

        info!(
            "Client id {:?} on {:?} updated with return message {:?}\n",
            self.id,
            self.dst_chain.id(),
            res
        );

        Ok(())
    }

    pub fn update_client_event(
        &self,
        consensus_height: Height,
    ) -> Result<Option<UpdateClient>, ForeignClientError> {
        let request = QueryClientEventRequest {
            height: Height::zero(),
            event_id: IbcEventType::UpdateClient,
            client_id: self.id.clone(),
            consensus_height,
        };

        // Tx query fails if we don't wait a bit here (block is not ready/ indexed?)
        thread::sleep(Duration::from_millis(100));
        let res = self
            .dst_chain
            .query_txs(QueryTxRequest::Client(request))
            .map_err(|e| {
                ForeignClientError::Misbehaviour(format!(
                    "failed to query Tx-es for update client event {}",
                    e
                ))
            })?;

        if res.is_empty() {
            return Ok(None);
        };

        assert_eq!(res.len(), 1);
        let event = res[0].clone();
        let update = downcast!(event.clone() => IbcEvent::UpdateClient).ok_or_else(|| {
            ForeignClientError::Misbehaviour(format!(
                "query Tx-es returned unexpected event {}",
                event.to_json()
            ))
        })?;
        Ok(Some(update))
    }

    /// Retrieves all consensus heights for this client and sorts them in reverse
    /// order. If they are not pruned on chain then last consensus state is the one
    /// installed by the `CreateClient` operation.
    fn consensus_state_heights(&self) -> Result<Vec<Height>, ForeignClientError> {
        let mut consensus_state_heights: Vec<Height> = self
            .dst_chain
            .query_consensus_states(QueryConsensusStatesRequest {
                client_id: self.id.to_string(),
                pagination: None,
            })
            .map_err(|e| {
                ForeignClientError::Misbehaviour(format!("failed to query consensus states {}", e))
            })?
            .iter()
            .filter_map(|cs| Option::from(cs.height))
            .collect();

        consensus_state_heights.sort();
        consensus_state_heights.reverse();
        Ok(consensus_state_heights)
    }

    /// Check for misbehaviour and submit evidence.
    /// The check starts with and `update_event` emitted by chain B (`dst_chain`) for a client update
    /// with a header from chain A (`src_chain`). The algorithm goes backwards through the headers
    /// until it gets to the first misbehaviour.
    ///
    /// The following cases are covered:
    /// 1 - forks, double signing, assumes at least one consensus state before the fork point exists
    ///
    /// Assuming existing consensus states on chain B are: [Sn,.., Sf, Sf-1, S0] with `Sf-1` being
    /// the most recent state before fork.
    /// Chain A is queried for a header `Hf'` at `Sf.height` and if it is different than the `Hf`
    /// in the event for the client update (the one that has generated `Sf` on chain), then the two
    /// headers are included in the evidence and submitted.
    /// Note that in this case the headers are different but have the same height.
    ///
    /// 2 - time travelling lunatic attack: some header with a height that is higher than the latest
    /// height on A has been accepted and a consensus state was created on B. Note that this implies
    /// that the timestamp of this header must be within the `clock_drift` of the client.
    ///
    /// Assuming the client on B has been updated with `h2`(not present on/ produced by chain A)
    /// and it has a timestamp of `t2` that is at most `clock_drift` in the future.
    /// The misbehavior detector (`MD`) gets the latest header from A, let it be `h1` with a
    /// timestamp of `t1`. If `t1 >= t2` then evidence of misbehavior is submitted to A.
    ///
    /// The following case is not covered:
    /// 2' - same as 2 but `t1 < t2`.
    /// Problem: If `h2` has a high height then no other client updates can be installed on `B`
    /// for a long time. There is currently no other opportunity to check `h2` except a process
    /// restart. In the future the `update_client` event should be rechecked for every new block
    /// on `A` or after `clock_drift` time so it falls back into case 2 above. In other words,
    /// given enough time the misbehavior detector will find a header `h1'` on A with `t1' >= t2`
    ///
    /// Other notes:
    /// - the algorithm builds misbehavior at each consensus height, starting with the
    /// highest height assuming the previous one is trusted. It submits the last constructed
    /// evidence (the one with the lowest height)
    /// - a lot of the logic here is derived from the behavior of the only implemented client
    /// (ics07-tendermint) and might not be general enough.
    ///
    pub fn handle_misbehaviour(
        &self,
        mut update_event: UpdateClient,
    ) -> Result<Option<AnyMisbehaviour>, ForeignClientError> {
        thread::sleep(Duration::from_millis(100));

        // get the list of consensus state heights in reverse order
        // Note: If chain does not prune consensus states then the last consensus state is
        // the one installed by the `CreateClient` which does not include a header.
        // For chains that do support pruning, it is possible that the last consensus state
        // was installed by an `UpdateClient` and an event and header will be found.
        let consensus_state_heights = self.consensus_state_heights()?;

        // there must exists at least two consensus states on-chain for evidence handling to work
        if consensus_state_heights.len() <= 1 {
            return Ok(None);
        }

        info!(
            "checking misbehaviour starting from {:?}, for consensus state heights {:?}",
            update_event.common.consensus_height, consensus_state_heights,
        );

        let mut first_misbehaviour = None;
        let latest_chain_height = self.src_chain.query_latest_height().map_err(|e| {
            ForeignClientError::Misbehaviour(format!("failed to get latest height {}", e))
        })?;

        for (i, h) in consensus_state_heights.iter().enumerate() {
            if h > &update_event.common.consensus_height {
                continue;
            }

            let misbehavior = self
                .src_chain
                .build_misbehaviour(update_event.clone(), latest_chain_height)
                .map_err(|e| {
                    ForeignClientError::Misbehaviour(format!("failed to build misbehaviour {}", e))
                })?;

            if misbehavior.is_some() {
                first_misbehaviour = misbehavior;
            }

            // get the previous update client event
            if (i + 1) < consensus_state_heights.len() {
                if let Some(new_update) =
                    self.update_client_event(consensus_state_heights[i + 1])?
                {
                    update_event = new_update;
                    continue;
                }
                break;
            }
        }

        Ok(first_misbehaviour)
    }

    pub fn detect_misbehaviour_and_send_evidence(
        &self,
        update: Option<UpdateClient>,
    ) -> Result<Option<IbcEvent>, ForeignClientError> {
        // if event is None start with the last client update event
        let update = match update {
            Some(update) => Some(update),
            None => {
                let client_state = self
                    .dst_chain
                    .query_client_state(self.id(), Height::zero())
                    .map_err(|e| {
                        ForeignClientError::Misbehaviour(format!(
                            "failed to query client state {}",
                            e
                        ))
                    })?;
                self.update_client_event(client_state.latest_height())?
            }
        };

        if update.is_none() {
            return Ok(None);
        }

        match self.handle_misbehaviour(update.unwrap())? {
            None => Ok(None),
            Some(misbehaviour) => {
                error!("MISBEHAVIOUR detected {:?}, sending evidence", misbehaviour);

                let signer = self.dst_chain().get_signer().map_err(|e| {
                    ForeignClientError::Misbehaviour(format!(
                        "failed getting signer for dst chain ({}) with error: {}",
                        self.dst_chain.id(),
                        e
                    ))
                })?;

                let msg = MsgSubmitAnyMisbehaviour {
                    client_id: self.id.clone(),
                    misbehaviour,
                    signer,
                };

                let events = self
                    .dst_chain()
                    .send_msgs(vec![msg.to_any()])
                    .map_err(|e| {
                        ForeignClientError::Misbehaviour(format!(
                            "failed sending evidence to dst chain ({}) with err: {}",
                            self.dst_chain.id(),
                            e
                        ))
                    })?;

                // TODO - invoke light client fork accountability

                Ok(Some(events[0].clone()))
            }
        }
    }
}

pub fn extract_client_id(event: &IbcEvent) -> Result<&ClientId, ForeignClientError> {
    match event {
        IbcEvent::CreateClient(ev) => Ok(ev.client_id()),
        IbcEvent::UpdateClient(ev) => Ok(ev.client_id()),
        _ => Err(ForeignClientError::ClientCreate(
            "cannot extract client_id from result".to_string(),
        )),
    }
}

/// Tests the integration of crates `relayer` plus `relayer-cli` against crate `ibc`. These tests
/// exercise various client methods (create, update, ForeignClient::new) using locally-running
/// instances of chains built using `MockChain`.
#[cfg(test)]
mod test {
    use std::str::FromStr;

    use ibc::{
        events::IbcEvent,
        ics02_client::{
            events::{Attributes, UpdateClient},
            header::AnyHeader,
        },
        mock::header::MockHeader,
    };
    use ibc::{ics02_client::client_type::ClientType, Height};
    use ibc::{ics07_tendermint::client_def::TendermintClient, ics24_host::identifier::ClientId};

    use crate::chain::mock::test_utils::get_basic_chain_config;
    use crate::chain::mock::MockChain;
    use crate::chain::runtime::ChainRuntime;
    use crate::foreign_client::ForeignClient;

    /// Basic test for the `build_create_client_and_send` method.
    #[test]
    fn create_client_and_send_method() {
        let a_cfg = get_basic_chain_config("chain_a");
        let b_cfg = get_basic_chain_config("chain_b");

        let (a_chain, _) = ChainRuntime::<MockChain>::spawn(a_cfg).unwrap();
        let (b_chain, _) = ChainRuntime::<MockChain>::spawn(b_cfg).unwrap();
        let a_client = ForeignClient {
            id: Default::default(),
            dst_chain: a_chain.clone(),
            src_chain: b_chain.clone(),
        };

        let b_client = ForeignClient {
            id: Default::default(),
            dst_chain: b_chain,
            src_chain: a_chain,
        };

        // Create the client on chain a
        let res = a_client.build_create_client_and_send();
        assert!(
            res.is_ok(),
            "build_create_client_and_send failed (chain a) with error {:?}",
            res
        );
        assert!(matches!(res.unwrap(), IbcEvent::CreateClient(_)));

        // Create the client on chain b
        let res = b_client.build_create_client_and_send();
        assert!(
            res.is_ok(),
            "build_create_client_and_send failed (chain b) with error {:?}",
            res
        );
        assert!(matches!(res.unwrap(), IbcEvent::CreateClient(_)));
    }

    /// Basic test for the `build_update_client_and_send` & `build_create_client_and_send` methods.
    #[test]
    fn update_client_and_send_method() {
        let a_cfg = get_basic_chain_config("chain_a");
        let b_cfg = get_basic_chain_config("chain_b");
        let a_client_id = ClientId::from_str("client_on_a_forb").unwrap();

        // The number of ping-pong iterations
        let num_iterations = 3;

        let (a_chain, _) = ChainRuntime::<MockChain>::spawn(a_cfg).unwrap();
        let (b_chain, _) = ChainRuntime::<MockChain>::spawn(b_cfg).unwrap();
        let mut a_client = ForeignClient {
            id: a_client_id,
            dst_chain: a_chain.clone(),
            src_chain: b_chain.clone(),
        };

        let mut b_client = ForeignClient {
            id: Default::default(),
            dst_chain: b_chain.clone(),
            src_chain: a_chain.clone(),
        };

        // This action should fail because no client exists (yet)
        let res = a_client.build_update_client_and_send();
        assert!(
            res.is_err(),
            "build_update_client_and_send was supposed to fail (no client existed)"
        );

        // Remember b's height.
        let b_height_start = b_chain.clone().query_latest_height().unwrap();

        // Create a client on chain a
        let res = a_client.create();
        assert!(
            res.is_ok(),
            "build_create_client_and_send failed (chain a) with error {:?}",
            res
        );

        // TODO: optionally add return events from `create` and assert on the event type, e.g.:
        //      assert!(matches!(res.as_ref().unwrap(), IBCEvent::CreateClient(_)));
        //      let a_client_id = extract_client_id(&res.unwrap()).unwrap().clone();

        // This should fail because the client on chain a already has the latest headers. Chain b,
        // the source chain for the client on a, is at the same height where it was when the client
        // was created, so an update should fail here.
        let res = a_client.build_update_client_and_send();
        assert!(
            res.is_err(),
            "build_update_client_and_send was supposed to fail",
        );

        // Remember b's height.
        let b_height_last = b_chain.query_latest_height().unwrap();
        assert_eq!(b_height_last, b_height_start);

        // Create a client on chain b
        let res = b_client.create();
        assert!(
            res.is_ok(),
            "build_create_client_and_send failed (chain b) with error {:?}",
            res
        );
        // TODO: assert return events
        //  assert!(matches!(res.as_ref().unwrap(), IBCEvent::CreateClient(_)));

        // Chain b should have advanced
        let mut b_height_last = b_chain.query_latest_height().unwrap();
        assert_eq!(b_height_last, b_height_start.increment());

        // Remember the current height of chain a
        let mut a_height_last = a_chain.query_latest_height().unwrap();

        // Now we can update both clients -- a ping pong, similar to ICS18 `client_update_ping_pong`
        for _i in 1..num_iterations {
            let res = a_client.build_update_client_and_send();
            assert!(
                res.is_ok(),
                "build_update_client_and_send failed (chain a) with error: {:?}",
                res
            );
            assert!(matches!(res.as_ref().unwrap(), IbcEvent::UpdateClient(_)));

            let a_height_current = a_chain.query_latest_height().unwrap();
            a_height_last = a_height_last.increment();
            assert_eq!(
                a_height_last, a_height_current,
                "after client update, chain a did not advance"
            );

            // And also update the client on chain b.
            let res = b_client.build_update_client_and_send();
            assert!(
                res.is_ok(),
                "build_update_client_and_send failed (chain b) with error: {:?}",
                res
            );
            assert!(matches!(res.as_ref().unwrap(), IbcEvent::UpdateClient(_)));

            let b_height_current = b_chain.query_latest_height().unwrap();
            b_height_last = b_height_last.increment();
            assert_eq!(
                b_height_last, b_height_current,
                "after client update, chain b did not advance"
            );
        }
    }

    /// Tests for `ForeignClient::new()`.
    #[test]
    fn foreign_client_create() {
        let a_cfg = get_basic_chain_config("chain_a");
        let b_cfg = get_basic_chain_config("chain_b");

        let (a_chain, _) = ChainRuntime::<MockChain>::spawn(a_cfg).unwrap();
        let (b_chain, _) = ChainRuntime::<MockChain>::spawn(b_cfg).unwrap();

        // Instantiate the foreign clients on the two chains.
        let res_client_on_a = ForeignClient::new(a_chain.clone(), b_chain.clone());
        assert!(
            res_client_on_a.is_ok(),
            "Client creation (on chain a) failed with error: {:?}",
            res_client_on_a
        );

        let client_on_a = res_client_on_a.unwrap();
        let a_client = client_on_a.id;

        let res_client_on_b = ForeignClient::new(b_chain.clone(), a_chain.clone());
        assert!(
            res_client_on_b.is_ok(),
            "Client creation (on chain a) failed with error: {:?}",
            res_client_on_b
        );
        let client_on_b = res_client_on_b.unwrap();
        let b_client = client_on_b.id;

        // Now that the clients exists, we should be able to query its state
        let b_client_state = b_chain.query_client_state(&b_client, Height::default());
        assert!(
            b_client_state.is_ok(),
            "Client query (on chain b) failed with error: {:?}",
            b_client_state
        );

        let a_client_state = a_chain.query_client_state(&a_client, Height::default());
        assert!(
            a_client_state.is_ok(),
            "Client query (on chain a) failed with error: {:?}",
            a_client_state
        );
    }

    /// Tests for `ForeignClient::update()`.
    #[test]
    fn foreign_client_update() {
        let a_cfg = get_basic_chain_config("chain_a");
        let b_cfg = get_basic_chain_config("chain_b");
        let mut _a_client_id = ClientId::from_str("client_on_a_forb").unwrap();
        let mut _b_client_id = ClientId::from_str("client_on_b_fora").unwrap();

        let (a_chain, _) = ChainRuntime::<MockChain>::spawn(a_cfg).unwrap();
        let (b_chain, _) = ChainRuntime::<MockChain>::spawn(b_cfg).unwrap();

        // Instantiate the foreign clients on the two chains.
        let client_on_a_res = ForeignClient::new(a_chain.clone(), b_chain.clone());
        assert!(
            client_on_a_res.is_ok(),
            "Client creation (on chain a) failed with error: {:?}",
            client_on_a_res
        );
        let client_on_a = client_on_a_res.unwrap();

        let client_on_b_res = ForeignClient::new(b_chain.clone(), a_chain.clone());
        assert!(
            client_on_b_res.is_ok(),
            "Client creation (on chain a) failed with error: {:?}",
            client_on_b_res
        );
        let client_on_b = client_on_b_res.unwrap();

        let num_iterations = 5;

        let mut b_height_start = b_chain.query_latest_height().unwrap();
        let mut a_height_start = a_chain.query_latest_height().unwrap();

        // Update each client
        for _i in 1..num_iterations {
            let res = client_on_a.update();
            assert!(res.is_ok(), "Client update for chain a failed {:?}", res);

            // Basic check that the height of the chain advanced
            let a_height_current = a_chain.query_latest_height().unwrap();
            a_height_start = a_height_start.increment();
            assert_eq!(
                a_height_start, a_height_current,
                "after client update, chain a did not advance"
            );

            let res = client_on_b.update();
            assert!(res.is_ok(), "Client update for chain b failed {:?}", res);

            // Basic check that the height of the chain advanced
            let b_height_current = b_chain.query_latest_height().unwrap();
            b_height_start = b_height_start.increment();
            assert_eq!(
                b_height_start, b_height_current,
                "after client update, chain b did not advance"
            );
        }
    }

    /// Tests for `ForeignClient::detect_misbehaviour_and_send_evidence( ? )`.
    #[test]
    fn foreign_client_detect_misbehaviour_and_send_evidence() {
        let a_cfg = get_basic_chain_config("chain_a");
        let b_cfg = get_basic_chain_config("chain_b");
        let mut a_client_id = ClientId::from_str("client_on_a_forb").unwrap();
        let mut _b_client_id = ClientId::from_str("client_on_b_fora").unwrap();

        let (a_chain, _) = ChainRuntime::<MockChain>::spawn(a_cfg).unwrap();
        let (b_chain, _) = ChainRuntime::<MockChain>::spawn(b_cfg).unwrap();

        // Instantiate the foreign clients on the two chains.
        let client_on_a_res = ForeignClient::new(a_chain.clone(), b_chain.clone());
        assert!(
            client_on_a_res.is_ok(),
            "Client creation (on chain a) failed with error: {:?}",
            client_on_a_res
        );
        let client_on_a = client_on_a_res.unwrap();

        //The chain advances.
        // thread::sleep(Duration::from_millis(10000));

        let client_on_b_res = ForeignClient::new(b_chain.clone(), a_chain.clone());
        assert!(
            client_on_b_res.is_ok(),
            "Client creation (on chain a) failed with error: {:?}",
            client_on_b_res
        );
        let client_on_b = client_on_b_res.unwrap();

        let mut b_height_start = b_chain.query_latest_height().unwrap();
        let mut a_height_start = a_chain.query_latest_height().unwrap();

        let mut a_height_current = Height::default();
        let mut b_height_current = Height::default();
        let num_iterations = 2;

        // Update each client
        for _i in 1..num_iterations {
            let res = client_on_a.update();
            assert!(res.is_ok(), "Client update for chain a failed {:?}", res);

            // Basic check that the height of the chain advanced
            a_height_current = a_chain.query_latest_height().unwrap();
            a_height_start = a_height_start.increment();
            assert_eq!(
                a_height_start, a_height_current,
                "after client update, chain a did not advance"
            );

            let res = client_on_b.update();
            assert!(res.is_ok(), "Client update for chain b failed {:?}", res);

            // Basic check that the height of the chain advanced
            b_height_current = b_chain.query_latest_height().unwrap();
            b_height_start = b_height_start.increment();
            assert_eq!(
                b_height_start, b_height_current,
                "after client update, chain b did not advance"
            );
        }

        let atribs = Attributes {
            height: a_height_current,
            client_id: a_client_id,
            client_type: ClientType::Mock,
            consensus_height: b_height_current,
        };

        let h = MockHeader {
            height: Height::new(40, 20),
            timestamp: 1,
        };

        let header_one = AnyHeader::Mock(h);

        let upd = UpdateClient {
            common: atribs,
            // header: None,
            header: Some(header_one),
        };

        let result = client_on_a.detect_misbehaviour_and_send_evidence(Some(upd));

        match result {
            Ok(e) => match e {
                None => println!(" No misbehavior event created  \n"),
                e => println!(" Misbehavior detected \n"),
            },
            Err(e) => println!("error: {:?}", e),
        }
    }

    /// Tests for `ForeignClient::detect_misbehaviour_and_send_evidence( ? )`.
    #[test]
    fn second_foreign_client_detect_misbehaviour_and_send_evidence() {
        let a_cfg = get_basic_chain_config("chain_a");
        let b_cfg = get_basic_chain_config("chain_b");
        let mut a_client_id = ClientId::from_str("client_on_a_forb").unwrap();
        let mut _b_client_id = ClientId::from_str("client_on_b_fora").unwrap();

        let (a_chain, _) = ChainRuntime::<MockChain>::spawn(a_cfg).unwrap();
        let (b_chain, _) = ChainRuntime::<MockChain>::spawn(b_cfg).unwrap();

        // Instantiate the foreign clients on the two chains.
        let client_on_a_res = ForeignClient::new(a_chain.clone(), b_chain.clone());
        assert!(
            client_on_a_res.is_ok(),
            "Client creation (on chain a) failed with error: {:?}",
            client_on_a_res
        );
        let client_on_a = client_on_a_res.unwrap();

        //The chain advances.
        // thread::sleep(Duration::from_millis(10000));

        let client_on_b_res = ForeignClient::new(b_chain.clone(), a_chain.clone());
        assert!(
            client_on_b_res.is_ok(),
            "Client creation (on chain a) failed with error: {:?}",
            client_on_b_res
        );
        let client_on_b = client_on_b_res.unwrap();

        let mut b_height_start = b_chain.query_latest_height().unwrap();
        let mut a_height_start = a_chain.query_latest_height().unwrap();

        let mut a_height_current = Height::default();
        let mut b_height_current = Height::default();
        let num_iterations = 2;

        // Update each client
        for _i in 1..num_iterations {
            let res = client_on_a.update();
            assert!(res.is_ok(), "Client update for chain a failed {:?}", res);

            // Basic check that the height of the chain advanced
            a_height_current = a_chain.query_latest_height().unwrap();
            a_height_start = a_height_start.increment();
            assert_eq!(
                a_height_start, a_height_current,
                "after client update, chain a did not advance"
            );

            let res = client_on_b.update();
            assert!(res.is_ok(), "Client update for chain b failed {:?}", res);

            // Basic check that the height of the chain advanced
            b_height_current = b_chain.query_latest_height().unwrap();
            b_height_start = b_height_start.increment();
            assert_eq!(
                b_height_start, b_height_current,
                "after client update, chain b did not advance"
            );
        }

        let atribs = Attributes {
            height: a_height_current,
            client_id: a_client_id,
            client_type: ClientType::Mock,
            consensus_height: b_height_current,
        };

        let _upd = UpdateClient {
            common: atribs,
            header: None,
        };
        let result = client_on_a.detect_misbehaviour_and_send_evidence(None);

        match result {
            Ok(e) => match e {
                None => println!(" No misbehavior event created  \n"),
                e => println!(" Misbehavior detected \n"),
            },
            Err(e) => println!("error: {:?}", e),
        }
    }
}
