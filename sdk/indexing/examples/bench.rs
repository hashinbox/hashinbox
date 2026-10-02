//! Throughput benchmark for the embedded SQLite indexing backend.
//!
//! One fixed workload with fixed reporting, so two runs can be compared
//! directly:
//!
//!   cargo run --release -p indexing --example bench
//!
//! Shape is environment-driven:
//! BENCH_BLOCKS, BENCH_TXS, BENCH_MOVEMENTS, BENCH_ADDRESSES, BENCH_CREATED,
//! and BENCH_SPENT.

use std::{env, time::Instant};

use indexing::sqlite::Repository;
use indexing::{
    Address, AssetId, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, CanonicalBlock,
    ChainName, ConfirmedHistory, Decimal, HistoryQuery, IndexStore, IndexedOutput,
    InterpretedBlock, MovementId, Network, ObservationDraft, ObservationDraftStatus, OutputChanges,
    OutputId, OutputKey, OutputRequest, OutputStore, TransactionRef, ValueMovement,
};

const CHAIN: &str = "primary";
const NETWORK: &str = "testing";

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

    fn rows(&self) -> usize {
        let addresses_per_tx = 2;
        let history = self.txs * addresses_per_tx;
        let movements = history * self.movements;
        history + movements + self.created + self.spent * 2
    }

    fn address(&self, index: usize) -> Address {
        let index = index % self.addresses;
        Address {
            network: network(),
            value: format!("addr-{index:04}"),
        }
    }

    fn transactions(&self, height: u64) -> Vec<ObservationDraft> {
        let mut transactions = Vec::with_capacity(self.txs);
        for index in 0..self.txs {
            let from = self.address(index);
            let to = self.address(index + 1);
            let movements = (0..self.movements)
                .map(|ordinal| {
                    ValueMovement::transfer(
                        MovementId(format!("{height}-{index}-{ordinal}")),
                        asset(),
                        Decimal::from(1_000 + ordinal as u64),
                        from.clone(),
                        to.clone(),
                    )
                })
                .collect();
            transactions.push(ObservationDraft {
                transaction: indexing::Transaction {
                    transaction_id: TransactionRef {
                        network: network(),
                        id: format!("{height:010}-{index:05}").into(),
                    },
                    movements,
                    fee: None,
                },
                transaction_order: index as u64,
                status: ObservationDraftStatus::Included,
            });
        }
        transactions
    }

    fn outputs(&self, height: u64) -> Vec<IndexedOutput> {
        let mut created = Vec::with_capacity(self.created);
        for index in 0..self.created {
            created.push(IndexedOutput {
                id: OutputId {
                    transaction: TransactionRef {
                        network: network(),
                        id: format!("{height:010}-{index:05}").into(),
                    },
                    index: index as u32,
                },
                address: self.address(index),
                asset: asset(),
                amount: Decimal::from(50_000 + index as u64),
                evidence: vec![0x51; 22],
                created_at: BlockHeight(height),
                coinbase: index == 0,
            });
        }
        created
    }
}

fn network() -> Network {
    Network {
        chain: ChainName::new(CHAIN),
        name: NETWORK.into(),
    }
}

fn asset() -> AssetId {
    AssetId {
        chain: ChainName::new(CHAIN),
        asset: "native".into(),
    }
}

fn interpret(
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
    let transactions = profile.transactions(height);
    let created = profile.outputs(height);
    let next_spend = created
        .iter()
        .take(profile.spent)
        .map(IndexedOutput::key)
        .collect();

    (
        InterpretedBlock {
            outputs: OutputChanges {
                created,
                spent: spend,
                tracked_spends: Vec::new(),
            },
            ..InterpretedBlock::new(&network(), block, transactions)
                .expect("unique transaction ids")
        },
        next_spend,
    )
}

/// SQLite work runs on `tokio::task::spawn_blocking`, so the benchmark drives
/// it from a Tokio runtime rather than a bare executor.
fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .build()
                .expect("benchmark runtime")
        })
        .block_on(future)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rate = |count: f64, seconds: f64| {
        if seconds <= 0.0 { 0.0 } else { count / seconds }
    };
    let profile = Profile::from_env();
    let directory = tempfile::TempDir::new()?;
    let storage = indexing::sqlite::Database::open(directory.path().join("index.db"))?;
    let repository = Repository::new(storage);

    println!(
        "commit: {} blocks x {} tx x {} movements + {} created / {} spent  (~{} rows/block)",
        profile.blocks,
        profile.txs,
        profile.movements,
        profile.created,
        profile.spent,
        profile.rows(),
    );

    let mut parent: Option<BlockRef> = None;
    let mut spend: Vec<OutputKey> = Vec::new();
    let mut worst = 0.0_f64;
    let started = Instant::now();
    for height in 1..=profile.blocks {
        let (interpreted, next_spend) = interpret(&profile, height, parent.as_ref(), spend);
        let block = interpreted.block.reference().clone();
        let canonical = CanonicalBlock::new(network(), interpreted)?;
        let addition = indexing::IndexedBatch::new(
            network(),
            parent.clone(),
            vec![canonical],
            Vec::new(),
            indexing::FeeSettlement::Charged,
        )?;
        let one = Instant::now();
        block_on(repository.commit(addition))?;
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

    let pages = 50;
    let started = Instant::now();
    let mut transactions = 0;
    for index in 0..pages {
        let page = block_on(ConfirmedHistory::history(
            &repository,
            HistoryQuery {
                address: profile.address(index),
                asset: indexing::AssetId::native(network().chain.clone()),
                after: None,
                limit: 100,
            },
        ))?;
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
        let page = block_on(OutputStore::list(
            &repository,
            OutputRequest {
                network: network(),
                address: profile.address(index),
                after: None,
                limit: 100,
            },
        ))?;
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

    fn fixture_network() -> Network {
        Network {
            chain: ChainName::new("primary"),
            network: "testing".into(),
        }
    }

    #[test]
    fn empty_workload_keeps_supplied_spends_and_complete_parent_coordinates() {
        let network = fixture_network();
        let profile = Profile {
            txs: 0,
            movements: 0,
            created: 0,
            spent: 9,
            ..profile()
        };
        let previous = OutputKey {
            address: Address {
                network: network.clone(),
                value: "prior-owner".into(),
            },
            output: OutputId {
                transaction: TransactionRef {
                    network: network.clone(),
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
        let (value, next) = interpret(&profile, 10, Some(&parent), vec![previous.clone()]);
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
        let network = fixture_network();
        let profile = profile();
        let (value, next) = interpret(&profile, 10, None, Vec::new());
        assert_eq!(value.transactions.len(), 2);
        assert_eq!(value.outputs.created.len(), 3);
        assert_eq!(value.block.parent, None);
        let ids = ["0000000010-00000", "0000000010-00001", "0000000010-00002"];
        for (index, draft) in value.transactions.iter().enumerate() {
            assert_eq!(
                draft.transaction.transaction_id,
                TransactionRef {
                    network: network.clone(),
                    value: ids[index].into(),
                }
            );
            assert_eq!(draft.status, ObservationDraftStatus::Included);
            assert_eq!(draft.transaction.fee, None);
            assert_eq!(draft.transaction.movements.len(), 2);
        }
        for (tx, ordinal, id, amount, from, to) in [
            (0, 0, "10-0-0", 1_000_u64, "addr-0000", "addr-0001"),
            (0, 1, "10-0-1", 1_001, "addr-0000", "addr-0001"),
            (1, 0, "10-1-0", 1_000, "addr-0001", "addr-0000"),
            (1, 1, "10-1-1", 1_001, "addr-0001", "addr-0000"),
        ] {
            assert_eq!(
                value.transactions[tx].transaction.movements[ordinal],
                ValueMovement::transfer(
                    MovementId(id.into()),
                    AssetId {
                        chain: network.chain.clone(),
                        asset: "native".into()
                    },
                    Decimal::from(amount),
                    Address {
                        network: network.clone(),
                        value: from.into()
                    },
                    Address {
                        network: network.clone(),
                        value: to.into()
                    }
                )
            );
        }
        for (index, output) in value.outputs.created.iter().enumerate() {
            assert_eq!(output.id.transaction.value, ids[index]);
            assert_eq!(output.id.transaction.network, network);
            assert_eq!(output.id.index, index as u32);
            assert_eq!(output.address.network, network);
            assert_eq!(
                output.address.value,
                ["addr-0000", "addr-0001", "addr-0000"][index]
            );
            assert_eq!(
                output.asset,
                AssetId {
                    chain: network.chain.clone(),
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
        assert_eq!(network(), fixture_network());
        for spent in [0, 1, 5] {
            let profile = Profile {
                txs: 0,
                created: 2,
                spent,
                ..profile()
            };
            let (first, next) = interpret(&profile, 1, None, Vec::new());
            assert_eq!(next.len(), spent.min(2));
            let expected = first
                .outputs
                .created
                .iter()
                .take(spent)
                .map(IndexedOutput::key)
                .collect::<Vec<_>>();
            assert_eq!(next, expected);
            let (second, following) = interpret(&profile, 2, Some(&first.block), next.clone());
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
