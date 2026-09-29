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

#[cfg(test)]
mod tests {
    use futures_executor::block_on;
    use serde_json::{Value, json};
    use solana_hash::Hash;

    use super::*;
    use crate::native::{
        Network,
        rpc::script::{GENESIS, Script, Scripted},
    };

    const MEMO: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

    fn memo(executable: bool, slot: u64, data: Value) -> Value {
        json!({
            "context": {"slot": slot},
            "value": {
                "lamports": 1,
                "owner": GENESIS,
                "executable": executable,
                "data": data,
                "space": 0
            }
        })
    }

    fn memo_probe(response: Value) -> [(&'static str, Value, Value); 2] {
        [
            ("getSlot", json!([{"commitment":"finalized"}]), json!(41)),
            (
                "getAccountInfo",
                json!([MEMO, {"encoding":"base64", "commitment":"finalized", "minContextSlot":41}]),
                response,
            ),
        ]
    }

    #[tokio::test]
    async fn reads_identity_and_contextual_slots_with_exact_shapes() {
        let hash = Hash::new_from_array([9; 32]).to_string();
        let rpc = Script::exchanges([
            ("getHealth", json!([]), json!("ok")),
            ("getGenesisHash", json!([]), json!(hash.clone())),
            (
                "getSlot",
                json!([{"commitment":"confirmed"}]),
                json!(u64::MAX),
            ),
            (
                "getSlot",
                json!([{"commitment":"finalized", "minContextSlot":8}]),
                json!(8),
            ),
        ]);
        let client = rpc.client();
        client.health().await.expect("health");
        assert_eq!(client.genesis_hash().await.unwrap().to_string(), hash);
        assert_eq!(
            client.slot(Commitment::Confirmed, None).await.unwrap(),
            u64::MAX
        );
        assert_eq!(
            client.slot(Commitment::Finalized, Some(8)).await.unwrap(),
            8
        );
        rpc.exhausted();
    }

    #[tokio::test]
    async fn rejects_non_ok_health_malformed_hash_number_forms_and_low_floor() {
        for value in [json!("behind"), json!(true), json!(null)] {
            assert!(
                Script::exchanges([("getHealth", json!([]), value)])
                    .client()
                    .health()
                    .await
                    .is_err()
            );
        }
        let client = Script::exchanges([("getGenesisHash", json!([]), json!("bad"))]).client();
        assert_eq!(
            client.genesis_hash().await.unwrap_err().message,
            "Solana RPC getGenesisHash returned malformed data"
        );
        for value in [json!(-1), json!(1.5), json!("9")] {
            let client =
                Script::exchanges([("getSlot", json!([{"commitment":"confirmed"}]), value)])
                    .client();
            assert_eq!(
                client
                    .slot(Commitment::Confirmed, None)
                    .await
                    .unwrap_err()
                    .message,
                "Solana RPC getSlot returned malformed data"
            );
        }
        let client = Script::exchanges([(
            "getSlot",
            json!([{"commitment":"confirmed", "minContextSlot":9}]),
            json!(8),
        )])
        .client();
        assert_eq!(
            client
                .slot(Commitment::Confirmed, Some(9))
                .await
                .unwrap_err(),
            Error::below_min_slot()
        );
    }

    #[test]
    fn connect_verifies_genesis_then_the_exact_finalized_executable_memo() {
        let [slot, account] = memo_probe(memo(true, 41, json!(["", "base64"])));
        let script =
            Script::exchanges([("getGenesisHash", json!([]), json!(GENESIS)), slot, account]);
        let client = block_on(Client::connect(script.clone(), &Script::network())).unwrap();
        assert_eq!(client.network(), &Script::network());
        script.exhausted();
    }

    #[test]
    fn connect_rejects_a_blank_slug_before_rpc_and_another_cluster_before_the_memo_probe() {
        let script = Script::exchanges([]);
        let blank = Network::new(" ", GENESIS.parse().unwrap());
        let error = block_on(Client::connect(script.clone(), &blank))
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::InvalidConfiguration);

        let other = Hash::new_from_array([7; 32]).to_string();
        let script = Script::exchanges([("getGenesisHash", json!([]), json!(other))]);
        let error = block_on(Client::connect(script.clone(), &Script::network()))
            .err()
            .unwrap();
        assert_eq!(
            (error.kind, error.retryable, error.message.as_str()),
            (
                ErrorKind::Divergent,
                false,
                "Solana RPC genesis hash does not match configuration"
            )
        );
        script.exhausted();
    }

    #[tokio::test]
    async fn rejects_absent_non_executable_malformed_and_below_floor_memo() {
        for (value, kind, message) in [
            (
                json!({"context":{"slot":41}, "value":null}),
                ErrorKind::InvalidConfiguration,
                "Solana Memo-v3 program is absent or not executable",
            ),
            (
                memo(false, 41, json!(["", "base64"])),
                ErrorKind::InvalidConfiguration,
                "Solana Memo-v3 program is absent or not executable",
            ),
            (
                memo(true, 41, json!(["%%%", "base64"])),
                ErrorKind::RpcUnavailable,
                "Solana RPC account returned malformed data",
            ),
            (
                memo(true, 40, json!(["", "base64"])),
                ErrorKind::RpcUnavailable,
                "Solana RPC response is below its requested context floor",
            ),
        ] {
            let script = Script::exchanges(memo_probe(value));
            let error = script
                .client()
                .verify_memo()
                .await
                .expect_err("invalid Memo");
            assert_eq!((error.kind, error.message.as_str()), (kind, message));
            script.exhausted();
        }
    }
}
