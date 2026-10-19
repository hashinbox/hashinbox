//! Pipes' Select and Check steps on a SQLite temp file: tags, selector
//! resolution in `models::WalletIndex`, confirmed balances, compares,
//! amounts, fees and run gates, and the JSON form.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use base::{Address, Asset, Chain, Decimal, FeeEstimator, Future, Network, Networked};
use diesel::{Connection as _, SqliteConnection, connection::SimpleConnection as _};

use crate::{
    models::{
        Json, Tag, Trigger, TriggerRun, Wallet, balance, int, tag, trigger,
        trigger::{Aggregate, Amount, Compare, Filter, Gate, Logic, Operator, Target, This},
        trigger_run, wallet,
        wallet::db::IndexedSelector,
        wallet_index,
        wallet_index::{Selector, WalletIndexes},
        wallet_index_balance::{self, WalletIndexBalances},
    },
    trigger::{Decimals, Evaluation, Fees as TriggerFees},
};

/// The confirmed tip of the test network.
const TIP: u64 = 2;

fn network() -> Network {
    Network::new(Chain::new("bitcoin"), "regtest")
}

/// Blocks 0..=3 live, 5 archived, of `bitcoin.regtest`; block 3 of
/// `bitcoin.other`. Indexes (`bitcoin.regtest.{address}.{asset}`):
/// a, b fully indexed in native and `tok`; c `tok` full, native behind; d
/// never indexed; e through an archived block; `bitcoin.other.a.native` full.
/// a native: received 1000 (block 1), spent 300 + fee 10 (block 2), reward 5
/// above the tip (block 3); b native: reward 50 (block 0); a tok: 2^200.
const SEED: &str = "
INSERT INTO block (id, chain, network, position, height, hash, archived_at) VALUES
 ('bitcoin.regtest.00','bitcoin','regtest',0,0,x'00',NULL),
 ('bitcoin.regtest.01','bitcoin','regtest',1,1,x'01',NULL),
 ('bitcoin.regtest.02','bitcoin','regtest',2,2,x'02',NULL),
 ('bitcoin.regtest.03','bitcoin','regtest',3,3,x'03',NULL),
 ('bitcoin.regtest.f5','bitcoin','regtest',5,5,x'f5',1),
 ('bitcoin.other.03','bitcoin','other',3,3,x'03',NULL);
INSERT INTO wallet_index (id, chain, network, address, asset, birthday, indexed_at) VALUES
 ('bitcoin.regtest.a.native','bitcoin','regtest','a','native',0,'bitcoin.regtest.03'),
 ('bitcoin.regtest.b.native','bitcoin','regtest','b','native',0,'bitcoin.regtest.02'),
 ('bitcoin.regtest.c.native','bitcoin','regtest','c','native',0,'bitcoin.regtest.01'),
 ('bitcoin.regtest.d.native','bitcoin','regtest','d','native',0,NULL),
 ('bitcoin.regtest.e.native','bitcoin','regtest','e','native',0,'bitcoin.regtest.f5'),
 ('bitcoin.regtest.a.tok','bitcoin','regtest','a','tok',0,'bitcoin.regtest.03'),
 ('bitcoin.regtest.b.tok','bitcoin','regtest','b','tok',0,'bitcoin.regtest.03'),
 ('bitcoin.regtest.c.tok','bitcoin','regtest','c','tok',0,'bitcoin.regtest.03'),
 ('bitcoin.other.a.native','bitcoin','other','a','native',0,'bitcoin.other.03');
INSERT INTO \"transaction\" (id, block, chain, network, txid, \"order\", state, bytes) VALUES
 ('bitcoin.regtest.t1','bitcoin.regtest.01','bitcoin','regtest','t1',0,1,x''),
 ('bitcoin.regtest.t2','bitcoin.regtest.02','bitcoin','regtest','t2',0,1,x''),
 ('bitcoin.regtest.t3','bitcoin.regtest.f5','bitcoin','regtest','t3',0,1,x'');
INSERT INTO leg (id, block, \"transaction\", leg_index, kind, address, asset, amount) VALUES
 ('bitcoin.regtest.t1.0','bitcoin.regtest.01','bitcoin.regtest.t1',0,0,'a','native','1000'),
 ('bitcoin.regtest.t1.1','bitcoin.regtest.01','bitcoin.regtest.t1',1,0,'a','tok',
  '1606938044258990275541962092341162602522202993782792835301376'),
 ('bitcoin.regtest.t2.0','bitcoin.regtest.02','bitcoin.regtest.t2',0,1,'a','native','300'),
 ('bitcoin.regtest.t2.1','bitcoin.regtest.02','bitcoin.regtest.t2',1,4,'a','native','10'),
 ('bitcoin.regtest.t3.0','bitcoin.regtest.f5','bitcoin.regtest.t3',0,0,'a','native','7777'),
 ('bitcoin.regtest.03.reward.0','bitcoin.regtest.03',NULL,0,5,'a','native','5'),
 ('bitcoin.regtest.00.reward.0','bitcoin.regtest.00',NULL,0,5,'b','native','50');
";

/// A migrated, seeded temp file, a runtime for fee estimates, and a raw
/// connection to it.
fn seeded() -> (tempfile::TempDir, tokio::runtime::Runtime, SqliteConnection) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("test.db");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let storage = crate::storage::Storage::open(&path).unwrap();
    runtime.block_on(storage.migrate()).unwrap();
    let mut connection = SqliteConnection::establish(path.to_str().unwrap()).unwrap();
    connection
        .batch_execute("PRAGMA foreign_keys = ON;")
        .unwrap();
    connection.batch_execute(SEED).unwrap();
    (directory, runtime, connection)
}

fn tag_with(
    connection: &mut SqliteConnection,
    address: &Address,
    name: &str,
) -> anyhow::Result<usize> {
    Ok(tag::db::insert(connection, &Tag::new(address, name)?)?)
}

fn start_run(
    connection: &mut SqliteConnection,
    trigger: &str,
    now: i64,
) -> diesel::QueryResult<TriggerRun> {
    let run = TriggerRun::new(trigger, now);
    trigger_run::db::insert(connection, &run)?;
    Ok(run)
}

fn address(name: &str) -> Address {
    Address::new(network(), name)
}

fn id(address: &str, asset: &str) -> String {
    format!("bitcoin.regtest.{address}.{asset}")
}

fn ids(selector: &Selector, connection: &mut SqliteConnection) -> Vec<String> {
    wallet_index::db::select(connection, &network(), selector, 2)
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect()
}

fn tag(name: &str) -> Selector {
    Selector::Tag(name.to_owned())
}

fn asset(name: &str) -> Selector {
    Selector::Asset(name.to_owned())
}

fn decimal(text: &str) -> Decimal {
    text.parse().unwrap()
}

#[test]
fn tags_are_kept_once_listed_and_removed() {
    let (_directory, _runtime, mut connection) = seeded();
    let a = address("a");
    tag_with(&mut connection, &a, "hot").unwrap();
    tag_with(&mut connection, &a, "hot").unwrap();
    tag_with(&mut connection, &a, "deposit-1").unwrap();
    assert_eq!(
        tag::db::find_by_address(&mut connection, &a)
            .unwrap()
            .names(),
        ["deposit-1", "hot"]
    );
    assert_eq!(
        tag::db::delete(&mut connection, &Tag::id(&a, "hot").unwrap()).unwrap(),
        1
    );
    assert_eq!(
        tag::db::find_by_address(&mut connection, &a)
            .unwrap()
            .names(),
        ["deposit-1"]
    );
    for bad in ["", "Hot", "a.b", "a_b", &"x".repeat(65)] {
        assert!(tag_with(&mut connection, &a, bad).is_err(), "{bad:?}");
    }
    // The schema refuses a bad tag written around the model.
    let raw = "INSERT INTO tag (id, chain, network, address, tag) \
               VALUES ('bitcoin.regtest.a.Hot','bitcoin','regtest','a','Hot');";
    assert!(connection.batch_execute(raw).is_err());
}

#[test]
fn leaves_pick_exactly_their_fully_indexed_indexes() {
    let (_directory, _runtime, mut connection) = seeded();
    let c = &mut connection;
    tag_with(c, &address("a"), "hot").unwrap();
    tag_with(c, &address("e"), "hot").unwrap();
    tag_with(
        c,
        &Address::new(Network::new(Chain::new("bitcoin"), "other"), "b"),
        "hot",
    )
    .unwrap();
    // c behind, d never indexed, e through an archived block: never picked.
    assert_eq!(
        ids(
            &Selector::Ids(vec![
                id("a", "native"),
                id("c", "native"),
                id("d", "native"),
                id("e", "native"),
                "nope".into()
            ]),
            c
        ),
        [id("a", "native")]
    );
    assert_eq!(
        ids(&asset("native"), c),
        [id("a", "native"), id("b", "native")]
    );
    assert_eq!(ids(&tag("hot"), c), [id("a", "native"), id("a", "tok")]);
    // Another network's index is never picked.
    assert!(ids(&Selector::Ids(vec!["bitcoin.other.a.native".into()]), c).is_empty());
}

#[test]
fn combinators_are_set_operations_within_the_universe() {
    let (_directory, _runtime, mut connection) = seeded();
    let c = &mut connection;
    tag_with(c, &address("a"), "hot").unwrap();
    let universe = [
        id("a", "native"),
        id("a", "tok"),
        id("b", "native"),
        id("b", "tok"),
        id("c", "tok"),
    ];
    assert_eq!(ids(&Selector::All(vec![]), c), universe);
    assert!(ids(&Selector::Any(vec![]), c).is_empty());
    assert_eq!(
        ids(&Selector::All(vec![tag("hot"), asset("tok")]), c),
        [id("a", "tok")]
    );
    assert_eq!(
        ids(&Selector::Any(vec![tag("hot"), asset("tok")]), c),
        [
            id("a", "native"),
            id("a", "tok"),
            id("b", "tok"),
            id("c", "tok")
        ]
    );
    assert_eq!(
        ids(&Selector::Not(Box::new(tag("hot"))), c),
        [id("b", "native"), id("b", "tok"), id("c", "tok")]
    );
    assert_eq!(
        ids(
            &Selector::All(vec![asset("native"), Selector::Not(Box::new(tag("hot")))]),
            c
        ),
        [id("b", "native")]
    );
}

#[test]
fn a_tag_or_indexing_brings_indexes_in() {
    let (_directory, _runtime, mut connection) = seeded();
    let c = &mut connection;
    assert!(ids(&tag("collect"), c).is_empty());
    tag_with(c, &address("b"), "collect").unwrap();
    tag_with(c, &address("c"), "collect").unwrap();
    assert_eq!(
        ids(&tag("collect"), c),
        [id("b", "native"), id("b", "tok"), id("c", "tok")]
    );
    c.batch_execute(
        "UPDATE wallet_index SET indexed_at = 'bitcoin.regtest.02' WHERE id = 'bitcoin.regtest.c.native';",
    )
    .unwrap();
    assert_eq!(
        ids(&tag("collect"), c),
        [
            id("b", "native"),
            id("b", "tok"),
            id("c", "native"),
            id("c", "tok")
        ]
    );
}

#[test]
fn contested_indexes_are_dropped_and_reported() {
    let (_directory, _runtime, mut connection) = seeded();
    let selected = wallet_index::db::select_owned(
        &mut connection,
        &network(),
        &asset("tok"),
        &[Selector::Ids(vec![id("b", "tok")]), asset("nothing")],
        int(TIP).unwrap(),
    )
    .unwrap();
    assert_eq!(
        selected
            .owned
            .iter()
            .map(|index| &index.id)
            .collect::<Vec<_>>(),
        [&id("a", "tok"), &id("c", "tok")]
    );
    assert_eq!(
        selected
            .contested
            .iter()
            .map(|index| &index.id)
            .collect::<Vec<_>>(),
        [&id("b", "tok")]
    );
    let alone = wallet_index::db::select_owned(
        &mut connection,
        &network(),
        &asset("tok"),
        &[],
        int(TIP).unwrap(),
    )
    .unwrap();
    assert_eq!(alone.owned.len(), 3);
    assert_eq!(alone.contested, WalletIndexes::default());
}

#[test]
fn balances_are_confirmed_exact_and_folded() {
    let (_directory, _runtime, mut connection) = seeded();
    let all = [
        id("a", "native"),
        id("b", "native"),
        id("a", "tok"),
        id("b", "tok"),
    ];
    let refs: Vec<&str> = all.iter().map(String::as_str).collect();
    let balances = balance::db::find_by_ids(&mut connection, &refs, 2).unwrap();
    // The reward at block 3 and the archived block's leg are not counted.
    assert_eq!(balances[&id("a", "native")].confirmed, decimal("690"));
    assert_eq!(balances[&id("b", "native")].confirmed, decimal("50"));
    assert_eq!(
        balances[&id("a", "tok")].confirmed,
        decimal("1606938044258990275541962092341162602522202993782792835301376")
    );
    assert_eq!(balances[&id("b", "tok")].confirmed, Decimal::zero());
    let later = balance::db::find_by_ids(&mut connection, &refs[..1], 3).unwrap();
    assert_eq!(later[&id("a", "native")].confirmed, decimal("695"));
}

/// A fee estimator answering a fixed fee and counting its calls.
struct Fees {
    network: Network,
    fee: Decimal,
    calls: AtomicUsize,
}

impl Networked for Fees {
    fn network(&self) -> &Network {
        &self.network
    }
}

impl FeeEstimator for Fees {
    fn estimate<'a>(
        &'a self,
        sources: &'a [(Address, Asset)],
    ) -> Future<'a, anyhow::Result<Decimal>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let fee = self.fee.clone() * Decimal::from(sources.len() as u64);
        Box::pin(async move { Ok(fee) })
    }
}

fn fees(fee: &str) -> Arc<Fees> {
    Arc::new(Fees {
        network: network(),
        fee: decimal(fee),
        calls: AtomicUsize::new(0),
    })
}

fn open<'a>(
    connection: &'a mut SqliteConnection,
    runtime: &tokio::runtime::Runtime,
    fees: Arc<Fees>,
    run: Option<TriggerRun>,
) -> Evaluation<'a> {
    Evaluation::new(
        connection,
        100_000,
        run,
        Decimals(BTreeMap::from([
            ("native".to_owned(), 8),
            ("tok".to_owned(), 2),
        ])),
        TriggerFees::new(fees, runtime.handle().clone()),
    )
}

/// A run of the test trigger started at `started_at`.
fn run(started_at: i64, settled: bool) -> TriggerRun {
    TriggerRun {
        settled_at: settled.then_some(started_at),
        ..TriggerRun::new("t", started_at)
    }
}

fn compares(
    evaluation: &mut Evaluation<'_>,
    compare: &Compare,
    this: &WalletIndexBalances,
) -> anyhow::Result<bool> {
    evaluation.filter(&Logic::Leaf(Filter::Compare(compare.clone())), this)
}

fn checks(
    evaluation: &mut Evaluation<'_>,
    check: &Logic<Gate>,
    this: &WalletIndexBalances,
) -> anyhow::Result<bool> {
    evaluation.check(check, this)
}

fn this(asset: Option<&str>) -> Target {
    Target::This(This {
        asset: asset.map(str::to_owned),
    })
}

fn compare(of: Target, aggregate: Aggregate, operator: Operator, value: Amount) -> Compare {
    Compare {
        of,
        aggregate,
        operator,
        value,
    }
}

fn value(text: &str) -> Amount {
    Amount::Value(decimal(text))
}

/// The kept wallet indexes `ids` with their balances at the tip, as Select
/// would hand them on (fully indexed or not).
fn selection(connection: &mut SqliteConnection, ids: &[String]) -> WalletIndexBalances {
    let indexes: WalletIndexes = wallet_index::db::find_every(connection)
        .unwrap()
        .into_iter()
        .filter(|index| ids.contains(&index.id))
        .collect();
    wallet_index_balance::db::with_balances(connection, &network(), indexes, int(TIP).unwrap())
        .unwrap()
}

#[test]
fn aggregates_and_operators() {
    let (_directory, runtime, mut connection) = seeded();
    let both = selection(&mut connection, &[id("a", "native"), id("b", "native")]);
    let mut evaluation = open(&mut connection, &runtime, fees("1"), None);
    let empty = WalletIndexBalances::new(network(), int(TIP).unwrap());
    let native = || Target::Select(asset("native"));
    let mut holds = |of: Target, aggregate, operator, amount, on: &WalletIndexBalances| {
        compares(
            &mut evaluation,
            &compare(of, aggregate, operator, amount),
            on,
        )
        .unwrap()
    };
    // a 690, b 50 sats; 0.0000074 BTC is 740 sats.
    assert!(holds(
        this(None),
        Aggregate::Sum,
        Operator::Equal,
        value("0.0000074"),
        &both
    ));
    assert!(holds(
        this(None),
        Aggregate::Min,
        Operator::Equal,
        value("0.0000005"),
        &both
    ));
    assert!(holds(
        this(None),
        Aggregate::Max,
        Operator::Equal,
        value("0.0000069"),
        &both
    ));
    assert!(holds(
        this(None),
        Aggregate::Count,
        Operator::Equal,
        value("2"),
        &both
    ));
    assert!(holds(
        native(),
        Aggregate::Sum,
        Operator::AtLeast,
        value("0.0000074"),
        &empty
    ));
    assert!(!holds(
        native(),
        Aggregate::Sum,
        Operator::Above,
        value("0.0000074"),
        &empty
    ));
    assert!(holds(
        native(),
        Aggregate::Sum,
        Operator::AtMost,
        value("0.0000074"),
        &empty
    ));
    assert!(!holds(
        native(),
        Aggregate::Sum,
        Operator::Below,
        value("0.0000074"),
        &empty
    ));
    // Empty: Sum and Count are 0, Min and Max have no value.
    let none = Target::Select(Selector::Ids(vec![]));
    assert!(holds(
        none.clone(),
        Aggregate::Count,
        Operator::Equal,
        value("0"),
        &empty
    ));
    assert!(holds(
        Target::Select(Selector::All(vec![asset("native"), Selector::Ids(vec![])])),
        Aggregate::Sum,
        Operator::Equal,
        value("0"),
        &empty
    ));
    assert!(!holds(
        none.clone(),
        Aggregate::Min,
        Operator::AtLeast,
        value("0"),
        &empty
    ));
    assert!(!holds(
        none,
        Aggregate::Max,
        Operator::AtLeast,
        value("0"),
        &empty
    ));
}

#[test]
fn values_must_fit_their_asset() {
    let (_directory, runtime, mut connection) = seeded();
    let a = selection(&mut connection, &[id("a", "native")]);
    let mut evaluation = open(&mut connection, &runtime, fees("1"), None);
    let too_fine = compare(
        this(None),
        Aggregate::Sum,
        Operator::Above,
        value("0.000000001"),
    );
    assert!(compares(&mut evaluation, &too_fine, &a).is_err());
    let fraction = compare(this(None), Aggregate::Count, Operator::Above, value("0.5"));
    assert!(compares(&mut evaluation, &fraction, &a).is_err());
}

#[test]
fn amounts_evaluate_in_atomic_units() {
    let (_directory, runtime, mut connection) = seeded();
    let a = selection(&mut connection, &[id("a", "native")]);
    let mut evaluation = open(&mut connection, &runtime, fees("100"), None);
    let balance = || Box::new(Amount::Balance { of: this(None) });
    let fee = || Box::new(Amount::Fee { of: this(None) });
    // 690 - 100 = 590; 590 + 10 = 600; min(690, 5 sats) = 5; max(-…, 0) = 0.
    let cases = [
        (Amount::Sub(balance(), fee()), "590"),
        (
            Amount::Add(
                Box::new(Amount::Sub(balance(), fee())),
                Box::new(value("0.0000001")),
            ),
            "600",
        ),
        (Amount::Min(balance(), Box::new(value("0.00000005"))), "5"),
        (
            Amount::Max(
                Box::new(Amount::Sub(fee(), Box::new(value("1")))),
                Box::new(value("0")),
            ),
            "0",
        ),
    ];
    for (amount, atomic) in cases {
        // Sum of `a` compared with the amount plus the difference.
        let atomic = decimal(atomic);
        let gap = decimal("690") - atomic;
        let gap_value = Amount::Value(gap.checked_div(&decimal("100000000")).unwrap());
        let total = Amount::Add(Box::new(amount), Box::new(gap_value));
        assert!(
            compares(
                &mut evaluation,
                &compare(this(None), Aggregate::Sum, Operator::Equal, total),
                &a
            )
            .unwrap()
        );
    }
    // Below zero compares as negative.
    let short = Amount::Sub(fee(), Box::new(value("0.00001")));
    assert!(
        compares(
            &mut evaluation,
            &compare(this(None), Aggregate::Sum, Operator::Above, short),
            &a
        )
        .unwrap()
    );
}

#[test]
fn counterparts_must_be_fully_indexed() {
    let (_directory, runtime, mut connection) = seeded();
    let toks = selection(&mut connection, &[id("a", "tok"), id("b", "tok")]);
    let with_c = selection(&mut connection, &[id("a", "tok"), id("c", "tok")]);
    let mut evaluation = open(&mut connection, &runtime, fees("1"), None);
    // The tok indexes read in native: a 690, b 50.
    let in_native = compare(
        this(Some("native")),
        Aggregate::Sum,
        Operator::Equal,
        value("0.0000074"),
    );
    assert!(compares(&mut evaluation, &in_native, &toks).unwrap());
    // c's native index is behind the tip: unknown, the compare fails, under Not too.
    let any = compare(
        this(Some("native")),
        Aggregate::Sum,
        Operator::AtLeast,
        value("0"),
    );
    assert!(!compares(&mut evaluation, &any, &with_c).unwrap());
    let check = Logic::Leaf(Gate::Compare(any));
    assert!(!checks(&mut evaluation, &check, &with_c).unwrap());
}

#[test]
fn a_fee_is_estimated_once_per_evaluation() {
    let (_directory, runtime, mut connection) = seeded();
    let estimator = fees("7");
    let a = selection(&mut connection, &[id("a", "native")]);
    let mut evaluation = open(&mut connection, &runtime, estimator.clone(), None);
    // ETH-forward style check: Sum above Fee, then the same fee as an amount.
    let above_fee = compare(
        this(None),
        Aggregate::Sum,
        Operator::Above,
        Amount::Fee { of: this(None) },
    );
    assert!(compares(&mut evaluation, &above_fee, &a).unwrap());
    assert!(compares(&mut evaluation, &above_fee, &a).unwrap());
    assert_eq!(estimator.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn logic_stops_early_and_errors_stay_errors() {
    let leaves = Logic::All(vec![Logic::Leaf(false), Logic::Leaf(true)]);
    let mut seen = 0;
    assert!(
        !leaves
            .eval(&mut |leaf| {
                seen += 1;
                Ok(*leaf)
            })
            .unwrap()
    );
    assert_eq!(seen, 1);
    let leaves = Logic::Any(vec![Logic::Leaf(true), Logic::Leaf(false)]);
    let mut seen = 0;
    assert!(
        leaves
            .eval(&mut |leaf| {
                seen += 1;
                Ok(*leaf)
            })
            .unwrap()
    );
    assert_eq!(seen, 1);
    assert!(Logic::<bool>::All(vec![]).eval(&mut |_| Ok(false)).unwrap());
    assert!(!Logic::<bool>::Any(vec![]).eval(&mut |_| Ok(true)).unwrap());
    let not = Logic::Not(Box::new(Logic::Leaf(true)));
    assert!(not.eval(&mut |_| anyhow::bail!("read failed")).is_err());

    // An unsettled run stops the check before the fee is read.
    let (_directory, runtime, mut connection) = seeded();
    let estimator = fees("1");
    let a = selection(&mut connection, &[id("a", "native")]);
    let mut evaluation = open(
        &mut connection,
        &runtime,
        estimator.clone(),
        Some(run(0, false)),
    );
    let check = Logic::All(vec![
        Logic::Leaf(Gate::Idle),
        Logic::Leaf(Gate::Compare(compare(
            this(None),
            Aggregate::Sum,
            Operator::Above,
            Amount::Fee { of: this(None) },
        ))),
    ]);
    assert!(!checks(&mut evaluation, &check, &a).unwrap());
    assert_eq!(estimator.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn idle_and_since_read_the_last_run() {
    let (_directory, runtime, mut connection) = seeded();
    let empty = WalletIndexBalances::new(network(), int(TIP).unwrap());
    let idle = Logic::Leaf(Gate::Idle);
    let since = |seconds| Logic::Leaf(Gate::Since { seconds });
    let cases = [
        (None, &idle, true),
        (Some(run(0, false)), &idle, false),
        (Some(run(0, true)), &idle, true),
    ];
    for (last, check, passes) in cases {
        let mut evaluation = open(&mut connection, &runtime, fees("1"), last);
        assert_eq!(checks(&mut evaluation, check, &empty).unwrap(), passes);
    }
    // now is 100 000 ms.
    for (started_at, passes) in [(40_001, false), (40_000, true)] {
        let last = Some(run(started_at, false));
        let mut evaluation = open(&mut connection, &runtime, fees("1"), last);
        assert_eq!(checks(&mut evaluation, &since(60), &empty).unwrap(), passes);
    }
    let mut evaluation = open(&mut connection, &runtime, fees("1"), None);
    assert!(checks(&mut evaluation, &since(60), &empty).unwrap());
}

#[test]
fn the_sweep_trigger_round_trips_through_json() {
    let selector = r#"{"all":[{"tag":"deposit"},{"asset":"0xa0b8eb48"}]}"#;
    let filter = r#"{"compare":{"of":{"this":{"asset":"native"}},"aggregate":"sum","operator":"at_least","value":{"fee":{"of":{"this":{}}}}}}"#;
    let check = r#""idle""#;
    let parsed: Selector = serde_json::from_str(selector).unwrap();
    assert_eq!(
        parsed,
        Selector::All(vec![tag("deposit"), asset("0xa0b8eb48")])
    );
    assert_eq!(serde_json::to_string(&parsed).unwrap(), selector);
    let parsed: Logic<Filter> = serde_json::from_str(filter).unwrap();
    assert_eq!(
        parsed,
        Logic::Leaf(Filter::Compare(compare(
            this(Some("native")),
            Aggregate::Sum,
            Operator::AtLeast,
            Amount::Fee { of: this(None) }
        )))
    );
    assert_eq!(serde_json::to_string(&parsed).unwrap(), filter);
    let parsed: Logic<Gate> = serde_json::from_str(check).unwrap();
    assert_eq!(parsed, Logic::Leaf(Gate::Idle));
    assert_eq!(serde_json::to_string(&parsed).unwrap(), check);
    // Combinators, Since and decimal strings; `{"balance":{}}` is This.
    let full = r#"{"all":["idle",{"since":{"seconds":60}},{"not":{"compare":{"of":{"select":{"ids":["x"]}},"aggregate":"max","operator":"at_most","value":{"max":[{"sub":[{"balance":{"of":{"this":{}}}},{"value":"1.25"}]},{"value":"0"}]}}}}]}"#;
    let parsed: Logic<Gate> = serde_json::from_str(full).unwrap();
    assert_eq!(serde_json::to_string(&parsed).unwrap(), full);
    let short: Amount = serde_json::from_str(r#"{"balance":{}}"#).unwrap();
    assert_eq!(short, Amount::Balance { of: this(None) });
    assert!(serde_json::from_str::<Amount>(r#"{"value":1.5}"#).is_err());
}

#[test]
fn triggers_and_their_runs_are_stored() {
    let (_directory, _runtime, mut connection) = seeded();
    let trigger = Trigger {
        id: "sweep".to_owned(),
        chain: "bitcoin".to_owned(),
        network: "regtest".to_owned(),
        selector: Json(serde_json::from_str(r#"{"tag":"deposit"}"#).unwrap()),
        filter: None,
        check: Json(serde_json::from_str(r#""idle""#).unwrap()),
        action: Json(serde_json::from_str(r#"{"sweep":{"to":"a"}}"#).unwrap()),
        enabled: true,
        created_at: 1,
        updated_at: 1,
    };
    trigger::db::insert(&mut connection, &trigger).unwrap();
    assert!(trigger::db::insert(&mut connection, &trigger).is_err());
    let found = trigger::db::find_enabled(&mut connection, &network()).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].selector, trigger.selector);
    assert_eq!(found[0].filter, None);
    assert_eq!(found[0].check, trigger.check);

    assert!(
        trigger_run::db::find_last(&mut connection, "sweep")
            .unwrap()
            .is_none()
    );
    let first = start_run(&mut connection, "sweep", 10).unwrap();
    start_run(&mut connection, "sweep", 20).unwrap();
    assert_eq!(
        trigger_run::db::settle(&mut connection, &first.id, 30).unwrap(),
        1
    );
    assert_eq!(
        trigger_run::db::settle(&mut connection, &first.id, 40).unwrap(),
        0
    );
    let last = trigger_run::db::find_last(&mut connection, "sweep")
        .unwrap()
        .unwrap();
    assert_eq!((last.started_at, last.settled_at), (20, None));
    assert!(start_run(&mut connection, "missing", 10).is_err());

    assert_eq!(trigger::db::delete(&mut connection, "sweep").unwrap(), 1);
    assert!(
        trigger_run::db::find_last(&mut connection, "sweep")
            .unwrap()
            .is_none()
    );
}

#[test]
fn wallets_are_found_through_their_indexes() {
    let (_directory, _runtime, mut connection) = seeded();
    let c = &mut connection;
    let other = Address::new(Network::new(Chain::new("bitcoin"), "other"), "a");
    for address in ["a", "b", "c", "d", "z"]
        .map(address)
        .into_iter()
        .chain([other])
    {
        let wallet = Wallet::new(
            &address,
            crypto::Curve::Secp256k1,
            crypto::SecretBytes::new(vec![1]),
        )
        .unwrap();
        wallet::db::insert(c, &wallet).unwrap();
    }
    tag_with(c, &address("a"), "hot").unwrap();
    let find = |c: &mut SqliteConnection, selector: IndexedSelector<'_>| -> Vec<String> {
        wallet::db::find_indexed(c, selector)
            .unwrap()
            .iter()
            .map(|wallet| wallet.address.clone())
            .collect()
    };
    let regtest = network();
    // z has no index; every other wallet has one.
    assert_eq!(
        find(c, IndexedSelector::default()),
        ["a", "a", "b", "c", "d"]
    );
    let indexed = IndexedSelector {
        network: Some(&regtest),
        confirmed: Some(int(TIP).unwrap()),
        selector: None,
    };
    // d never indexed; c through its token index only.
    assert_eq!(find(c, indexed), ["a", "b", "c"]);
    let native = serde_json::from_str(r#"{"asset":"native"}"#).unwrap();
    assert_eq!(
        find(
            c,
            IndexedSelector {
                selector: Some(&native),
                ..indexed
            }
        ),
        ["a", "b"]
    );
    let hot = serde_json::from_str(r#"{"tag":"hot"}"#).unwrap();
    // The tag is on regtest's a, not on other's.
    let tagged = IndexedSelector {
        selector: Some(&hot),
        ..IndexedSelector::default()
    };
    assert_eq!(find(c, tagged), ["a"]);
}
