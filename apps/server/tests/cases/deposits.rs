//! Watch → index → legs and balances, of the native currency and a token.

use base::testing::{Caps, TestChain};

use super::{App, reaches, settle, unit};

pub(super) const NEEDS: Caps = Caps::TOKEN;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let network = chain.network();
    let token = network.assets[1].asset.clone();
    let wallet = app.api.wallet(&network).await;
    app.api.index(&wallet, "native", None).await;
    app.api.index(&wallet, &token, None).await;
    chain.fund(&wallet.address, &unit(chain), "native");
    chain.fund(&wallet.address, "2500000", &token);
    settle(chain);
    reaches(app, &wallet, "native", "balance", &unit(chain)).await;
    let index = reaches(app, &wallet, &token, "balance", "2500000").await;
    assert_eq!(index["balance_received"], "2500000");
    assert_eq!(index["balance_pending"], "2500000");
    for (asset, amount) in [
        ("native", unit(chain)),
        (token.as_str(), "2500000".to_owned()),
    ] {
        let legs = app.api.legs(&wallet, asset).await;
        assert_eq!(legs.len(), 1, "{asset}: {legs:?}");
        assert_eq!(
            (legs[0]["kind"].as_i64(), legs[0]["amount"].as_str()),
            (Some(0), Some(amount.as_str()))
        );
        assert_eq!(legs[0]["total"], amount.as_str());
    }
}
