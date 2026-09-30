mod address;
mod asset;
mod contract;
mod cursor;
mod error;
mod health;
mod history;
mod openapi;
mod signers;
mod transaction;
mod wallet;

#[cfg(test)]
mod openapi_contract;
#[cfg(test)]
mod route_contract;

pub(crate) use asset::AssetTag;
pub(crate) use contract::Start;
pub(crate) use signers::Signers;

use error::Error;

use std::sync::Arc;

use axum::{Extension, Router};
use indexing::Watching as _;
use tokio::sync::watch;
use utoipa::{
    Modify, OpenApi,
    openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
};
use utoipa_axum::router::OpenApiRouter;

/// The wallet factories the routes serve, keyed by configured asset tags. The
/// composition root owns this type; the HTTP layer only borrows it.
pub(crate) type Wallets = wallets::Wallets<AssetTag>;

/// The document root: title, version, description, servers, and the one
/// `bearerAuth` security scheme the protected operations declare. Title and
/// description are stated here and the version is this crate's own, so a
/// published contract never carries a dependency's metadata. The title is the
/// contract's published name, not the name of the crate that serves it.
// linter:disable rust/empty-struct -- Utoipa attaches the document root to a type, not a value.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "payment-api",
        description = "The payment HTTP routes and OpenAPI contract over the chain-neutral \
            wallet and index abstractions. Every asset is an open tag; no chain or asset is \
            named in this contract."
    ),
    servers((url = "/", description = "The payment server's own origin")),
    modifiers(&BearerAuth),
    tags(
        (name = "assets", description = "Configured assets"),
        (name = "wallets", description = "Wallets and balances"),
        (name = "addresses", description = "Watch-only addresses"),
        (name = "transactions", description = "History, single transfers, and batches"),
        (name = "health", description = "Readiness, network state, and RPC statistics"),
        (name = "openapi", description = "This document")
    )
)]
struct Contract;

/// Declares the `bearerAuth` scheme the protected operations reference.
// linter:disable rust/empty-struct -- Utoipa's Modify contract is a trait on a type.
struct BearerAuth;

impl BearerAuth {
    /// The one credential the protected routes accept.
    fn scheme() -> SecurityScheme {
        SecurityScheme::Http(
            HttpBuilder::new()
                .scheme(HttpAuthScheme::Bearer)
                .description(Some(
                    "The configured bearer token; health and this document are public.",
                ))
                .build(),
        )
    }
}

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        // Cargo always defines CARGO_PKG_LICENSE, so Utoipa's env defaults
        // publish an empty license object for a crate that declares none.
        openapi.info.license = None;
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme("bearerAuth", Self::scheme());
    }
}

/// Shared handler state: the wallet factories, the wallets this process holds
/// keys for, the index that observes them, the two query readers beside it, and
/// every network's indexing health. Every value is an abstraction; no route
/// names a chain, none of them names the supervisor that produces the health,
/// and none of them names a repository.
///
/// Cheap to clone; every route receives a clone.
#[derive(Clone)]
pub(crate) struct Payments {
    wallets: Arc<Wallets>,
    units: Arc<base::Units>,
    signers: Arc<Signers>,
    registry: Arc<crate::registry::Registry>,
    index: Arc<indexing::Index>,
    /// The one history read these routes make, for whichever network the wallet
    /// belongs to.
    ///
    /// A narrow capability declared at this boundary rather than a capability of
    /// `index`: a page is the repository's own query joined to one network's
    /// blocks above its confirmed line, and neither half answers it alone. What
    /// satisfies it is `wallets::PendingJoin`, one per network, collected by
    /// `wallets::Composition`.
    history: Arc<dyn wallets::History>,
    /// The one balance read these routes make, held for the same reason and in
    /// the same way as `history`: a total is the repository's own query
    /// completed by the blocks one index still holds above its line, so it is
    /// asked of the fold that owns the holding's network. One per network,
    /// collected by `wallets::Composition` — never reached for through `index`,
    /// which can route one and is not what owns the answer.
    balance: Arc<dyn wallets::Balances>,
    health: watch::Receiver<indexing::SyncHealth>,
    rpc: Vec<(base::Network, Vec<json_rpc::Statistics>)>,
}

/// Everything [`Payments`] is built from, one field per field of it, so a
/// composition root names each part where it hands it over — rather than eight
/// positional arguments, which is a defect waiting for the day two of them
/// share a type. Every field is named and none has a default; what each one is,
/// and why it is held rather than reached for, is on [`Payments`] itself.
pub(crate) struct PaymentsParams {
    /// Every configured asset's wallet factory, under its tag
    /// (`wallets::Composition::wallets`).
    pub(crate) wallets: Arc<Wallets>,
    /// What an amount of each configured asset is, by identity
    /// (`wallets::Composition::units`).
    pub(crate) units: Arc<base::Units>,
    /// The wallets this process holds keys for; one missing from it is
    /// watch-only.
    pub(crate) signers: Arc<Signers>,
    /// Encrypted application-owned keys, committed before watch publication.
    pub(crate) registry: Arc<crate::registry::Registry>,
    /// What observes every configured network (`wallets::Composition::index`):
    /// watches and checkpoints — never a stored query.
    pub(crate) index: Arc<indexing::Index>,
    /// One history page, routed per network (`wallets::Composition::history`).
    pub(crate) history: Arc<dyn wallets::History>,
    /// One balance, routed per network (`wallets::Composition::balance`).
    pub(crate) balance: Arc<dyn wallets::Balances>,
    /// The channel the composition root publishes each network's state on; `GET
    /// /healthz` answers 200 only while it is ready and its sender alive.
    pub(crate) health: watch::Receiver<indexing::SyncHealth>,
    /// Endpoint counters supplied by configured RPC clients; no URLs or credentials.
    pub(crate) rpc: Vec<(base::Network, Vec<json_rpc::Statistics>)>,
}

impl Payments {
    /// The state [`PaymentsParams`] describes.
    #[must_use]
    pub(crate) fn from_params(params: PaymentsParams) -> Self {
        let PaymentsParams {
            wallets,
            units,
            signers,
            registry,
            index,
            history,
            balance,
            health,
            rpc,
        } = params;
        Self {
            wallets,
            units,
            signers,
            registry,
            index,
            history,
            balance,
            health,
            rpc,
        }
    }

    /// What this process knows about one wallet id.
    ///
    /// Two owners answer it: the index says what is observed and from where,
    /// and [`Signers`] says whether this process can sign for it. Neither
    /// knows the other's half, which is the split `docs/design/030 §8 (ex-029)` exists to
    /// make; putting the two answers together is what `docs/design/030 §8 (ex-026)`
    /// section 1.9 called `subject(id)`.
    ///
    /// # Errors
    ///
    /// [`error::Error`]'s 404 when no network watches `id`, and the index's
    /// own error.
    pub(crate) async fn custody(&self, id: &str) -> Result<Custody, Error> {
        for network in self.index.networks() {
            let found = self
                .index
                .watched(network)
                .await?
                .into_iter()
                .find(|watch| watch.id == id);
            if let Some(watch) = found {
                return self.resolved(watch);
            }
        }
        Err(Error::unknown_wallet())
    }

    /// Every watched id this process knows, in id order across networks.
    pub(crate) async fn custodies(&self) -> Result<Vec<Custody>, Error> {
        let mut held = Vec::new();
        for network in self.index.networks() {
            for watch in self.index.watched(network).await? {
                held.push(self.resolved(watch)?);
            }
        }
        held.sort_by(|left, right| left.watch.id.cmp(&right.watch.id));
        Ok(held)
    }

    /// The configured tag of a watched row's asset. One tag is one asset in
    /// one network, so the pair recovers it.
    fn resolved(&self, watch: indexing::Watch) -> Result<Custody, Error> {
        let tag = self
            .wallets
            .assets()
            .into_iter()
            .find(|info| info.network == watch.address.network && info.id == watch.asset)
            .map(|info| info.tag)
            .ok_or_else(Error::unknown_wallet)?;
        let wallet = self.signers.get(&watch.id);
        Ok(Custody { watch, tag, wallet })
    }

    /// Persists a generated/imported key, publishes its watch, then installs
    /// the signing handle. A rejected or cancelled watch leaves a private key
    /// row but no wallet route; a committed watch is recoverable at boot.
    pub(crate) async fn registered(
        &self,
        wallet: Arc<dyn wallets::Wallet>,
        tag: AssetTag,
        material: wallets::SecretBytes,
    ) -> Result<(axum::http::StatusCode, axum::Json<contract::Wallet>), Error> {
        let id = uuid::Uuid::now_v7().to_string();
        self.registry.keep(&id, &tag, material).await?;
        let watch = self
            .index
            .watch(&id, wallet.as_ref(), indexing::Birthday::Fresh)
            .await?;
        self.signers.keep(id, wallet.clone())?;
        let custody = Custody {
            watch,
            tag,
            wallet: Some(wallet),
        };
        Ok((axum::http::StatusCode::CREATED, axum::Json(custody.into())))
    }

    /// Observes `text`'s address in `tag`'s network, with no key behind it.
    ///
    /// The asset's own factory parses the text, so the address is canonical
    /// and of that asset's network before the index is asked to watch it.
    ///
    /// # Errors
    ///
    /// A 404 for an unconfigured tag, a 400 for address text the asset's
    /// chain refuses, and the index's error.
    pub(crate) async fn observed(
        &self,
        tag: AssetTag,
        text: &str,
    ) -> Result<(axum::http::StatusCode, axum::Json<contract::Wallet>), Error> {
        let info = self.wallets.asset(&tag).map_err(Error::by_asset)?;
        let address = self.wallets.parse(&tag, text).map_err(Error::by_asset)?;
        let subject = indexing::Subject {
            address,
            asset: info.id,
        };
        let id = uuid::Uuid::now_v7().to_string();
        let watch = self
            .index
            .watch(&id, &subject, indexing::Birthday::Fresh)
            .await?;
        let custody = Custody {
            watch,
            tag,
            wallet: None,
        };
        Ok((axum::http::StatusCode::CREATED, axum::Json(custody.into())))
    }

    /// The canonical destination of `text` in the custody's own asset network.
    ///
    /// # Errors
    ///
    /// A 400 for text the asset's chain refuses.
    pub(crate) fn destination(
        &self,
        custody: &Custody,
        text: &str,
    ) -> Result<base::Address, Error> {
        self.wallets
            .parse(&custody.tag, text)
            .map_err(Error::by_asset)
    }

    /// One batch item: its asset tag and the transfer its wallet will send.
    ///
    /// # Errors
    ///
    /// A 404 for an unknown id, a 422 for a watch-only one, a 400 for a
    /// malformed amount or destination.
    pub(crate) async fn transfer(
        &self,
        item: &transaction::BatchedTransfer,
    ) -> Result<(AssetTag, wallets::Transfer), Error> {
        let custody = self.custody(&item.wallet_id).await?;
        let wallet = custody.signing()?.clone();
        let amount = transaction::Transfer::amount(&item.amount)?;
        let to = self.destination(&custody, &item.destination)?;
        Ok((custody.tag, wallets::Transfer { wallet, to, amount }))
    }

    /// One `page` of a custody's history, in display units.
    ///
    /// The reader answers the page for the custody itself — it is a
    /// [`indexing::Holder`] — in exact atomic units; the unit each asset's own
    /// chain declared turns them into display units, looked up by the identity
    /// every movement and every fee already carries. That is the whole of the
    /// projection and the only place it happens.
    ///
    /// The subject names its network, so the reader that owns that network is
    /// the one that answers; nothing here holds a repository or knows one
    /// exists.
    ///
    /// # Errors
    ///
    /// The index's error, and a 503 when the page breaks the query contract.
    pub(crate) async fn history(
        &self,
        custody: &Custody,
        page: indexing::Page,
    ) -> Result<history::TransactionPage, Error> {
        let network = custody.watch.address.network.clone();
        let page = self.history.history(custody, page).await?;
        history::TransactionPage::new(page, &network, &custody.watch.asset, &self.units)
    }

    /// One custody's balance, folded over the movements its network's owner
    /// stores, on both sides of that chain's confirmed line.
    ///
    /// One owner answers both halves, so they cannot disagree about what is
    /// confirmed, and no key is involved: a watched address reads its balance
    /// exactly as a wallet does. The amounts arrive in atomic units and are
    /// scaled here, by the unit the asset's own chain declared.
    ///
    /// The custody names its network, so the fold that owns it answers — the
    /// same route the page takes. Nothing here holds a repository, knows one
    /// exists, or reaches an answer through an indexer.
    ///
    /// # Errors
    ///
    /// The fold's own error, and a 503 when a stored amount cannot be scaled.
    pub(crate) async fn balance(&self, custody: &Custody) -> Result<wallet::Balance, Error> {
        let unit = self
            .units
            .of(&custody.watch.address.network, &custody.watch.asset)
            .ok_or_else(Error::undeclared)?;
        let balance = self.balance.balance(custody).await?;
        wallet::Balance::new(balance, unit)
    }
}

/// One wallet id as this process knows it: what the index observes, which
/// configured asset that is, and the key this process holds for it, if any.
///
/// Custody is exactly the question the two halves answer together, and
/// `watch_only` on the wire is its one boolean.
pub(crate) struct Custody {
    pub(crate) watch: indexing::Watch,
    pub(crate) tag: AssetTag,
    pub(crate) wallet: Option<Arc<dyn wallets::Wallet>>,
}

/// A custody is one address in one asset, which is exactly what the index
/// reads by, so it is the subject of its own history and balance.
impl indexing::Holder for Custody {
    fn address(&self) -> &base::Address {
        &self.watch.address
    }

    fn asset(&self) -> &indexing::AssetId {
        &self.watch.asset
    }
}

impl Custody {
    /// The wallet that signs for this id.
    ///
    /// # Errors
    ///
    /// A 422 `WatchOnly` when this process holds no key for it.
    pub(crate) fn signing(&self) -> Result<&Arc<dyn wallets::Wallet>, Error> {
        self.wallet.as_ref().ok_or_else(Error::watch_only)
    }
}

/// The complete OpenAPI 3 document, without binding a listener.
///
/// `payment-server --openapi` prints exactly this, and `docs/openapi.json`
/// holds the committed copy typed clients are generated from.
#[must_use]
pub(crate) fn openapi() -> utoipa::openapi::OpenApi {
    let (_, mut document) = protected_routes().split_for_parts();
    let (_, public) = public_routes().split_for_parts();
    document.merge(public);
    document
}

/// The complete API router.
///
/// Wallet and transaction routes are protected by `config`'s bearer
/// authentication and body limit ([`http_server::protected_router`]); health
/// routes and `GET /openapi.json` are public. The OpenAPI contract is built
/// from the same handlers.
///
/// # Errors
///
/// Returns the [`http_server::ConfigError`] of an invalid `config` (for
/// example a non-loopback bind without TLS termination).
pub(crate) fn router(
    payments: Payments,
    config: &http_server::Config,
) -> Result<Router, http_server::ConfigError> {
    let (protected, mut contract) = protected_routes().split_for_parts();
    let (public, public_contract) = public_routes().split_for_parts();
    contract.merge(public_contract);
    let contract = Arc::new(contract);

    let protected = http_server::protected_router(protected.with_state(payments.clone()), config)?;
    Ok(protected
        .merge(public.with_state(payments))
        .layer(Extension(contract)))
}

fn protected_routes() -> OpenApiRouter<Payments> {
    OpenApiRouter::with_openapi(Contract::openapi())
        .merge(asset::routes())
        .merge(wallet::routes())
        .merge(address::routes())
        .merge(history::routes())
        .merge(transaction::routes())
}

fn public_routes() -> OpenApiRouter<Payments> {
    OpenApiRouter::new()
        .merge(health::routes())
        .merge(openapi::routes())
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn openapi_contains_every_route_method_and_schema() {
        let document = document();

        for (path, method) in [
            ("/healthz", "get"),
            ("/openapi.json", "get"),
            ("/v1/assets", "get"),
            ("/v1/wallets", "post"),
            ("/v1/wallets", "get"),
            ("/v1/addresses", "post"),
            ("/v1/wallets/{id}", "get"),
            ("/v1/wallets/{id}/balance", "get"),
            ("/v1/wallets/{id}/transactions", "get"),
            ("/v1/wallets/{id}/transactions", "post"),
            ("/v1/transactions", "post"),
        ] {
            assert_pointer(&document, &format!("/paths/{}/{method}", escape(path)));
        }

        for schema in [
            "Address",
            "Asset",
            "AssetList",
            "Backfill",
            "Balance",
            "Completeness",
            "BatchAccepted",
            "BatchedTransfer",
            "Block",
            "CreateWallet",
            "ErrorBody",
            "Fee",
            "Movement",
            "MovementKind",
            "Network",
            "NetworkCheckpoint",
            "ParentBlock",
            "Status",
            "Report",
            "Transaction",
            "TransactionPage",
            "Transfer",
            "TransferAccepted",
            "TransferBatch",
            "Wallet",
            "WalletList",
            "WatchAddress",
        ] {
            assert_pointer(&document, &format!("/components/schemas/{schema}"));
        }

        for retired in [
            "AddressInput",
            "Destination",
            "Scope",
            "SendFunds",
            "Submission",
            "TransferRequest",
            "TransferResponse",
            "WalletTransfer",
            "NetworkSync",
            "ScopedId",
            "Proof",
        ] {
            assert!(
                document
                    .pointer(&format!("/components/schemas/{retired}"))
                    .is_none(),
                "the retired schema {retired} must be gone"
            );
        }

        assert_pointer(
            &document,
            "/components/schemas/TransactionPage/properties/transactions/items/$ref",
        );
        assert!(
            document
                .pointer("/components/schemas/TransactionPage/properties/items")
                .is_none(),
            "transaction history must not expose anonymous items"
        );
        assert_pointer(
            &document,
            "/components/schemas/Transaction/properties/movements/items/$ref",
        );
        assert_eq!(
            document.pointer("/components/schemas/Transaction/properties/transaction_id/type"),
            Some(&Value::String("string".to_owned())),
        );
        assert!(
            document["components"]["schemas"]["MovementKind"]["enum"]
                .as_array()
                .is_some_and(|variants| {
                    variants
                        .iter()
                        .all(|value| value.as_str() != Some("internal_transfer"))
                })
        );

        for schema in [
            "CreateWallet",
            "Transfer",
            "BatchedTransfer",
            "TransferBatch",
            "WatchAddress",
        ] {
            assert_eq!(
                document.pointer(&format!(
                    "/components/schemas/{schema}/additionalProperties"
                )),
                Some(&Value::Bool(false)),
                "{schema} must reject and document unknown properties"
            );
        }
    }

    /// The `{chain, network}` pair is one nested object on every wire type; it
    /// is never flattened back into two sibling fields.
    #[test]
    fn every_chain_and_network_pair_is_one_nested_network_object() {
        let document = document();

        assert_exact_required_properties(&document, "Network", &["chain", "name"]);
        for schema in ["Wallet", "Address", "Transaction", "NetworkCheckpoint"] {
            assert_eq!(
                document.pointer(&format!(
                    "/components/schemas/{schema}/properties/network/$ref"
                )),
                Some(&serde_json::json!("#/components/schemas/Network")),
                "{schema}.network must reference the shared Network object"
            );
            assert!(
                document
                    .pointer(&format!("/components/schemas/{schema}/properties/chain"))
                    .is_none(),
                "{schema} must not carry a flattened chain field"
            );
            assert!(
                document
                    .pointer(&format!("/components/schemas/{schema}/properties/scope"))
                    .is_none(),
                "{schema} must not carry a scope field"
            );
        }
    }

    #[test]
    fn openapi_names_no_chain_or_closed_asset_list() {
        let document = document();
        let text = document.to_string();

        assert!(document.pointer("/components/schemas/Chain").is_none());
        assert!(
            document
                .pointer("/components/schemas/WalletAsset")
                .is_none()
        );
        for schema in ["CreateWallet", "WatchAddress"] {
            let asset = &document["components"]["schemas"][schema]["properties"]["asset"];
            assert_eq!(asset["type"], "string", "{schema}.asset is an open tag");
            assert_eq!(asset["pattern"], "^[a-z0-9][a-z0-9-]{0,31}$");
            assert!(
                asset.get("enum").is_none(),
                "{schema}.asset lists no assets"
            );
        }
        for chain in ["bitcoin", "ethereum", "solana", "btc", "usdc"] {
            assert!(
                !text
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .any(|word| word.eq_ignore_ascii_case(chain)),
                "the contract must not name `{chain}`"
            );
        }
        assert!(
            document["components"]["schemas"]
                .as_object()
                .is_some_and(|schemas| !schemas.keys().any(|name| name.ends_with("Encoding"))),
            "the address text alone determines its encoding"
        );
        // Word match: "display" (units) must not count as "spl".
        assert!(
            !document
                .to_string()
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|word| word.eq_ignore_ascii_case("spl")),
            "the contract must not publish a token-program asset or route"
        );
    }

    #[test]
    fn openapi_publishes_exact_transaction_contracts() {
        let document = document();
        let transfers = "/components/schemas/TransferBatch/properties/transfers";

        for (schema, properties) in [
            ("Transfer", &["destination", "amount"][..]),
            ("BatchedTransfer", &["wallet_id", "destination", "amount"]),
            ("TransferBatch", &["transfers"]),
            ("TransferAccepted", &["transaction_id"]),
            ("BatchAccepted", &["transaction_ids"]),
        ] {
            assert_exact_required_properties(&document, schema, properties);
        }
        // A destination is address text, never an object: `AddressInput` is gone.
        for pointer in [
            "/components/schemas/Transfer/properties/destination/type",
            "/components/schemas/Transfer/properties/amount/type",
            "/components/schemas/BatchedTransfer/properties/destination/type",
            "/components/schemas/BatchedTransfer/properties/wallet_id/type",
            "/components/schemas/BatchedTransfer/properties/amount/type",
        ] {
            assert_eq!(
                document.pointer(pointer),
                Some(&serde_json::json!("string"))
            );
        }
        for pointer in [
            "/components/schemas/Transfer/properties/destination/$ref",
            "/components/schemas/BatchedTransfer/properties/destination/$ref",
        ] {
            assert!(
                document.pointer(pointer).is_none(),
                "{pointer} must be plain address text"
            );
        }
        assert_eq!(
            document.pointer(&format!("{transfers}/minItems")),
            Some(&serde_json::json!(1)),
        );
        assert_eq!(
            document.pointer(&format!("{transfers}/maxItems")),
            Some(&serde_json::json!(wallets::MAX_TRANSFERS)),
        );
        assert_eq!(
            document.pointer(&format!("{transfers}/items/$ref")),
            Some(&Value::String(
                "#/components/schemas/BatchedTransfer".to_owned()
            )),
        );
        for unsupported_keyword in ["uniqueItems", "default"] {
            assert!(
                document
                    .pointer(&format!("{transfers}/{unsupported_keyword}"))
                    .is_none(),
                "ordered batch occurrences must not publish {unsupported_keyword}"
            );
        }

        assert_eq!(
            document.pointer("/components/schemas/ErrorBody/required"),
            Some(&serde_json::json!(["message"])),
        );
        assert_description_contains(
            &document,
            "/components/schemas/ErrorBody/properties/transaction_ids/description",
            &["Definitely acknowledged", "Present only"],
        );
        assert_description_contains(
            &document,
            "/components/schemas/ErrorBody/properties/failed_index/description",
            &["original request index", "Present only"],
        );
        assert_description_contains(
            &document,
            "/components/schemas/ErrorBody/properties/ambiguous_transaction_id/description",
            &["exact locally signed envelope", "Present only", "503"],
        );

        for operation in [
            "/paths/~1v1~1wallets~1{id}~1transactions/post/description",
            "/paths/~1v1~1transactions/post/description",
        ] {
            assert_description_contains(&document, operation, &["shared route"]);
        }
        assert_description_contains(
            &document,
            "/paths/~1v1~1transactions/post/description",
            &["Definitely acknowledged", "only when"],
        );
        assert!(
            document["paths"]
                .as_object()
                .is_some_and(|paths| paths.keys().all(|path| !path.contains("solana"))),
            "every asset must use the shared transaction routes"
        );
        assert_eq!(
            document.pointer(
                "/paths/~1v1~1wallets~1{id}~1transactions/post/requestBody/content/\
            application~1json/schema/$ref"
            ),
            Some(&serde_json::json!("#/components/schemas/Transfer")),
        );
        assert_eq!(
            document.pointer(
                "/paths/~1v1~1transactions/post/requestBody/content/\
            application~1json/schema/$ref"
            ),
            Some(&serde_json::json!("#/components/schemas/TransferBatch")),
        );
    }

    /// Arbitrary caller text is a query parameter, never a path segment.

    fn document() -> Value {
        serde_json::to_value(openapi()).expect("OpenAPI contract must serialize")
    }

    fn escape(path: &str) -> String {
        path.replace('~', "~0").replace('/', "~1")
    }

    fn assert_pointer(document: &Value, pointer: &str) {
        assert!(
            document.pointer(pointer).is_some(),
            "OpenAPI contract is missing {pointer}"
        );
    }

    fn assert_description_contains(document: &Value, pointer: &str, expected: &[&str]) {
        let description = document
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("OpenAPI contract is missing description {pointer}"));
        for fragment in expected {
            assert!(
                description.contains(fragment),
                "OpenAPI description {pointer} is missing `{fragment}`: {description}"
            );
        }
    }

    fn assert_exact_required_properties(document: &Value, schema: &str, expected: &[&str]) {
        let schema_pointer = format!("/components/schemas/{schema}");
        let properties = document
            .pointer(&format!("{schema_pointer}/properties"))
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("OpenAPI schema {schema} is missing properties"));
        assert_eq!(
            properties.len(),
            expected.len(),
            "OpenAPI schema {schema} has unexpected properties: {properties:?}"
        );
        for property in expected {
            assert!(
                properties.contains_key(*property),
                "OpenAPI schema {schema} is missing property {property}"
            );
        }

        let required = document
            .pointer(&format!("{schema_pointer}/required"))
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("OpenAPI schema {schema} is missing required properties"));
        assert_eq!(required.len(), expected.len());
        for property in expected {
            assert!(
                required
                    .iter()
                    .any(|value| value.as_str() == Some(property)),
                "OpenAPI schema {schema} does not require {property}"
            );
        }
    }
}
