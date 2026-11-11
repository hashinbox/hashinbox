//! Pipes' Select and Check steps on a SQLite temp file: tags, selector
//! resolution in `models::WalletIndex`, confirmed balances, compares,
//! amounts, fees and run gates, and the JSON form.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use base::{Address, Asset, Chain, Decimal, FeeEstimator, Future, Network, Networked};
use diesel::{Connection as _, SqliteConnection, connection::SimpleConnection as _};

use crate::{
    models::{
        Json, Trigger, TriggerRun, Wallet, int, trigger,
        trigger::{
            Aggregate, Amount, Balance, Compare, Filter, Gate, Logic, Operator, Target, This,
        },
        trigger_run, wallet, wallet_index,
        wallet_index::Selection,
        wallet_index_selector::Selector,
    },
    usecase::{
        trigger::{Context, Trigger as Deciding},
        workers::{
            broadcast::SendIntent,
            trigger::{
                Invocation,
                actions::{Action as _, SendInput, Transfer},
            },
        },
    },
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
/// Every indexed address is a watch-only wallet.
/// Its indexer confirmed through block [`TIP`]. a native: received 1000
/// (block 1), spent 300 + fee 10 (block 2), reward 5 above the confirmed
/// block (block 3); b native: reward 50 (block 0); a tok: 2^200.
/// Mined legs carry their index, position, seq and running total, as the
/// store writes.
const SEED: &str = "
INSERT INTO block (id, chain, network, position, height, hash, archived_at) VALUES
 ('bitcoin.regtest.00','bitcoin','regtest',0,0,x'00',NULL),
 ('bitcoin.regtest.01','bitcoin','regtest',1,1,x'01',NULL),
 ('bitcoin.regtest.02','bitcoin','regtest',2,2,x'02',NULL),
 ('bitcoin.regtest.03','bitcoin','regtest',3,3,x'03',NULL),
 ('bitcoin.regtest.f5','bitcoin','regtest',5,5,x'f5',1),
 ('bitcoin.other.03','bitcoin','other',3,3,x'03',NULL);
INSERT INTO wallet (id, chain, network, address) VALUES
 ('bitcoin.regtest.a','bitcoin','regtest','a'),
 ('bitcoin.regtest.b','bitcoin','regtest','b'),
 ('bitcoin.regtest.c','bitcoin','regtest','c'),
 ('bitcoin.regtest.d','bitcoin','regtest','d'),
 ('bitcoin.regtest.e','bitcoin','regtest','e'),
 ('bitcoin.other.a','bitcoin','other','a');
INSERT INTO wallet_index (id, wallet_id, asset, requested_from, birthday, indexed_at, state) VALUES
 ('bitcoin.regtest.a.native','bitcoin.regtest.a','native',0,0,'bitcoin.regtest.03',0),
 ('bitcoin.regtest.b.native','bitcoin.regtest.b','native',0,0,'bitcoin.regtest.02',0),
 ('bitcoin.regtest.c.native','bitcoin.regtest.c','native',0,0,'bitcoin.regtest.01',0),
 ('bitcoin.regtest.d.native','bitcoin.regtest.d','native',0,0,NULL,0),
 ('bitcoin.regtest.e.native','bitcoin.regtest.e','native',0,0,'bitcoin.regtest.f5',0),
 ('bitcoin.regtest.a.tok','bitcoin.regtest.a','tok',0,0,'bitcoin.regtest.03',0),
 ('bitcoin.regtest.b.tok','bitcoin.regtest.b','tok',0,0,'bitcoin.regtest.03',0),
 ('bitcoin.regtest.c.tok','bitcoin.regtest.c','tok',0,0,'bitcoin.regtest.03',0),
 ('bitcoin.other.a.native','bitcoin.other.a','native',0,0,'bitcoin.other.03',0);
INSERT INTO \"transaction\" (id, block, chain, network, txid, \"order\", state, bytes) VALUES
 ('bitcoin.regtest.t1','bitcoin.regtest.01','bitcoin','regtest','t1',0,1,x''),
 ('bitcoin.regtest.t2','bitcoin.regtest.02','bitcoin','regtest','t2',0,1,x''),
 ('bitcoin.regtest.t3','bitcoin.regtest.f5','bitcoin','regtest','t3',0,1,x'');
INSERT INTO leg (id, block, \"transaction\", leg_index, kind, address, asset, amount,
  wallet_index_id, position, seq, total, total_received, total_sent) VALUES
 ('bitcoin.regtest.t1.0','bitcoin.regtest.01','bitcoin.regtest.t1',0,0,'a','native','1000',
  'bitcoin.regtest.a.native',1,1,'1000','1000','0'),
 ('bitcoin.regtest.t1.1','bitcoin.regtest.01','bitcoin.regtest.t1',1,0,'a','tok',
  '1606938044258990275541962092341162602522202993782792835301376','bitcoin.regtest.a.tok',1,1,
  '1606938044258990275541962092341162602522202993782792835301376',
  '1606938044258990275541962092341162602522202993782792835301376','0'),
 ('bitcoin.regtest.t2.0','bitcoin.regtest.02','bitcoin.regtest.t2',0,1,'a','native','300',
  'bitcoin.regtest.a.native',2,2,'700','1000','300'),
 ('bitcoin.regtest.t2.1','bitcoin.regtest.02','bitcoin.regtest.t2',1,4,'a','native','10',
  'bitcoin.regtest.a.native',2,3,'690','1000','310'),
 ('bitcoin.regtest.t3.0','bitcoin.regtest.f5','bitcoin.regtest.t3',0,0,'a','native','7777',
  'bitcoin.regtest.a.native',5,5,'8467','8782','310'),
 ('bitcoin.regtest.03.reward.0','bitcoin.regtest.03',NULL,0,5,'a','native','5',
  'bitcoin.regtest.a.native',3,4,'695','1005','310'),
 ('bitcoin.regtest.00.reward.0','bitcoin.regtest.00',NULL,0,5,'b','native','50',
  'bitcoin.regtest.b.native',0,1,'50','50','0');
UPDATE network_index SET tip = 3, confirmed = 2 WHERE chain = 'bitcoin' AND network = 'regtest';
";

/// A migrated, seeded temp file, a runtime for fee estimates, and a raw
/// connection to it.
fn seeded() -> (tempfile::TempDir, tokio::runtime::Runtime, SqliteConnection) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("test.db");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let db = crate::db::Db::open(&path).unwrap();
    runtime.block_on(crate::tests::migrate(&db)).unwrap();
    let mut connection = SqliteConnection::establish(path.to_str().unwrap()).unwrap();
    connection
        .batch_execute("PRAGMA foreign_keys = ON;")
        .unwrap();
    crate::db::register(&mut connection).unwrap();
    connection.batch_execute(SEED).unwrap();
    (directory, runtime, connection)
}

/// Tags the wallet of `address`, kept watch-only first when it is not;
/// whether its tags changed.
fn tag_with(
    connection: &mut SqliteConnection,
    address: &Address,
    name: &str,
) -> anyhow::Result<bool> {
    wallet::db::insert(connection, &Wallet::from_address(address)?)?;
    wallet::db::set_tag(connection, &Wallet::id(address)?, name, true)?
        .ok_or_else(|| anyhow::anyhow!("the wallet is kept"))
}

/// The tags of the wallet of `address`.
fn tags_of(connection: &mut SqliteConnection, address: &Address) -> Vec<String> {
    wallet::db::find(connection, &Wallet::id(address).unwrap())
        .unwrap()
        .map(|wallet| wallet.tags.0.into_iter().collect())
        .unwrap_or_default()
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
        .ids()
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
    assert!(tag_with(&mut connection, &a, "hot").unwrap());
    assert!(!tag_with(&mut connection, &a, "hot").unwrap());
    tag_with(&mut connection, &a, "deposit-1").unwrap();
    assert_eq!(tags_of(&mut connection, &a), ["deposit-1", "hot"]);
    let id = Wallet::id(&a).unwrap();
    assert_eq!(
        wallet::db::set_tag(&mut connection, &id, "hot", false).unwrap(),
        Some(true)
    );
    assert_eq!(tags_of(&mut connection, &a), ["deposit-1"]);
    assert_eq!(
        wallet::db::set_tag(&mut connection, "bitcoin.regtest.nobody", "hot", true).unwrap(),
        None
    );
    for bad in ["", "Hot", "a.b", "a_b", &"x".repeat(65)] {
        assert!(tag_with(&mut connection, &a, bad).is_err(), "{bad:?}");
    }
    // The schema refuses tags that are not JSON, written around the model.
    let raw = "UPDATE wallet SET tags = 'hot' WHERE id = 'bitcoin.regtest.a';";
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
    ); // No tag: only addresses no tag names, whatever the tag.
    tag_with(c, &address("b"), "cold").unwrap();
    assert_eq!(ids(&Selector::Untagged {}, c), [id("c", "tok")]);
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
fn balances_are_confirmed_exact_and_folded() {
    let (_directory, _runtime, mut connection) = seeded();
    let all = [
        id("a", "native"),
        id("b", "native"),
        id("a", "tok"),
        id("b", "tok"),
    ];
    let balances = wallet_index::db::find_by_ids(&mut connection, &network(), &all, 2).unwrap();
    // The reward above the confirmed block and the archived block's leg
    // are not counted.
    assert_eq!(confirmed(&balances, &id("a", "native")), decimal("690"));
    assert_eq!(confirmed(&balances, &id("b", "native")), decimal("50"));
    assert_eq!(
        confirmed(&balances, &id("a", "tok")),
        decimal("1606938044258990275541962092341162602522202993782792835301376")
    );
    assert_eq!(confirmed(&balances, &id("b", "tok")), Decimal::zero());
    assert_eq!(
        balances[&id("a", "native")].spendable().unwrap(),
        decimal("690")
    );
}

/// a sends 100 to b, fee 5; still pending.
fn pending_send(connection: &mut SqliteConnection) {
    connection
        .batch_execute(
            "INSERT INTO \"transaction\" (id, block, chain, network, txid, state, bytes, \
               sent_at, expires_at) \
             VALUES ('bitcoin.regtest.p',NULL,'bitcoin','regtest','p',0,x'',1,9); \
             INSERT INTO leg (id, block, \"transaction\", leg_index, kind, address, asset, amount, \
               wallet_index_id) \
             VALUES ('bitcoin.regtest.p.0',NULL,'bitcoin.regtest.p',0,1,'a','native','100', \
                       'bitcoin.regtest.a.native'), \
                    ('bitcoin.regtest.p.1',NULL,'bitcoin.regtest.p',1,4,'a','native','5', \
                       'bitcoin.regtest.a.native'), \
                    ('bitcoin.regtest.p.2',NULL,'bitcoin.regtest.p',2,0,'b','native','100', \
                       'bitcoin.regtest.b.native');",
        )
        .unwrap();
}

#[test]
fn a_pending_send_is_taken_off_what_its_sender_can_spend() {
    let (_directory, _runtime, mut connection) = seeded();
    pending_send(&mut connection);
    let ids = [id("a", "native"), id("b", "native")];
    let balances = wallet_index::db::find_by_ids(&mut connection, &network(), &ids, 2).unwrap();
    let a = &balances[&id("a", "native")];
    assert_eq!(
        (confirmed(&balances, &a.id), a.spendable().unwrap()),
        (decimal("690"), decimal("585"))
    );
    // What arrives counts once mined.
    let b = &balances[&id("b", "native")];
    assert_eq!(
        (confirmed(&balances, &b.id), b.spendable().unwrap()),
        (decimal("50"), decimal("50"))
    );
}

/// A fee estimator answering a fixed fee per spend, counting its calls and
/// keeping the recipients it was asked about.
struct Fees {
    network: Network,
    fee: Decimal,
    calls: AtomicUsize,
    to: std::sync::Mutex<Vec<Option<Address>>>,
}

impl Networked for Fees {
    fn network(&self) -> &Network {
        &self.network
    }
}

impl FeeEstimator for Fees {
    fn estimate<'a>(&'a self, spends: &'a base::Spends) -> Future<'a, anyhow::Result<Decimal>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.to
            .lock()
            .unwrap()
            .extend(spends.iter().map(|spend| spend.to.clone()));
        let fee = self.fee.clone() * Decimal::from(spends.len() as u64);
        Box::pin(async move { Ok(fee) })
    }
}

fn fees(fee: &str) -> Arc<Fees> {
    Arc::new(Fees {
        network: network(),
        fee: decimal(fee),
        calls: AtomicUsize::new(0),
        to: std::sync::Mutex::default(),
    })
}

fn open<'a>(
    connection: &'a mut SqliteConnection,
    runtime: &tokio::runtime::Runtime,
    fees: Arc<Fees>,
    run: Option<TriggerRun>,
) -> Deciding<'a> {
    let row = Trigger {
        id: "t".to_owned(),
        chain: "bitcoin".to_owned(),
        network: "regtest".to_owned(),
        group_id: "group".to_owned(),
        selector: Json(Selector::Ids(Vec::new())),
        filter: None,
        check: Json(Logic::All(Vec::new())),
        action: Json(serde_json::from_str(r#"{"name":"none"}"#).unwrap()),
        enabled: true,
        created_at: 1,
        updated_at: 1,
        last_round: None,
    };
    let assets = [("native", 8), ("tok", 2)]
        .into_iter()
        .map(|(id, decimals)| {
            base::AssetInfo::token(
                Asset {
                    network: network(),
                    id: id.to_owned(),
                },
                decimals,
            )
        })
        .collect();
    let context = Context {
        assets,
        estimator: fees,
        runtime: runtime.handle().clone(),
        now: 100_000,
    };
    Deciding::new(row, connection, context, run)
}

/// A run of the test trigger started at `started_at`.
fn run(started_at: i64) -> TriggerRun {
    TriggerRun::new("t", started_at)
}

fn compares(
    evaluation: &mut Deciding<'_>,
    compare: &Compare,
    this: &Selection,
) -> anyhow::Result<bool> {
    evaluation.check(&Logic::Leaf(Gate::Compare(compare.clone())), this)
}

fn checks(
    evaluation: &mut Deciding<'_>,
    check: &Logic<Gate>,
    this: &Selection,
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
        balance: Balance::Confirmed,
        operator,
        value,
    }
}

fn value(text: &str) -> Amount {
    Amount::Value(decimal(text))
}

/// The kept wallet indexes `ids` with their balances at the tip, as Select
/// would hand them on (fully indexed or not).
fn selection(connection: &mut SqliteConnection, ids: &[String]) -> Selection {
    wallet_index::db::find_by_ids(connection, &network(), ids, int(TIP).unwrap()).unwrap()
}

/// The balance of index `id` of `selection` at its final block.
fn confirmed(selection: &Selection, id: &str) -> Decimal {
    selection[id]
        .balance_confirmed
        .as_deref()
        .unwrap_or("0")
        .parse()
        .unwrap()
}

#[test]
fn aggregates_and_operators() {
    let (_directory, runtime, mut connection) = seeded();
    let both = selection(&mut connection, &[id("a", "native"), id("b", "native")]);
    let mut evaluation = open(&mut connection, &runtime, fees("1"), None);
    let empty = Selection::new(network(), int(TIP).unwrap());
    let native = || Target::Select(asset("native"));
    let mut holds = |of: Target, aggregate, operator, amount, on: &Selection| {
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
    let balance = || {
        Box::new(Amount::Balance {
            of: this(None),
            balance: Balance::Confirmed,
        })
    };
    let fee = || {
        Box::new(Amount::Fee {
            of: this(None),
            to: None,
        })
    };
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
fn a_fee_is_priced_for_its_one_recipient_or_a_new_address() {
    let (_directory, runtime, mut connection) = seeded();
    let a = selection(&mut connection, &[id("a", "native")]);
    let estimator = fees("100");
    let mut evaluation = open(&mut connection, &runtime, estimator.clone(), None);
    let fee_to = |to: Option<Selector>| Amount::Fee {
        of: this(None),
        to: to.map(Target::Select),
    };
    let one = Selector::Ids(vec![id("b", "native")]);
    let two = Selector::Ids(vec![id("b", "native"), id("c", "tok")]);
    for to in [Some(one), Some(two), None] {
        let fee = fee_to(to);
        assert!(
            compares(
                &mut evaluation,
                &compare(this(None), Aggregate::Sum, Operator::Above, fee),
                &a
            )
            .unwrap()
        );
    }
    // `b` alone is its recipient; two addresses, or none, a new address.
    let asked: Vec<Option<String>> = estimator
        .to
        .lock()
        .unwrap()
        .iter()
        .map(|to| to.as_ref().map(|to| to.value.clone()))
        .collect();
    assert_eq!(asked, [Some("b".to_owned()), None, None]);
}

#[test]
fn holders_are_keyed_wallets_with_a_balance_of_the_asset() {
    let (_directory, _runtime, mut connection) = seeded();
    let c = &mut connection;
    let pair = |name: &str, asset: &str| {
        let asset = Asset {
            network: network(),
            id: asset.to_owned(),
        };
        (address(name), asset)
    };
    let asked = [
        pair("a", "native"),
        pair("b", "native"),
        pair("b", "tok"),
        pair("a", "tok"),
        pair("c", "native"),
    ];
    let held = |c: &mut SqliteConnection| {
        let mut held =
            wallet_index::db::find_held(c, asked.iter().map(|(to, asset)| (to, asset))).unwrap();
        held.sort();
        held
    };
    // Watch-only wallets hold nothing we can count on.
    assert!(held(c).is_empty());

    // Keys kept for a and b: a holds both its assets, b its 50 native (a
    // reward) and no tok; c holds native, but watch-only.
    c.batch_execute(
        "UPDATE wallet SET secret = x'01', curve = 'secp256k1'
         WHERE id IN ('bitcoin.regtest.a', 'bitcoin.regtest.b')",
    )
    .unwrap();
    let mut expected = vec![pair("a", "native"), pair("a", "tok"), pair("b", "native")];
    expected.sort();
    assert_eq!(held(c), expected);
    assert!(wallet_index::db::find_held(c, []).unwrap().is_empty());
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

    // A run too recent stops the check before the fee is read (now is
    // 100 000 ms).
    let (_directory, runtime, mut connection) = seeded();
    let estimator = fees("1");
    let a = selection(&mut connection, &[id("a", "native")]);
    let mut evaluation = open(
        &mut connection,
        &runtime,
        estimator.clone(),
        Some(run(50_000)),
    );
    let check = Logic::All(vec![
        Logic::Leaf(Gate::Since { seconds: 60 }),
        Logic::Leaf(Gate::Compare(compare(
            this(None),
            Aggregate::Sum,
            Operator::Above,
            Amount::Fee {
                of: this(None),
                to: None,
            },
        ))),
    ]);
    assert!(!checks(&mut evaluation, &check, &a).unwrap());
    assert_eq!(estimator.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn since_reads_the_last_run() {
    let (_directory, runtime, mut connection) = seeded();
    let empty = Selection::new(network(), int(TIP).unwrap());
    let since = |seconds| Logic::Leaf(Gate::Since { seconds });
    // now is 100 000 ms.
    for (started_at, passes) in [(40_001, false), (40_000, true)] {
        let last = Some(run(started_at));
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
    let check = r#"{"since":{"seconds":60}}"#;
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
            Amount::Fee {
                of: this(None),
                to: None
            }
        )))
    );
    assert_eq!(serde_json::to_string(&parsed).unwrap(), filter);
    let parsed: Logic<Gate> = serde_json::from_str(check).unwrap();
    assert_eq!(parsed, Logic::Leaf(Gate::Since { seconds: 60 }));
    assert_eq!(serde_json::to_string(&parsed).unwrap(), check);
    // Combinators, Since and decimal strings; `{"balance":{}}` is This.
    let full = r#"{"all":[{"since":{"seconds":60}},{"not":{"compare":{"of":{"select":{"ids":["x"]}},"aggregate":"max","operator":"at_most","value":{"max":[{"sub":[{"balance":{"of":{"this":{}}}},{"value":"1.25"}]},{"value":"0"}]}}}}]}"#;
    let parsed: Logic<Gate> = serde_json::from_str(full).unwrap();
    assert_eq!(serde_json::to_string(&parsed).unwrap(), full);
    let short: Amount = serde_json::from_str(r#"{"balance":{}}"#).unwrap();
    assert_eq!(
        short,
        Amount::Balance {
            of: this(None),
            balance: Balance::Confirmed,
        }
    );
    assert!(serde_json::from_str::<Amount>(r#"{"value":1.5}"#).is_err());
}

#[test]
fn triggers_and_their_runs_are_stored() {
    let (_directory, _runtime, mut connection) = seeded();
    let trigger = Trigger {
        id: "sweep".to_owned(),
        chain: "bitcoin".to_owned(),
        network: "regtest".to_owned(),
        group_id: "group".to_owned(),
        selector: Json(serde_json::from_str(r#"{"tag":"deposit"}"#).unwrap()),
        filter: None,
        check: Json(serde_json::from_str(r#"{"since":{"seconds":60}}"#).unwrap()),
        action: Json(serde_json::from_str(r#"{"name":"send","input":{"source":{"this":{}},"asset":"native","destination":{"select":{"ids":["x"]}},"amount":{"balance":{}}}}"#).unwrap()),
        enabled: true,
        created_at: 1,
        updated_at: 1,
        last_round: None,
    };
    trigger::db::insert(&mut connection, &trigger).unwrap();
    assert!(trigger::db::insert(&mut connection, &trigger).is_err());
    let found = trigger::db::find_enabled(&mut connection, &network()).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0.selector, trigger.selector);
    assert_eq!(found[0].0.filter, None);
    assert_eq!(found[0].0.check, trigger.check);
    assert!(found[0].1.is_none(), "never run");

    start_run(&mut connection, "sweep", 10).unwrap();
    start_run(&mut connection, "sweep", 20).unwrap();
    let found = trigger::db::find_enabled(&mut connection, &network()).unwrap();
    assert_eq!(found[0].1.as_ref().map(|run| run.started_at), Some(20));
    assert!(start_run(&mut connection, "missing", 10).is_err());

    assert_eq!(trigger::db::delete(&mut connection, "sweep").unwrap(), 1);
    assert!(
        trigger_run::db::find_page(
            &mut connection,
            "sweep",
            crate::db::Page {
                after: None,
                limit: 10
            }
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn a_tag_picks_its_wallets_indexes_on_their_network_only() {
    let (_directory, _runtime, mut connection) = seeded();
    let c = &mut connection;
    let other = Network::new(Chain::new("bitcoin"), "other");
    let picked = |c: &mut SqliteConnection, selector: &Selector| {
        wallet_index::db::select(c, &other, selector, 3)
            .unwrap()
            .ids()
    };
    tag_with(c, &address("a"), "hot").unwrap();
    assert_eq!(ids(&tag("hot"), c), [id("a", "native"), id("a", "tok")]);
    // The tag is on regtest's a, not on other's; other's a is still picked.
    assert!(picked(c, &tag("hot")).is_empty());
    assert_eq!(picked(c, &asset("native")), ["bitcoin.other.a.native"]);
}

#[test]
fn a_filter_keeps_each_index_on_its_own() {
    let (_directory, runtime, mut connection) = seeded();
    let both = selection(&mut connection, &[id("a", "native"), id("b", "native")]);
    let mut evaluation = open(&mut connection, &runtime, fees("1"), None);
    // a 690, b 50 sats: only a holds more than 100 sats; together they would.
    let filter = Logic::Leaf(Filter::Compare(compare(
        this(None),
        Aggregate::Sum,
        Operator::Above,
        value("0.000001"),
    )));
    let kept = evaluation.filter(&filter, &both).unwrap();
    assert_eq!(kept.ids(), vec![id("a", "native")]);
    assert_eq!(kept.confirmed_at(), both.confirmed_at());
}

fn send(source: Target, destination: Target, amount: Amount) -> SendInput {
    SendInput {
        source,
        asset: "native".to_owned(),
        destination,
        amount,
    }
}

fn ids_of(names: &[&str]) -> Target {
    Target::Select(Selector::Ids(
        names.iter().map(|name| id(name, "native")).collect(),
    ))
}

#[test]
fn a_send_pays_per_source_or_per_destination() {
    let (_directory, runtime, mut connection) = seeded();
    // Only a keyed source sends.
    for name in ["a", "b"] {
        let key = crypto::SecretBytes::new(vec![1]);
        let keyed = Wallet::new(&address(name), crypto::Curve::Secp256k1, key).unwrap();
        wallet::db::insert(&mut connection, &keyed).unwrap();
    }
    let both = selection(&mut connection, &[id("a", "native"), id("b", "native")]);
    let b = selection(&mut connection, &[id("b", "native")]);
    let mut evaluation = open(&mut connection, &runtime, fees("100"), None);
    let balance = || {
        Box::new(Amount::Balance {
            of: this(None),
            balance: Balance::Confirmed,
        })
    };
    let fee = || {
        Box::new(Amount::Fee {
            of: this(None),
            to: None,
        })
    };

    // Many sources to one: everything less the fee, each source alone;
    // b's 50 sats do not cover its 100 sat fee, so it is skipped.
    let sweep = send(this(None), ids_of(&["b"]), Amount::Sub(balance(), fee()));
    // Each started job's sender and its (recipient, amount) transfers.
    let mut plan = |send: &SendInput, this: &Selection| {
        let mut invocation = Invocation::new(this.network(), this, &mut evaluation);
        Transfer.run(&mut invocation, &serde_json::to_value(send)?)?;
        let sends = invocation.started().into_iter().map(|job| {
            let intent: SendIntent = serde_json::from_value(job.input).unwrap();
            let paid = intent
                .transfers
                .into_iter()
                .map(|p| (p.to, p.amount))
                .collect();
            (intent.address, paid)
        });
        anyhow::Ok(sends.collect::<Vec<(String, Vec<(String, String)>)>>())
    };
    let sent = |from: &str, to: &str, amount: &str| {
        vec![(
            address(from).value,
            vec![(address(to).value, amount.to_owned())],
        )]
    };
    assert_eq!(plan(&sweep, &both).unwrap(), sent("a", "b", "590"));

    // One source to many: each destination's shortfall up to 100 sats.
    let shortfall = Amount::Max(
        Box::new(Amount::Sub(Box::new(value("0.000001")), balance())),
        Box::new(value("0")),
    );
    let fund = send(ids_of(&["a"]), this(None), shortfall);
    assert_eq!(plan(&fund, &b).unwrap(), sent("a", "b", "50"));

    // Many to many is refused.
    let tangled = send(
        this(None),
        ids_of(&["a", "b"]),
        Amount::Balance {
            of: this(None),
            balance: Balance::Confirmed,
        },
    );
    assert!(plan(&tangled, &both).is_err());
}

#[test]
fn a_pending_balance_counts_what_is_already_on_its_way() {
    let (_directory, runtime, mut connection) = seeded();
    pending_send(&mut connection);
    let balances = selection(&mut connection, &[id("a", "native"), id("b", "native")]);
    let held = |name: &str, balance| balances[&id(name, "native")].held(balance).unwrap();
    // a: 695 at the last block (the reward included) less 105 on its way out.
    assert_eq!(held("a", Balance::Pending), decimal("590"));
    // b: 50, and 100 on its way in.
    assert_eq!(held("b", Balance::Confirmed), decimal("50"));
    assert_eq!(held("b", Balance::Pending), decimal("150"));
    // A prefund of b while it holds under 100 sats: the confirmed balance
    // says it is short; the pending one, with 100 on its way, does not.
    let only_b = selection(&mut connection, &[id("b", "native")]);
    let mut evaluation = open(&mut connection, &runtime, fees("1"), None);
    let short = |balance| Compare {
        balance,
        ..compare(
            this(None),
            Aggregate::Sum,
            Operator::Below,
            value("0.000001"),
        )
    };
    assert!(compares(&mut evaluation, &short(Balance::Confirmed), &only_b).unwrap());
    assert!(!compares(&mut evaluation, &short(Balance::Pending), &only_b).unwrap());
}
