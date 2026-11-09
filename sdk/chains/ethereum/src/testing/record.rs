//! The record step, by hand only (`RECORD=1`): asks a real Sepolia node
//! every method this crate calls, and the edge replies the mock must give,
//! and writes each reply under `recordings/`. Blocks keep two transactions
//! (one ether, one token transfer) so every file stays small.

use std::{path::Path, time::SystemTime};

use ::rpc::{Client as _, Endpoint, Http, Request};
use alloy_consensus::{SignableTransaction as _, TxEip1559, TxEnvelope};
use alloy_eips::Encodable2718 as _;
use alloy_primitives::{Bytes, Signature, TxKind, U256, hex, keccak256};
use anyhow::{Context as _, anyhow};
use base::testing::Recording;
use crypto::{PrivateKey as _, SecretBytes};
use serde_json::{Value, json};

use super::{PLACEHOLDER, TOKEN, TRANSFER};
use crate::{KeyPair, Network, erc20};

/// How often a reply that differs by client is asked again.
const TRIES: usize = 10;

/// A block old enough that the public node pruned its receipts.
const PRUNED: u64 = 5_000_000;

/// The node and the date every recording carries.
struct Recorder {
    node: Http,
    provider: String,
    date: String,
    directory: std::path::PathBuf,
}

impl Recorder {
    /// Asks `method` and returns the whole reply.
    async fn ask(&self, method: &str, params: Value) -> anyhow::Result<Recording> {
        let reply = self
            .node
            .request(Request::new(method, params.clone()))
            .await
            .with_context(|| method.to_owned())?;
        let (result, error) = match reply {
            Ok(raw) => (Some(raw.deserialize::<Value>()?), None),
            Err(failure) => {
                let mut error = json!({"code": failure.code, "message": failure.message});
                if let Some(data) = failure.data {
                    error["data"] = data.deserialize()?;
                }
                (None, Some(error))
            }
        };
        Ok(Recording {
            provider: self.provider.clone(),
            network: "sepolia".to_owned(),
            recorded_at: self.date.clone(),
            method: method.to_owned(),
            params,
            result,
            error,
        })
    }

    /// Asks and writes `name`.json.
    async fn keep(&self, name: &str, method: &str, params: Value) -> anyhow::Result<Recording> {
        let recording = self.ask(method, params).await?;
        self.write(name, &recording)?;
        Ok(recording)
    }

    fn write(&self, name: &str, recording: &Recording) -> anyhow::Result<()> {
        let text = serde_json::to_string_pretty(recording)? + "\n";
        std::fs::write(self.directory.join(format!("{name}.json")), text)?;
        Ok(())
    }

    async fn result(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let recording = self.ask(method, params).await?;
        recording
            .result
            .ok_or_else(|| anyhow!("{method}: {:?}", recording.error))
    }
}

/// Records every reply from the node at `url` into `directory`.
///
/// # Errors
///
/// A node failure, or no recent block with an ether and a token transfer.
pub async fn record(url: &str, directory: &Path) -> anyhow::Result<()> {
    let endpoint = Endpoint::new(url);
    let provider = endpoint.parts().map(|parts| parts.host).unwrap_or_default();
    let recorder = Recorder {
        node: endpoint.connect()?,
        provider,
        date: today(),
        directory: directory.to_owned(),
    };
    recorder
        .keep("eth_chainId", "eth_chainId", json!([]))
        .await?;
    let tip = recorder
        .keep("eth_blockNumber", "eth_blockNumber", json!([]))
        .await?;
    let tip = u64::from_str_radix(
        tip.result()
            .as_str()
            .unwrap_or("0x0")
            .trim_start_matches("0x"),
        16,
    )?;
    let (height, native, token, holder) = find(&recorder, tip).await?;
    let hashes = [native["hash"].clone(), token["hash"].clone()];
    let at = format!("{height:#x}");

    let mut full = recorder
        .ask("eth_getBlockByNumber", json!([at, true]))
        .await?;
    trim(&mut full, |transactions| {
        *transactions = vec![native.clone(), token.clone()]
    });
    recorder.write("eth_getBlockByNumber.full", &full)?;
    let block_hash = full.result()["hash"].clone();
    let filter =
        json!([{"fromBlock": at, "toBlock": at, "address": [TOKEN], "topics": [TRANSFER]}]);
    let mut logs = recorder.ask("eth_getLogs", filter).await?;
    if let Some(Value::Array(all)) = &mut logs.result {
        all.retain(|log| log["transactionHash"] == token["hash"]);
    }
    recorder.write("eth_getLogs", &logs)?;
    let mut header = recorder
        .ask("eth_getBlockByNumber", json!([at, false]))
        .await?;
    trim(&mut header, |transactions| *transactions = hashes.to_vec());
    recorder.write("eth_getBlockByNumber.hashes", &header)?;
    for tag in ["finalized", "latest"] {
        let mut tagged = recorder
            .ask("eth_getBlockByNumber", json!([tag, false]))
            .await?;
        trim(&mut tagged, |transactions| transactions.truncate(2));
        recorder.write(&format!("eth_getBlockByNumber.{tag}"), &tagged)?;
    }
    let future = format!("{:#x}", tip + 100_000);
    recorder
        .keep(
            "eth_getBlockByNumber.future",
            "eth_getBlockByNumber",
            json!([future, false]),
        )
        .await?;

    let mut receipts = recorder
        .ask("eth_getBlockReceipts", json!([block_hash]))
        .await?;
    if let Some(Value::Array(all)) = &mut receipts.result {
        let wanted = |receipt: &Value| hashes.contains(&receipt["transactionHash"]);
        all.retain(wanted);
    }
    recorder.write("eth_getBlockReceipts", &receipts)?;
    let old = recorder
        .result(
            "eth_getBlockByNumber",
            json!([format!("{PRUNED:#x}"), false]),
        )
        .await?;
    recorder
        .keep(
            "eth_getBlockReceipts.pruned",
            "eth_getBlockReceipts",
            json!([old["hash"]]),
        )
        .await?;
    recorder
        .keep(
            "eth_getTransactionReceipt",
            "eth_getTransactionReceipt",
            json!([token["hash"]]),
        )
        .await?;
    // The public node balances several clients; keep the one whose
    // pruned receipts are `null` throughout (the mock's).
    let pruned = json!([old["transactions"][0]]);
    for _ in 0..TRIES {
        let receipt = recorder
            .ask("eth_getTransactionReceipt", pruned.clone())
            .await?;
        if receipt.result == Some(Value::Null) {
            recorder.write("eth_getTransactionReceipt.pruned", &receipt)?;
            break;
        }
    }
    recorder
        .keep(
            "eth_getTransactionByHash",
            "eth_getTransactionByHash",
            json!([token["hash"]]),
        )
        .await?;
    let unknown = format!("0x{}", "ab".repeat(32));
    recorder
        .keep(
            "eth_getTransactionByHash.unknown",
            "eth_getTransactionByHash",
            json!([unknown]),
        )
        .await?;
    recorder
        .keep(
            "eth_getTransactionCount",
            "eth_getTransactionCount",
            json!([holder, "pending"]),
        )
        .await?;

    let key = throwaway()?;
    let stranger = key.native_address().to_string();
    let transfer = hex::encode_prefixed(erc20::transfer(PLACEHOLDER, U256::from(1)));
    let estimate =
        |from: &str| json!([{"from": from, "to": TOKEN, "value": "0x0", "data": transfer}]);
    recorder
        .keep("eth_estimateGas", "eth_estimateGas", estimate(&holder))
        .await?;
    recorder
        .keep(
            "eth_estimateGas.reverted",
            "eth_estimateGas",
            estimate(&stranger),
        )
        .await?;
    let tip_fee = recorder
        .keep(
            "eth_maxPriorityFeePerGas",
            "eth_maxPriorityFeePerGas",
            json!([]),
        )
        .await?;
    let decimals = json!({"to": TOKEN, "data": hex::encode_prefixed(erc20::decimals())});
    recorder
        .keep("eth_call.decimals", "eth_call", json!([decimals, "latest"]))
        .await?;
    recorder
        .keep("method_not_found", "eth_notAMethod", json!([]))
        .await?;

    let latest = recorder
        .result("eth_getBlockByNumber", json!(["latest", false]))
        .await?;
    let base = quantity(&latest["baseFeePerGas"])?;
    let priority = quantity(tip_fee.result())?;
    let refusals = [
        ("funds", 11_155_111, 2 * base + priority, priority),
        ("underpriced", 11_155_111, 1, 1),
        ("chain", 1, 2 * base + priority, priority),
    ];
    // Both funds refusals from one client: the mock words them alike.
    for _ in 0..TRIES {
        let mut asked = Vec::new();
        for (name, chain_id, max_fee, tip) in refusals {
            let bytes = sign(&key, chain_id, max_fee, tip)?;
            let params = json!([hex::encode_prefixed(bytes)]);
            asked.push((name, recorder.ask("eth_sendRawTransaction", params).await?));
        }
        let code = |at: usize| {
            asked[at]
                .1
                .error
                .as_ref()
                .map(|error| error["code"].clone())
        };
        if code(0) != code(1) {
            continue;
        }
        for (name, recording) in &asked {
            recorder.write(&format!("eth_sendRawTransaction.{name}"), recording)?;
        }
        break;
    }
    recorder
        .keep(
            "eth_sendRawTransaction.invalid",
            "eth_sendRawTransaction",
            json!(["0x02"]),
        )
        .await?;
    Ok(())
}

/// A recent block (below `tip`) with an EIP-1559 ether transfer and an
/// EIP-1559 token transfer: its height, both transaction objects, and the
/// token's recipient (a holder).
async fn find(recorder: &Recorder, tip: u64) -> anyhow::Result<(u64, Value, Value, String)> {
    let filter = json!([{
        "fromBlock": format!("{:#x}", tip - 400), "toBlock": format!("{:#x}", tip - 20),
        "address": TOKEN, "topics": [TRANSFER],
    }]);
    let logs = recorder.result("eth_getLogs", filter).await?;
    let logs = logs.as_array().cloned().unwrap_or_default();
    for log in logs.iter().rev().take(20) {
        let height = u64::try_from(quantity(&log["blockNumber"])?)?;
        let block = recorder
            .result(
                "eth_getBlockByNumber",
                json!([format!("{height:#x}"), true]),
            )
            .await?;
        let transactions = block["transactions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let eip1559 = |transaction: &&Value| transaction["type"] == "0x2";
        let token = transactions
            .iter()
            .filter(eip1559)
            .find(|transaction| transaction["hash"] == log["transactionHash"]);
        let native = transactions.iter().filter(eip1559).find(|transaction| {
            transaction["input"] == "0x"
                && transaction["value"] != "0x0"
                && !transaction["to"].is_null()
        });
        let (Some(native), Some(token)) = (native, token) else {
            continue;
        };
        let holder = format!("0x{}", &log["topics"][2].as_str().unwrap_or_default()[26..]);
        return Ok((height, native.clone(), token.clone(), holder));
    }
    Err(anyhow!("no recent block has an ether and a token transfer"))
}

/// Keeps what `keep` leaves of a block recording's transactions, and two
/// withdrawals.
fn trim(recording: &mut Recording, keep: impl FnOnce(&mut Vec<Value>)) {
    let Some(block) = recording.result.as_mut().and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(Value::Array(transactions)) = block.get_mut("transactions") {
        keep(transactions);
    }
    if let Some(Value::Array(withdrawals)) = block.get_mut("withdrawals") {
        withdrawals.truncate(2);
    }
}

fn quantity(value: &Value) -> anyhow::Result<u128> {
    let text = value.as_str().context("a quantity")?;
    Ok(u128::from_str_radix(text.trim_start_matches("0x"), 16)?)
}

/// The key every refusal is signed with: fixed, and holding nothing.
fn throwaway() -> anyhow::Result<KeyPair> {
    let secret = keccak256(b"payment-sdk recordings: a key that holds nothing");
    KeyPair::import(
        &SecretBytes::new(secret.as_slice()),
        &Network::new(11_155_111, "sepolia"),
    )
}

/// One wei to the placeholder, nonce 0, signed for `chain_id`.
fn sign(key: &KeyPair, chain_id: u64, max_fee: u128, tip: u128) -> anyhow::Result<Vec<u8>> {
    let native = TxEip1559 {
        chain_id,
        nonce: 0,
        gas_limit: 21_000,
        max_fee_per_gas: max_fee,
        max_priority_fee_per_gas: tip,
        to: TxKind::Call(PLACEHOLDER),
        value: U256::from(1),
        input: Bytes::new(),
        ..TxEip1559::default()
    };
    let signature = key.key.sign(&native.signature_hash().0)?;
    let signature = Signature::try_from(signature.as_slice()).map_err(|_| anyhow!("signature"))?;
    Ok(TxEnvelope::from(native.into_signed(signature)).encoded_2718())
}

/// Today, UTC, as `YYYY-MM-DD`.
fn today() -> String {
    let days = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() / 86_400);
    // Howard Hinnant's civil-from-days.
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let day = z - era * 146_097;
    let year = (day - day / 1_460 + day / 36_524 - day / 146_096) / 365;
    let of_year = day - (365 * year + year / 4 - year / 100);
    let month = (5 * of_year + 2) / 153;
    let date = of_year - (153 * month + 2) / 5 + 1;
    let month = if month < 10 { month + 3 } else { month - 9 };
    let year = year + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{date:02}")
}
