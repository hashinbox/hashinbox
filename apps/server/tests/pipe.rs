//! Triggers on a SQLite temp file: tags, selector resolution in
//! `models::WalletIndex`, confirmed and pending balances, the store a round
//! reads (`usecase::trigger::Sqlite`), `send` over it, the stored JSON and
//! the rows. How a trigger evaluates is the `trigger` package's own tests.

use std::collections::BTreeMap;

use ::trigger::{
    Context,
    actions::Action as _,
    amounts::{self, Arithmetic},
    conditions::{self, Aggregate, Logic, Operator},
    selection::{BalanceKind, Selector, Target, This},
    testing::Fees,
};
use base::{Address, Asset, Chain, Decimal, Network};
use diesel::{Connection as _, SqliteConnection, connection::SimpleConnection as _};

use crate::{
    models::{
        Json, Trigger, TriggerRun, Wallet, int, trigger,
        trigger::{Amount, AmountTerm, Check, Compare, Filter},
        trigger_run, wallet, wallet_index,
        wallet_index::WalletIndex,
    },
    usecase::{
        trigger::Sqlite,
        workers::{
            broadcast::SendIntent,
            trigger::actions::{SendInput, Transfer},
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
INSERT INTO block (id, chain, network, position, height, hash, reorged_at) VALUES
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
    wallet::db::set_tag(connection, &wallet::WalletId::of(address)?, name, true)?
        .ok_or_else(|| anyhow::anyhow!("the wallet is kept"))
}

/// The tags of the wallet of `address`.
fn tags_of(connection: &mut SqliteConnection, address: &Address) -> Vec<String> {
    wallet::db::find(connection, wallet::WalletId::of(address).unwrap())
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
    let found = wallet_index::db::select(connection, &network(), selector, 2).unwrap();
    found.into_iter().map(|one| one.id).collect()
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
    let id = wallet::WalletId::of(&a).unwrap();
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
               sent_at) \
             VALUES ('bitcoin.regtest.p',NULL,'bitcoin','regtest','p',0,x'',1); \
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
        (confirmed(&balances, a.id.as_ref()), a.spendable().unwrap()),
        (decimal("690"), decimal("585"))
    );
    // What arrives counts once mined.
    let b = &balances[&id("b", "native")];
    assert_eq!(
        (confirmed(&balances, b.id.as_ref()), b.spendable().unwrap()),
        (decimal("50"), decimal("50"))
    );
}

/// The balance of index `id` of `balances` at its final block.
fn confirmed(balances: &BTreeMap<String, WalletIndex>, id: &str) -> Decimal {
    balances[id]
        .balance_confirmed
        .as_deref()
        .unwrap_or("0")
        .parse()
        .unwrap()
}

fn this(asset: Option<&str>) -> Target {
    Target::This(This {
        asset: asset.map(str::to_owned),
    })
}

fn value(text: &str) -> Amount {
    Arithmetic::Term(AmountTerm::Value(amounts::Value(decimal(text))))
}

/// The confirmed balance of `This`.
fn balance() -> Box<Amount> {
    Box::new(Arithmetic::Term(AmountTerm::Balance(
        amounts::Balance::default(),
    )))
}

/// The fee of sending everything `This` holds to a new address.
fn fee() -> Box<Amount> {
    Box::new(Arithmetic::Term(AmountTerm::Fee(amounts::Fee::default())))
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
fn a_pair_is_found_only_fully_indexed() {
    let (_directory, _runtime, mut connection) = seeded();
    let native = |name: &str| (address(name), Asset::native(network()));
    // c's native index is behind the confirmed block, d's never indexed.
    let wanted = [native("a"), native("b"), native("c"), native("d")];
    let found = wallet_index::db::find(&mut connection, &network(), &wanted, 2).unwrap();
    let found: Vec<String> = found.into_iter().map(|one| one.id).collect();
    assert_eq!(found, [id("a", "native"), id("b", "native")]);
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
        Logic::One(Filter::Compare(Compare {
            of: this(Some("native")),
            aggregate: Aggregate::Sum,
            balance: BalanceKind::Confirmed,
            operator: Operator::AtLeast,
            value: *fee(),
        }))
    );
    assert_eq!(serde_json::to_string(&parsed).unwrap(), filter);
    let parsed: Logic<Check> = serde_json::from_str(check).unwrap();
    let since = conditions::Since { seconds: 60 };
    assert_eq!(parsed, Logic::One(Check::Since(since)));
    assert_eq!(serde_json::to_string(&parsed).unwrap(), check);
    // Combinators, Since and decimal strings; `{"balance":{}}` is This.
    let full = r#"{"all":[{"since":{"seconds":60}},{"not":{"compare":{"of":{"select":{"ids":["x"]}},"aggregate":"max","operator":"at_most","value":{"max":[{"sub":[{"balance":{"of":{"this":{}}}},{"value":"1.25"}]},{"value":"0"}]}}}}]}"#;
    let parsed: Logic<Check> = serde_json::from_str(full).unwrap();
    assert_eq!(serde_json::to_string(&parsed).unwrap(), full);
    let short: Amount = serde_json::from_str(r#"{"balance":{}}"#).unwrap();
    assert_eq!(short, *balance());
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
    assert_eq!(found[0].trigger, trigger.definition());
    assert_eq!(found[0].trigger.filter, None);
    assert!(found[0].last_run.is_none(), "never run");

    start_run(&mut connection, "sweep", 10).unwrap();
    start_run(&mut connection, "sweep", 20).unwrap();
    let found = trigger::db::find_enabled(&mut connection, &network()).unwrap();
    assert_eq!(found[0].last_run, Some(20));
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
    let picked = |c: &mut SqliteConnection, selector: &Selector| -> Vec<String> {
        let found = wallet_index::db::select(c, &other, selector, 3).unwrap();
        found.into_iter().map(|one| one.id).collect()
    };
    tag_with(c, &address("a"), "hot").unwrap();
    assert_eq!(ids(&tag("hot"), c), [id("a", "native"), id("a", "tok")]);
    // The tag is on regtest's a, not on other's; other's a is still picked.
    assert!(picked(c, &tag("hot")).is_empty());
    assert_eq!(picked(c, &asset("native")), ["bitcoin.other.a.native"]);
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

/// Each started job's sender, recipient and amount: one payment each.
type Plan = Vec<(String, String, String)>;

/// The jobs `send` starts over the store with `this` (native indexes by
/// name) kept, each spend priced 100 sats.
fn plan(
    connection: &mut SqliteConnection,
    send: &SendInput,
    this: &[&str],
) -> anyhow::Result<Plan> {
    let network = network();
    let assets = [("native", 8), ("tok", 2)]
        .into_iter()
        .map(|(id, decimals)| {
            let id = id.to_owned();
            base::AssetInfo::token(
                Asset {
                    network: network.clone(),
                    id,
                },
                decimals,
            )
        })
        .collect();
    let fees = Fees::new(network.clone(), decimal("100"));
    let mut store = Sqlite::new(connection, network.clone(), int(TIP)?);
    let mut on = Context::new(&mut store, &fees, &assets, &network, 100_000, None);
    let Target::Select(kept) = ids_of(this) else {
        unreachable!("ids are a selector")
    };
    let kept = futures_executor::block_on(on.select(&kept))?;
    let jobs = futures_executor::block_on(Transfer.run(send, &mut on.with(kept)))?;
    let sends = jobs.into_iter().map(|job| {
        let intent: SendIntent = serde_json::from_value(job.input).unwrap();
        (intent.address, intent.to, intent.amount)
    });
    Ok(sends.collect())
}

#[test]
fn a_send_pays_per_source_or_per_destination() {
    let (_directory, _runtime, mut connection) = seeded();
    // Only a keyed source sends.
    for name in ["a", "b"] {
        let key = crypto::SecretBytes::new(vec![1]);
        let keyed = Wallet::new(&address(name), crypto::Curve::Secp256k1, key).unwrap();
        wallet::db::insert(&mut connection, &keyed).unwrap();
    }
    let c = &mut connection;
    let sent = |from: &str, to: &str, amount: &str| {
        vec![(address(from).value, address(to).value, amount.to_owned())]
    };

    // Many sources to one: everything less the fee, each source alone;
    // b's 50 sats do not cover its 100 sat fee, so it is skipped.
    let sweep = send(
        this(None),
        ids_of(&["b"]),
        Arithmetic::Sub(balance(), fee()),
    );
    assert_eq!(plan(c, &sweep, &["a", "b"]).unwrap(), sent("a", "b", "590"));

    // One source to many: each destination's shortfall up to 100 sats.
    let shortfall = Arithmetic::Max(
        Box::new(Arithmetic::Sub(Box::new(value("0.000001")), balance())),
        Box::new(value("0")),
    );
    let fund = send(ids_of(&["a"]), this(None), shortfall);
    assert_eq!(plan(c, &fund, &["b"]).unwrap(), sent("a", "b", "50"));

    // Many to many is refused.
    let tangled = send(this(None), ids_of(&["a", "b"]), *balance());
    assert!(plan(c, &tangled, &["a", "b"]).is_err());
}

#[test]
fn a_send_to_many_is_one_job_per_payment_in_order() {
    let (_directory, _runtime, mut connection) = seeded();
    let key = crypto::SecretBytes::new(vec![1]);
    let keyed = Wallet::new(&address("a"), crypto::Curve::Secp256k1, key).unwrap();
    wallet::db::insert(&mut connection, &keyed).unwrap();
    // f: a second destination fully indexed in native.
    connection
        .batch_execute(
            "INSERT INTO wallet (id, chain, network, address) VALUES
              ('bitcoin.regtest.f','bitcoin','regtest','f');
             INSERT INTO wallet_index
              (id, wallet_id, asset, requested_from, birthday, indexed_at, state) VALUES
              ('bitcoin.regtest.f.native','bitcoin.regtest.f','native',0,0,'bitcoin.regtest.03',0);",
        )
        .unwrap();
    let pay = send(ids_of(&["a"]), this(None), value("0.0000001"));
    let paid = |to: &str| (address("a").value, address(to).value, "10".to_owned());
    assert_eq!(
        plan(&mut connection, &pay, &["b", "f"]).unwrap(),
        [paid("b"), paid("f")]
    );
}

#[test]
fn a_pending_balance_counts_what_is_already_on_its_way() {
    let (_directory, _runtime, mut connection) = seeded();
    pending_send(&mut connection);
    let ids = [id("a", "native"), id("b", "native")];
    let balances = wallet_index::db::find_by_ids(&mut connection, &network(), &ids, 2).unwrap();
    let held = |name: &str, kind| {
        let index = balances[&id(name, "native")].clone().index().unwrap();
        index.held(kind).clone()
    };
    // a: 695 at the last block (the reward included) less 105 on its way out.
    assert_eq!(held("a", BalanceKind::Pending), decimal("590"));
    // b: 50, and 100 on its way in.
    assert_eq!(held("b", BalanceKind::Confirmed), decimal("50"));
    assert_eq!(held("b", BalanceKind::Pending), decimal("150"));
}
