//! Throughput benchmark for the PostgreSQL indexing backend.
//!
//! Commits synthetic blocks through the real `Blocks::add` path, then reads
//! pages back through `Transactions::list` and `Outputs::list`, so the numbers
//! cover exactly what an indexer and an API do in production.
//!
//!   cargo run --release -p indexing-postgres --example bench -- <url>
//!
//! Shape is environment-driven so variants need no recompile:
//!
//!   BENCH_BLOCKS      blocks to commit                       default 200
//!   BENCH_TXS         transactions per block                 default 40
//!   BENCH_MOVEMENTS   movements per transaction              default 2
//!   BENCH_ADDRESSES   distinct watched addresses             default 16
//!   BENCH_CREATED     outputs created per block              default 40
//!   BENCH_SPENT       outputs spent per block                default 30
//!   BENCH_SCOPE       network label for an intentional rerun default unique
//!   BENCH_RESET       clear only the exact run scope (1/0)    default 1

#[path = "bench/cleanup.rs"]
mod cleanup;

use std::{
    env,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use indexing::{
    AssetId, BlockAddition, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, Blocks,
    CanonicalAddress, ChainId, Decimal, HistoryQuery, IndexScope, IndexedOutput, InterpretedBlock,
    MovementId, ObservationDraft, ObservationDraftStatus, OutputChanges, OutputId, OutputKey,
    OutputRequest, Outputs, TransactionRef, Transactions, ValueMovement,
};

const RETENTION: u64 = 100;

struct Profile {
    blocks: u64,
    txs: usize,
    movements: usize,
    addresses: usize,
    created: usize,
    spent: usize,
}

impl Profile {
    fn from_env() -> Self {
        let number = |key: &str, fallback: usize| {
            env::var(key)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(fallback)
        };
        Self {
            blocks: number("BENCH_BLOCKS", 200) as u64,
            txs: number("BENCH_TXS", 40),
            movements: number("BENCH_MOVEMENTS", 2),
            addresses: number("BENCH_ADDRESSES", 16).max(2),
            created: number("BENCH_CREATED", 40),
            spent: number("BENCH_SPENT", 30),
        }
    }

    /// Rows one block writes, so throughput can be reported per row as well as
    /// per block. Every transaction is stored once per address it touches.
    fn rows(&self) -> usize {
        let addresses_per_tx = 2;
        let history = self.txs * addresses_per_tx;
        let movements = history * self.movements;
        history + movements + self.created + self.spent * 2
    }

    fn address(&self, scope: &IndexScope, index: usize) -> CanonicalAddress {
        let index = index % self.addresses;
        CanonicalAddress {
            scope: scope.clone(),
            value: format!("addr-{index:04}"),
        }
    }

    fn transactions(
        &self,
        scope: &IndexScope,
        height: u64,
        asset: &AssetId,
    ) -> Vec<ObservationDraft> {
        let mut transactions = Vec::with_capacity(self.txs);
        for index in 0..self.txs {
            let from = self.address(scope, index);
            let to = self.address(scope, index + 1);
            let movements = (0..self.movements)
                .map(|ordinal| ValueMovement::Transfer {
                    id: MovementId(format!("{height}-{index}-{ordinal}")),
                    asset: asset.clone(),
                    amount: Decimal::from(1_000 + ordinal as u64),
                    from: from.clone(),
                    to: to.clone(),
                })
                .collect();
            transactions.push(ObservationDraft {
                scope: scope.clone(),
                transaction_id: transaction(scope, height, index),
                status: ObservationDraftStatus::Included,
                movements,
                fee: None,
            });
        }
        transactions
    }

    fn outputs(&self, scope: &IndexScope, height: u64, asset: &AssetId) -> Vec<IndexedOutput> {
        let mut created = Vec::with_capacity(self.created);
        for index in 0..self.created {
            let owner = self.address(scope, index);
            created.push(IndexedOutput {
                id: OutputId {
                    transaction: transaction(scope, height, index),
                    index: index as u32,
                },
                address: owner,
                asset: asset.clone(),
                amount: Decimal::from(50_000 + index as u64),
                // A P2WPKH script is 22 bytes; a P2WSH witness script is larger.
                evidence: vec![0x51; 22],
                created_at: BlockHeight(height),
                coinbase: index == 0,
            });
        }
        created
    }
}

fn scope() -> IndexScope {
    let network = env::var("BENCH_SCOPE").unwrap_or_else(|_| {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        format!("run-{}-{stamp}", std::process::id())
    });
    IndexScope {
        chain: ChainId("benchmark".into()),
        network,
    }
}

fn transaction(scope: &IndexScope, height: u64, index: usize) -> TransactionRef {
    TransactionRef {
        scope: scope.clone(),
        value: format!("{height:010}-{index:05}"),
    }
}

/// One block's worth of interpreted facts, spending outputs handed in from the
/// previous block so the spend path is exercised the way a real chain does.
fn interpret(
    scope: &IndexScope,
    profile: &Profile,
    height: u64,
    parent: Option<&BlockRef>,
    spend: Vec<OutputKey>,
) -> (InterpretedBlock, Vec<OutputKey>) {
    let block = BlockRef {
        position: BlockPosition(height),
        height: BlockHeight(height),
        hash: BlockHash(format!("hash-{height:010}").into_bytes()),
        parent: parent.map(|block| BlockParent {
            position: block.position,
            hash: block.hash.clone(),
        }),
        timestamp: Some(1_700_000_000 + height),
    };
    let asset = AssetId {
        chain: scope.chain.clone(),
        asset: "native".into(),
    };
    let transactions = profile.transactions(scope, height, &asset);
    let created = profile.outputs(scope, height, &asset);
    let next_spend = created
        .iter()
        .take(profile.spent)
        .map(IndexedOutput::key)
        .collect();

    (
        InterpretedBlock {
            block,
            transactions,
            outputs: OutputChanges {
                created,
                spent: spend,
                tracked_spends: Vec::new(),
            },
        },
        next_spend,
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rate = |count: f64, seconds: f64| {
        if seconds <= 0.0 { 0.0 } else { count / seconds }
    };
    let url = env::args()
        .nth(1)
        .unwrap_or_else(|| "postgres://prop@127.0.0.1:5433/integration-test".to_owned());
    let profile = Profile::from_env();
    let scope = scope();
    let pool = indexing_postgres::pool(&url, 8)?;

    if env::var("BENCH_RESET").unwrap_or_else(|_| "1".into()) != "0" {
        let mut client = pool.get().await?;
        cleanup::clear_scope(&mut client, &scope).await?;
    }

    let repository = indexing_postgres::Repository::new(pool, scope.clone())?;

    println!(
        "commit: {} blocks x {} tx x {} movements + {} created / {} spent  (~{} rows/block)",
        profile.blocks,
        profile.txs,
        profile.movements,
        profile.created,
        profile.spent,
        profile.rows(),
    );

    // ------------------------------------------------------------ commit path
    let mut parent: Option<BlockRef> = None;
    let mut spend: Vec<OutputKey> = Vec::new();
    let mut worst = 0.0_f64;
    let started = Instant::now();
    for height in 1..=profile.blocks {
        let (interpreted, next_spend) = interpret(&scope, &profile, height, parent.as_ref(), spend);
        let block = interpreted.block.clone();
        let addition = BlockAddition::new(scope.clone(), parent.clone(), RETENTION, interpreted)?;
        let one = Instant::now();
        repository.add(addition).await?;
        worst = worst.max(one.elapsed().as_secs_f64());
        parent = Some(block);
        spend = next_spend;
    }
    let commit = started.elapsed().as_secs_f64();
    println!(
        "  commit  {commit:8.3}s  {:8.1} blocks/s  {:9.0} rows/s  worst block {:.1}ms",
        rate(profile.blocks as f64, commit),
        rate((profile.blocks as usize * profile.rows()) as f64, commit),
        worst * 1000.0,
    );

    // -------------------------------------------------------------- read path
    let pages = 50;
    let started = Instant::now();
    let mut transactions = 0;
    for index in 0..pages {
        let page = Transactions::list(
            &repository,
            HistoryQuery {
                scope: scope.clone(),
                address: profile.address(&scope, index),
                after: None,
                limit: 100,
            },
        )
        .await?;
        transactions += page.transactions.len();
    }
    let history = started.elapsed().as_secs_f64();
    println!(
        "  history {history:8.3}s  {:8.1} pages/s  {:8.2}ms/page  ({transactions} tx read)",
        rate(pages as f64, history),
        history * 1000.0 / pages as f64,
    );

    let started = Instant::now();
    let mut listed = 0;
    for index in 0..pages {
        let page = Outputs::list(
            &repository,
            OutputRequest {
                scope: scope.clone(),
                address: profile.address(&scope, index),
                after: None,
                limit: 100,
            },
        )
        .await?;
        listed += page.outputs.len();
    }
    let outputs = started.elapsed().as_secs_f64();
    println!(
        "  outputs {outputs:8.3}s  {:8.1} pages/s  {:8.2}ms/page  ({listed} outputs read)",
        rate(pages as f64, outputs),
        outputs * 1000.0 / pages as f64,
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> Profile {
        Profile {
            blocks: 2,
            txs: 2,
            movements: 2,
            addresses: 2,
            created: 3,
            spent: 2,
        }
    }

    fn fixture_scope() -> IndexScope {
        IndexScope {
            chain: ChainId("benchmark".into()),
            network: "fixture-isolated".into(),
        }
    }

    #[test]
    fn empty_workload_keeps_supplied_spends_and_complete_parent_coordinates() {
        let scope = fixture_scope();
        let profile = Profile {
            txs: 0,
            movements: 0,
            created: 0,
            spent: 9,
            ..profile()
        };
        let previous = OutputKey {
            address: CanonicalAddress {
                scope: scope.clone(),
                value: "prior-owner".into(),
            },
            output: OutputId {
                transaction: TransactionRef {
                    scope: scope.clone(),
                    value: "older".into(),
                },
                index: 7,
            },
        };
        let parent = BlockRef {
            position: BlockPosition(42),
            height: BlockHeight(9),
            hash: BlockHash(vec![4, 2]),
            parent: None,
            timestamp: None,
        };
        let (value, next) = interpret(&scope, &profile, 10, Some(&parent), vec![previous.clone()]);
        assert_eq!(
            value.block,
            BlockRef {
                position: BlockPosition(10),
                height: BlockHeight(10),
                hash: BlockHash(b"hash-0000000010".to_vec()),
                parent: Some(BlockParent {
                    position: BlockPosition(42),
                    hash: BlockHash(vec![4, 2])
                }),
                timestamp: Some(1_700_000_010),
            }
        );
        assert!(value.transactions.is_empty());
        assert!(value.outputs.created.is_empty());
        assert_eq!(value.outputs.spent, [previous]);
        assert!(value.outputs.tracked_spends.is_empty());
        assert!(next.is_empty());
    }

    #[test]
    fn populated_workload_keeps_exact_ordered_transaction_and_output_facts() {
        let scope = fixture_scope();
        let profile = profile();
        let (value, next) = interpret(&scope, &profile, 10, None, Vec::new());
        assert_eq!(value.transactions.len(), 2);
        assert_eq!(value.outputs.created.len(), 3);
        assert_eq!(value.block.parent, None);
        let ids = ["0000000010-00000", "0000000010-00001", "0000000010-00002"];
        for (index, draft) in value.transactions.iter().enumerate() {
            assert_eq!(draft.scope, scope);
            assert_eq!(
                draft.transaction_id,
                TransactionRef {
                    scope: scope.clone(),
                    value: ids[index].into(),
                }
            );
            assert_eq!(draft.status, ObservationDraftStatus::Included);
            assert_eq!(draft.fee, None);
            assert_eq!(draft.movements.len(), 2);
        }
        for (tx, ordinal, id, amount, from, to) in [
            (0, 0, "10-0-0", 1_000_u64, "addr-0000", "addr-0001"),
            (0, 1, "10-0-1", 1_001, "addr-0000", "addr-0001"),
            (1, 0, "10-1-0", 1_000, "addr-0001", "addr-0000"),
            (1, 1, "10-1-1", 1_001, "addr-0001", "addr-0000"),
        ] {
            assert_eq!(
                value.transactions[tx].movements[ordinal],
                ValueMovement::Transfer {
                    id: MovementId(id.into()),
                    asset: AssetId {
                        chain: scope.chain.clone(),
                        asset: "native".into()
                    },
                    amount: Decimal::from(amount),
                    from: CanonicalAddress {
                        scope: scope.clone(),
                        value: from.into()
                    },
                    to: CanonicalAddress {
                        scope: scope.clone(),
                        value: to.into()
                    },
                }
            );
        }
        for (index, output) in value.outputs.created.iter().enumerate() {
            assert_eq!(output.id.transaction.value, ids[index]);
            assert_eq!(output.id.transaction.scope, scope);
            assert_eq!(output.id.index, index as u32);
            assert_eq!(output.address.scope, scope);
            assert_eq!(
                output.address.value,
                ["addr-0000", "addr-0001", "addr-0000"][index]
            );
            assert_eq!(
                output.asset,
                AssetId {
                    chain: scope.chain.clone(),
                    asset: "native".into()
                }
            );
            assert_eq!(
                output.amount,
                Decimal::from([50_000_u64, 50_001, 50_002][index])
            );
            assert_eq!(output.evidence, vec![0x51; 22]);
            assert_eq!(output.created_at, BlockHeight(10));
            assert_eq!(output.coinbase, index == 0);
        }
        assert_eq!(
            next,
            value.outputs.created[..2]
                .iter()
                .map(IndexedOutput::key)
                .collect::<Vec<_>>()
        );
        assert!(value.outputs.spent.is_empty());
        assert!(value.outputs.tracked_spends.is_empty());
    }

    #[test]
    fn spend_handoff_uses_only_the_requested_prefix_of_previously_created_outputs() {
        let scope = fixture_scope();
        for spent in [0, 1, 5] {
            let profile = Profile {
                txs: 0,
                created: 2,
                spent,
                ..profile()
            };
            let (first, next) = interpret(&scope, &profile, 1, None, Vec::new());
            assert_eq!(next.len(), spent.min(2));
            let expected = first
                .outputs
                .created
                .iter()
                .take(spent)
                .map(IndexedOutput::key)
                .collect::<Vec<_>>();
            assert_eq!(next, expected);
            let (second, following) =
                interpret(&scope, &profile, 2, Some(&first.block), next.clone());
            assert_eq!(second.outputs.spent, next);
            assert_eq!(
                second.block.parent,
                Some(BlockParent {
                    position: first.block.position,
                    hash: first.block.hash,
                })
            );
            assert_eq!(following.len(), spent.min(2));
            assert!(
                following
                    .iter()
                    .all(|key| key.output.transaction.value.starts_with("0000000002-"))
            );
        }
    }
}
