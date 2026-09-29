//! What signing a transaction took, so two in-flight sends can be compared
//! before either is broadcast.

use std::collections::BTreeSet;

/// What an outbox held to sign a transaction, named exactly enough to answer
/// "will this collide with my other in-flight send?".
///
/// The claim is the set of scarce things those exact bytes take, each under
/// a name the signing chain chose. The rule is one rule everywhere — two
/// sends collide when they take one thing in common — and what differs is
/// only what a chain puts in the set, which is the chain's own business:
/// one names the outputs it spends, another a sender's sequence number,
/// another the payer whose sends its protocol offers no way to order. The
/// names are opaque here and compared only for equality, so a chain may
/// start claiming something new without this crate learning about it.
///
/// They are not network-qualified, because the claims a caller compares are
/// always produced by one wallet provider, which serves one network.
///
/// A claim is a **report, never a lock**. `TransactionBuilder::prepare`
/// deliberately releases what it reserved, and `Broadcaster::broadcast`
/// reserves it again; a caller holding two colliding claims knows that at
/// most one of the two can be accepted, and that the loser fails definitely
/// rather than ambiguously.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim(BTreeSet<String>);

impl Claim {
    /// The claim on exactly `resources`, as the chain that signed names them.
    #[must_use]
    pub fn new(resources: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(resources.into_iter().map(Into::into).collect())
    }

    /// Whether at most one of the two transactions can be accepted.
    ///
    /// The comparison is conservative in neither direction: each chain puts
    /// exactly the resource its protocol enforces into the set, so two
    /// claims that do not collide describe transactions that can both be
    /// broadcast.
    #[must_use]
    pub fn collides_with(&self, other: &Self) -> bool {
        !self.0.is_disjoint(&other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_collide_only_when_they_name_one_resource_in_common() {
        let first = Claim::new(["a:0", "b:1"]);
        let shared = Claim::new(["c:0", "b:1"]);
        let apart = Claim::new(["b:0", "a:1"]);

        assert!(first.collides_with(&shared));
        assert!(shared.collides_with(&first));
        assert!(!first.collides_with(&apart));
    }

    #[test]
    fn a_resource_is_one_name_however_often_it_is_claimed() {
        let once = Claim::new(["alice/7"]);
        let twice = Claim::new(["alice/7", "alice/7"]);

        assert_eq!(once, twice, "a claim is a set, not a list");
        assert!(once.collides_with(&twice));
        assert!(!once.collides_with(&Claim::new(["alice/8"])));
        assert!(!once.collides_with(&Claim::new(["bob/7"])));
    }

    #[test]
    fn order_is_not_part_of_a_claim() {
        assert_eq!(Claim::new(["a:0", "b:1"]), Claim::new(["b:1", "a:0"]));
    }

    #[test]
    fn a_claim_on_nothing_collides_with_nothing() {
        let empty = Claim::new(Vec::<String>::new());

        assert!(!empty.collides_with(&empty));
        assert!(!empty.collides_with(&Claim::new(["a:0"])));
    }
}
