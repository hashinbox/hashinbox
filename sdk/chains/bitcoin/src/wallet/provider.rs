use std::sync::Arc;

use base::{
    Address as BaseAddress, Addresser, Broadcaster, Submission as BroadcastReceipt,
    TransactionBuilder as BaseBuilder, TransactionError, TransactionErrorKind, TransactionFuture,
    TransactionId as BaseTransactionId,
};
use indexing::{History as IndexHistory, IndexScope};
use wallets::{
    AddressFormat as WalletAddressFormat, AddressText, Balance, BalanceReader,
    Error as WalletError, ErrorKind as WalletErrorKind, FutureResult, Provider, SecretBytes,
    TransactionFactory, Wallet as WalletContract,
};

use crate::{
    AddressType, FeeRate, Fees, IndexUtxos, KeyPair, Network, Satoshi, SignedTransaction,
    Transactions,
};

pub(super) const SNAPSHOT_KIND: &str = "bitcoin.transfer";
pub(crate) const PREPARED_KIND: &str = "bitcoin.signed";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub scope: IndexScope,
    pub network: Network,
    pub address_type: AddressType,
    pub fee_target_blocks: u16,
    pub max_fee_rate: FeeRate,
}

impl Config {
    fn validate(&self) -> Result<(), WalletError> {
        if self.scope.chain.0 != "bitcoin" || self.scope.network != self.network.canonical_name() {
            return Err(WalletError::new(
                WalletErrorKind::Unsupported,
                "Bitcoin wallet chain and network must agree",
            ));
        }
        Ok(())
    }
}

pub struct Factory {
    config: Config,
    utxos: Arc<IndexUtxos>,
    fees: Arc<dyn Fees>,
    transactions: Arc<dyn Transactions>,
    history: Arc<dyn IndexHistory>,
}

impl Factory {
    #[must_use]
    pub fn new(
        config: Config,
        utxos: Arc<IndexUtxos>,
        fees: Arc<dyn Fees>,
        transactions: Arc<dyn Transactions>,
        history: Arc<dyn IndexHistory>,
    ) -> Self {
        Self {
            config,
            utxos,
            fees,
            transactions,
            history,
        }
    }

    /// Consumes a local key and attaches this provider's operational capabilities.
    /// The key must match the provider's exact network and address type.
    pub fn attach(&self, key: KeyPair) -> Result<Arc<dyn WalletContract>, WalletError> {
        self.config.validate()?;
        if key.network != self.config.network || key.address_type != self.config.address_type {
            return Err(WalletError::new(
                WalletErrorKind::AddressMismatch,
                "Bitcoin key network and address type must match the wallet provider",
            ));
        }
        Ok(Arc::new(Wallet {
            config: self.config.clone(),
            key: Arc::new(key),
            utxos: self.utxos.clone(),
            fees: self.fees.clone(),
            transactions: self.transactions.clone(),
            history: self.history.clone(),
        }))
    }

    fn generate_with(
        &self,
        generator: fn() -> Result<SecretBytes, crypto::Error>,
    ) -> FutureResult<'_, Arc<dyn WalletContract>> {
        match generator() {
            Ok(secret) => self.create(secret),
            Err(error) => {
                let error = WalletError::new(WalletErrorKind::Generation, error.to_string());
                Box::pin(async move { Err(error) })
            }
        }
    }

    #[must_use]
    pub fn transactions(&self) -> Arc<dyn wallets::Sender> {
        Arc::new(crate::batch::Batch::new(
            self.config.network,
            self.utxos.clone(),
            self.fees.clone(),
            self.transactions.clone(),
            self.config.fee_target_blocks,
            self.config.max_fee_rate,
        ))
    }
}

impl Provider for Factory {
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn WalletContract>> {
        Box::pin(async move {
            self.config.validate()?;
            let key = KeyPair::import(secret, self.config.network, self.config.address_type)?;
            self.attach(key)
        })
    }

    fn generate(&self) -> FutureResult<'_, Arc<dyn WalletContract>> {
        self.generate_with(SecretBytes::generate_secp256k1)
    }

    fn validate(&self, scope: &IndexScope, wallet: &dyn WalletContract) -> Result<(), WalletError> {
        self.config.validate()?;
        let Some(wallet) = (wallet as &dyn std::any::Any).downcast_ref::<Wallet>() else {
            return Err(WalletError::new(
                WalletErrorKind::Unsupported,
                "wallet is not a Bitcoin wallet created by this provider type",
            ));
        };
        if scope != &self.config.scope
            || wallet.config != self.config
            || !Arc::ptr_eq(&wallet.utxos, &self.utxos)
            || !Arc::ptr_eq(&wallet.fees, &self.fees)
            || !Arc::ptr_eq(&wallet.transactions, &self.transactions)
            || !Arc::ptr_eq(&wallet.history, &self.history)
        {
            return Err(WalletError::new(
                WalletErrorKind::Unsupported,
                "Bitcoin wallet must match provider scope, configuration, and capabilities",
            ));
        }
        Ok(())
    }
}

pub(super) struct Wallet {
    pub(super) config: Config,
    pub(super) key: Arc<KeyPair>,
    pub(super) utxos: Arc<IndexUtxos>,
    pub(super) fees: Arc<dyn Fees>,
    pub(super) transactions: Arc<dyn Transactions>,
    pub(super) history: Arc<dyn IndexHistory>,
}

impl Addresser for Wallet {
    fn address(&self) -> BaseAddress {
        self.key.address()
    }
}

impl base::Signer for Wallet {
    fn sign<'a>(&'a self, request: base::SignRequest) -> base::SignFuture<'a> {
        self.key.sign(request)
    }
}

impl WalletAddressFormat for Wallet {
    fn address_text(&self, address: &BaseAddress) -> Result<AddressText, WalletError> {
        self.key.address_text(address)
    }

    fn parse_address(&self, address: &AddressText) -> Result<BaseAddress, WalletError> {
        self.key.parse_address(address)
    }
}

impl BalanceReader for Wallet {
    fn balance<'a>(&'a self) -> FutureResult<'a, Balance> {
        Box::pin(async move {
            let set = self
                .utxos
                .utxos(vec![self.key.pair.address.clone()])
                .await
                .map_err(|error| WalletError::new(WalletErrorKind::Balance, error.to_string()))?;
            let atomic = set
                .outputs
                .iter()
                .try_fold(0_u64, |sum, output| sum.checked_add(output.value.0))
                .ok_or_else(|| {
                    WalletError::new(WalletErrorKind::Balance, "Bitcoin balance exceeds u64")
                })?;
            Ok(Balance {
                amount: Satoshi(atomic).decimal(),
                observed_at: Some(set.checkpoint),
            })
        })
    }
}

impl TransactionFactory for Wallet {
    fn transaction(&self) -> Box<dyn BaseBuilder> {
        Box::new(self.builder())
    }

    fn restore(
        &self,
        snapshot: &base::TransactionSnapshot,
    ) -> Result<Box<dyn BaseBuilder>, TransactionError> {
        Ok(Box::new(super::builder::Builder::restore(self, snapshot)?))
    }

    fn broadcaster(&self) -> &dyn Broadcaster {
        self
    }
}

impl wallets::SingleSender for Wallet {
    fn send<'a>(
        &'a self,
        destination: wallets::AddressText,
        amount: base::Decimal,
    ) -> wallets::FutureResult<'a, base::TransactionId> {
        wallets::send_with_transaction(self, destination, amount)
    }
}

impl Broadcaster for Wallet {
    fn broadcast<'a>(
        &'a self,
        prepared: &'a base::SignedTransaction,
    ) -> TransactionFuture<'a, Result<BroadcastReceipt, TransactionError>> {
        Box::pin(broadcast_prepared(
            self.transactions.as_ref(),
            self.config.max_fee_rate,
            prepared,
        ))
    }
}

pub(crate) async fn broadcast_prepared(
    transactions: &dyn Transactions,
    max_fee_rate: FeeRate,
    prepared: &base::SignedTransaction,
) -> Result<BroadcastReceipt, TransactionError> {
    if prepared.version() != base::SignedTransaction::VERSION || prepared.kind() != PREPARED_KIND {
        return Err(TransactionError::new(
            TransactionErrorKind::InvalidTransaction,
            "prepared transaction is not a Bitcoin signed envelope",
        ));
    }
    let id = prepared
        .id()
        .as_str()
        .parse::<crate::TransactionId>()
        .map_err(|error| {
            TransactionError::new(TransactionErrorKind::InvalidTransaction, error.to_string())
        })?;
    let signed =
        SignedTransaction::from_consensus_bytes(id, prepared.envelope().as_bytes().to_vec())
            .map_err(|error| {
                TransactionError::new(TransactionErrorKind::InvalidTransaction, error.to_string())
            })?;
    let native_id = signed.id();
    let preflight = transactions
        .preflight(&signed, max_fee_rate)
        .await
        .map_err(|error| {
            TransactionError::new(TransactionErrorKind::Unavailable, error.to_string())
        })?;
    if !preflight.allowed {
        return Err(TransactionError::new(
            TransactionErrorKind::Rejected,
            "Bitcoin node rejected transaction preflight",
        ));
    }
    let submitted = transactions.broadcast(signed, max_fee_rate).await?;
    if submitted != native_id {
        return Err(TransactionError::new(
            TransactionErrorKind::Unavailable,
            "Bitcoin transaction capability returned a different transaction ID",
        ));
    }
    Ok(BroadcastReceipt {
        id: BaseTransactionId::new(native_id.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use crate::Address;
    use base::{Digest, SignRequest, SignablePayload, SignatureEncoding, SignatureScheme};
    use bitcoin::{
        Address as NativeAddress, Amount, CompressedPublicKey, OutPoint, PublicKey, ScriptBuf,
        Sequence, Transaction as NativeTransaction, TxIn, TxOut, Txid, Witness, XOnlyPublicKey,
        absolute, consensus, hashes::Hash, secp256k1::Secp256k1, transaction::Version,
    };
    use crypto::PublicKeyFormat;
    use futures_executor::block_on;
    use indexing::{
        AssetId, BlockHash, BlockHeight, BlockPosition, BlockRef, BoxFuture, ChainId, HistoryQuery,
        IndexError, MovementId, ObservedTransaction, OutputPage, OutputRequest, Outputs,
        SourceError, TransactionPage, TransactionRef, TransactionStatus, ValueMovement,
    };
    use std::sync::Mutex;
    use wallets::AddressEncoding;

    use super::*;

    struct InactiveDependencies;

    impl indexing::Checkpoint for InactiveDependencies {
        fn checkpoint<'a>(
            &'a self,
            _scope: &'a IndexScope,
        ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
            unreachable!("startup wallet insertion must not read the checkpoint")
        }
    }

    impl Outputs for InactiveDependencies {
        fn list<'a>(
            &'a self,
            _request: OutputRequest,
        ) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
            Box::pin(async { unreachable!("wallet generation must not read indexed outputs") })
        }
    }

    impl Fees for InactiveDependencies {
        fn estimate<'a>(
            &'a self,
            _target_blocks: u16,
        ) -> BoxFuture<'a, Result<FeeRate, SourceError>> {
            Box::pin(async { unreachable!("wallet generation must not estimate fees") })
        }
    }

    impl Transactions for InactiveDependencies {
        fn preflight<'a>(
            &'a self,
            _transaction: &'a SignedTransaction,
            _max_fee_rate: FeeRate,
        ) -> BoxFuture<'a, Result<crate::Preflight, SourceError>> {
            Box::pin(async { unreachable!("wallet generation must not preflight a transaction") })
        }

        fn broadcast<'a>(
            &'a self,
            _transaction: SignedTransaction,
            _max_fee_rate: FeeRate,
        ) -> BoxFuture<'a, Result<crate::TransactionId, TransactionError>> {
            Box::pin(async { unreachable!("wallet generation must not broadcast a transaction") })
        }
    }

    impl IndexHistory for InactiveDependencies {
        fn history<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
            Box::pin(async { unreachable!("wallet generation must not read indexed history") })
        }
    }

    struct ForeignWallet(Arc<dyn WalletContract>);

    impl Addresser for ForeignWallet {
        fn address(&self) -> BaseAddress {
            self.0.address()
        }
    }

    impl WalletAddressFormat for ForeignWallet {
        fn address_text(&self, address: &BaseAddress) -> Result<AddressText, WalletError> {
            self.0.address_text(address)
        }

        fn parse_address(&self, address: &AddressText) -> Result<BaseAddress, WalletError> {
            self.0.parse_address(address)
        }
    }

    impl BalanceReader for ForeignWallet {
        fn balance<'a>(&'a self) -> FutureResult<'a, Balance> {
            self.0.balance()
        }
    }

    impl wallets::HistoryReader for ForeignWallet {
        fn history<'a>(
            &'a self,
            request: wallets::HistoryRequest,
        ) -> FutureResult<'a, wallets::History> {
            self.0.history(request)
        }
    }

    impl wallets::SingleSender for ForeignWallet {
        fn send<'a>(
            &'a self,
            destination: AddressText,
            amount: base::Decimal,
        ) -> FutureResult<'a, BaseTransactionId> {
            self.0.send(destination, amount)
        }
    }

    impl base::Signer for ForeignWallet {
        fn sign<'a>(&'a self, request: SignRequest) -> base::SignFuture<'a> {
            self.0.sign(request)
        }
    }

    struct RecordedHistory(TransactionPage);

    impl IndexHistory for RecordedHistory {
        fn history<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
            Box::pin(async { Ok(self.0.clone()) })
        }
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct TransactionCalls {
        preflight: Vec<(crate::TransactionId, Vec<u8>)>,
        broadcast: Vec<(crate::TransactionId, Vec<u8>)>,
    }

    struct InspectingTransactions {
        preflight: Result<crate::Preflight, SourceError>,
        broadcast: Result<crate::TransactionId, TransactionError>,
        calls: Mutex<TransactionCalls>,
    }

    impl InspectingTransactions {
        fn new(
            preflight: Result<crate::Preflight, SourceError>,
            broadcast: Result<crate::TransactionId, TransactionError>,
        ) -> Self {
            Self {
                preflight,
                broadcast,
                calls: Mutex::new(TransactionCalls::default()),
            }
        }

        fn calls(&self) -> TransactionCalls {
            self.calls
                .lock()
                .expect("transaction call lock must be healthy")
                .clone()
        }
    }

    impl Transactions for InspectingTransactions {
        fn preflight<'a>(
            &'a self,
            transaction: &'a SignedTransaction,
            _max_fee_rate: FeeRate,
        ) -> BoxFuture<'a, Result<crate::Preflight, SourceError>> {
            self.calls
                .lock()
                .expect("transaction call lock must be healthy")
                .preflight
                .push((transaction.id(), transaction.consensus_bytes().to_vec()));
            let result = self.preflight.clone();
            Box::pin(async move { result })
        }

        fn broadcast<'a>(
            &'a self,
            transaction: SignedTransaction,
            _max_fee_rate: FeeRate,
        ) -> BoxFuture<'a, Result<crate::TransactionId, TransactionError>> {
            self.calls
                .lock()
                .expect("transaction call lock must be healthy")
                .broadcast
                .push((transaction.id(), transaction.consensus_bytes().to_vec()));
            let result = self.broadcast.clone();
            Box::pin(async move { result })
        }
    }

    fn factory(address_type: AddressType) -> Factory {
        let transactions: Arc<dyn Transactions> = Arc::new(InactiveDependencies);
        factory_with_transactions(address_type, transactions)
    }

    fn factory_with_transactions(
        address_type: AddressType,
        transactions: Arc<dyn Transactions>,
    ) -> Factory {
        let network = Network::Regtest;
        let scope = IndexScope {
            chain: ChainId(crate::CHAIN.to_owned()),
            network: network.canonical_name().to_owned(),
        };
        let dependencies = Arc::new(InactiveDependencies);
        let outputs: Arc<dyn Outputs> = dependencies.clone();
        let fees: Arc<dyn Fees> = dependencies.clone();
        let history: Arc<dyn IndexHistory> = dependencies;
        let utxos = Arc::new(
            IndexUtxos::new(scope.clone(), network, outputs)
                .expect("fixture scope must match the Bitcoin network"),
        );
        Factory::new(
            Config {
                scope,
                network,
                address_type,
                fee_target_blocks: 6,
                max_fee_rate: FeeRate::new(1_000),
            },
            utxos,
            fees,
            transactions,
            history,
        )
    }

    fn equivalent_factory(provider: &Factory) -> Factory {
        Factory::new(
            provider.config.clone(),
            provider.utxos.clone(),
            provider.fees.clone(),
            provider.transactions.clone(),
            provider.history.clone(),
        )
    }

    #[test]
    fn validation_accepts_attached_keys_and_equivalent_providers_without_io() {
        for address_type in [AddressType::SegwitV0, AddressType::Taproot] {
            let provider = factory(address_type);
            let equivalent = equivalent_factory(&provider);
            let key = KeyPair::import(SecretBytes::new([1_u8; 32]), Network::Regtest, address_type)
                .unwrap();
            let address = key.address();
            let wallet = equivalent.attach(key).unwrap();

            provider
                .validate(&provider.config.scope, wallet.as_ref())
                .unwrap();
            equivalent
                .validate(&provider.config.scope, wallet.as_ref())
                .unwrap();
            assert_eq!(wallet.address(), address);
        }
    }

    #[test]
    fn collection_inserts_attached_keys_with_their_address_and_birthday_without_io() {
        let provider = factory(AddressType::SegwitV0);
        let key = KeyPair::import(
            SecretBytes::new([1_u8; 32]),
            Network::Regtest,
            AddressType::SegwitV0,
        )
        .unwrap();
        let address = key.address_text(&key.address()).unwrap();
        let wallet = provider.attach(key).unwrap();
        let scope = provider.config.scope.clone();
        let sender = provider.transactions();
        let mut collection = wallets::Wallets::new(Arc::new(InactiveDependencies));
        collection
            .register("btc", scope.clone(), provider, sender, None)
            .unwrap();

        let info =
            block_on(collection.insert("existing", &"btc", wallet, BlockPosition(42))).unwrap();

        assert_eq!(info.address, address);
        assert_eq!(collection.get(&"existing").unwrap(), info);
        assert_eq!(
            collection.filters().unwrap(),
            vec![indexing::AddressFilter {
                address: indexing::CanonicalAddress {
                    scope,
                    value: address.text,
                },
                start_position: BlockPosition(42),
            }],
        );
    }

    #[test]
    fn collection_rejects_a_wallet_with_unrelated_dependencies_before_publication() {
        let provider = factory(AddressType::SegwitV0);
        let unrelated = factory(AddressType::SegwitV0);
        let wallet = block_on(unrelated.create(SecretBytes::new([1_u8; 32]))).unwrap();
        let sender = provider.transactions();
        let mut collection = wallets::Wallets::new(Arc::new(InactiveDependencies));
        collection
            .register("btc", provider.config.scope.clone(), provider, sender, None)
            .unwrap();

        let error =
            block_on(collection.insert("existing", &"btc", wallet, BlockPosition(42))).unwrap_err();

        assert_eq!(error.kind, WalletErrorKind::Unsupported);
        assert_eq!(
            collection.get(&"existing").unwrap_err().kind,
            WalletErrorKind::NotFound
        );
        assert!(collection.filters().unwrap().is_empty());
    }

    #[test]
    fn validation_rejects_a_different_registered_scope_without_io() {
        let provider = factory(AddressType::SegwitV0);
        let wallet = block_on(provider.create(SecretBytes::new([1_u8; 32]))).unwrap();
        for scope in [
            IndexScope {
                chain: ChainId("ethereum".to_owned()),
                network: provider.config.scope.network.clone(),
            },
            IndexScope {
                chain: provider.config.scope.chain.clone(),
                network: Network::Mainnet.canonical_name().to_owned(),
            },
        ] {
            let error = provider.validate(&scope, wallet.as_ref()).unwrap_err();
            assert_eq!(error.kind, WalletErrorKind::Unsupported);
        }
    }

    #[test]
    fn validation_rejects_another_wallet_type_even_with_the_same_address() {
        let provider = factory(AddressType::SegwitV0);
        let wallet = block_on(provider.create(SecretBytes::new([1_u8; 32]))).unwrap();
        let foreign = ForeignWallet(wallet.clone());
        assert_eq!(foreign.address(), wallet.address());

        let error = provider
            .validate(&provider.config.scope, &foreign)
            .unwrap_err();

        assert_eq!(error.kind, WalletErrorKind::Unsupported);
    }

    #[test]
    fn validation_rejects_every_mismatched_configuration_field_without_io() {
        let provider = factory(AddressType::SegwitV0);
        let wallet = block_on(provider.create(SecretBytes::new([1_u8; 32]))).unwrap();
        for field in [
            "scope",
            "network",
            "address_type",
            "fee_target",
            "max_fee_rate",
        ] {
            let mut candidate = equivalent_factory(&provider);
            match field {
                "scope" => candidate.config.scope.chain = ChainId("other".to_owned()),
                "network" => {
                    candidate.config.network = Network::Signet;
                    candidate.config.scope.network = Network::Signet.canonical_name().to_owned();
                }
                "address_type" => candidate.config.address_type = AddressType::Taproot,
                "fee_target" => candidate.config.fee_target_blocks += 1,
                "max_fee_rate" => candidate.config.max_fee_rate = FeeRate::new(2_000),
                _ => unreachable!("fixture lists configuration fields exhaustively"),
            }
            let error = candidate
                .validate(&candidate.config.scope, wallet.as_ref())
                .unwrap_err();
            assert_eq!(error.kind, WalletErrorKind::Unsupported, "{field}");
        }
    }

    #[test]
    fn validation_rejects_each_unrelated_operational_dependency_without_io() {
        let provider = factory(AddressType::SegwitV0);
        let wallet = block_on(provider.create(SecretBytes::new([1_u8; 32]))).unwrap();
        for dependency in ["utxos", "fees", "transactions", "history"] {
            let mut candidate = equivalent_factory(&provider);
            match dependency {
                "utxos" => {
                    let utxos = IndexUtxos::new(
                        provider.config.scope.clone(),
                        provider.config.network,
                        Arc::new(InactiveDependencies),
                    )
                    .unwrap();
                    candidate.utxos = Arc::new(utxos);
                }
                "fees" => candidate.fees = Arc::new(InactiveDependencies),
                "transactions" => candidate.transactions = Arc::new(InactiveDependencies),
                "history" => candidate.history = Arc::new(InactiveDependencies),
                _ => unreachable!("fixture lists dependencies exhaustively"),
            }
            let error = candidate
                .validate(&provider.config.scope, wallet.as_ref())
                .unwrap_err();
            assert_eq!(error.kind, WalletErrorKind::Unsupported, "{dependency}");
        }
    }

    #[test]
    fn history_preserves_native_precision_and_rejects_unsupported_assets() {
        for (chain, name, expected_error) in [
            (crate::CHAIN, "native", None),
            (
                crate::CHAIN,
                "token",
                Some("Bitcoin history contains an unsupported asset"),
            ),
            (
                "ethereum",
                "native",
                Some("indexed asset does not belong to the transaction chain"),
            ),
        ] {
            let mut factory = factory(AddressType::SegwitV0);
            let scope = factory.config.scope.clone();
            let asset = AssetId {
                chain: ChainId(chain.to_owned()),
                asset: name.to_owned(),
            };
            let block = BlockRef {
                position: BlockPosition(0),
                height: BlockHeight(0),
                hash: BlockHash(vec![1; 32]),
                parent: None,
                timestamp: None,
            };
            factory.history = Arc::new(RecordedHistory(TransactionPage {
                checkpoint: Some(block.clone()),
                transactions: vec![ObservedTransaction {
                    scope: scope.clone(),
                    transaction_id: TransactionRef {
                        scope,
                        value: "transaction".to_owned(),
                    },
                    status: TransactionStatus::Included {
                        block,
                        confirmations: 1,
                    },
                    movements: vec![ValueMovement::Output {
                        id: MovementId("output".to_owned()),
                        asset: asset.clone(),
                        amount: base::Decimal::from(123_456_789_u64),
                        owner: None,
                    }],
                    fee: None,
                }],
                next: None,
            }));
            let wallet = block_on(factory.create(SecretBytes::new([1_u8; 32])))
                .expect("fixture wallet must be valid");
            let result = block_on(wallet.history(wallets::HistoryRequest {
                after: None,
                limit: 10,
            }));

            if let Some(message) = expected_error {
                let error = result.expect_err("unsupported history asset must fail");
                assert_eq!(error.kind, WalletErrorKind::History);
                assert_eq!(error.message, message);
            } else {
                let history = result.expect("native history must be presented");
                let movement = &history.transactions[0].movements[0];
                assert_eq!(movement.asset.id, asset);
                assert_eq!(movement.asset.name.as_deref(), Some("Bitcoin"));
                assert_eq!(movement.asset.ticker.as_deref(), Some("BTC"));
                assert_eq!(movement.asset.decimals, 8);
                assert_eq!(movement.amount.to_string(), "1.23456789");
            }
        }
    }

    fn prepared_transaction() -> (base::SignedTransaction, crate::TransactionId) {
        let transaction = NativeTransaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array([3; 32]), 0),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let id = crate::TransactionId::from(transaction.compute_txid());
        let envelope = consensus::serialize(&transaction);
        (
            base::SignedTransaction::new(
                PREPARED_KIND,
                BaseTransactionId::new(id.to_string()),
                base::TransactionEnvelope::new(envelope),
            ),
            id,
        )
    }

    fn allowed_preflight() -> crate::Preflight {
        crate::Preflight {
            allowed: true,
            reject_reason: None,
            virtual_size: None,
            base_fee: None,
        }
    }

    #[test]
    fn broadcast_preserves_the_transaction_layers_exact_local_id() {
        let provider_candidate = format!("{:064x}", 9);
        let (prepared, native_id) = prepared_transaction();
        let local_id = BaseTransactionId::new(native_id.to_string());
        let transactions = Arc::new(InspectingTransactions::new(
            Ok(allowed_preflight()),
            Err(TransactionError::new(
                TransactionErrorKind::Unavailable,
                format!("provider claimed transaction {provider_candidate}"),
            )
            .with_ambiguous_transaction_id(local_id.clone())),
        ));
        let error = block_on(broadcast_prepared(
            transactions.as_ref(),
            FeeRate::new(1_000),
            &prepared,
        ))
        .expect_err("an unknown Bitcoin broadcast outcome must fail with reconciliation data");

        assert_eq!(error.kind, TransactionErrorKind::Unavailable);
        assert_eq!(error.ambiguous_transaction_id, Some(local_id));
        assert_ne!(
            error
                .ambiguous_transaction_id
                .as_ref()
                .expect("ambiguity must carry the local ID")
                .as_str(),
            provider_candidate
        );
        let expected = (native_id, prepared.envelope().as_bytes().to_vec());
        let calls = transactions.calls();
        assert_eq!(calls.preflight, vec![expected.clone()]);
        assert_eq!(calls.broadcast, vec![expected]);
    }

    #[test]
    fn matching_broadcast_returns_the_validated_local_id() {
        let (prepared, native_id) = prepared_transaction();
        let transactions = Arc::new(InspectingTransactions::new(
            Ok(allowed_preflight()),
            Ok(native_id),
        ));
        let submitted = block_on(broadcast_prepared(
            transactions.as_ref(),
            FeeRate::new(1_000),
            &prepared,
        ))
        .expect("a matching Bitcoin broadcast ID must be acknowledged");

        assert_eq!(submitted.id, BaseTransactionId::new(native_id.to_string()));
    }

    #[test]
    fn invalid_local_envelope_has_no_ambiguity_or_chain_io() {
        let transactions = Arc::new(InspectingTransactions::new(
            Ok(allowed_preflight()),
            Ok(crate::TransactionId([9; 32])),
        ));
        let (prepared, native_id) = prepared_transaction();
        let different_id = crate::TransactionId([9; 32]);
        assert_ne!(native_id, different_id);
        let invalid = base::SignedTransaction::new(
            PREPARED_KIND,
            BaseTransactionId::new(different_id.to_string()),
            base::TransactionEnvelope::new(prepared.envelope().as_bytes().to_vec()),
        );

        let error = block_on(broadcast_prepared(
            transactions.as_ref(),
            FeeRate::new(1_000),
            &invalid,
        ))
        .expect_err("a declared ID that disagrees with its bytes must fail locally");

        assert_eq!(error.kind, TransactionErrorKind::InvalidTransaction);
        assert_eq!(error.ambiguous_transaction_id, None);
        assert_eq!(transactions.calls(), TransactionCalls::default());
    }

    #[test]
    fn preflight_failure_has_no_ambiguity_and_never_broadcasts() {
        let transactions = Arc::new(InspectingTransactions::new(
            Err(SourceError {
                message: "Bitcoin preflight is unavailable".to_owned(),
                retryable: true,
            }),
            Ok(crate::TransactionId([9; 32])),
        ));
        let (prepared, native_id) = prepared_transaction();

        let error = block_on(broadcast_prepared(
            transactions.as_ref(),
            FeeRate::new(1_000),
            &prepared,
        ))
        .expect_err("preflight failure must stop before broadcast");

        assert_eq!(error.kind, TransactionErrorKind::Unavailable);
        assert_eq!(error.message, "Bitcoin preflight is unavailable");
        assert_eq!(error.ambiguous_transaction_id, None);
        assert_eq!(
            transactions.calls(),
            TransactionCalls {
                preflight: vec![(native_id, prepared.envelope().as_bytes().to_vec(),)],
                broadcast: Vec::new(),
            }
        );
    }

    #[test]
    fn rejected_preflight_has_no_ambiguity_and_never_broadcasts() {
        let transactions = Arc::new(InspectingTransactions::new(
            Ok(crate::Preflight {
                allowed: false,
                reject_reason: Some("missing-inputs".to_owned()),
                virtual_size: Some(82),
                base_fee: None,
            }),
            Ok(crate::TransactionId([9; 32])),
        ));
        let (prepared, _) = prepared_transaction();

        let error = block_on(broadcast_prepared(
            transactions.as_ref(),
            FeeRate::new(1_000),
            &prepared,
        ))
        .expect_err("a rejected Bitcoin preflight must stop before broadcast");

        assert_eq!(error.kind, TransactionErrorKind::Rejected);
        assert_eq!(error.ambiguous_transaction_id, None);
        assert!(transactions.calls().broadcast.is_empty());
    }

    #[test]
    fn attaching_local_key_preserves_address_and_signer_without_io() {
        for address_type in [AddressType::SegwitV0, AddressType::Taproot] {
            let key = crate::KeyPair::import(
                SecretBytes::new([1_u8; 32]),
                Network::Regtest,
                address_type,
            )
            .unwrap();
            let address = key.address();
            let request = SignRequest {
                payload: SignablePayload::Digest(Digest { bytes: vec![9; 32] }),
                scheme: SignatureScheme::EcdsaSecp256k1,
                encoding: SignatureEncoding::Der,
                public_key_format: PublicKeyFormat::Compressed,
                key_tweak: None,
            };
            let signed = block_on(base::Signer::sign(&key, request.clone())).unwrap();
            let wallet = factory(address_type).attach(key).unwrap();

            assert_eq!(wallet.address(), address);
            assert_eq!(block_on(wallet.sign(request)).unwrap(), signed);
        }
    }

    #[test]
    fn attachment_rejects_network_and_address_type_mismatches() {
        for (network, address_type) in [
            (Network::Mainnet, AddressType::SegwitV0),
            (Network::Regtest, AddressType::Taproot),
        ] {
            let key = crate::KeyPair::import(SecretBytes::new([1_u8; 32]), network, address_type)
                .unwrap();
            let error = factory(AddressType::SegwitV0).attach(key).err().unwrap();
            assert_eq!(error.kind, WalletErrorKind::AddressMismatch);
        }

        let mut provider = factory(AddressType::SegwitV0);
        provider.config.network = Network::Signet;
        provider.config.scope.network = Network::Signet.canonical_name().to_owned();
        let key = crate::KeyPair::import(
            SecretBytes::new([1_u8; 32]),
            Network::Testnet3,
            AddressType::SegwitV0,
        )
        .unwrap();
        let error = provider.attach(key).err().unwrap();
        assert_eq!(error.kind, WalletErrorKind::AddressMismatch);
    }

    #[test]
    fn generation_builds_each_supported_regtest_address_type() {
        for (address_type, encoding, native_type, scheme, signature_encoding, public_format) in [
            (
                AddressType::SegwitV0,
                AddressEncoding::Bech32,
                bitcoin::AddressType::P2wpkh,
                SignatureScheme::EcdsaSecp256k1,
                SignatureEncoding::Der,
                PublicKeyFormat::Compressed,
            ),
            (
                AddressType::Taproot,
                AddressEncoding::Bech32m,
                bitcoin::AddressType::P2tr,
                SignatureScheme::SchnorrSecp256k1,
                SignatureEncoding::Raw,
                PublicKeyFormat::XOnly,
            ),
        ] {
            let wallet = block_on(factory(address_type).generate())
                .expect("native Bitcoin generation must create a wallet");
            let signed = block_on(wallet.sign(SignRequest {
                payload: SignablePayload::Digest(Digest {
                    bytes: vec![0x42; 32],
                }),
                scheme,
                encoding: signature_encoding,
                public_key_format: public_format,
                key_tweak: None,
            }))
            .expect("generated Bitcoin wallet must sign with its native key");
            let address = wallet
                .address_text(&wallet.address())
                .expect("generated address must use its configured Bitcoin format");
            let parsed = address
                .text
                .parse::<NativeAddress<bitcoin::address::NetworkUnchecked>>()
                .expect("generated address must be valid Bitcoin text")
                .require_network(bitcoin::Network::Regtest)
                .expect("generated address must use regtest");
            let derived = match address_type {
                AddressType::SegwitV0 => {
                    let key = PublicKey::from_slice(&signed.public_key.bytes)
                        .expect("signer must return a compressed Bitcoin public key");
                    let key = CompressedPublicKey::try_from(key)
                        .expect("signer public key must be compressed");
                    NativeAddress::p2wpkh(&key, bitcoin::Network::Regtest)
                }
                AddressType::Taproot => {
                    let key = XOnlyPublicKey::from_slice(&signed.public_key.bytes)
                        .expect("signer must return an x-only Bitcoin public key");
                    NativeAddress::p2tr(
                        &Secp256k1::verification_only(),
                        key,
                        None,
                        bitcoin::Network::Regtest,
                    )
                }
            };

            assert_eq!(address.encoding, encoding);
            assert_eq!(parsed.address_type(), Some(native_type));
            assert_eq!(signed.public_key.curve, crypto::Curve::Secp256k1);
            assert_eq!(signed.public_key.format, public_format);
            assert_eq!(address.text, derived.to_string());
        }
    }

    #[test]
    fn generated_secret_reuses_create_address_derivation() {
        for address_type in [AddressType::SegwitV0, AddressType::Taproot] {
            let factory = factory(address_type);
            let generated = block_on(factory.generate_with(fixed_secret))
                .expect("fixed valid secret must generate a wallet");
            let created = block_on(factory.create(SecretBytes::new([1_u8; 32])))
                .expect("fixed valid secret must create a wallet");

            assert_eq!(generated.address(), created.address());
        }
    }

    #[test]
    fn generation_failure_is_typed_before_wallet_creation() {
        let mut factory = factory(AddressType::SegwitV0);
        factory.config.scope.chain = ChainId("not-bitcoin".to_owned());

        let result = block_on(factory.generate_with(unavailable_secret));
        let error = match result {
            Ok(_) => panic!("generation failure must not create a wallet"),
            Err(error) => error,
        };

        assert_eq!(error.kind, WalletErrorKind::Generation);
        assert_eq!(
            error.message,
            "operating system random source is unavailable"
        );
    }

    fn unavailable_secret() -> Result<SecretBytes, crypto::Error> {
        Err(crypto::Error {
            kind: crypto::ErrorKind::KeyGeneration,
            message: "operating system random source is unavailable".to_owned(),
        })
    }

    fn fixed_secret() -> Result<SecretBytes, crypto::Error> {
        Ok(SecretBytes::new([1_u8; 32]))
    }

    #[test]
    fn wallet_context_keeps_invalid_secret_and_address_errors_distinct() {
        let factory = factory(AddressType::SegwitV0);
        let error = match block_on(factory.create(SecretBytes::new(Vec::new()))) {
            Ok(_) => panic!("empty secret must fail"),
            Err(error) => error,
        };
        assert_eq!(error.kind, WalletErrorKind::InvalidSecret);
        assert_eq!(
            error.message,
            "secret key must be a valid 32-byte secp256k1 scalar"
        );
        assert_eq!(error.ambiguous_transaction_id, None);

        let wallet = block_on(factory.create(SecretBytes::new([1_u8; 32]))).unwrap();
        let expected = Address::parse_for_network("not-an-address", Network::Regtest).unwrap_err();
        let error = wallet
            .parse_address(&AddressText::new(AddressEncoding::Bech32, "not-an-address"))
            .unwrap_err();
        assert_eq!(error.kind, WalletErrorKind::InvalidAddress);
        assert_eq!(error.message, expected.to_string());
        assert_eq!(error.ambiguous_transaction_id, None);
    }
}
