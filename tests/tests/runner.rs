use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Error;
use async_stream::stream;
use futures03::{Stream, StreamExt};
use graph::blockchain::block_stream::{BlockStream, BlockStreamBuilder, BlockStreamEvent};
use graph::blockchain::{Block, Blockchain, BlockchainMap, ChainIdentifier};
use graph::cheap_clone::CheapClone;
use graph::components::store::{BlockStore, DeploymentId, DeploymentLocator};
use graph::env::{EnvVars, ENV_VARS};
use graph::firehose::FirehoseEndpoints;
use graph::ipfs_client::IpfsClient;
use graph::prelude::{
    async_trait, DeploymentHash, LightEthereumBlock, LinkResolver as LinkResolverTrait,
    LoggerFactory, MetricsRegistry, NodeId, SubgraphAssignmentProvider, SubgraphName,
    SubgraphRegistrar, SubgraphStore, SubgraphVersionSwitchingMode,
};
use graph_chain_ethereum::{self as ethereum};
use graph_core::{
    LinkResolver, SubgraphAssignmentProvider as IpfsSubgraphAssignmentProvider,
    SubgraphInstanceManager, SubgraphRegistrar as IpfsSubgraphRegistrar,
};
use graph_mock::MockMetricsRegistry;
use graph_node::manager::PanicSubscriptionManager;
use graph_node::{
    config::{Config, Opt},
    store_builder::StoreBuilder,
};
use slog::{debug, info, Logger};

// #[tokio::test(flavor = "multi_thread", worker_threads = 5)]
#[tokio::test]
async fn test_runner() -> anyhow::Result<()> {
    let subgraph_name = SubgraphName::new("test1")
        .expect("Subgraph name must contain only a-z, A-Z, 0-9, '-' and '_'");

    let deployment = DeploymentLocator {
        id: DeploymentId::new(1),
        hash: DeploymentHash::new("QmXKJdPSnVaxS2hJrUUtPhc55KEFa8GaVC9LFaB4UNtpWX")
            .expect("unable to parse hash"),
    };

    let start_block =
        graph_chain_ethereum::chain::BlockFinality::Final(Arc::new(LightEthereumBlock::default()));
    let stop_block = 1982444;

    let ctx = setup(subgraph_name.clone(), &deployment, start_block, vec![]).await;

    let registrar = ctx.registrar.clone();
    let provider = ctx.provider.clone();
    let store = ctx.store.clone();

    let logger = ctx.logger_factory.subgraph_logger(&deployment);

    // let mapping = ctx
    //     .link_resolver
    //     .cat(
    //         &logger,
    //         &Link {
    //             link: "QmUuqWUHTNaHkq6d333n7gCrZbwzLN2yophaHeeK9x6BjJ".into(),
    //         },
    //     )
    //     .await
    //     .expect("failed to fetch manifest");

    // let mapping = serde_yaml::from_slice(mapping.as_ref()).expect("unable to parse manifest");

    SubgraphRegistrar::create_subgraph(registrar.as_ref(), subgraph_name.clone())
        .await
        .expect("unable to create subgraph");

    SubgraphAssignmentProvider::start(provider.as_ref(), deployment.clone(), Some(stop_block))
        .await
        .expect("unabel to start subgraph");

    loop {
        tokio::time::sleep(Duration::from_millis(1000)).await;

        let block_ptr = store
            .least_block_ptr(&deployment.hash)
            .await
            .unwrap()
            .unwrap();

        debug!(&logger, "subgraph block: {:?}", block_ptr);

        if block_ptr.number >= stop_block {
            info!(
                &logger,
                "subgraph now at block {}, reached stop block {}", block_ptr.number, stop_block
            );
            break;
        }
    }

    // FIXME: wait for instance manager to stop.
    // If we remove the subgraph first, it will panic on:
    // 1504c9d8-36e4-45bb-b4f2-71cf58789ed9
    tokio::time::sleep(Duration::from_millis(4000)).await;

    info!(&logger, "Removing subgraph {}", subgraph_name);
    store.clone().remove_subgraph(subgraph_name)?;

    // let mapping = serde_yaml::from_str(
    //     r#"
    //     dataSources:
    //     - kind: ethereum/contract
    //       mapping:
    //         abis:
    //           - file:
    //               /: /ipfs/QmVPdBm7yEA5rDQgFJF6a7gwu851Gghnja1fShwCvwyhKB
    //             name: ConverterRegistryContract
    //         apiVersion: 0.0.6
    //         blockHandlers:
    //           - handler: handleBlock
    //         entities:
    //           - ConverterAddition
    //         file:
    //           /: /ipfs/QmVrZNzLCBGTuWt6tKvNcoc3zceXF2LMRbL2MDAd8bvZ5m
    //         kind: ethereum/events
    //         language: wasm/assemblyscript
    //       name: ConverterRegistryContract
    //       network: mainnet
    //       source:
    //         abi: ConverterRegistryContract
    //         address: '0x0ddff327ddf7fe838e3e63d02001ef23ad1ede8e'
    //         startBlock: 6175244
    //   description: Indexing all Ethereum Block data
    //   repository: 'https://github.com/blocklytics/ethereum-blocks'
    //   schema:
    //     file:
    //       /: /ipfs/Qmbdxh2z2mwmwxzG27XRXjSm9rXZkvHTqsfeNuJEWMeQpb
    //   specVersion: 0.0.2
    //     "#,
    // )
    // .unwrap();

    // sg_instance_mgr
    //     .start_subgraph_inner::<graph_chain_ethereum::Chain>(
    //         logger,
    //         deployment,
    //         mapping,
    //         Some(6175250),
    //     )
    //     .await
    //     .expect("failed to start subgraph");

    Ok(())
}

struct TestContext {
    logger_factory: LoggerFactory,
    registry: Arc<dyn MetricsRegistry>,
    registrar: Arc<
        IpfsSubgraphRegistrar<
            IpfsSubgraphAssignmentProvider<
                SubgraphInstanceManager<graph_store_postgres::SubgraphStore>,
            >,
            graph_store_postgres::SubgraphStore,
            PanicSubscriptionManager,
        >,
    >,
    provider: Arc<
        IpfsSubgraphAssignmentProvider<
            SubgraphInstanceManager<graph_store_postgres::SubgraphStore>,
        >,
    >,
    chains: Arc<BlockchainMap>,
    link_resolver: Arc<dyn LinkResolverTrait>,
    store: Arc<dyn SubgraphStore>,
}

async fn setup(
    subgraph_name: SubgraphName,
    deployment: &DeploymentLocator,
    start_block: <graph_chain_ethereum::Chain as Blockchain>::Block,
    events: Vec<BlockStreamEvent<graph_chain_ethereum::Chain>>,
) -> TestContext {
    let logger = Logger::root(slog::Discard, slog::o!());
    let logger_factory = LoggerFactory::new(logger.clone(), None);
    let mut opt = Opt::default();
    opt.postgres_url = Some("postgresql://graph-node:let-me-in@127.0.0.1:5432/graph-node".into());
    let node_id = NodeId::new(opt.node_id.clone()).expect("invalid node_id");
    let config = Config::load(&logger, &opt).expect("failed to create configuration");

    let mock_registry: Arc<dyn MetricsRegistry> = Arc::new(MockMetricsRegistry::new());

    let network_name: String = "mainnet".into();

    let store_builder =
        StoreBuilder::new(&logger, &node_id, &config, None, mock_registry.clone()).await;

    let ipfs =
        IpfsClient::new("https://api.thegraph.com/ipfs/").expect("failed to start ipfs client");

    let link_resolver = Arc::new(LinkResolver::new(vec![ipfs], Arc::new(EnvVars::default())));

    // let eth_networks = create_ethereum_networks(logger.clone(), mock_registry.clone(), &config)
    //     .await
    //     .expect("Failed to parse Ethereum networks");
    // let firehose_networks_by_kind =
    //     create_firehose_networks(logger.clone(), mock_registry.clone(), &config)
    //         .await
    //         .expect("Failed to parse Firehose endpoints");
    // let firehose_networks = firehose_networks_by_kind.get(&BlockchainKind::Ethereum);
    // let firehose_endpoints = firehose_networks.and_then(|v| v.networks.get(&network_name));

    // let eth_adapters = match eth_networks.networks.get(&network_name) {
    //     Some(adapters) => adapters.clone(),
    //     None => {
    //         panic!("{}",format!("No ethereum adapters found, but required in this state of graphman run command"))
    //     }
    // };

    // let (_, ethereum_idents) = connect_ethereum_networks(&logger, eth_networks).await;

    let chain_head_update_listener = store_builder.chain_head_update_listener();
    // let network_identifiers = ethereum_idents.into_iter().collect();
    let network_identifiers: Vec<(String, Vec<ChainIdentifier>)> = vec![(
        "mainnet".into(),
        (vec![ChainIdentifier {
            net_version: "".into(),
            genesis_block_hash: start_block.hash(),
        }]),
    )];
    let network_store = store_builder.network_store(network_identifiers);

    let subgraph_store = network_store.subgraph_store();
    let chain_store = network_store
        .block_store()
        .chain_store(network_name.as_ref())
        .expect(format!("No chain store for {}", &network_name).as_ref());

    let chain = ethereum::Chain::new(
        logger_factory.clone(),
        network_name.clone(),
        node_id.clone(),
        mock_registry.clone(),
        chain_store.cheap_clone(),
        chain_store,
        FirehoseEndpoints::new(),
        ethereum::network::EthereumNetworkAdapters { adapters: vec![] },
        chain_head_update_listener,
        Arc::new(StaticStreamBuilder { events }),
        ethereum::ENV_VARS.reorg_threshold,
        // We assume the tested chain is always ingestible for now
        true,
    );

    // let chain = Chain::new(
    //     logger_factory.clone(),
    //     network_name.clone(),
    //     chain_store.cheap_clone(),
    //     FirehoseEndpoints::new(),
    //     mock_registry.clone(),
    //     Arc::new(StaticStreamBuilder { events }),
    // );

    let mut blockchain_map = BlockchainMap::new();
    blockchain_map.insert(network_name.clone(), Arc::new(chain));

    let static_filters = ENV_VARS.experimental_static_filters;

    let blockchain_map = Arc::new(blockchain_map);
    let subgraph_instance_manager = SubgraphInstanceManager::new(
        &logger_factory,
        subgraph_store.clone(),
        blockchain_map.clone(),
        mock_registry.clone(),
        link_resolver.cheap_clone(),
        static_filters,
    );

    // Create IPFS-based subgraph provider
    let subgraph_provider: Arc<
        IpfsSubgraphAssignmentProvider<
            SubgraphInstanceManager<graph_store_postgres::SubgraphStore>,
        >,
    > = Arc::new(IpfsSubgraphAssignmentProvider::new(
        &logger_factory,
        link_resolver.cheap_clone(),
        subgraph_instance_manager,
    ));

    let panicking_subscription_manager = Arc::new(PanicSubscriptionManager {});

    let subgraph_registrar: Arc<
        IpfsSubgraphRegistrar<
            IpfsSubgraphAssignmentProvider<
                SubgraphInstanceManager<graph_store_postgres::SubgraphStore>,
            >,
            graph_store_postgres::SubgraphStore,
            PanicSubscriptionManager,
        >,
    > = Arc::new(IpfsSubgraphRegistrar::new(
        &logger_factory,
        link_resolver.cheap_clone(),
        subgraph_provider.clone(),
        subgraph_store.clone(),
        panicking_subscription_manager,
        blockchain_map.clone(),
        node_id.clone(),
        SubgraphVersionSwitchingMode::Instant,
    ));

    SubgraphRegistrar::create_subgraph_version(
        subgraph_registrar.as_ref(),
        subgraph_name.clone(),
        deployment.hash.clone(),
        node_id.clone(),
        None,
        None,
    )
    .await
    .expect("failed to create subgraph version");

    TestContext {
        logger_factory,
        registry: mock_registry,
        chains: blockchain_map,
        link_resolver,
        provider: subgraph_provider,
        registrar: subgraph_registrar,
        store: subgraph_store,
    }
}

struct StaticStreamBuilder<C: Blockchain> {
    events: Vec<BlockStreamEvent<C>>,
}

#[async_trait]
impl<C: Blockchain> BlockStreamBuilder<C> for StaticStreamBuilder<C> {
    fn build_firehose(
        &self,
        _chain: &C,
        _deployment: DeploymentLocator,
        _block_cursor: Option<String>,
        _start_blocks: Vec<graph::prelude::BlockNumber>,
        _subgraph_current_block: Option<graph::blockchain::BlockPtr>,
        _filter: Arc<C::TriggerFilter>,
        _unified_api_version: graph::data::subgraph::UnifiedMappingApiVersion,
    ) -> anyhow::Result<Box<dyn BlockStream<C>>> {
        Ok(Box::new(StaticStream {
            stream: Box::pin(stream_events(self.events.clone())),
        }))
    }

    async fn build_polling(
        &self,
        _chain: Arc<C>,
        _deployment: DeploymentLocator,
        _start_blocks: Vec<graph::prelude::BlockNumber>,
        _subgraph_current_block: Option<graph::blockchain::BlockPtr>,
        _filter: Arc<C::TriggerFilter>,
        _unified_api_version: graph::data::subgraph::UnifiedMappingApiVersion,
    ) -> anyhow::Result<Box<dyn BlockStream<C>>> {
        Ok(Box::new(StaticStream {
            stream: Box::pin(stream_events(self.events.clone())),
        }))
    }
}

struct StaticStream<C: Blockchain> {
    stream: Pin<Box<dyn Stream<Item = Result<BlockStreamEvent<C>, Error>> + Send>>,
}

impl<C: Blockchain> BlockStream<C> for StaticStream<C> {}

impl<C: Blockchain> Stream for StaticStream<C> {
    type Item = Result<BlockStreamEvent<C>, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.poll_next_unpin(cx)
    }
}

fn stream_events<C: Blockchain>(
    blocks: Vec<BlockStreamEvent<C>>,
) -> impl Stream<Item = Result<BlockStreamEvent<C>, Error>> {
    stream! {
        for event in blocks.into_iter() {
            yield Ok(event);
        }
    }
}
