//! The container end to end over the memory store and a scripted chain: a
//! chain built in code, the store attached slot by slot, `start` driving the
//! index to readiness, a watch, the operational handle and shutdown.

use std::sync::Arc;

use base::{AssetId, BoxFuture, ChainName, Network, Unit, Units};
use indexing::{Birthday, memory};
use json_rpc::Statistics;
use payments::{
    Payments, Split,
    config::{Chain, ComposedChain, Connection, Error, Transport},
};
use wallets::{SubmissionRegistrar, WalletFactory, testing::Factory};

fn network() -> Network {
    Network::new(ChainName::new("test"), "net")
}

/// A chain section built in code: its node is always the scripted chain.
struct Fixture;

struct Verified;

impl Chain<&'static str, Split> for Fixture {
    fn network(&self) -> Network {
        network()
    }

    fn accept(&self) -> Result<Option<String>, Error> {
        Ok(None)
    }

    fn verify<'a>(&'a self, _transport: &'a Transport) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }

    fn connect<'a>(
        &'a self,
        _statistics: &'a mut Vec<Statistics>,
        _logger: logging::Logger,
        _transport: Option<Transport>,
    ) -> BoxFuture<'a, Result<Box<dyn Connection<&'static str, Split>>, Error>> {
        Box::pin(async { Ok(Box::new(Verified) as Box<dyn Connection<_, _>>) })
    }
}

impl Connection<&'static str, Split> for Verified {
    fn compose<'a>(
        self: Box<Self>,
        repository: Split,
        _registrar: &'a Arc<dyn SubmissionRegistrar>,
    ) -> BoxFuture<'a, Result<ComposedChain<&'static str>, Error>> {
        Box::pin(async move {
            let scripted = indexing::testing::Networks::new(&[network()], 3, 1, repository);
            let index = scripted.index().units()[0].indexer();
            let (_, pending) = scripted.pending().remove(0);
            let native = AssetId::native(network().chain);
            Ok(ComposedChain {
                index,
                pending,
                factories: vec![(
                    "coin",
                    Box::new(Factory::new(network())) as Box<dyn WalletFactory>,
                )],
                units: Units::from_iter([((network(), native), Unit::token(0))]),
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_coded_chain_starts_watches_lists_and_shuts_down() {
    let store = Arc::new(memory::Repository::new());
    let payments = Payments::builder()
        .chain(Fixture)
        .with_store(store.clone())
        .start()
        .await
        .expect("started");

    let wallet = payments.wallets.create(&"coin").await.expect("a wallet");
    payments
        .watch("alice", &wallet, Birthday::Fresh)
        .await
        .expect("watched");
    assert!(
        payments.changes().is_some(),
        "the default index slot notifies"
    );
    let listed = payments.networks.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].network, network());
    let balance = store
        .balance(wallet.as_ref())
        .expect("the repository answers");
    assert_eq!(
        balance
            .completed(wallet.as_ref(), indexing::Sample::new(Vec::new(), 0))
            .net,
        0.into()
    );

    payments.shutdown().await.expect("clean shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn start_refuses_without_the_required_store() {
    let store = Arc::new(memory::Repository::new());
    let payments = Payments::builder()
        .chain(Fixture)
        .with_store(store)
        .start()
        .await
        .expect("started");
    assert!(payments.registry.is_none());
    payments.shutdown().await.expect("clean shutdown");

    let missing = Payments::<&'static str>::builder()
        .chain(Fixture)
        .start()
        .await
        .err()
        .expect("no required store");
    assert!(missing.to_string().contains("with_store"), "{missing}");
}
