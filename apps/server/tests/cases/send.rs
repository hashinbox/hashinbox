//! Send → pending → mined: what the wallet can spend, what is pending, and
//! the legs once mined.

use base::testing::{Caps, TestChain};

use super::{App, funded, number, quarter, reaches, settle, succeeded, text, unit, until};

pub(super) const NEEDS: Caps = Caps::NONE;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = funded(app, chain).await;
    let other = app.api.wallet(&chain.network()).await;
    let job = app
        .api
        .send(&wallet, "native", &other.address, "0.25")
        .await;
    let pending = until("the send to be pending", || async {
        let index = app.api.index_of(&wallet, "native").await;
        if text(&index, "balance_spendable") != text(&index, "balance") {
            return Ok(index);
        }
        Err(index.to_string())
    })
    .await;
    // In flight: the quarter and the worst-case fee held, nothing mined.
    let (whole, quarter) = (number(&unit(chain)), number(&quarter(chain)));
    assert!(
        number(&text(&pending, "balance_spendable")) < whole - quarter,
        "{pending}"
    );
    assert_eq!(text(&pending, "balance_pending"), unit(chain));
    assert_eq!(succeeded(app, &job).await["accepted"], 1);

    settle(chain);
    let mined = until("the send to be mined", || async {
        let index = app.api.index_of(&wallet, "native").await;
        if text(&index, "balance") != unit(chain)
            && text(&index, "balance") == text(&index, "balance_pending")
        {
            return Ok(index);
        }
        Err(index.to_string())
    })
    .await;
    assert_eq!(text(&mined, "balance_spendable"), text(&mined, "balance"));
    assert!(
        number(&text(&mined, "balance")) < whole - quarter,
        "{mined}"
    );
    let legs = app.api.legs(&wallet, "native").await;
    let kinds: Vec<_> = legs
        .iter()
        .map(|leg| (leg["kind"].as_i64(), leg["amount"].clone()))
        .collect();
    assert!(
        kinds.contains(&(Some(1), quarter.to_string().into())),
        "{legs:?}"
    );
    assert!(
        kinds.iter().any(|(kind, _)| *kind == Some(4)),
        "a fee leg: {legs:?}"
    );
    reaches(
        app,
        &wallet,
        "native",
        "balance_spendable",
        &text(&mined, "balance"),
    )
    .await;
}
