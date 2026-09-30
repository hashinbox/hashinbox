//! The balance witness: an independent read that vouches for a folded
//! balance and never replaces it.
//!
//! A balance the index folds over the movements it stores cannot disagree
//! with the history it serves, which is the whole point of folding it. What
//! it cannot do is notice a change it never saw: a rebasing token, a mint
//! spelled as something other than a `Transfer`, a proxy that changed its
//! accounting mid-life, or an address funded before the index started
//! watching it. None of those is visible
//! in logs, by construction.
//!
//! `balanceOf` is. Pinned to a canonical block **hash** it is not a matter of
//! node opinion but deterministic EVM execution, so two honest nodes at that
//! block must return the same word; `eth_getBalance` at the same pin is the
//! same claim for ether. That is worth exactly one thing, and this module
//! does only that thing: it compares, and it reports. The number a caller is
//! given is the fold's, agreement or not.

use base::{BlockRef, Decimal};

use super::{Accounts, View};
use crate::native::{Address, AssetKind};

/// What an independent, canonically pinned node read says about a folded
/// amount.
///
/// There is no "the node's number wins" variant, because there is no case in
/// which it does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attestation {
    /// The node computed the same amount at the same block. Every silent
    /// failure mode of a log fold is excluded at that block.
    Agreed,
    /// It did not. One of the two is wrong and the difference is the
    /// evidence, so both amounts are carried in atomic units.
    Disagreed {
        /// What the fold says.
        folded: Decimal,
        /// What the node says.
        witness: Decimal,
    },
    /// Nothing was proven: the node could not answer for that block, had
    /// reorganised it out, or was not asked.
    Unavailable {
        /// Why, in the node's own words where it gave them.
        reason: String,
    },
}

/// Asks one node what a holding was worth at one canonical block, and
/// reports whether that agrees with a folded amount.
///
/// It holds only the account reads, borrowed for one question; the block,
/// the address, the asset and the amount to check are the question.
pub struct Attester<'a> {
    accounts: &'a dyn Accounts,
}

impl<'a> Attester<'a> {
    /// An attester that reads through `accounts`.
    #[must_use]
    pub const fn new(accounts: &'a dyn Accounts) -> Self {
        Self { accounts }
    }

    /// Whether `folded` — atomic units of `asset` held by `address` — is what
    /// the node computes at `at`.
    ///
    /// The read is pinned with `requireCanonical`, so a node that has
    /// reorganised `at` out refuses rather than answering about a different
    /// chain; that refusal is [`Attestation::Unavailable`], never agreement.
    ///
    /// This returns no `Result` on purpose. An attester that could fail a
    /// balance read would make the node part of the answer again, which is
    /// the dependency this independent check avoids. Every failure is an
    /// absence of proof.
    pub async fn attest(
        &self,
        address: Address,
        asset: &AssetKind,
        at: &BlockRef,
        folded: &Decimal,
    ) -> Attestation {
        match self
            .accounts
            .balance(address, asset, View::Block(at.clone()))
            .await
        {
            Ok(witness) if witness.atomic() == *folded => Attestation::Agreed,
            Ok(witness) => Attestation::Disagreed {
                folded: folded.clone(),
                witness: witness.atomic(),
            },
            Err(error) => Attestation::Unavailable {
                reason: error.message,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use base::{BlockHash, BlockHeight, BlockPosition};
    use futures_executor::block_on;
    use serde_json::json;

    use super::super::script::{Reply, Script, Scripted};
    use super::*;

    fn at() -> BlockRef {
        BlockRef {
            position: BlockPosition(9),
            height: BlockHeight(9),
            hash: BlockHash(vec![0xaa; 32]),
            parent: None,
            timestamp: None,
        }
    }

    fn address() -> Address {
        Address::from_bytes([0x0a; 20])
    }

    #[test]
    fn a_witness_pins_the_block_it_vouches_for_and_agrees_only_on_the_same_amount() {
        let script = Script::node(vec![
            Reply::ok("eth_getBalance", json!("0x2a")),
            Reply::ok(
                "eth_call",
                json!(format!("0x{}{:02x}", "00".repeat(31), 42)),
            ),
        ]);
        let rpc = script.client();
        let witness = Attester::new(&rpc);
        let token = AssetKind::Erc20(Address::from_bytes([0x0b; 20]));
        for asset in [AssetKind::Native, token] {
            assert_eq!(
                block_on(witness.attest(address(), &asset, &at(), &Decimal::from(42_u64))),
                Attestation::Agreed
            );
        }
        // Both reads name the block by hash and refuse a non-canonical one:
        // an attestation about another chain is not an attestation.
        let pinned = json!({"blockHash": format!("0x{}", "aa".repeat(32)),
            "requireCanonical": true});
        for (_, params) in script.requests() {
            assert_eq!(params[1], pinned);
        }
    }

    #[test]
    fn a_seeded_disagreement_is_reported_with_both_amounts_and_never_silently_passes() {
        let script = Script::node(vec![Reply::ok("eth_getBalance", json!("0x2b"))]);
        let rpc = script.client();
        // The fold missed one unit: the node says 43 where it folded 42.
        assert_eq!(
            block_on(Attester::new(&rpc).attest(
                address(),
                &AssetKind::Native,
                &at(),
                &Decimal::from(42_u64)
            )),
            Attestation::Disagreed {
                folded: Decimal::from(42_u64),
                witness: Decimal::from(43_u64),
            }
        );
    }

    #[test]
    fn a_node_that_cannot_answer_for_the_block_proves_nothing_rather_than_agreeing() {
        let script = Script::node(vec![Reply::failure(
            "eth_getBalance",
            -32_000,
            "block is not canonical",
        )]);
        let rpc = script.client();
        let attestation = block_on(Attester::new(&rpc).attest(
            address(),
            &AssetKind::Native,
            &at(),
            &Decimal::from(42_u64),
        ));
        let Attestation::Unavailable { reason } = attestation else {
            panic!("a refused pin is not agreement");
        };
        assert!(reason.contains("eth_getBalance failed"), "{reason}");
    }
}
