//! Nodes disagree on what is finalized, neither yet the deposit's block:
//! the confirmed balance counts nothing the safest node has not finalized.

use base::testing::{Caps, TestChain};

use super::{App, reaches, text, unit, until};

pub(super) const NEEDS: Caps = Caps::CLUSTER;

pub(super) fn prepare<T: TestChain>(chain: &T) {
    chain.node(0).finality(Some(4));
    chain.node(1).finality(Some(6));
}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = app.api.wallet(&chain.network()).await;
    app.api.index(&wallet, "native", None).await;
    chain.fund(&wallet.address, &unit(chain), "native");
    chain.mine(4);
    let tip = chain.tip();
    until(&format!("the index at {tip}"), || async {
        let network = app.api.network().await;
        if network["index"]["tip"] == tip {
            return Ok(());
        }
        Err(network.to_string())
    })
    .await;
    let index = app.api.index_of(&wallet, "native").await;
    assert_eq!(text(&index, "balance_pending"), unit(chain), "{index}");
    assert_eq!(
        text(&index, "balance"),
        "0",
        "not final on any node: {index}"
    );
    chain.mine(3);
    reaches(app, &wallet, "native", "balance", &unit(chain)).await;
}
