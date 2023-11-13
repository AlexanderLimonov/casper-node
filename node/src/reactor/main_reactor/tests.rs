use std::{collections::BTreeMap, iter, sync::Arc, time::Duration};
use std::cmp::max;
use std::collections::BTreeSet;

use either::Either;
use num::Zero;
use num_rational::Ratio;
use rand::Rng;
use tempfile::TempDir;
use tokio::time;
use tracing::{error, info};

use casper_execution_engine::core::engine_state::{Error, GetBidsRequest, QueryRequest, QueryResult};
use casper_execution_engine::core::engine_state::QueryResult::{CircularReference, DepthLimit, Success, ValueNotFound};
use casper_storage::global_state::shared::CorrelationId;
use casper_types::{system::auction::{Bids, DelegationRate}, testing::TestRng, EraId, Motes, ProtocolVersion, PublicKey, SecretKey, TimeDiff, Timestamp, U512, ContractHash};
use casper_types::system::mint;

use crate::{components::{
    consensus::{
        self, ClContext, ConsensusMessage, HighwayMessage, HighwayVertex, NewBlockPayload,
    },
    gossiper, network, storage,
    upgrade_watcher::NextUpgrade,
}, effect::{
    incoming::ConsensusMessageIncoming,
    requests::{ContractRuntimeRequest, NetworkRequest},
    EffectExt,
}, protocol::Message, reactor::{
    main_reactor::{Config, MainEvent, MainReactor, ReactorState},
    Runner,
}, testing::{
    self, filter_reactor::FilterReactor, network::TestingNetwork, ConditionCheckReactor,
}, types::{
    chainspec::{AccountConfig, AccountsConfig, ValidatorConfig},
    ActivationPoint, BlockHeader, BlockPayload, Chainspec, ChainspecRawBytes, Deploy, ExitCode,
    NodeRng,
}, utils::{External, Loadable, Source, RESOURCES_PATH}, WithDir};
use crate::failpoints::FailpointActivation;
use crate::reactor::Reactor;
use crate::types::BlockWithMetadata;
use crate::types::chainspec::ConsensusProtocolName;

struct TestChain {
    // Keys that validator instances will use, can include duplicates
    keys: Vec<Arc<SecretKey>>,
    storages: Vec<TempDir>,
    chainspec: Arc<Chainspec>,
    chainspec_raw_bytes: Arc<ChainspecRawBytes>,
}

type Nodes = testing::network::Nodes<FilterReactor<MainReactor>>;

impl Runner<ConditionCheckReactor<FilterReactor<MainReactor>>> {
    fn main_reactor(&self) -> &MainReactor {
        self.reactor().inner().inner()
    }
}

impl TestChain {
    /// Instantiates a new test chain configuration.
    ///
    /// Generates secret keys for `size` validators and creates a matching chainspec.
    fn new(rng: &mut TestRng, size: usize, initial_stakes: Option<&[U512]>) -> Self {
        let keys: Vec<Arc<SecretKey>> = (0..size)
            .map(|_| Arc::new(SecretKey::random(rng)))
            .collect();

        let stake_values = if let Some(initial_stakes) = initial_stakes {
            assert_eq!(size, initial_stakes.len());
            initial_stakes.to_vec()
        } else {
            // By default we use very large stakes so we would catch overflow issues.
            std::iter::from_fn(|| Some(U512::from(rng.gen_range(100..999)) * U512::from(u128::MAX)))
                .take(size)
                .collect()
        };

        let stakes = keys
            .iter()
            .zip(stake_values)
            .map(|(secret_key, stake)| {
                let secret_key = secret_key.clone();
                (PublicKey::from(&*secret_key), stake)
            })
            .collect();
        Self::new_with_keys(keys, stakes)
    }

    /// Instantiates a new test chain configuration.
    ///
    /// Takes a vector of bonded keys with specified bond amounts.
    fn new_with_keys(
        keys: Vec<Arc<SecretKey>>,
        stakes: BTreeMap<PublicKey, U512>,
    ) -> Self {
        // Load the `local` chainspec.
        let (mut chainspec, chainspec_raw_bytes) =
            <(Chainspec, ChainspecRawBytes)>::from_resources("local");

        // Override accounts with those generated from the keys.
        // TODO: This needs more flexibility to define users and delegators
        let accounts = stakes
            .into_iter()
            .map(|(public_key, bonded_amount)| {
                let validator_config =
                    ValidatorConfig::new(Motes::new(bonded_amount), DelegationRate::zero());
                AccountConfig::new(
                    public_key,
                    Motes::new(U512::zero()),
                    Some(validator_config),
                )
            })
            .collect();
        let delegators = vec![];
        chainspec.network_config.accounts_config = AccountsConfig::new(accounts, delegators);

        // Make the genesis timestamp 60 seconds from now, to allow for all validators to start up.
        let genesis_time = Timestamp::now() + TimeDiff::from_seconds(60);
        info!(
            "creating test chain configuration, genesis: {}",
            genesis_time
        );
        chainspec.protocol_config.activation_point = ActivationPoint::Genesis(genesis_time);

        chainspec.core_config.minimum_era_height = 1;
        chainspec.core_config.finality_threshold_fraction = Ratio::new(34, 100);
        chainspec.core_config.era_duration = TimeDiff::from_millis(10);
        chainspec.core_config.auction_delay = 1;
        chainspec.core_config.unbonding_delay = 3;

        TestChain {
            keys,
            storages: Vec::new(),
            chainspec: Arc::new(chainspec),
            chainspec_raw_bytes: Arc::new(chainspec_raw_bytes),
        }
    }

    fn chainspec_mut(&mut self) -> &mut Chainspec {
        Arc::get_mut(&mut self.chainspec).unwrap()
    }

    /// Creates an initializer/validator configuration for the `idx`th validator.
    fn create_node_config(&mut self, idx: usize, first_node_port: u16) -> Config {
        // Set the network configuration.
        let mut cfg = Config {
            network: if idx == 0 {
                network::Config::default_local_net_first_node(first_node_port)
            } else {
                network::Config::default_local_net(first_node_port)
            },
            gossip: gossiper::Config::new_with_small_timeouts(),
            ..Default::default()
        };

        // Additionally set up storage in a temporary directory.
        let (storage_cfg, temp_dir) = storage::Config::default_for_tests();
        // ...and the secret key for our validator.
        {
            let secret_key_path = temp_dir.path().join("secret_key");
            self.keys[idx]
                .to_file(secret_key_path.clone())
                .expect("could not write secret key");
            cfg.consensus.secret_key_path = External::Path(secret_key_path);
        }
        self.storages.push(temp_dir);
        cfg.storage = storage_cfg;
        cfg
    }

    async fn create_initialized_network(
        &mut self,
        rng: &mut NodeRng,
    ) -> anyhow::Result<TestingNetwork<FilterReactor<MainReactor>>> {
        let root = RESOURCES_PATH.join("local");

        let mut network: TestingNetwork<FilterReactor<MainReactor>> = TestingNetwork::new();
        let first_node_port = testing::unused_port_on_localhost();

        for idx in 0..self.keys.len() {
            info!("creating node {}", idx);
            let cfg = self.create_node_config(idx, first_node_port);
            network
                .add_node_with_config_and_chainspec(
                    WithDir::new(root.clone(), cfg),
                    Arc::clone(&self.chainspec),
                    Arc::clone(&self.chainspec_raw_bytes),
                    rng,
                )
                .await
                .expect("could not add node to reactor");
        }

        Ok(network)
    }
}

/// Given an era number, returns a predicate to check if all of the nodes are in the specified era.
fn is_in_era(era_id: EraId) -> impl Fn(&Nodes) -> bool {
    move |nodes: &Nodes| {
        nodes
            .values()
            .all(|runner| runner.main_reactor().consensus().current_era() == Some(era_id))
    }
}

/// Given an era number, returns a predicate to check if all of the nodes have completed the
/// specified era.
fn has_completed_era(era_id: EraId) -> impl Fn(&Nodes) -> bool {
    move |nodes: &Nodes| {
        nodes.values().all(|runner| {
            runner
                .main_reactor()
                .storage()
                .read_highest_switch_block_headers(1)
                .unwrap()
                .last()
                .map_or(false, |header| header.era_id() == era_id)
        })
    }
}

fn is_ping(event: &MainEvent) -> bool {
    if let MainEvent::ConsensusMessageIncoming(ConsensusMessageIncoming { message, .. }) = event {
        if let ConsensusMessage::Protocol { ref payload, .. } = **message {
            return matches!(
                payload.deserialize_incoming::<HighwayMessage::<ClContext>>(),
                Ok(HighwayMessage::<ClContext>::NewVertex(HighwayVertex::Ping(
                    _
                )))
            );
        }
    }
    false
}

/// A set of consecutive switch blocks.
struct SwitchBlocks {
    headers: Vec<BlockHeader>,
}

impl SwitchBlocks {
    /// Collects all switch blocks of the first `era_count` eras, and asserts that they are equal
    /// in all nodes.
    fn collect(nodes: &Nodes, era_count: u64) -> SwitchBlocks {
        let mut headers = Vec::new();
        for era_number in 0..era_count {
            let mut header_iter = nodes.values().map(|runner| {
                let storage = runner.main_reactor().storage();
                let maybe_block = storage
                    .read_switch_block_header_by_era_id(era_number.into())
                    .expect("failed to get switch block header by era id");
                maybe_block.expect("missing switch block header")
            });
            let header = header_iter.next().unwrap();
            assert_eq!(era_number, header.era_id().value());
            for other_header in header_iter {
                assert_eq!(header, other_header);
            }
            headers.push(header);
        }
        SwitchBlocks { headers }
    }

    /// Returns the list of equivocators in the given era.
    fn equivocators(&self, era_number: u64) -> &[PublicKey] {
        &self.headers[era_number as usize]
            .era_end()
            .expect("era end")
            .era_report()
            .equivocators
    }

    /// Returns the list of inactive validators in the given era.
    fn inactive_validators(&self, era_number: u64) -> &[PublicKey] {
        &self.headers[era_number as usize]
            .era_end()
            .expect("era end")
            .era_report()
            .inactive_validators
    }

    /// Returns the list of validators in the successor era.
    fn next_era_validators(&self, era_number: u64) -> &BTreeMap<PublicKey, U512> {
        self.headers[era_number as usize]
            .next_era_validator_weights()
            .expect("validators")
    }

    /// Returns the set of bids in the auction contract at the end of the given era.
    fn bids(&self, nodes: &Nodes, era_number: u64) -> Bids {
        let correlation_id = Default::default();
        let state_root_hash = *self.headers[era_number as usize].state_root_hash();
        for runner in nodes.values() {
            let request = GetBidsRequest::new(state_root_hash);
            let engine_state = runner.main_reactor().contract_runtime().engine_state();
            let bids_result = engine_state
                .get_bids(correlation_id, request)
                .expect("get_bids failed");
            if let Some(bids) = bids_result.into_success() {
                return bids;
            }
        }
        unreachable!("at least one node should have bids for era {}", era_number);
    }
}

#[tokio::test]
async fn run_network() {
    testing::init_logging();

    let mut rng = crate::new_rng();

    // Instantiate a new chain with a fixed size.
    const NETWORK_SIZE: usize = 5;
    let mut chain = TestChain::new(&mut rng, NETWORK_SIZE, None);

    let mut net = chain
        .create_initialized_network(&mut rng)
        .await
        .expect("network initialization failed");

    // Wait for all nodes to agree on one era.
    net.settle_on(
        &mut rng,
        is_in_era(EraId::from(1)),
        Duration::from_secs(1000),
    )
    .await;

    net.settle_on(
        &mut rng,
        is_in_era(EraId::from(2)),
        Duration::from_secs(1001),
    )
    .await;
}

#[tokio::test]
async fn run_equivocator_network() {
    testing::init_logging();

    let mut rng = crate::new_rng();

    let alice_secret_key = Arc::new(SecretKey::random(&mut rng));
    let alice_public_key = PublicKey::from(&*alice_secret_key);
    let bob_secret_key = Arc::new(SecretKey::random(&mut rng));
    let bob_public_key = PublicKey::from(&*bob_secret_key);

    let size: usize = 3;
    // Leave two free slots for Alice and Bob.
    let mut keys: Vec<Arc<SecretKey>> = (2..size)
        .map(|_| Arc::new(SecretKey::random(&mut rng)))
        .collect();
    let mut stakes: BTreeMap<PublicKey, U512> = keys
        .iter()
        .map(|secret_key| (PublicKey::from(&*secret_key.clone()), U512::from(100000u64)))
        .collect();
    stakes.insert(PublicKey::from(&*alice_secret_key), U512::from(1));
    stakes.insert(PublicKey::from(&*bob_secret_key), U512::from(1));

    // Here's where things go wrong: Bob doesn't run a node at all, and Alice runs two!
    keys.push(alice_secret_key.clone());
    keys.push(alice_secret_key);

    // We configure the era to take ten rounds. That should guarantee that the two nodes
    // equivocate.
    let mut chain = TestChain::new_with_keys(keys, stakes.clone());
    chain.chainspec_mut().core_config.minimum_era_height = 10;
    chain.chainspec_mut().highway_config.maximum_round_length =
        chain.chainspec.core_config.minimum_block_time * 2;
    chain.chainspec_mut().core_config.validator_slots = size as u32;

    let mut net = chain
        .create_initialized_network(&mut rng)
        .await
        .expect("network initialization failed");
    let min_round_len = chain.chainspec.core_config.minimum_block_time;
    let mut maybe_first_message_time = None;

    let mut alice_reactors = net
        .reactors_mut()
        .filter(|reactor| *reactor.inner().consensus().public_key() == alice_public_key);

    // Delay all messages to and from the first of Alice's nodes until three rounds after the first
    // message.  Further, significantly delay any incoming pings to avoid the node detecting the
    // doppelganger and deactivating itself.
    alice_reactors.next().unwrap().set_filter(move |event| {
        if is_ping(&event) {
            return Either::Left(time::sleep((min_round_len * 30).into()).event(move |_| event));
        }
        let now = Timestamp::now();
        match &event {
            MainEvent::ConsensusMessageIncoming(_) => {}
            MainEvent::NetworkRequest(
                NetworkRequest::SendMessage { payload, .. }
                | NetworkRequest::ValidatorBroadcast { payload, .. }
                | NetworkRequest::Gossip { payload, .. },
            ) if matches!(**payload, Message::Consensus(_)) => {}
            _ => return Either::Right(event),
        };
        let first_message_time = *maybe_first_message_time.get_or_insert(now);
        if now < first_message_time + min_round_len * 3 {
            return Either::Left(time::sleep(min_round_len.into()).event(move |_| event));
        }
        Either::Right(event)
    });

    // Significantly delay all incoming pings to the second of Alice's nodes.
    alice_reactors.next().unwrap().set_filter(move |event| {
        if is_ping(&event) {
            return Either::Left(time::sleep((min_round_len * 30).into()).event(move |_| event));
        }
        Either::Right(event)
    });

    drop(alice_reactors);

    let era_count = 4;

    let timeout = Duration::from_secs(90 * era_count);
    info!("Waiting for {} eras to end.", era_count);
    net.settle_on(
        &mut rng,
        has_completed_era(EraId::new(era_count - 1)),
        timeout,
    )
    .await;
    let switch_blocks = SwitchBlocks::collect(net.nodes(), era_count);
    let bids: Vec<Bids> = (0..era_count)
        .map(|era_number| switch_blocks.bids(net.nodes(), era_number))
        .collect();

    // Since this setup sometimes produces no equivocation or an equivocation in era 2 rather than
    // era 1, we set an offset here.  If neither eras has an equivocation, exit early.
    // TODO: Remove this once https://github.com/casper-network/casper-node/issues/1859 is fixed.
    for switch_block in &switch_blocks.headers {
        let era_id = switch_block.era_id();
        let count = switch_blocks.equivocators(era_id.value()).len();
        info!("equivocators in {}: {}", era_id, count);
    }
    let offset = if !switch_blocks.equivocators(1).is_empty() {
        0
    } else if !switch_blocks.equivocators(2).is_empty() {
        error!("failed to equivocate in era 1 - asserting equivocation detected in era 2");
        1
    } else {
        error!("failed to equivocate in era 1");
        return;
    };

    // Era 0 consists only of the genesis block.
    // In era 1, Alice equivocates. Since eviction takes place with a delay of one
    // (`auction_delay`) era, she is still included in the next era's validator set.
    assert_eq!(
        switch_blocks.equivocators(1 + offset),
        [alice_public_key.clone()]
    );
    assert!(bids[1 + offset as usize][&alice_public_key].inactive());
    assert!(switch_blocks
        .next_era_validators(1 + offset)
        .contains_key(&alice_public_key));

    // In era 2 Alice is banned. Banned validators count neither as faulty nor inactive, even
    // though they cannot participate. In the next era, she will be evicted.
    assert_eq!(switch_blocks.equivocators(2 + offset), []);
    assert!(bids[2 + offset as usize][&alice_public_key].inactive());
    assert!(!switch_blocks
        .next_era_validators(2 + offset)
        .contains_key(&alice_public_key));

    // In era 3 she is not a validator anymore and her bid remains deactivated.
    if offset == 0 {
        assert_eq!(switch_blocks.equivocators(3), []);
        assert!(bids[3][&alice_public_key].inactive());
        assert!(!switch_blocks
            .next_era_validators(3)
            .contains_key(&alice_public_key));
    }

    // Bob is inactive.
    assert_eq!(
        switch_blocks.inactive_validators(1),
        [bob_public_key.clone()]
    );
    assert_eq!(
        switch_blocks.inactive_validators(2),
        [bob_public_key.clone()]
    );

    // We don't slash, so the stakes are never reduced.
    for (public_key, stake) in &stakes {
        assert!(bids[0][public_key].staked_amount() >= stake);
        assert!(bids[1][public_key].staked_amount() >= stake);
        assert!(bids[2][public_key].staked_amount() >= stake);
        assert!(bids[3][public_key].staked_amount() >= stake);
    }
}

async fn assert_network_shutdown_for_upgrade_with_stakes(rng: &mut TestRng, stakes: &[U512]) {
    const NETWORK_SIZE: usize = 2;
    const INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(20);

    let mut chain = TestChain::new(rng, NETWORK_SIZE, Some(stakes));
    chain.chainspec_mut().core_config.minimum_era_height = 2;
    chain.chainspec_mut().core_config.era_duration = TimeDiff::from_millis(0);
    chain.chainspec_mut().core_config.minimum_block_time = "1second".parse().unwrap();

    let mut net = chain
        .create_initialized_network(rng)
        .await
        .expect("network initialization failed");

    // Wait until initialization is finished, so upgrade watcher won't reject test requests.
    net.settle_on(
        rng,
        move |nodes: &Nodes| {
            nodes
                .values()
                .all(|runner| !matches!(runner.main_reactor().state, ReactorState::Initialize))
        },
        INITIALIZATION_TIMEOUT,
    )
    .await;

    // An upgrade is scheduled for era 2, after the switch block in era 1 (height 2).
    for runner in net.runners_mut() {
        runner
            .process_injected_effects(|effect_builder| {
                let upgrade = NextUpgrade::new(
                    ActivationPoint::EraId(2.into()),
                    ProtocolVersion::from_parts(999, 0, 0),
                );
                effect_builder
                    .announce_upgrade_activation_point_read(upgrade)
                    .ignore()
            })
            .await;
    }

    // Run until the nodes shut down for the upgrade.
    let timeout = Duration::from_secs(90);
    net.settle_on_exit(rng, ExitCode::Success, timeout).await;
}

#[tokio::test]
async fn nodes_should_have_enough_signatures_before_upgrade_with_equal_stake() {
    // Equal stake ensures that one node was able to learn about signatures created by the other, by
    // whatever means necessary (gossiping, broadcasting, fetching, etc.).
    testing::init_logging();

    let mut rng = crate::new_rng();

    let stakes = [U512::from(u128::MAX), U512::from(u128::MAX)];
    assert_network_shutdown_for_upgrade_with_stakes(&mut rng, &stakes).await;
}

#[tokio::test]
async fn nodes_should_have_enough_signatures_before_upgrade_with_one_dominant_stake() {
    testing::init_logging();

    let mut rng = crate::new_rng();

    let stakes = [U512::from(u128::MAX), U512::from(u8::MAX)];
    assert_network_shutdown_for_upgrade_with_stakes(&mut rng, &stakes).await;
}

#[tokio::test]
async fn dont_upgrade_without_switch_block() {
    testing::init_logging();

    let mut rng = crate::new_rng();

    eprintln!(
        "Running 'dont_upgrade_without_switch_block' test with rng={}",
        rng
    );

    const NETWORK_SIZE: usize = 2;
    const INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(20);

    let mut chain = TestChain::new(&mut rng, NETWORK_SIZE, None);
    chain.chainspec_mut().core_config.minimum_era_height = 2;
    chain.chainspec_mut().core_config.era_duration = TimeDiff::from_millis(0);
    chain.chainspec_mut().core_config.minimum_block_time = "1second".parse().unwrap();

    let mut net = chain
        .create_initialized_network(&mut rng)
        .await
        .expect("network initialization failed");

    // Wait until initialization is finished, so upgrade watcher won't reject test requests.
    net.settle_on(
        &mut rng,
        move |nodes: &Nodes| {
            nodes
                .values()
                .all(|runner| !matches!(runner.main_reactor().state, ReactorState::Initialize))
        },
        INITIALIZATION_TIMEOUT,
    )
    .await;

    // An upgrade is scheduled for era 2, after the switch block in era 1 (height 2).
    // We artificially delay the execution of that block.
    for runner in net.runners_mut() {
        runner
            .process_injected_effects(|effect_builder| {
                let upgrade = NextUpgrade::new(
                    ActivationPoint::EraId(2.into()),
                    ProtocolVersion::from_parts(999, 0, 0),
                );
                effect_builder
                    .announce_upgrade_activation_point_read(upgrade)
                    .ignore()
            })
            .await;
        let mut exec_request_received = false;
        runner.reactor_mut().inner_mut().set_filter(move |event| {
            if let MainEvent::ContractRuntimeRequest(
                ContractRuntimeRequest::EnqueueBlockForExecution {
                    finalized_block, ..
                },
            ) = &event
            {
                if finalized_block.era_report().is_some()
                    && finalized_block.era_id() == EraId::from(1)
                    && !exec_request_received
                {
                    info!("delaying {}", finalized_block);
                    exec_request_received = true;
                    return Either::Left(
                        time::sleep(Duration::from_secs(10)).event(move |_| event),
                    );
                }
                info!("not delaying {}", finalized_block);
            }
            Either::Right(event)
        });
    }

    // Run until the nodes shut down for the upgrade.
    let timeout = Duration::from_secs(90);
    net.settle_on_exit(&mut rng, ExitCode::Success, timeout)
        .await;

    // Verify that the switch block has been stored: Even though it was delayed the node didn't
    // restart before executing and storing it.
    for runner in net.nodes().values() {
        let header = runner
            .main_reactor()
            .storage()
            .read_block_by_height(2)
            .expect("failed to read from storage")
            .expect("missing switch block")
            .take_header();
        assert_eq!(EraId::from(1), header.era_id(), "era should be 1");
        assert!(header.is_switch_block(), "header should be switch block");
    }
}

#[tokio::test]
async fn should_store_finalized_approvals() {
    testing::init_logging();

    let mut rng = crate::new_rng();

    // Set up a network with two nodes.
    let alice_secret_key = Arc::new(SecretKey::random(&mut rng));
    let alice_public_key = PublicKey::from(&*alice_secret_key);
    let bob_secret_key = Arc::new(SecretKey::random(&mut rng));
    let charlie_secret_key = Arc::new(SecretKey::random(&mut rng)); // just for ordering testing purposes
    let keys: Vec<Arc<SecretKey>> = vec![alice_secret_key.clone(), bob_secret_key.clone()];
    // only Alice will be proposing blocks
    let stakes: BTreeMap<PublicKey, U512> =
        iter::once((alice_public_key.clone(), U512::from(100))).collect();

    // Eras have exactly two blocks each, and there is one block per second.
    let mut chain = TestChain::new_with_keys(keys, stakes.clone());
    chain.chainspec_mut().core_config.minimum_era_height = 2;
    chain.chainspec_mut().core_config.era_duration = TimeDiff::from_millis(0);
    chain.chainspec_mut().core_config.minimum_block_time = "1second".parse().unwrap();
    chain.chainspec_mut().core_config.validator_slots = 1;

    let mut net = chain
        .create_initialized_network(&mut rng)
        .await
        .expect("network initialization failed");

    // Wait for all nodes to complete era 0.
    net.settle_on(
        &mut rng,
        has_completed_era(EraId::from(0)),
        Duration::from_secs(90),
    )
    .await;

    // Submit a deploy.
    let mut deploy_alice_bob = Deploy::random_valid_native_transfer_without_deps(&mut rng);
    let mut deploy_alice_bob_charlie = deploy_alice_bob.clone();
    let mut deploy_bob_alice = deploy_alice_bob.clone();

    deploy_alice_bob.sign(&alice_secret_key);
    deploy_alice_bob.sign(&bob_secret_key);

    deploy_alice_bob_charlie.sign(&alice_secret_key);
    deploy_alice_bob_charlie.sign(&bob_secret_key);
    deploy_alice_bob_charlie.sign(&charlie_secret_key);

    deploy_bob_alice.sign(&bob_secret_key);
    deploy_bob_alice.sign(&alice_secret_key);

    // We will be testing the correct sequence of approvals against the deploy signed by Bob and
    // Alice.
    // The deploy signed by Alice and Bob should give the same ordering of approvals.
    let expected_approvals: Vec<_> = deploy_bob_alice.approvals().iter().cloned().collect();

    // We'll give the deploy signed by Alice, Bob and Charlie to Bob, so these will be his original
    // approvals. Save these for checks later.
    let bobs_original_approvals: Vec<_> = deploy_alice_bob_charlie
        .approvals()
        .iter()
        .cloned()
        .collect();
    assert_ne!(bobs_original_approvals, expected_approvals);

    let deploy_hash = *deploy_alice_bob.deploy_or_transfer_hash().deploy_hash();

    for runner in net.runners_mut() {
        if runner.main_reactor().consensus().public_key() == &alice_public_key {
            // Alice will propose the deploy signed by Alice and Bob.
            runner
                .process_injected_effects(|effect_builder| {
                    effect_builder
                        .put_deploy_to_storage(Box::new(deploy_alice_bob.clone()))
                        .ignore()
                })
                .await;
            runner
                .process_injected_effects(|effect_builder| {
                    effect_builder
                        .announce_new_deploy_accepted(
                            Box::new(deploy_alice_bob.clone()),
                            Source::Client,
                        )
                        .ignore()
                })
                .await;
        } else {
            // Bob will receive the deploy signed by Alice, Bob and Charlie.
            runner
                .process_injected_effects(|effect_builder| {
                    effect_builder
                        .put_deploy_to_storage(Box::new(deploy_alice_bob_charlie.clone()))
                        .ignore()
                })
                .await;
            runner
                .process_injected_effects(|effect_builder| {
                    effect_builder
                        .announce_new_deploy_accepted(
                            Box::new(deploy_alice_bob_charlie.clone()),
                            Source::Client,
                        )
                        .ignore()
                })
                .await;
        }
    }

    // Run until the deploy gets executed.
    let timeout = Duration::from_secs(90);
    net.settle_on(
        &mut rng,
        |nodes| {
            nodes.values().all(|runner| {
                runner
                    .main_reactor()
                    .storage()
                    .get_deploy_metadata_by_hash(&deploy_hash)
                    .is_some()
            })
        },
        timeout,
    )
    .await;

    // Check if the approvals agree.
    for runner in net.nodes().values() {
        let maybe_dwa = runner
            .main_reactor()
            .storage()
            .get_deploy_with_finalized_approvals_by_hash(&deploy_hash);
        let maybe_finalized_approvals = maybe_dwa
            .as_ref()
            .and_then(|dwa| dwa.finalized_approvals())
            .map(|fa| fa.inner().iter().cloned().collect());
        let maybe_original_approvals = maybe_dwa
            .as_ref()
            .map(|dwa| dwa.original_approvals().iter().cloned().collect());
        if runner.main_reactor().consensus().public_key() != &alice_public_key {
            // Bob should have finalized approvals, and his original approvals should be different.
            assert_eq!(
                maybe_finalized_approvals.as_ref(),
                Some(&expected_approvals)
            );
            assert_eq!(
                maybe_original_approvals.as_ref(),
                Some(&bobs_original_approvals)
            );
        } else {
            // Alice should only have the correct approvals as the original ones, and no finalized
            // approvals (as they wouldn't be stored, because they would be the same as the
            // original ones).
            assert_eq!(maybe_finalized_approvals.as_ref(), None);
            assert_eq!(maybe_original_approvals.as_ref(), Some(&expected_approvals));
        }
    }
}

// This test exercises a scenario in which a proposed block contains invalid accusations.
// Blocks containing no deploys or transfers used to be incorrectly marked as not needing
// validation even if they contained accusations, which opened up a security hole through which a
// malicious validator could accuse whomever they wanted of equivocating and have these
// accusations accepted by the other validators. This has been patched and the test asserts that
// such a scenario is no longer possible.
#[tokio::test]
async fn empty_block_validation_regression() {
    testing::init_logging();

    let mut rng = crate::new_rng();

    let size: usize = 4;
    let keys: Vec<Arc<SecretKey>> = (0..size)
        .map(|_| Arc::new(SecretKey::random(&mut rng)))
        .collect();
    let stakes: BTreeMap<PublicKey, U512> = keys
        .iter()
        .map(|secret_key| (PublicKey::from(&*secret_key.clone()), U512::from(100u64)))
        .collect();

    // We make the first validator always accuse everyone else.
    let mut chain = TestChain::new_with_keys(keys, stakes.clone());
    chain.chainspec_mut().core_config.minimum_block_time = "1second".parse().unwrap();
    chain.chainspec_mut().highway_config.maximum_round_length = "1second".parse().unwrap();
    chain.chainspec_mut().core_config.minimum_era_height = 15;
    let mut net = chain
        .create_initialized_network(&mut rng)
        .await
        .expect("network initialization failed");
    let malicious_validator = stakes.keys().next().unwrap().clone();
    info!("Malicious validator: {:?}", malicious_validator);
    let everyone_else: Vec<_> = stakes
        .keys()
        .filter(|pub_key| **pub_key != malicious_validator)
        .cloned()
        .collect();
    let malicious_runner = net
        .runners_mut()
        .find(|runner| runner.main_reactor().consensus().public_key() == &malicious_validator)
        .unwrap();
    malicious_runner
        .reactor_mut()
        .inner_mut()
        .set_filter(move |event| match event {
            MainEvent::Consensus(consensus::Event::NewBlockPayload(NewBlockPayload {
                era_id,
                block_payload: _,
                block_context,
            })) => {
                info!("Accusing everyone else!");
                // We hook into the NewBlockPayload event to replace the block being proposed with
                // an empty one that accuses all the validators, except the malicious validator.
                Either::Right(MainEvent::Consensus(consensus::Event::NewBlockPayload(
                    NewBlockPayload {
                        era_id,
                        block_payload: Arc::new(BlockPayload::new(
                            vec![],
                            vec![],
                            everyone_else.clone(),
                            Default::default(),
                            false,
                        )),
                        block_context,
                    },
                )))
            }
            event => Either::Right(event),
        });

    let timeout = Duration::from_secs(300);
    info!("Waiting for the first era after genesis to end.");
    net.settle_on(&mut rng, is_in_era(EraId::new(2)), timeout)
        .await;
    let switch_blocks = SwitchBlocks::collect(net.nodes(), 2);

    // Nobody actually double-signed. The accusations should have had no effect.
    assert_eq!(
        switch_blocks.equivocators(0),
        [],
        "expected no equivocators"
    );
    // If the malicious validator was the first proposer, all their Highway units might be invalid,
    // because they all refer to the invalid proposal, so they might get flagged as inactive. No
    // other validators should be considered inactive.
    match switch_blocks.inactive_validators(0) {
        [] => {}
        [inactive_validator] if malicious_validator == *inactive_validator => {}
        inactive => panic!("unexpected inactive validators: {:?}", inactive),
    }
}

/*
        let sender_public_key = filtered_node.main_reactor().consensus().public_key();

        let filter_closure =
            |event| match &event {
                /*
                MainEvent::Network(inner_event) =>
                    {
                        info!{"\n=========GENERIC NETWORK========\nMESSAGE {}", inner_event}
                        Either::Right(event)
                    }*/
                // If we were about to broadcast a finality signature we just created, do nothing instead
                MainEvent::NetworkRequest(
                    NetworkRequest::SendMessage { payload, .. }
                    | NetworkRequest::ValidatorBroadcast { payload, .. }
                    | NetworkRequest::Gossip { payload, .. },
                ) => {
                    info!{"\n=========NETWORK REQUEST========\nPAYLOAD {}", &payload}
                    match &**payload {
                        Message::FinalitySignature(..)
                        | Message::FinalitySignatureGossiper(..) /*if matches!(&inner.public_key, sender_public_key)*/ => {
                            info! {"\n=========WAS ABOUT TO COMMUNICATE A FINALITY SIGNATURE========\nEVENT {}\nPAYLOAD {}", event, payload}
                            Either::Left(Effects::new())
                        }
                        _ => Either::Right(event),
                    }
                }
                _ => Either::Right(event),
            };
        */
//filtered_node.reactor_mut().inner_mut().set_filter(filter_closure);

#[tokio::test]
#[cfg_attr(not(feature = "failpoints"), ignore)]
async fn basic_simple_rewards_test() {
    testing::init_logging();

    // Constants to "parametrize" the test
    const CONSENSUS: ConsensusProtocolName = ConsensusProtocolName::Zug;
    const VALIDATOR_SLOTS: u32 = 10;
    const NETWORK_SIZE: u64 = 10;
    const STAKE: u64 = 1000000000;
    const ERA_COUNT: u64 = 3;
    const ERA_DURATION: u64 = 30000; //milliseconds
    const MIN_HEIGHT: u64 = 10;
    const BLOCK_TIME: u64 = 3000; //milliseconds
    const TIME_OUT: u64 = 3000; //seconds
    const SEIGNIORAGE: (u64, u64) = (1u64, 100u64);
    const FINDERS_FEE: (u64, u64) = (0u64, 1u64);
    const FINALITY_SIG_PROP: (u64, u64) = (1u64, 1u64);
    const REPRESENTATIVE_NODE_INDEX: usize = 0;
    const FILTERED_NODES_INDICES: &'static [usize] = &[3, 4];

    // SETUP
    // TODO: Consider fixing the seed
    let mut rng = crate::new_rng();

    // Create random keypairs to populate our network
    let keys: Vec<Arc<SecretKey>> = (1..NETWORK_SIZE + 1)
        .map(|_| Arc::new(SecretKey::random(&mut rng)))
        .collect();
    let stakes: BTreeMap<PublicKey, U512> = keys
        .iter()
        .map(|secret_key| (PublicKey::from(&*secret_key.clone()), U512::from(STAKE)))
        .collect();

    // Instantiate the chain
    let mut chain = TestChain::new_with_keys(keys, stakes.clone());

    chain.chainspec_mut().core_config.validator_slots = VALIDATOR_SLOTS;
    chain.chainspec_mut().core_config.era_duration = TimeDiff::from_millis(ERA_DURATION);
    chain.chainspec_mut().core_config.minimum_era_height = MIN_HEIGHT;
    chain.chainspec_mut().core_config.minimum_block_time = TimeDiff::from_millis(BLOCK_TIME);
    chain.chainspec_mut().core_config.round_seigniorage_rate = Ratio::from(SEIGNIORAGE);
    chain.chainspec_mut().core_config.consensus_protocol = CONSENSUS;
    chain.chainspec_mut().core_config.finders_fee = Ratio::from(FINDERS_FEE);
    chain.chainspec_mut().core_config.finality_signature_proportion = Ratio::from(FINALITY_SIG_PROP);

    let mut net = chain
        .create_initialized_network(&mut rng)
        .await
        .expect("network initialization failed");
/*
    let bad_node = net.runners_mut().nth(0).unwrap();
    let sender_public_key = bad_node.main_reactor().consensus().public_key();

    let filter_closure_2 =
        |event| match &event {
            // If we were about to broadcast a finality signature we just created, do nothing instead
            MainEvent::NetworkRequest(
                NetworkRequest::SendMessage { payload, .. }
                | NetworkRequest::ValidatorBroadcast { payload, .. }
                | NetworkRequest::Gossip { payload, .. },
            ) =>
                match &**payload {
                    Message::FinalitySignature(inner) if matches!(&inner.public_key, sender_public_key) => {
                        info!{"\n=========WAS ABOUT TO COMMUNICATE A FINALITY SIGNATURE========\nEVENT {}\nPAYLOAD {}", event, payload}
                        Either::Left(Effects::new())}
                    _ => Either::Right(event),
                }
            _ => Either::Right(event),
        };

    // Set the bad node to forget signing blocks
    bad_node
    .reactor_mut()
    .inner_mut()
    .set_filter(filter_closure_2);
*/
    for i in FILTERED_NODES_INDICES {
        let filtered_node = net.runners_mut().nth(*i).unwrap();

        filtered_node.reactor_mut().inner_mut().activate_failpoint(&FailpointActivation::new("finality_signature_creation"));
    }

    // Run the network for a specified number of eras
    // TODO: Consider replacing era duration estimate with actual chainspec value
    let timeout = Duration::from_secs(TIME_OUT);
    net.settle_on(
        &mut rng,
        has_completed_era(EraId::new(ERA_COUNT - 1)),
        timeout)
        .await;

    // DATA COLLECTION
    // Get the switch blocks and bid structs first
    let switch_blocks = SwitchBlocks::collect(net.nodes(), ERA_COUNT);
    let bids: Vec<Bids> = (0..ERA_COUNT)
        .map(|era_number| switch_blocks.bids(net.nodes(), era_number))
        .collect();

    // Representative node
    // (this test should normally run a network at nominal performance with identical nodes)
    let representative_node = net.nodes().values().nth(REPRESENTATIVE_NODE_INDEX).unwrap();
    let representative_storage = &representative_node.main_reactor().storage;
    let representative_runtime = &representative_node.main_reactor().contract_runtime;

    // Recover highest completed block height
    let highest_completed_height = representative_storage
        .highest_complete_block_height()
        .expect("missing highest completed block");

    // Get all the blocks
    let blocks: Vec<BlockWithMetadata> =
        (0..highest_completed_height + 1).map(
            |i| representative_storage.read_block_and_metadata_by_height(i).expect("block not found").unwrap()
        ).collect();

    // Recover history of total supply
    let mint_hash: ContractHash = {
        let any_state_hash = *switch_blocks.headers[0].state_root_hash();
        representative_runtime
            .engine_state()
            .get_system_mint_hash(CorrelationId::new(), any_state_hash)
            .expect("mint contract hash not found")
    };

    // Get total supply history
    let total_supply: Vec<U512> = (0..highest_completed_height + 1)
        .map(|height: u64| {
            let state_hash = *representative_storage
                .read_block_header_by_height(height, true)
                .expect("failure to read block header")
                .unwrap()
                .state_root_hash();

            let request = QueryRequest::new(
                state_hash.clone(),
                mint_hash.into(),
                vec![mint::TOTAL_SUPPLY_KEY.to_owned()],
            );

            representative_runtime
                .engine_state()
                .run_query(CorrelationId::new(), request)
                .and_then(move |query_result| match query_result {
                    Success { value, proofs: _ } => value
                        .as_cl_value()
                        .ok_or_else(|| Error::Mint("Value not a CLValue".to_owned()))?
                        .clone()
                        .into_t::<U512>()
                        .map_err(|e| Error::Mint(format!("CLValue not a U512: {e}"))),
                    ValueNotFound(s) => Err(Error::Mint(format!("ValueNotFound({s})"))),
                    CircularReference(s) => Err(Error::Mint(format!("CircularReference({s})"))),
                    DepthLimit { depth } => Err(Error::Mint(format!("DepthLimit({depth})"))),
                    QueryResult::RootNotFound => Err(Error::RootNotFound(state_hash)),
                })
                .expect("failure to recover total supply")
        })
        .collect();

    // Verify that it "works"
    // TODO: Make this more interesting
    for entry in &bids[ERA_COUNT as usize - 1] {
        let (_, bid) = entry;
        assert!(bid.staked_amount() > &U512::from(STAKE), "expected an increase in stakes");
    }

}

#[tokio::test]
#[cfg_attr(not(feature = "failpoints"), ignore)]
async fn basic_parametrized_rewards_test() {
    // Constants to "parametrize" the test
    const CONSENSUS: ConsensusProtocolName = ConsensusProtocolName::Zug;
    const VALIDATOR_SLOTS: u32 = 10;
    const NETWORK_SIZE: u64 = 10;
    const STAKE: u64 = 1000000000;
    const ERA_COUNT: u64 = 3;
    const ERA_DURATION: u64 = 30000; //milliseconds
    const MIN_HEIGHT: u64 = 10;
    const BLOCK_TIME: u64 = 3000; //milliseconds
    const TIME_OUT: u64 = 3000; //seconds
    const SEIGNIORAGE: (u64, u64) = (1u64, 100u64);
    const FINDERS_FEE: (u64, u64) = (0u64, 1u64);
    const FINALITY_SIG_PROP: (u64, u64) = (1u64, 1u64);
    const REPRESENTATIVE_NODE_INDEX: usize = 0;
    const FILTERED_NODES_INDICES: &'static [usize] = &[3, 4];

    async fn run_rewards_network_scenario(
        consensus: ConsensusProtocolName,
        initial_stakes: &[u64],
        era_count: u64,
        era_duration: u64, //milliseconds
        min_height: u64,
        block_time: u64, //milliseconds
        time_out: u64, //seconds
        seigniorage: (u64, u64),
        finders_fee: (u64, u64),
        finality_sig_prop: (u64, u64),
        representative_node_index: usize,
        filtered_nodes_indices: &[usize]) {

        testing::init_logging();

        let total_initial_stake = &initial_stakes
            .iter()
            .fold(0u64, move |acc, s| acc + s);

        // SETUP
        // TODO: Consider fixing the seed
        let mut rng = crate::new_rng();

        // Create random keypairs to populate our network
        let keys: Vec<Arc<SecretKey>> = (1..&initial_stakes.len() + 1)
            .map(|_| Arc::new(SecretKey::random(&mut rng)))
            .collect();
        let stakes: BTreeMap<PublicKey, U512> = keys
            .iter()
            .enumerate()
            .map(|(i, secret_key)| (PublicKey::from(&*secret_key.clone()), U512::from(*&initial_stakes[i])))
            .collect();

        // Instantiate the chain
        let mut chain = TestChain::new_with_keys(keys, stakes.clone());

        chain.chainspec_mut().core_config.validator_slots = *&stakes.len() as u32;
        chain.chainspec_mut().core_config.era_duration = TimeDiff::from_millis(era_duration);
        chain.chainspec_mut().core_config.minimum_era_height = min_height;
        chain.chainspec_mut().core_config.minimum_block_time = TimeDiff::from_millis(block_time);
        chain.chainspec_mut().core_config.round_seigniorage_rate = Ratio::from(seigniorage);
        chain.chainspec_mut().core_config.consensus_protocol = consensus;
        chain.chainspec_mut().core_config.finders_fee = Ratio::from(finders_fee);
        chain.chainspec_mut().core_config.finality_signature_proportion = Ratio::from(finality_sig_prop);

        let mut net = chain
            .create_initialized_network(&mut rng)
            .await
            .expect("network initialization failed");

        for i in filtered_nodes_indices {
            let filtered_node = net.runners_mut().nth(*i).unwrap();
            filtered_node
                .reactor_mut()
                .inner_mut()
                .activate_failpoint(&FailpointActivation::new("finality_signature_creation"));
        }

        // Run the network for a specified number of eras
        // TODO: Consider replacing era duration estimate with actual chainspec value
        let timeout = Duration::from_secs(time_out);
        net.settle_on(
            &mut rng,
            has_completed_era(EraId::new(era_count - 1)),
            timeout)
            .await;

        // DATA COLLECTION
        // Get the switch blocks and bid structs first
        let switch_blocks = SwitchBlocks::collect(net.nodes(), era_count);
        let bids: Vec<Bids> = (0..era_count)
            .map(|era_number| switch_blocks.bids(net.nodes(), era_number))
            .collect();

        // Representative node
        // (this test should normally run a network at nominal performance with identical nodes)
        let representative_node = net.nodes().values().nth(representative_node_index).unwrap();
        let representative_storage = &representative_node.main_reactor().storage;
        let representative_runtime = &representative_node.main_reactor().contract_runtime;

        // Recover highest completed block height
        let highest_completed_height = representative_storage
            .highest_complete_block_height()
            .expect("missing highest completed block");

        // Get all the blocks
        let blocks: Vec<BlockWithMetadata> =
            (0..highest_completed_height + 1).map(
                |i| representative_storage.read_block_and_metadata_by_height(i).expect("block not found").unwrap()
            ).collect();

        // Recover history of total supply
        let mint_hash: ContractHash = {
            let any_state_hash = *switch_blocks.headers[0].state_root_hash();
            representative_runtime
                .engine_state()
                .get_system_mint_hash(CorrelationId::new(), any_state_hash)
                .expect("mint contract hash not found")
        };

        // Get total supply history
        let total_supply: Vec<U512> = (0..highest_completed_height + 1)
            .map(|height: u64| {
                let state_hash = *representative_storage
                    .read_block_header_by_height(height, true)
                    .expect("failure to read block header")
                    .unwrap()
                    .state_root_hash();

                let request = QueryRequest::new(
                    state_hash.clone(),
                    mint_hash.into(),
                    vec![mint::TOTAL_SUPPLY_KEY.to_owned()],
                );

                representative_runtime
                    .engine_state()
                    .run_query(CorrelationId::new(), request)
                    .and_then(move |query_result| match query_result {
                        Success { value, proofs: _ } => value
                            .as_cl_value()
                            .ok_or_else(|| Error::Mint("Value not a CLValue".to_owned()))?
                            .clone()
                            .into_t::<U512>()
                            .map_err(|e| Error::Mint(format!("CLValue not a U512: {e}"))),
                        ValueNotFound(s) => Err(Error::Mint(format!("ValueNotFound({s})"))),
                        CircularReference(s) => Err(Error::Mint(format!("CircularReference({s})"))),
                        DepthLimit { depth } => Err(Error::Mint(format!("DepthLimit({depth})"))),
                        QueryResult::RootNotFound => Err(Error::RootNotFound(state_hash)),
                    })
                    .expect("failure to recover total supply")
            })
            .collect();

        // Tiny helper function
        #[inline]
        fn add_to_rewards(recipient: PublicKey, reward: Ratio<u64>, rewards: &mut BTreeMap<PublicKey, Ratio<u64>>, era: usize, total_supply: &mut BTreeMap<usize, Ratio<u64>>) {
            match (rewards.get_mut(&recipient.clone()), total_supply.get_mut(&era)) {
                (Some(value), Some(supply)) => {
                    *value += reward;
                    *supply += reward;
                }
                (None, Some(supply)) => {
                    rewards.insert(recipient.clone(), reward);
                    *supply += reward;
                }
                (Some(value), None) => panic!("rewards present without corresponding supply increase"),
                (None, None) => {
                    total_supply.insert(era, reward);
                    rewards.insert(recipient.clone(), reward);
                }
            }
        }

        let mut recomputed_total_supply = BTreeMap::<usize, Ratio<u64>>::new();
        recomputed_total_supply.insert(0, Ratio::from(total_supply[0].as_u64()));
        let recomputed_rewards = switch_blocks.headers
            .iter()
            .enumerate()
            .map(|(i, switch_block)|
                if switch_block.is_genesis() || switch_block.height() > highest_completed_height {
                    return (i, BTreeMap::<PublicKey, Ratio<u64>>::new())
                } else {
                    let mut recomputed_era_rewards = BTreeMap::<PublicKey, Ratio<u64>>::new();

                    // It's not a genesis block, so we know there's something with a lower era id
                    let previous_switch_block_height = switch_blocks.headers[i - 1].height();
                    let current_era_slated_weights =
                        match switch_blocks.headers[i - 1].era_end() {
                            Some(era_report) => era_report.next_era_validator_weights().clone(),
                            _ => panic!("unexpectedly absent era report"),
                        };
                    let total_current_era_weights = current_era_slated_weights
                        .iter()
                        .fold(0u64, move |acc, s| acc + s.1.as_u64());
                    let (previous_era_slated_weights, total_previous_era_weights) =
                        if switch_blocks.headers[i - 1].is_genesis() {
                            (None, None)
                        } else {
                            match switch_blocks.headers[i - 2].era_end() {
                                Some(era_report) => {
                                    let next_weights = era_report.next_era_validator_weights().clone();
                                    let total_next_weights = next_weights
                                        .iter()
                                        .fold(0u64, move |acc, s| acc + s.1.as_u64());
                                    (Some(next_weights), Some(total_next_weights))},
                                _ => panic!("unexpectedly absent era report"),
                            }
                        };
                    let era_length = switch_block.height() - previous_switch_block_height;
                    let last_era_length =
                        if switch_blocks.headers[i - 1].is_genesis() {
                            None
                        } else {
                            Some(switch_block.height() - switch_blocks.headers[i - 2].height())
                        };
                    let total_expected_pot = Ratio::from(recomputed_total_supply[&(previous_switch_block_height as usize)] * chain.chainspec.core_config.minimum_era_height) * chain.chainspec.core_config.round_seigniorage_rate;
                    let total_previous_expected_pot =
                        if switch_blocks.headers[i - 1].is_genesis() {
                            None
                        } else {
                            Some(Ratio::from(recomputed_total_supply[&(switch_blocks.headers[i - 2].height() as usize)] * chain.chainspec.core_config.minimum_era_height) * chain.chainspec.core_config.round_seigniorage_rate)
                        };

                    // TODO: Investigate whether the rewards pay out for the signatures _in the switch block itself_
                    let rewarded_range = previous_switch_block_height as usize + 1..switch_block.height() as usize + 1;
                    let rewarded_blocks = &blocks[rewarded_range];
                    let block_reward = (Ratio::new(1, 1)
                        - chain.chainspec.core_config.finality_signature_proportion)
                        * (total_expected_pot / max(chain.chainspec.core_config.minimum_era_height, era_length));
                    let signatures_reward = chain.chainspec.core_config.finality_signature_proportion
                        * (total_expected_pot / max(chain.chainspec.core_config.minimum_era_height, era_length));
                    let previous_signatures_reward =
                        if switch_blocks.headers[i - 1].is_genesis() {
                            None
                        } else {
                            Some(chain.chainspec.core_config.finality_signature_proportion
                                * (total_previous_expected_pot.unwrap() / max(chain.chainspec.core_config.minimum_era_height, last_era_length.unwrap())))
                        };

                    rewarded_blocks
                        .iter()
                        .for_each(|block: &BlockWithMetadata| {
                            // Block production rewards
                            let proposer = block.block.proposer().clone();
                            add_to_rewards(proposer.clone(), block_reward, &mut recomputed_era_rewards, i, &mut recomputed_total_supply);

                            // Recover relevant finality signatures
                            // TODO: Deal with the implicit assumption that lookback only look backs one previous era
                            block.block.rewarded_signatures()
                                .iter()
                                .enumerate()
                                .for_each(|(offset, signatures_packed)| {
                                    if block.block.height() as usize - offset - 1 <= previous_switch_block_height as usize && !switch_blocks.headers[i - 1].is_genesis() {
                                        let rewarded_contributors = signatures_packed.into_validator_set(previous_era_slated_weights.as_ref().expect("expected previous era weights").keys().cloned().collect::<BTreeSet<PublicKey>>());
                                        rewarded_contributors
                                            .iter()
                                            .for_each(|contributor| {
                                                let contributor_proportion = Ratio::from(previous_era_slated_weights.as_ref().expect("expected previous era weights")
                                                    .get(contributor)
                                                    .expect("expected current era validator").as_u64())
                                                    / total_previous_era_weights.expect("expected total previous era weight");
                                                add_to_rewards(proposer.clone(), chain.chainspec.core_config.finders_fee * contributor_proportion * previous_signatures_reward.unwrap(), &mut recomputed_era_rewards, i, &mut recomputed_total_supply);
                                                add_to_rewards(contributor.clone(), (Ratio::new(1, 1) - chain.chainspec.core_config.finders_fee) * contributor_proportion * signatures_reward, &mut recomputed_era_rewards, i, &mut recomputed_total_supply)
                                            });
                                    } else {
                                        let rewarded_contributors = signatures_packed.into_validator_set(current_era_slated_weights.keys().map(|key| key.clone()).collect::<BTreeSet<PublicKey>>());
                                        rewarded_contributors
                                            .iter()
                                            .for_each(|contributor| {
                                                let contributor_proportion = Ratio::from(current_era_slated_weights
                                                    .get(contributor)
                                                    .expect("expected current era validator").as_u64())
                                                    / total_current_era_weights;
                                                add_to_rewards(proposer.clone(), chain.chainspec.core_config.finders_fee * contributor_proportion * signatures_reward, &mut recomputed_era_rewards, i, &mut recomputed_total_supply);
                                                add_to_rewards(contributor.clone(), (Ratio::new(1, 1) - chain.chainspec.core_config.finders_fee) * contributor_proportion * signatures_reward, &mut recomputed_era_rewards, i, &mut recomputed_total_supply);
                                            });
                                    }
                                });
                        });
                    return (i, recomputed_era_rewards) }
            )
            .collect::<BTreeMap<usize,BTreeMap<PublicKey, Ratio<u64>>>>();

        // Recalculated total supply is equal to observed total supply
        switch_blocks.headers
            .iter()
            .for_each(|header|
                if header.height() <= highest_completed_height {
                    assert_eq!(Ratio::<u64>::from(total_supply[header.height() as usize].as_u64()), *(recomputed_total_supply.get(&(header.era_id().value() as usize)).expect("expected recalculated supply")))
                } else {}
            );

        // Recalculated rewards are equal to observed rewards; total supply increase is equal to total rewards;
        recomputed_rewards
            .iter()
            .for_each(|(era, rewards)| {
                if era > &0 && switch_blocks.headers[*era].height() <= highest_completed_height {
                    let observed_total_rewards = switch_blocks.headers[*era].era_end().expect("expected EraEnd").rewards()
                        .iter()
                        .fold(Ratio::from(0u64), |acc, reward| Ratio::<u64>::from(reward.1.as_u64()) + acc);
                    let recomputed_total_rewards = rewards.iter().fold(Ratio::from(0u64), |acc, x| x.1 + acc);
                    assert_eq!(Ratio::<u64>::from(recomputed_total_rewards), Ratio::<u64>::from(observed_total_rewards));
                    assert_eq!(Ratio::<u64>::from(recomputed_total_rewards), recomputed_total_supply.get(era).expect("expected recalculated supply") - recomputed_total_supply.get(&(era - &1)).expect("expected recalculated supply"))
                }
            }
            )

    }

    run_rewards_network_scenario(
        CONSENSUS,
        &[1000000000, 1000000000, 1000000000, 1000000000, 1000000000, 1000000000, 1000000000, 1000000000, 1000000000, 1000000000],
        ERA_COUNT,
        ERA_DURATION,
        MIN_HEIGHT,
        BLOCK_TIME,
        TIME_OUT,
        SEIGNIORAGE,
        FINDERS_FEE,
        FINALITY_SIG_PROP,
        REPRESENTATIVE_NODE_INDEX,
        FILTERED_NODES_INDICES
    ).await;
}
