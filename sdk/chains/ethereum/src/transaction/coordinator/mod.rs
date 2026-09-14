use std::{collections::BTreeMap, sync::Arc};

use base::{TransactionError, TransactionErrorKind, TransactionId as BaseTransactionId};
use indexing::SourceError;

use super::{BuildContext, SignedTransaction, TransactionBuilder, TransactionId, TransferRequest};
use crate::{Accounts, Address, AssetKind, ChainError, ChainErrorKind, Transactions};

mod requirements;
mod state;

use requirements::{RequiredAsset, Requirements};
use state::{Admission, Claim, Core, Operation};

/// Process-local nonce, preparation, and ambiguous-submission coordination.
///
/// One instance must be shared by every native and ERC-20 provider using the
/// same Ethereum accounts. It deliberately does not claim cross-process
/// coordination or durable recovery after a process crash.
#[derive(Clone)]
pub struct TransactionCoordinator {
    core: Arc<Core>,
}

impl TransactionCoordinator {
    #[must_use]
    pub fn new(accounts: Arc<dyn Accounts>, transactions: Arc<dyn Transactions>) -> Self {
        Self {
            core: Arc::new(Core::new(accounts, transactions)),
        }
    }

    pub(crate) async fn prepare_one(
        &self,
        preparation: Preparation<'_>,
    ) -> Result<SignedTransaction, ChainError> {
        self.prepare_batch(vec![preparation])
            .await
            .map_err(|error| error.source)?
            .detach_one()
    }

    pub(crate) async fn prepare_batch(
        &self,
        preparations: Vec<Preparation<'_>>,
    ) -> Result<PreparedBatch, PreparationError> {
        if preparations.is_empty() {
            let source = ChainError::new(
                ChainErrorKind::InvalidTransaction,
                "Ethereum transaction batch is empty",
            );
            return Err(PreparationError::new(0, source));
        }

        let mut senders = BTreeMap::new();
        for (index, preparation) in preparations.iter().enumerate() {
            senders
                .entry(preparation.request.from().clone())
                .or_insert(index);
        }
        let mut senders = senders.into_iter().collect::<Vec<_>>();
        senders.sort_by_key(|(_, index)| *index);
        let operation = self.admit(&senders).await?;
        let nonces = self.nonces(&preparations, &senders).await?;
        let mut drafts = Vec::with_capacity(preparations.len());
        for (index, (preparation, nonce)) in preparations.into_iter().zip(nonces).enumerate() {
            let draft = Draft::prepare(preparation, nonce, self.core.transactions.as_ref())
                .await
                .map_err(|source| PreparationError::new(index, source))?;
            drafts.push(draft);
        }
        self.ensure_aggregate_balances(&drafts).await?;

        let mut entries = Vec::with_capacity(drafts.len());
        for (index, draft) in drafts.into_iter().enumerate() {
            let entry = draft
                .sign()
                .await
                .map_err(|source| PreparationError::new(index, source))?;
            entries.push(entry);
        }

        self.core
            .register(operation.id, &entries)
            .map_err(|source| PreparationError::new(0, source))?;
        Ok(PreparedBatch {
            coordinator: self.clone(),
            entries,
            cursor: 0,
            operation: Some(operation),
        })
    }

    pub(crate) async fn broadcast(
        &self,
        transaction: SignedTransaction,
    ) -> Result<TransactionId, TransactionError> {
        self.submit(Some(&transaction), transaction.id.clone())
            .await
    }

    async fn admit(&self, senders: &[(Address, usize)]) -> Result<Operation, PreparationError> {
        loop {
            let mut notified = Box::pin(self.core.changed.notified());
            notified.as_mut().enable();
            let (id, index) = match self.core.admission(senders) {
                Admission::Acquired(operation) => return Ok(operation),
                Admission::Wait => {
                    notified.await;
                    continue;
                }
                Admission::Recover { id, index } => (id, index),
                Admission::Exhausted(index) => {
                    let source = ChainError::new(
                        ChainErrorKind::Other,
                        "Ethereum transaction coordinator exhausted operation identifiers",
                    );
                    return Err(PreparationError::new(index, source));
                }
            };
            self.submit(None, id).await.map_err(|source| {
                let source = ChainError::new(
                    ChainErrorKind::RpcUnavailable,
                    format!("Ethereum sender is blocked by an ambiguous transaction: {source}"),
                );
                PreparationError::new(index, source)
            })?;
        }
    }

    async fn nonces(
        &self,
        preparations: &[Preparation<'_>],
        senders: &[(Address, usize)],
    ) -> Result<Vec<u64>, PreparationError> {
        let mut next = BTreeMap::new();
        for (source, first_index) in senders {
            let start = self
                .nonce_start(source, preparations)
                .await
                .map_err(|source| PreparationError::new(*first_index, source))?;
            next.insert(source.clone(), start);
        }

        preparations
            .iter()
            .enumerate()
            .map(|(index, preparation)| {
                let nonce = next.get_mut(preparation.request.from()).ok_or_else(|| {
                    let source = ChainError::new(
                        ChainErrorKind::Other,
                        "Ethereum sender was not admitted for nonce assignment",
                    );
                    PreparationError::new(index, source)
                })?;
                let assigned = *nonce;
                *nonce = nonce.checked_add(1).ok_or_else(|| {
                    let source = ChainError::new(
                        ChainErrorKind::InvalidTransaction,
                        "Ethereum transaction nonce overflows u64",
                    );
                    PreparationError::new(index, source)
                })?;
                Ok(assigned)
            })
            .collect()
    }

    async fn nonce_start(
        &self,
        source: &Address,
        preparations: &[Preparation<'_>],
    ) -> Result<u64, ChainError> {
        let pending = self
            .core
            .accounts
            .nonce(source.clone())
            .await
            .map_err(ChainError::from_rpc)?;
        let start = self
            .core
            .floor(source)
            .map_or(pending, |floor| floor.max(pending));
        let count = preparations
            .iter()
            .filter(|preparation| preparation.request.from() == source)
            .count();
        let count = u64::try_from(count).map_err(|_| {
            ChainError::new(
                ChainErrorKind::InvalidTransaction,
                "Ethereum transaction count exceeds u64",
            )
        })?;
        start.checked_add(count).ok_or_else(|| {
            ChainError::new(
                ChainErrorKind::InvalidTransaction,
                "Ethereum batch nonce range overflows u64",
            )
        })?;
        Ok(start)
    }

    async fn ensure_aggregate_balances(
        &self,
        drafts: &[Draft<'_>],
    ) -> Result<(), PreparationError> {
        let requirements = Requirements::from_drafts(drafts)?;
        let mut insufficient = None;
        for requirement in requirements.values {
            let first_index = requirement.first_index();
            let earliest_failure_known = insufficient.is_some_and(|index| first_index >= index);
            if earliest_failure_known {
                break;
            }
            let asset = match &requirement.asset {
                RequiredAsset::Native => AssetKind::Native,
                RequiredAsset::Erc20(token) => AssetKind::Erc20(token.clone()),
            };
            let balance = self
                .core
                .accounts
                .balance(requirement.source.clone(), &asset, None)
                .await
                .map_err(|error| PreparationError::new(first_index, ChainError::from_rpc(error)))?;
            if balance >= requirement.amount {
                continue;
            }
            let index = requirement.failure_index(&balance);
            insufficient = Some(insufficient.map_or(index, |current: usize| current.min(index)));
        }
        if let Some(index) = insufficient {
            let source = ChainError::new(
                ChainErrorKind::InsufficientFunds,
                "Ethereum aggregate batch balance is insufficient",
            );
            return Err(PreparationError::new(index, source));
        }
        Ok(())
    }

    async fn submit(
        &self,
        expected: Option<&SignedTransaction>,
        id: TransactionId,
    ) -> Result<TransactionId, TransactionError> {
        let claim = loop {
            let mut notified = Box::pin(self.core.changed.notified());
            notified.as_mut().enable();
            match self
                .core
                .claim(&id, expected)
                .map_err(definite_submission_error)?
            {
                Claim::Ready(claim) => break claim,
                Claim::Wait => notified.await,
            }
        };
        if claim.recovery {
            match self.core.transactions.known(&id).await {
                Ok(true) => return claim.guard.accept().map_err(definite_submission_error),
                Ok(false) => {}
                Err(error) => return Err(ambiguous_submission_error(&id, error)),
            }
        }

        match self
            .core
            .transactions
            .broadcast(claim.transaction.clone())
            .await
        {
            Ok(returned) if returned == id => {
                claim.guard.accept().map_err(definite_submission_error)
            }
            Ok(_) => Err(ambiguous_submission_error(
                &id,
                "Ethereum node returned a different hash for the exact signed envelope",
            )),
            Err(error) if !claim.recovery && error.ambiguous_transaction_id.is_none() => {
                claim.guard.reject();
                Err(error)
            }
            Err(error) => Err(ambiguous_submission_error(&id, error)),
        }
    }
}

pub(crate) struct Preparation<'a> {
    pub(super) request: TransferRequest,
    expected_chain_id: u64,
    signer: PreparationSigner<'a>,
}

impl<'a> Preparation<'a> {
    pub(crate) fn signer(
        request: TransferRequest,
        expected_chain_id: u64,
        signer: &'a dyn base::Signer,
    ) -> Self {
        Self {
            request,
            expected_chain_id,
            signer: PreparationSigner::Signer(signer),
        }
    }

    pub(crate) fn wallet(
        request: TransferRequest,
        expected_chain_id: u64,
        wallet: &'a dyn wallets::Wallet,
    ) -> Self {
        Self {
            request,
            expected_chain_id,
            signer: PreparationSigner::Wallet(wallet),
        }
    }

    async fn sign(self, builder: TransactionBuilder) -> Result<SignedTransaction, ChainError> {
        match self.signer {
            PreparationSigner::Signer(signer) => builder.sign(signer).await,
            PreparationSigner::Wallet(wallet) => builder.sign(&WalletSigner(wallet)).await,
        }
    }
}

enum PreparationSigner<'a> {
    Signer(&'a dyn base::Signer),
    Wallet(&'a dyn wallets::Wallet),
}

struct WalletSigner<'a>(&'a dyn wallets::Wallet);

impl base::Signer for WalletSigner<'_> {
    fn sign<'a>(&'a self, request: base::SignRequest) -> base::SignFuture<'a> {
        self.0.sign(request)
    }
}

#[derive(Debug)]
pub(crate) struct PreparationError {
    pub(crate) index: usize,
    pub(crate) source: ChainError,
}

impl PreparationError {
    pub(super) fn new(index: usize, source: ChainError) -> Self {
        Self { index, source }
    }
}

pub(crate) struct PreparedBatch {
    coordinator: TransactionCoordinator,
    entries: Vec<PreparedEntry>,
    cursor: usize,
    operation: Option<Operation>,
}

impl PreparedBatch {
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) async fn next(&mut self) -> Result<Option<TransactionId>, TransactionError> {
        let Some(entry) = self.entries.get(self.cursor) else {
            self.operation.take();
            return Ok(None);
        };
        let id = self
            .coordinator
            .submit(Some(&entry.signed), entry.signed.id.clone())
            .await?;
        self.cursor += 1;
        if self.cursor == self.entries.len() {
            self.operation.take();
        }
        Ok(Some(id))
    }

    fn detach_one(mut self) -> Result<SignedTransaction, ChainError> {
        let entry = self.entries.pop().ok_or_else(|| {
            ChainError::new(
                ChainErrorKind::Other,
                "Ethereum single preparation produced no transaction",
            )
        })?;
        let operation = self.operation.as_ref().ok_or_else(|| {
            ChainError::new(
                ChainErrorKind::Other,
                "Ethereum single preparation lost its coordinator admission",
            )
        })?;
        self.coordinator
            .core
            .detach(operation.id, &entry.signed.id)?;
        self.cursor = 1;
        self.operation.take();
        Ok(entry.signed)
    }
}

struct Draft<'a> {
    preparation: Preparation<'a>,
    context: BuildContext,
}

impl<'a> Draft<'a> {
    async fn prepare(
        preparation: Preparation<'a>,
        nonce: u64,
        transactions: &dyn Transactions,
    ) -> Result<Self, ChainError> {
        let context = transactions
            .build_context(&preparation.request, nonce)
            .await?;
        if context.chain_id != preparation.expected_chain_id {
            return Err(ChainError::new(
                ChainErrorKind::Divergent,
                "Ethereum RPC chain ID does not match the wallet network",
            ));
        }
        Ok(Self {
            preparation,
            context,
        })
    }

    async fn sign(self) -> Result<PreparedEntry, ChainError> {
        let source = self.preparation.request.from().clone();
        let nonce = self.context.nonce;
        let request = self.preparation.request.clone();
        let signed = self
            .preparation
            .sign(TransactionBuilder::new(request, self.context))
            .await?;
        Ok(PreparedEntry {
            source,
            nonce,
            signed,
        })
    }
}

struct PreparedEntry {
    source: Address,
    nonce: u64,
    signed: SignedTransaction,
}

// coordinator claim and acceptance boundaries construct foreign SourceError values with explicit
// retryability while preserving exact envelope and nonce error context
// design-lint: allow unclassified-free-function -- See the boundary rationale above.
fn source_error(message: impl Into<String>, retryable: bool) -> SourceError {
    SourceError {
        message: message.into(),
        retryable,
    }
}

// coordinator claim and acceptance errors share a context-specific conversion between foreign
// errors that preserves retryability and excludes an ambiguous transaction ID
// design-lint: allow unclassified-free-function -- See the boundary rationale above.
fn definite_submission_error(error: SourceError) -> TransactionError {
    let kind = if error.retryable {
        TransactionErrorKind::Unavailable
    } else {
        TransactionErrorKind::Rejected
    };
    TransactionError::new(kind, error.message)
}

// shared coordinator uncertainty mapping preserves the original error message and exact local
// envelope ID without changing claim state
// design-lint: allow unclassified-free-function -- See the boundary rationale above.
fn ambiguous_submission_error(
    id: &TransactionId,
    error: impl std::fmt::Display,
) -> TransactionError {
    TransactionError::new(TransactionErrorKind::Unavailable, error.to_string())
        .with_ambiguous_transaction_id(BaseTransactionId::new(id.to_string()))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, VecDeque},
        future::Future,
        sync::{Arc, Mutex},
        task::{Context, Poll},
    };

    use alloy_primitives::keccak256;
    use indexing::{BlockRef, BoxFuture, SourceError};

    use super::*;
    use crate::{AssetKind, BuildContext, Wei};

    const CHAIN_ID: u64 = 31_337;

    #[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
    enum BalanceKey {
        Native(Address),
        Token(Address, Address),
    }

    #[derive(Default)]
    struct AccountStub {
        nonces: Mutex<BTreeMap<Address, u64>>,
        balances: Mutex<BTreeMap<BalanceKey, Wei>>,
        nonce_reads: Mutex<Vec<Address>>,
        balance_reads: Mutex<Vec<BalanceKey>>,
    }

    impl AccountStub {
        fn set_nonce(&self, address: Address, nonce: u64) {
            self.nonces
                .lock()
                .expect("nonce lock must be healthy")
                .insert(address, nonce);
        }

        fn set_native_balance(&self, address: Address, balance: u128) {
            self.balances
                .lock()
                .expect("balance lock must be healthy")
                .insert(BalanceKey::Native(address), Wei::from_u128(balance));
        }

        fn set_token_balance(&self, address: Address, token: Address, balance: u128) {
            self.balances
                .lock()
                .expect("balance lock must be healthy")
                .insert(BalanceKey::Token(address, token), Wei::from_u128(balance));
        }
    }

    impl Accounts for AccountStub {
        fn balance<'a>(
            &'a self,
            address: Address,
            asset: &'a AssetKind,
            _at: Option<BlockRef>,
        ) -> BoxFuture<'a, Result<Wei, SourceError>> {
            Box::pin(async move {
                let key = match asset {
                    AssetKind::Native => BalanceKey::Native(address),
                    AssetKind::Erc20(token) => BalanceKey::Token(address, token.clone()),
                };
                self.balance_reads.lock().unwrap().push(key.clone());
                Ok(self
                    .balances
                    .lock()
                    .expect("balance lock must be healthy")
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| Wei::from_u128(1_000_000)))
            })
        }

        fn nonce<'a>(&'a self, address: Address) -> BoxFuture<'a, Result<u64, SourceError>> {
            Box::pin(async move {
                self.nonce_reads.lock().unwrap().push(address.clone());
                Ok(self
                    .nonces
                    .lock()
                    .expect("nonce lock must be healthy")
                    .get(&address)
                    .copied()
                    .unwrap_or(0))
            })
        }
    }

    enum BroadcastAction {
        Accept,
        Reject,
        Ambiguous,
        Pending,
    }

    #[derive(Default)]
    struct Endpoint {
        contexts: Mutex<Vec<(Address, u64)>>,
        broadcasts: Mutex<Vec<SignedTransaction>>,
        actions: Mutex<VecDeque<BroadcastAction>>,
        known_results: Mutex<VecDeque<Result<bool, SourceError>>>,
        known_ids: Mutex<Vec<TransactionId>>,
    }

    impl Endpoint {
        fn actions(&self, actions: impl IntoIterator<Item = BroadcastAction>) {
            self.actions
                .lock()
                .expect("action lock must be healthy")
                .extend(actions);
        }

        fn known(&self, results: impl IntoIterator<Item = Result<bool, SourceError>>) {
            self.known_results
                .lock()
                .expect("known-result lock must be healthy")
                .extend(results);
        }
    }

    impl Transactions for Endpoint {
        fn build_context<'a>(
            &'a self,
            request: &'a TransferRequest,
            nonce: u64,
        ) -> BoxFuture<'a, Result<BuildContext, ChainError>> {
            Box::pin(async move {
                self.contexts
                    .lock()
                    .expect("context lock must be healthy")
                    .push((request.from().clone(), nonce));
                Ok(BuildContext {
                    chain_id: CHAIN_ID,
                    nonce,
                    gas_limit: 1,
                    max_fee_per_gas: Wei::from_u128(1),
                    max_priority_fee_per_gas: Wei::from_u128(1),
                })
            })
        }

        fn broadcast<'a>(
            &'a self,
            transaction: SignedTransaction,
        ) -> BoxFuture<'a, Result<TransactionId, base::TransactionError>> {
            Box::pin(async move {
                let id = transaction.id.clone();
                self.broadcasts
                    .lock()
                    .expect("broadcast lock must be healthy")
                    .push(transaction);
                let action = self
                    .actions
                    .lock()
                    .expect("action lock must be healthy")
                    .pop_front()
                    .unwrap_or(BroadcastAction::Accept);
                match action {
                    BroadcastAction::Accept => Ok(id),
                    BroadcastAction::Reject => Err(base::TransactionError::new(
                        base::TransactionErrorKind::Rejected,
                        "terminal rejection",
                    )),
                    BroadcastAction::Ambiguous => Err(base::TransactionError::new(
                        base::TransactionErrorKind::Unavailable,
                        "ambiguous submission",
                    )
                    .with_ambiguous_transaction_id(base::TransactionId::new(id.to_string()))),
                    BroadcastAction::Pending => std::future::pending().await,
                }
            })
        }

        fn known<'a>(
            &'a self,
            transaction: &'a TransactionId,
        ) -> BoxFuture<'a, Result<bool, SourceError>> {
            Box::pin(async move {
                self.known_ids
                    .lock()
                    .expect("known-ID lock must be healthy")
                    .push(transaction.clone());
                self.known_results
                    .lock()
                    .expect("known-result lock must be healthy")
                    .pop_front()
                    .unwrap_or(Ok(false))
            })
        }
    }

    fn signer(seed: u8) -> base::KeyPair<Address> {
        let secret = vec![seed; 32];
        let key = crypto::SecretKey::new(secret.clone()).expect("test key must be valid");
        let public = key
            .public_key(crypto::PublicKeyFormat::Raw)
            .expect("test public key must derive");
        let hash = keccak256(&public.bytes);
        let mut bytes = [0_u8; 20];
        bytes.copy_from_slice(&hash[12..]);
        base::KeyPair::new(Address(bytes), secret).expect("test signer must construct")
    }

    fn transfer(from: &Address, value: u128) -> TransferRequest {
        TransferRequest::native_atomic(from.clone(), Address([0x55; 20]), Wei::from_u128(value))
    }

    fn token_transfer(from: &Address, token: &Address, amount: u128) -> TransferRequest {
        TransferRequest::erc20(
            from.clone(),
            token.clone(),
            Address([0x55; 20]),
            Wei::from_u128(amount),
        )
    }

    fn coordinator(
        accounts: Arc<AccountStub>,
        transactions: Arc<Endpoint>,
    ) -> TransactionCoordinator {
        TransactionCoordinator::new(accounts, transactions)
    }

    #[cfg(test)]
    mod characterization {
        use super::*;

        struct UnusedSigner;

        impl base::Signer for UnusedSigner {
            fn sign<'a>(&'a self, _request: base::SignRequest) -> base::SignFuture<'a> {
                panic!("invalid preparation must fail before signing")
            }
        }

        #[tokio::test]
        async fn nonce_overflow_reports_first_occurrence_before_contexts_or_later_sender_reads() {
            let first = Address([0xff; 20]);
            let second = Address([1; 20]);
            for (exhausted, failed_index, expected_reads) in [
                (first.clone(), 0, vec![first.clone()]),
                (second.clone(), 1, vec![first.clone(), second.clone()]),
            ] {
                let accounts = Arc::new(AccountStub::default());
                accounts.set_nonce(exhausted, u64::MAX);
                let transactions = Arc::new(Endpoint::default());
                let coordinator = coordinator(accounts.clone(), transactions.clone());
                let error = coordinator
                    .prepare_batch(vec![
                        Preparation::signer(transfer(&first, 1), CHAIN_ID, &UnusedSigner),
                        Preparation::signer(transfer(&second, 1), CHAIN_ID, &UnusedSigner),
                        Preparation::signer(transfer(&first, 1), CHAIN_ID, &UnusedSigner),
                    ])
                    .await
                    .err()
                    .expect("nonce range must overflow");
                assert_eq!(error.index, failed_index);
                assert_eq!(error.source.kind, ChainErrorKind::InvalidTransaction);
                assert_eq!(
                    error.source.message,
                    "Ethereum batch nonce range overflows u64"
                );
                assert_eq!(*accounts.nonce_reads.lock().unwrap(), expected_reads);
                assert!(accounts.balance_reads.lock().unwrap().is_empty());
                assert!(transactions.contexts.lock().unwrap().is_empty());
                assert!(transactions.broadcasts.lock().unwrap().is_empty());
            }
        }

        #[tokio::test]
        async fn context_and_aggregate_failures_precede_signing_and_release_sender_admission() {
            let source = Address([1; 20]);
            let accounts = Arc::new(AccountStub::default());
            accounts.set_native_balance(source.clone(), 3);
            let transactions = Arc::new(Endpoint::default());
            let coordinator = coordinator(accounts.clone(), transactions.clone());
            for (expected_chain, expected_kind, expected_index, message) in [
                (
                    CHAIN_ID + 1,
                    ChainErrorKind::Divergent,
                    1,
                    "Ethereum RPC chain ID does not match the wallet network",
                ),
                (
                    CHAIN_ID,
                    ChainErrorKind::InsufficientFunds,
                    1,
                    "Ethereum aggregate batch balance is insufficient",
                ),
            ] {
                let error = coordinator
                    .prepare_batch(vec![
                        Preparation::signer(transfer(&source, 1), CHAIN_ID, &UnusedSigner),
                        Preparation::signer(transfer(&source, 1), expected_chain, &UnusedSigner),
                    ])
                    .await
                    .err()
                    .expect("preflight must fail");
                assert_eq!(error.index, expected_index);
                assert_eq!(error.source.kind, expected_kind);
                assert_eq!(error.source.message, message);
                let contexts = transactions.contexts.lock().unwrap();
                assert_eq!(
                    &contexts[contexts.len() - 2..],
                    &[(source.clone(), 0), (source.clone(), 1)]
                );
                assert_eq!(
                    accounts.balance_reads.lock().unwrap().len(),
                    usize::from(expected_chain == CHAIN_ID)
                );
                assert!(transactions.broadcasts.lock().unwrap().is_empty());
            }
        }

        fn draft(request: TransferRequest, fee: Wei, gas_limit: u64) -> Draft<'static> {
            Draft {
                preparation: Preparation::signer(request, CHAIN_ID, &UnusedSigner),
                context: BuildContext {
                    chain_id: CHAIN_ID,
                    nonce: 0,
                    gas_limit,
                    max_fee_per_gas: fee,
                    max_priority_fee_per_gas: Wei::ZERO,
                },
            }
        }

        #[test]
        fn aggregate_requirements_preserve_native_token_contributions_and_failure_indices() {
            let source = Address([1; 20]);
            let token = Address([2; 20]);
            let drafts = [
                draft(transfer(&source, 7), Wei::from_u128(2), 3),
                draft(token_transfer(&source, &token, 11), Wei::from_u128(3), 2),
                draft(transfer(&source, 5), Wei::from_u128(2), 1),
            ];
            let requirements = Requirements::from_drafts(&drafts).unwrap();
            assert_eq!(requirements.values.len(), 2);
            let native = &requirements.values[0];
            assert_eq!(native.source, source);
            assert!(native.asset == RequiredAsset::Native);
            assert_eq!(native.amount, Wei::from_u128(26));
            assert_eq!(native.first_index(), 0);
            assert_eq!(native.failure_index(&Wei::from_u128(12)), 0);
            assert_eq!(native.failure_index(&Wei::from_u128(18)), 1);
            assert_eq!(native.failure_index(&Wei::from_u128(25)), 2);
            let erc20 = &requirements.values[1];
            assert_eq!(erc20.source, source);
            assert!(erc20.asset == RequiredAsset::Erc20(token));
            assert_eq!(erc20.amount, Wei::from_u128(11));
            assert_eq!(erc20.first_index(), 1);
        }

        #[test]
        fn aggregate_overflow_keeps_the_original_item_and_exact_error_class() {
            let source = Address([1; 20]);
            let token = Address([2; 20]);
            let max = Wei([255; 32]);
            let maximum_token_transfer = TransferRequest::erc20(
                source.clone(),
                token.clone(),
                Address([3; 20]),
                max.clone(),
            );
            for (drafts, kind, message) in [
                (
                    vec![
                        draft(transfer(&source, 0), Wei::ZERO, 1),
                        draft(transfer(&source, 0), max.clone(), 2),
                    ],
                    ChainErrorKind::FeeUnavailable,
                    "Ethereum aggregate maximum fee overflows U256",
                ),
                (
                    vec![
                        draft(transfer(&source, 0), Wei::ZERO, 1),
                        draft(transfer(&source, 1), max.clone(), 1),
                    ],
                    ChainErrorKind::InsufficientFunds,
                    "Ethereum aggregate native requirement overflows U256",
                ),
                (
                    vec![
                        draft(transfer(&source, 0), max.clone(), 1),
                        draft(transfer(&source, 1), Wei::ZERO, 1),
                    ],
                    ChainErrorKind::InsufficientFunds,
                    "Ethereum aggregate asset requirement overflows U256",
                ),
                (
                    vec![
                        draft(maximum_token_transfer, Wei::ZERO, 1),
                        draft(token_transfer(&source, &token, 1), Wei::ZERO, 1),
                    ],
                    ChainErrorKind::InsufficientFunds,
                    "Ethereum aggregate asset requirement overflows U256",
                ),
            ] {
                let error = Requirements::from_drafts(&drafts)
                    .err()
                    .expect("sum must overflow");
                assert_eq!(error.index, 1);
                assert_eq!(error.source.kind, kind);
                assert_eq!(error.source.message, message);
            }
        }
    }

    #[tokio::test]
    async fn prepares_a_b_a_with_consecutive_per_sender_nonces_before_broadcast() {
        let signer_a = signer(1);
        let signer_b = signer(2);
        let accounts = Arc::new(AccountStub::default());
        accounts.set_nonce(signer_a.address.clone(), 5);
        accounts.set_nonce(signer_b.address.clone(), 9);
        let transactions = Arc::new(Endpoint::default());
        let coordinator = coordinator(accounts, transactions.clone());

        let mut batch = coordinator
            .prepare_batch(vec![
                Preparation::signer(transfer(&signer_a.address, 1), CHAIN_ID, &signer_a),
                Preparation::signer(transfer(&signer_b.address, 2), CHAIN_ID, &signer_b),
                Preparation::signer(transfer(&signer_a.address, 3), CHAIN_ID, &signer_a),
            ])
            .await
            .expect("valid batch must prepare");

        assert_eq!(
            *transactions
                .contexts
                .lock()
                .expect("context lock must be healthy"),
            [
                (signer_a.address.clone(), 5),
                (signer_b.address.clone(), 9),
                (signer_a.address.clone(), 6),
            ]
        );
        assert!(
            transactions
                .broadcasts
                .lock()
                .expect("broadcast lock must be healthy")
                .is_empty()
        );
        for _ in 0..3 {
            assert!(
                batch
                    .next()
                    .await
                    .expect("submission must succeed")
                    .is_some()
            );
        }
        assert!(batch.next().await.expect("batch must finish").is_none());
    }

    #[tokio::test]
    async fn aggregate_overspend_reports_first_threshold_crossing_without_broadcast() {
        let signer = signer(3);
        let accounts = Arc::new(AccountStub::default());
        accounts.set_native_balance(signer.address.clone(), 10);
        let transactions = Arc::new(Endpoint::default());
        let coordinator = coordinator(accounts, transactions.clone());

        let result = coordinator
            .prepare_batch(vec![
                Preparation::signer(transfer(&signer.address, 4), CHAIN_ID, &signer),
                Preparation::signer(transfer(&signer.address, 5), CHAIN_ID, &signer),
            ])
            .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("aggregate value plus fees must exceed the balance"),
        };

        assert_eq!(error.index, 1);
        assert_eq!(error.source.kind, ChainErrorKind::InsufficientFunds);
        assert!(
            transactions
                .broadcasts
                .lock()
                .expect("broadcast lock must be healthy")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn cumulative_token_amount_and_native_gas_fail_before_any_broadcast() {
        let signer = signer(7);
        let token = Address([0x77; 20]);
        let accounts = Arc::new(AccountStub::default());
        let transactions = Arc::new(Endpoint::default());
        let coordinator = coordinator(accounts.clone(), transactions.clone());
        for (token_balance, native_balance, failure) in [
            (
                10,
                2,
                "cumulative token amount must exceed the token balance",
            ),
            (
                12,
                1,
                "cumulative maximum gas must exceed the native balance",
            ),
        ] {
            accounts.set_token_balance(signer.address.clone(), token.clone(), token_balance);
            accounts.set_native_balance(signer.address.clone(), native_balance);
            let preparations = (0..2)
                .map(|_| {
                    Preparation::signer(
                        token_transfer(&signer.address, &token, 6),
                        CHAIN_ID,
                        &signer,
                    )
                })
                .collect();
            let error = match coordinator.prepare_batch(preparations).await {
                Err(error) => error,
                Ok(_) => panic!("{failure}"),
            };
            assert_eq!(error.index, 1);
            assert_eq!(error.source.kind, ChainErrorKind::InsufficientFunds);
            assert!(
                transactions
                    .broadcasts
                    .lock()
                    .expect("broadcast lock must be healthy")
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn ambiguous_submission_reconciles_and_replays_the_exact_envelope() {
        let signer = signer(4);
        let accounts = Arc::new(AccountStub::default());
        accounts.set_nonce(signer.address.clone(), 5);
        let transactions = Arc::new(Endpoint::default());
        transactions.actions([BroadcastAction::Ambiguous, BroadcastAction::Accept]);
        transactions.known([Ok(false)]);
        let coordinator = coordinator(accounts, transactions.clone());
        let signed = coordinator
            .prepare_one(Preparation::signer(
                transfer(&signer.address, 1),
                CHAIN_ID,
                &signer,
            ))
            .await
            .expect("transaction must prepare");

        let first = coordinator
            .broadcast(signed.clone())
            .await
            .expect_err("first submission must be ambiguous");
        assert_eq!(first.kind, base::TransactionErrorKind::Unavailable);
        assert_eq!(first.message, "ambiguous submission");
        assert_eq!(
            first.ambiguous_transaction_id,
            Some(base::TransactionId::new(signed.id.to_string()))
        );
        assert_eq!(
            coordinator
                .broadcast(signed.clone())
                .await
                .expect("exact replay must resolve"),
            signed.id
        );
        let broadcasts = transactions
            .broadcasts
            .lock()
            .expect("broadcast lock must be healthy");
        assert_eq!(broadcasts.as_slice(), [signed.clone(), signed]);
        assert_eq!(
            transactions
                .known_ids
                .lock()
                .expect("known-ID lock must be healthy")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn known_recovery_accepts_without_rebroadcast_and_advances_the_nonce_floor() {
        let signer = signer(4);
        let accounts = Arc::new(AccountStub::default());
        accounts.set_nonce(signer.address.clone(), 5);
        let transactions = Arc::new(Endpoint::default());
        transactions.actions([BroadcastAction::Ambiguous]);
        transactions.known([Ok(true)]);
        let coordinator = coordinator(accounts, transactions.clone());
        let old = coordinator
            .prepare_one(Preparation::signer(
                transfer(&signer.address, 1),
                CHAIN_ID,
                &signer,
            ))
            .await
            .expect("old transaction must prepare");
        coordinator
            .broadcast(old.clone())
            .await
            .expect_err("initial broadcast must be ambiguous");
        assert_eq!(
            coordinator
                .broadcast(old.clone())
                .await
                .expect("known exact hash resolves acceptance without replay"),
            old.id
        );
        assert_eq!(
            transactions.broadcasts.lock().unwrap().as_slice(),
            std::slice::from_ref(&old)
        );
        assert_eq!(
            transactions.known_ids.lock().unwrap().as_slice(),
            std::slice::from_ref(&old.id)
        );

        let new = coordinator
            .prepare_one(Preparation::signer(
                transfer(&signer.address, 2),
                CHAIN_ID,
                &signer,
            ))
            .await
            .expect("the accepted nonce cannot be reused despite the unchanged RPC nonce");
        assert_eq!(
            transactions
                .contexts
                .lock()
                .unwrap()
                .iter()
                .map(|(_, nonce)| *nonce)
                .collect::<Vec<_>>(),
            [5, 6]
        );
        coordinator
            .broadcast(new.clone())
            .await
            .expect("the next prepared transaction submits normally");
        assert_eq!(
            transactions.broadcasts.lock().unwrap().as_slice(),
            [old, new]
        );
        assert_eq!(transactions.known_ids.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn recovery_lookup_failure_preserves_the_message_local_id_and_exact_replay() {
        let signer = signer(4);
        let accounts = Arc::new(AccountStub::default());
        let transactions = Arc::new(Endpoint::default());
        transactions.actions([BroadcastAction::Ambiguous, BroadcastAction::Accept]);
        transactions.known([
            Err(SourceError {
                message: "recovery lookup failed".to_owned(),
                retryable: false,
            }),
            Ok(false),
        ]);
        let coordinator = coordinator(accounts, transactions.clone());
        let signed = coordinator
            .prepare_one(Preparation::signer(
                transfer(&signer.address, 1),
                CHAIN_ID,
                &signer,
            ))
            .await
            .expect("transaction must prepare");
        coordinator
            .broadcast(signed.clone())
            .await
            .expect_err("first submission must be ambiguous");

        let error = coordinator
            .broadcast(signed.clone())
            .await
            .expect_err("failed lookup must leave the original submission unresolved");
        assert_eq!(error.kind, base::TransactionErrorKind::Unavailable);
        assert_eq!(error.message, "recovery lookup failed");
        assert_eq!(
            error.ambiguous_transaction_id,
            Some(base::TransactionId::new(signed.id.to_string()))
        );
        assert_eq!(
            transactions
                .broadcasts
                .lock()
                .expect("broadcast lock must be healthy")
                .len(),
            1,
            "a failed lookup must not issue another broadcast"
        );

        assert_eq!(
            coordinator
                .broadcast(signed.clone())
                .await
                .expect("a later lookup must allow the retained exact envelope to replay"),
            signed.id
        );
        let broadcasts = transactions
            .broadcasts
            .lock()
            .expect("broadcast lock must be healthy");
        assert_eq!(broadcasts.as_slice(), [signed.clone(), signed]);
    }

    #[tokio::test]
    async fn new_same_sender_preparation_recovers_old_envelope_before_nonce_plus_one() {
        let signer = signer(8);
        let accounts = Arc::new(AccountStub::default());
        accounts.set_nonce(signer.address.clone(), 5);
        let transactions = Arc::new(Endpoint::default());
        transactions.actions([
            BroadcastAction::Ambiguous,
            BroadcastAction::Accept,
            BroadcastAction::Accept,
        ]);
        transactions.known([Ok(false)]);
        let coordinator = coordinator(accounts, transactions.clone());
        let old = coordinator
            .prepare_one(Preparation::signer(
                transfer(&signer.address, 1),
                CHAIN_ID,
                &signer,
            ))
            .await
            .expect("old transaction must prepare");

        let error = coordinator
            .broadcast(old.clone())
            .await
            .expect_err("old submission must be ambiguous");
        assert_eq!(
            error.ambiguous_transaction_id,
            Some(base::TransactionId::new(old.id.to_string()))
        );
        let new = coordinator
            .prepare_one(Preparation::signer(
                transfer(&signer.address, 2),
                CHAIN_ID,
                &signer,
            ))
            .await
            .expect("new preparation must recover the old transaction first");
        coordinator
            .broadcast(new.clone())
            .await
            .expect("new transaction must submit");

        assert_eq!(
            transactions
                .contexts
                .lock()
                .expect("context lock must be healthy")
                .iter()
                .map(|(_, nonce)| *nonce)
                .collect::<Vec<_>>(),
            [5, 6]
        );
        let broadcasts = transactions
            .broadcasts
            .lock()
            .expect("broadcast lock must be healthy");
        assert_eq!(broadcasts.as_slice(), [old.clone(), old.clone(), new]);
        assert_eq!(
            *transactions
                .known_ids
                .lock()
                .expect("known-ID lock must be healthy"),
            [old.id]
        );
    }

    #[tokio::test]
    async fn terminal_initial_rejection_releases_the_nonce_for_reuse() {
        let signer = signer(9);
        let accounts = Arc::new(AccountStub::default());
        accounts.set_nonce(signer.address.clone(), 5);
        let transactions = Arc::new(Endpoint::default());
        transactions.actions([BroadcastAction::Reject]);
        let coordinator = coordinator(accounts, transactions.clone());
        let rejected = coordinator
            .prepare_one(Preparation::signer(
                transfer(&signer.address, 1),
                CHAIN_ID,
                &signer,
            ))
            .await
            .expect("rejected transaction must prepare");
        let error = coordinator
            .broadcast(rejected)
            .await
            .expect_err("scripted submission must be rejected");
        assert_eq!(error.kind, base::TransactionErrorKind::Rejected);
        assert_eq!(error.ambiguous_transaction_id, None);

        let replacement = coordinator
            .prepare_one(Preparation::signer(
                transfer(&signer.address, 2),
                CHAIN_ID,
                &signer,
            ))
            .await
            .expect("terminal rejection must release the nonce");
        coordinator
            .broadcast(replacement)
            .await
            .expect("replacement must submit");
        assert_eq!(
            transactions
                .contexts
                .lock()
                .expect("context lock must be healthy")
                .iter()
                .map(|(_, nonce)| *nonce)
                .collect::<Vec<_>>(),
            [5, 5]
        );
    }

    #[tokio::test]
    async fn cancellation_after_submission_starts_retains_unknown_for_exact_recovery() {
        let signer = signer(5);
        let accounts = Arc::new(AccountStub::default());
        let transactions = Arc::new(Endpoint::default());
        transactions.actions([BroadcastAction::Pending, BroadcastAction::Accept]);
        transactions.known([Ok(false)]);
        let coordinator = coordinator(accounts, transactions.clone());
        let signed = coordinator
            .prepare_one(Preparation::signer(
                transfer(&signer.address, 1),
                CHAIN_ID,
                &signer,
            ))
            .await
            .expect("transaction must prepare");

        let mut submission = Box::pin(coordinator.broadcast(signed.clone()));
        let waker = futures_util::task::noop_waker_ref();
        let mut context = Context::from_waker(waker);
        assert_eq!(submission.as_mut().poll(&mut context), Poll::Pending);
        drop(submission);

        assert_eq!(
            coordinator
                .broadcast(signed.clone())
                .await
                .expect("cancelled submission must recover exact bytes"),
            signed.id
        );
        let broadcasts = transactions
            .broadcasts
            .lock()
            .expect("broadcast lock must be healthy");
        assert_eq!(broadcasts.as_slice(), [signed.clone(), signed]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_same_sender_operations_never_reuse_a_nonce() {
        let signer = Arc::new(signer(6));
        let accounts = Arc::new(AccountStub::default());
        accounts.set_nonce(signer.address.clone(), 5);
        let transactions = Arc::new(Endpoint::default());
        let coordinator = coordinator(accounts, transactions.clone());
        let (prepared, prepared_rx) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());

        let first = {
            let coordinator = coordinator.clone();
            let signer = signer.clone();
            let release = release.clone();
            tokio::spawn(async move {
                let signed = coordinator
                    .prepare_one(Preparation::signer(
                        transfer(&signer.address, 1),
                        CHAIN_ID,
                        signer.as_ref(),
                    ))
                    .await
                    .expect("first transaction must prepare");
                prepared.send(()).expect("test receiver must remain");
                release.notified().await;
                coordinator.broadcast(signed).await
            })
        };
        prepared_rx.await.expect("first transaction must prepare");
        let second = {
            let coordinator = coordinator.clone();
            let signer = signer.clone();
            tokio::spawn(async move {
                let signed = coordinator
                    .prepare_one(Preparation::signer(
                        transfer(&signer.address, 2),
                        CHAIN_ID,
                        signer.as_ref(),
                    ))
                    .await
                    .expect("second transaction must prepare");
                coordinator.broadcast(signed).await
            })
        };
        tokio::task::yield_now().await;
        release.notify_one();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            first
                .await
                .expect("first task must join")
                .expect("first submission must succeed");
            second
                .await
                .expect("second task must join")
                .expect("second submission must succeed");
        })
        .await
        .expect("same-sender coordination must not hang");
        assert_eq!(
            transactions
                .contexts
                .lock()
                .expect("context lock must be healthy")
                .iter()
                .map(|(_, nonce)| *nonce)
                .collect::<Vec<_>>(),
            [5, 6]
        );
    }

    #[test]
    fn definite_submission_errors_preserve_retryability_without_an_ambiguous_id() {
        for (retryable, kind) in [
            (true, TransactionErrorKind::Unavailable),
            (false, TransactionErrorKind::Rejected),
        ] {
            let error = definite_submission_error(SourceError {
                message: "coordinator claim failed".to_owned(),
                retryable,
            });
            assert_eq!(error.kind, kind);
            assert_eq!(error.message, "coordinator claim failed");
            assert!(error.ambiguous_transaction_id.is_none());
        }
    }
}
