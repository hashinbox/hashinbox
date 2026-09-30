use std::collections::BTreeMap;

use super::{Address, Error, ErrorKind, Lamport};

/// Complete native account facts observed at one contextual read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    owner: Address,
    lamports: Lamport,
    executable: bool,
    data: Vec<u8>,
}

impl Account {
    /// An account owned by `owner` holding `lamports` and `data`.
    #[must_use]
    pub fn new(owner: Address, lamports: Lamport, executable: bool, data: Vec<u8>) -> Self {
        Self {
            owner,
            lamports,
            executable,
            data,
        }
    }

    /// The program that owns the account.
    #[must_use]
    pub fn owner(&self) -> &Address {
        &self.owner
    }

    /// The account's balance.
    #[must_use]
    pub const fn lamports(&self) -> Lamport {
        self.lamports
    }

    /// Whether the account is a loaded program.
    #[must_use]
    pub const fn executable(&self) -> bool {
        self.executable
    }

    /// The account's data bytes.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// A plain System-owned wallet account: the only shape a native SOL
    /// transfer may debit or credit here.
    #[must_use]
    pub fn is_wallet(&self) -> bool {
        !self.executable && self.owner == Address::from_bytes([0; 32]) && self.data.is_empty()
    }
}

/// Every account a batch touches, read at one confirmed slot bracketed by
/// two slot reads, so all balances come from the same state.
///
/// The bracket names the technique, and it is what this type *is*: a multi-account
/// read pinned between a slot before it and a slot after it. It is not a
/// balance witness — Solana has none, because an account cannot be read at a
/// past slot — and the name deliberately
/// does not borrow ethereum's `BalanceWitness`, which attests a folded balance and
/// answers a different question (`docs/DEFECT_PATTERNS.md` section 2.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bracket {
    /// A slot at or after the read; later reads use it as their minimum
    /// context slot.
    pub min_slot: u64,
    /// Each queried address; `None` for an account that does not exist yet.
    pub accounts: BTreeMap<Address, Option<Account>>,
}

impl Bracket {
    /// The balance `from` can spend on a transfer to `to`: zero for an
    /// account that does not exist.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RpcUnavailable`] when either address was not observed,
    /// and [`ErrorKind::Unsupported`] for a non-wallet account.
    pub fn balance(&self, from: &Address, to: &Address) -> Result<Lamport, Error> {
        let source = self.observed(from, "Solana source observation is missing")?;
        if !source.as_ref().is_none_or(Account::is_wallet) {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "unsupported Solana source account",
            ));
        }
        let destination = self.observed(to, "Solana destination observation is missing")?;
        if !destination.as_ref().is_none_or(Account::is_wallet) {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "unsupported Solana destination account",
            ));
        }
        Ok(source.as_ref().map_or(Lamport::ZERO, Account::lamports))
    }

    fn observed(&self, address: &Address, missing: &str) -> Result<&Option<Account>, Error> {
        self.accounts
            .get(address)
            .ok_or_else(|| Error::rpc(missing, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(byte: u8) -> Address {
        Address::from_bytes([byte; 32])
    }

    fn bracket(entries: Vec<(Address, Option<Account>)>) -> Bracket {
        Bracket {
            min_slot: 1,
            accounts: entries.into_iter().collect(),
        }
    }

    #[test]
    fn retains_complete_structural_account_facts() {
        let owner = address(23);
        let account = Account::new(
            owner.clone(),
            Lamport::from_atomic(u64::MAX),
            true,
            vec![1; 17],
        );
        assert_eq!(account.owner(), &owner);
        assert_eq!(account.lamports().units(), u64::MAX);
        assert!(account.executable());
        assert_eq!(account.data().len(), 17);
    }

    #[test]
    fn missing_observations_are_node_failures_in_source_then_destination_order() {
        let (source, destination) = (address(1), address(2));
        for (entries, message) in [
            (vec![], "Solana source observation is missing"),
            (
                vec![(source.clone(), None)],
                "Solana destination observation is missing",
            ),
        ] {
            let error = bracket(entries).balance(&source, &destination).unwrap_err();
            assert_eq!(
                (error.kind, error.message.as_str(), error.retryable),
                (ErrorKind::RpcUnavailable, message, true)
            );
        }
    }

    #[test]
    fn classification_checks_the_source_before_the_destination() {
        let (source, destination) = (address(7), address(8));
        let unsupported = Account::new(address(9), Lamport::ZERO, false, Vec::new());
        for (first_source, message) in [
            (None, "unsupported Solana destination account"),
            (
                Some(unsupported.clone()),
                "unsupported Solana source account",
            ),
        ] {
            let error = bracket(vec![
                (source.clone(), first_source),
                (destination.clone(), Some(unsupported.clone())),
            ])
            .balance(&source, &destination)
            .unwrap_err();
            assert_eq!(
                (error.kind, error.message.as_str()),
                (ErrorKind::Unsupported, message)
            );
        }
    }

    #[test]
    fn classification_rejects_every_unsupported_native_account_shape() {
        let (source, destination) = (address(7), address(8));
        let system = address(0);
        for (owner, executable, data, failed) in [
            (system.clone(), false, Vec::new(), false),
            (address(9), false, Vec::new(), true),
            (system.clone(), true, Vec::new(), true),
            (system.clone(), false, vec![1], true),
        ] {
            let account = Account::new(owner, Lamport::from_atomic(1), executable, data);
            for failing in [&source, &destination] {
                let mut entries =
                    BTreeMap::from([(source.clone(), None), (destination.clone(), None)]);
                entries.insert(failing.clone(), Some(account.clone()));
                let result = Bracket {
                    min_slot: 1,
                    accounts: entries,
                }
                .balance(&source, &destination);
                assert_eq!(result.is_err(), failed);
                if let Ok(balance) = result {
                    assert_eq!(balance.units(), u64::from(failing == &source));
                }
            }
        }
    }
}
