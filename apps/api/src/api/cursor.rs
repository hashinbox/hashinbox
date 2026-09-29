use serde::{Deserialize, Serialize};

use super::error::Error;

/// The opaque `cursor` of a history page: a position in one address's
/// confirmed history, and nothing else.
///
/// It used to carry the checkpoint it was issued at, so the index could refuse
/// a cursor whose checkpoint had moved. That refusal is gone with the journal:
/// the confirmed store is append-only, so a checkpoint that moved can only
/// have appended *above* this position, and an ascending keyset page is stable
/// however long a caller holds it (`docs/design/030 §8 (ex-025)` section 6.3).
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HistoryCursor {
    chain: String,
    network: String,
    transaction: String,
    height: u64,
    /// The transaction's native place in its block, which is the middle
    /// component of the page order. Without it a cursor could not name a
    /// position at all once two transactions of one block are stored.
    order: u64,
}

impl HistoryCursor {
    pub fn encode(position: &indexing::HistoryPosition) -> Result<String, Error> {
        use base64::Engine;

        let bytes = serde_json::to_vec(&Self {
            chain: position.transaction.network.chain.to_string(),
            network: position.transaction.network.name.clone(),
            transaction: position.transaction.id.to_string(),
            height: position.height.0,
            order: position.transaction_order,
        })
        .map_err(|_| Self::invalid_response("history cursor could not be encoded"))?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn decode(value: &str) -> Result<indexing::HistoryPosition, Error> {
        use base64::Engine;

        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| Self::invalid_request("history cursor is invalid"))?;
        let cursor: Self = serde_json::from_slice(&bytes)
            .map_err(|_| Self::invalid_request("history cursor is invalid"))?;
        if cursor.chain.is_empty() || cursor.network.is_empty() || cursor.transaction.is_empty() {
            return Err(Self::invalid_request("history cursor is invalid"));
        }
        let network = base::Network {
            chain: base::ChainName::new(cursor.chain),
            name: cursor.network,
        };
        Ok(indexing::HistoryPosition {
            height: base::BlockHeight(cursor.height),
            transaction_order: cursor.order,
            transaction: base::TransactionRef {
                network,
                id: cursor.transaction.into(),
            },
        })
    }

    fn invalid_request(message: impl Into<String>) -> Error {
        Error::invalid_request(message)
    }

    fn invalid_response(message: impl Into<String>) -> Error {
        Error::invalid_response(message)
    }
}
