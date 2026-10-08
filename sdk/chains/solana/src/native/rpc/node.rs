//! Node identity and slot reads; `Client::connect`'s verification.

use serde_json::json;

use super::{Client, Commitment};
use crate::native::{Address, Error, ErrorKind, GenesisHash};

impl<C: json_rpc::Client> Client<C> {
    /// Fails unless the node reports itself healthy (`getHealth` is `ok`).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RpcUnavailable`].
    pub async fn health(&self) -> Result<(), Error> {
        let health = self.request::<String>("getHealth", json!([])).await?;
        if health != "ok" {
            return Err(Error::rpc(
                "Solana RPC health result is not exactly ok",
                true,
            ));
        }
        Ok(())
    }

    /// The genesis hash the node reports.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RpcUnavailable`].
    pub async fn genesis_hash(&self) -> Result<GenesisHash, Error> {
        let text = self.request::<String>("getGenesisHash", json!([])).await?;
        text.parse().map_err(|_| Error::malformed("getGenesisHash"))
    }

    /// The node's slot at `commitment`, no older than `minimum`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RpcUnavailable`], also when the slot is below `minimum`.
    pub async fn slot(&self, commitment: Commitment, minimum: Option<u64>) -> Result<u64, Error> {
        let slot = self
            .request::<u64>("getSlot", json!([commitment.config(minimum)]))
            .await?;
        if minimum.is_some_and(|min_slot| slot < min_slot) {
            return Err(Error::below_min_slot());
        }
        Ok(slot)
    }

    /// Verifies the configured cluster identity with one call.
    pub(super) async fn verify_genesis(&self) -> Result<(), Error> {
        if &self.genesis_hash().await? != self.network().genesis_hash() {
            return Err(Error::new(
                ErrorKind::Divergent,
                "Solana RPC genesis hash does not match configuration",
            ));
        }
        Ok(())
    }

    /// Requires the exact Memo-v3 program to be executable at one finalized
    /// minimum context slot before storage is opened.
    pub(super) async fn verify_memo(&self) -> Result<(), Error> {
        let missing = || {
            Error::new(
                ErrorKind::InvalidConfiguration,
                "Solana Memo-v3 program is absent or not executable",
            )
        };
        let min_slot = self.slot(Commitment::Finalized, None).await?;
        let address = Address::from_bytes(spl_memo_interface::v3::ID.to_bytes());
        let account = self
            .account(&address, Commitment::Finalized, Some(min_slot))
            .await?
            .value
            .ok_or_else(missing)?;
        if !account.executable() {
            return Err(missing());
        }
        Ok(())
    }
}
