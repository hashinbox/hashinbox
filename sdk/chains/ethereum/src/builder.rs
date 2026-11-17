//! [`Builder`]: one EIP-1559 transaction for one Ethereum transfer.

use std::sync::Arc;

use base::{Addressed as _, Asset, Future, Output, Signer, TransactionError, Transfer};

use crate::{
    Address, Client, KeyPair,
    fee_cache::Cached,
    transaction::{Transaction, request, sender_and_nonce},
};

/// Pays one transfer of one asset (ether, or the `contract` token) in one
/// transaction, from its own source or the default one, at the sender's
/// next nonce ([`Builder::nonce`]).
pub(crate) struct Builder {
    pub(crate) rpc: Client,
    /// The sends in flight: the nonces they hold.
    pub(crate) transactions: Arc<dyn base::TransactionIndexStore>,
    /// The factory's estimator: its cached fees and gas price the transfer.
    pub(crate) fees: Arc<Cached>,
    pub(crate) asset: Asset,
    pub(crate) contract: Option<alloy_primitives::Address>,
    pub(crate) default: Option<Signer>,
}

impl base::Builder for Builder {
    fn with_default_source(&mut self, source: Signer) {
        self.default = Some(source);
    }

    fn sign<'b>(
        self: Box<Self>,
        transfer: Transfer,
    ) -> Future<'b, Result<base::Transaction, TransactionError>>
    where
        Self: 'b,
    {
        Box::pin(async move {
            let Transfer { to, amount, from } = transfer;
            let network = self.rpc.network();
            let address = Address::parse(&to.value, network).map_err(TransactionError::failed)?;
            if *address.address() != to {
                return Err(TransactionError::failed(
                    "not a canonical address of this Ethereum network",
                ));
            }
            if amount
                .to_atomic_be_bytes::<32>(0)
                .ok()
                .is_none_or(|word| word == [0; 32])
            {
                return Err(TransactionError::failed(
                    "Ethereum amount must be a positive 256-bit number of atomic units",
                ));
            }
            let Some(signer) = from.as_ref().or(self.default.as_ref()) else {
                return Err(TransactionError::failed("the transfer has no source"));
            };
            let key = KeyPair::from_signer(signer, network)?;
            let output = Output {
                address,
                asset: self.asset.clone(),
                amount,
            };
            let nonce = self.nonce(&key).await?;
            let fees = self.fees.fees().await?;
            let sender = key.native_address();
            let spend = base::Spend::from((
                sender.address().clone(),
                self.asset.clone(),
                output.address.address().clone(),
            ));
            // Whether the recipient holds the asset already: which kind of
            // send it is.
            let held = self
                .fees
                .holds(&[spend.clone()].into_iter().collect())
                .await?;
            let mut native = request(&output, self.contract).map_err(TransactionError::failed)?;
            native.chain_id = network.chain_id;
            native.nonce = nonce;
            native.gas_limit = self.fees.gas(&spend, held.first() == Some(&true)).await?;
            native.max_fee_per_gas = fees.max_fee_per_gas;
            native.max_priority_fee_per_gas = fees.max_priority_fee_per_gas;
            self.fees
                .inner
                .limits
                .total(native.gas_limit, fees)
                .map_err(TransactionError::failed)?;
            let mut transaction = Transaction::pending(native, sender, &output);
            transaction.sign(&key).map_err(TransactionError::failed)?;
            Ok(transaction.into())
        })
    }
}

impl Builder {
    /// `key`'s next nonce: the node's pending count, or one above the
    /// highest nonce of its own sends in flight when that is higher,
    /// so a node whose mempool misses one never hands its nonce out again.
    async fn nonce(&self, key: &KeyPair) -> Result<u64, TransactionError> {
        let counted = self
            .rpc
            .nonce(key.native_address())
            .await
            .map_err(TransactionError::unavailable)?;
        let sent = self
            .transactions
            .find_sent(key.address())
            .await
            .map_err(TransactionError::unavailable)?;
        sent.iter().try_fold(counted, |next, transaction| {
            let (_, nonce) =
                sender_and_nonce(&transaction.bytes).map_err(TransactionError::failed)?;
            Ok(next.max(nonce.saturating_add(1)))
        })
    }
}
