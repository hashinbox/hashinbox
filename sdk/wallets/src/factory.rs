use std::{any::Any, sync::Arc};

use base::{Address, AssetId, Network};
use crypto::SecretBytes;

use crate::{Authorship, FutureResult, Wallet, WalletError, WalletErrorKind};

/// Makes the wallets of one asset in one network; a chain's `WalletFactory`
/// implements it, and [`crate::Wallets::add`] takes it under an asset key at
/// the composition root.
///
/// Every factory must select its native key-generation policy explicitly. An
/// implementation that supplies only the import path must not compile:
///
/// ```compile_fail,E0046
/// use std::sync::Arc;
/// use wallets::{FutureResult, SecretBytes, Wallet, WalletFactory};
///
/// // (AddressParser and FactoryProfile impls omitted.)
///
/// struct IncompleteFactory;
///
/// impl WalletFactory for IncompleteFactory {
///     fn validate(&self, _: &dyn Wallet) -> Result<(), wallets::WalletError> {
///         Ok(())
///     }
///
///     fn create<'a>(
///         &'a self,
///         _secret: SecretBytes,
///     ) -> FutureResult<'a, Arc<dyn Wallet>> {
///         unimplemented!()
///     }
/// }
/// ```
pub trait WalletFactory: AddressParser + FactoryProfile + Send + Sync {
    /// The wallet of `secret`: 32 bytes of key material, which every
    /// supported chain writes as 64 hexadecimal characters and
    /// [`crypto::SecretBytes::from_hex`] decodes chain-neutrally. The
    /// factory's own `KeyPair::import` is what checks the length and the
    /// curve, so no separate parse exists to disagree with it.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::InvalidSecret`] when the bytes are not a key of this
    /// chain's curve.
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>>;

    /// Generates a wallet and the material [`WalletFactory::create`] rebuilds
    /// it from. [`crate::Wallets::create`] drops the material and returns the
    /// wallet; [`crate::Wallets::secret`] returns the material instead. The
    /// index never sees either.
    ///
    /// A chain implements this as one line over [`Generation::of`]: the only
    /// chain-specific part is how it makes a secret.
    fn generate(&self) -> FutureResult<'_, Generation>;

    /// Checks an existing wallet, without I/O.
    ///
    /// Accept only a wallet this factory could have made: the same network,
    /// configuration, and shared handles ([`Origin`]), so single and batch
    /// sends use one pipeline. Address equality alone does not establish
    /// wallet compatibility.
    fn validate(&self, wallet: &dyn Wallet) -> Result<(), WalletError>;
}

/// Parses user-supplied address text of one network; every
/// [`WalletFactory`] is one, for its own network.
/// A chain's `WalletConfig` implements this too, which is where the parse
/// actually lives: it needs no store, no node and no key, so a caller holding
/// only a `Network` can validate user text. A `WalletFactory`'s own impl
/// delegates to it, so there is one parse and not two.
pub trait AddressParser {
    /// The canonical [`Address`] of `text` in this parser's network, without
    /// I/O or a key.
    ///
    /// This is the only lenient address parse: it accepts every textual form
    /// the chain defines (for example any-case hexadecimal with its checksum)
    /// and returns the one canonical text.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::InvalidAddress`] when `text` is not an address of this
    /// parser's network.
    fn parse_address(&self, text: &str) -> Result<Address, WalletError>;
}

/// A freshly generated wallet and the secret material it was made from.
///
/// The material is in the factory's import format: `create(material)`
/// returns the same wallet.
pub struct Generation {
    /// The new wallet.
    pub wallet: Arc<dyn Wallet>,
    /// Its key in the chain's import format; zeroized on drop.
    pub material: SecretBytes,
}

impl Generation {
    /// The generation of `secret` by `factory`, keeping `secret` as the
    /// material.
    ///
    /// Generation has one shape on every chain — make a secret, keep it, and
    /// build the wallet from it with [`WalletFactory::create`] — so it is
    /// written once here instead of once per chain. `secret` is the chain's
    /// own generation result, already in [`WalletError`] terms; a failed generation
    /// is returned unchanged and no wallet is built.
    pub fn of<'a>(
        factory: &'a (impl WalletFactory + ?Sized),
        secret: Result<SecretBytes, WalletError>,
    ) -> FutureResult<'a, Self> {
        Box::pin(async move {
            let secret = secret?;
            let material = SecretBytes::new(secret.as_bytes());
            let wallet = factory.create(secret).await?;
            Ok(Self { wallet, material })
        })
    }
}

/// What [`crate::Wallets::add`] reads once from a [`WalletFactory`]:
/// the facts every wallet it makes shares.
///
/// Named for the factory, not for registering, because `wallets` already uses
/// "registration" for a submission's lifetime
/// ([`crate::RegistrationError`], [`crate::SubmissionRegistrar`]).
pub trait FactoryProfile {
    /// The one network its wallets live in; `(chain, network)` is their index
    /// network.
    fn network(&self) -> &Network;

    /// The identity of the asset its wallets hold — the same value every one
    /// of them answers [`indexing::Holder::asset`] with, which selects their
    /// history and balance.
    ///
    /// Identity only: how much an amount of it is, and what to call it, is
    /// the asset's own ([`base::Units`]), and no wallet carries it.
    fn asset(&self) -> AssetId;

    /// How this factory's wallets spend, or `None` for an asset the system
    /// receives and reports and never spends.
    ///
    /// This is the one declaration the whole spending capability hangs from.
    /// A factory that returns `None` can hand out nothing that sends: its
    /// wallets are plain [`Wallet`]s, [`crate::Wallets::author`] refuses the
    /// tag at lookup, and [`crate::Wallets::send_all`] has no
    /// [`crate::Sender`] to reach. Nothing downstream has to check again.
    fn authorship(&self) -> Option<Arc<dyn Authorship>>;
}

/// A boxed factory is a factory, so a composition root can collect the
/// factories of several chains as `Box<dyn WalletFactory>` before registering
/// them.
impl<F: WalletFactory + ?Sized> WalletFactory for Box<F> {
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>> {
        (**self).create(secret)
    }

    fn generate(&self) -> FutureResult<'_, Generation> {
        (**self).generate()
    }

    fn validate(&self, wallet: &dyn Wallet) -> Result<(), WalletError> {
        (**self).validate(wallet)
    }
}

impl<F: AddressParser + ?Sized> AddressParser for Box<F> {
    fn parse_address(&self, text: &str) -> Result<Address, WalletError> {
        (**self).parse_address(text)
    }
}

impl<F: FactoryProfile + ?Sized> FactoryProfile for Box<F> {
    fn network(&self) -> &Network {
        (**self).network()
    }

    fn asset(&self) -> AssetId {
        (**self).asset()
    }

    fn authorship(&self) -> Option<Arc<dyn Authorship>> {
        (**self).authorship()
    }
}

/// Pointer identity of one shared factory dependency.
///
/// Two wallets built by one factory hold the same `Arc`s; comparing their
/// dependencies proves shared RPC, index, and send-pipeline handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dependency(usize);

impl Dependency {
    /// The identity of the value `shared` points to.
    #[must_use]
    pub fn of<T: ?Sized>(shared: &Arc<T>) -> Self {
        Self(Arc::as_ptr(shared).cast::<()>().addr())
    }
}

/// What [`WalletFactory::validate`] compares to accept only wallets its
/// factory made.
///
/// The fingerprint is any `PartialEq` value, usually the wallet configuration
/// (which names the network) plus the [`Dependency`] of each shared handle.
pub struct Origin<F> {
    fingerprint: F,
}

impl<F: PartialEq> Origin<F> {
    /// The factory's own fingerprint.
    #[must_use]
    pub const fn new(fingerprint: F) -> Self {
        Self { fingerprint }
    }

    /// Accepts `wallet` when it is a `W` and `fingerprint(wallet)` equals
    /// this origin's fingerprint.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unsupported`] when either check fails.
    pub fn validate<W: Wallet>(
        &self,
        wallet: &dyn Wallet,
        fingerprint: impl FnOnce(&W) -> F,
    ) -> Result<(), WalletError> {
        let Some(wallet) = (wallet as &dyn Any).downcast_ref::<W>() else {
            return Err(WalletError::new(
                WalletErrorKind::Unsupported,
                "wallet was not created by this factory type",
            ));
        };
        if fingerprint(wallet) != self.fingerprint {
            return Err(WalletError::new(
                WalletErrorKind::Unsupported,
                "wallet does not match its factory's configuration or dependencies",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use base::{SignFuture, SignRequest, Signer};

    use super::*;
    use indexing::Holder;

    /// A wallet made with configuration `config` over the handle `outbox`.
    struct Probe {
        config: u8,
        outbox: Arc<()>,
    }

    impl Probe {
        fn fingerprint(&self) -> (u8, Dependency) {
            (self.config, Dependency::of(&self.outbox))
        }
    }

    /// Another wallet type.
    struct Other;

    macro_rules! inert_wallet {
        ($wallet:ty) => {
            impl Holder for $wallet {
                fn address(&self) -> &Address {
                    unreachable!("validation must not read the address")
                }

                fn asset(&self) -> &indexing::AssetId {
                    unreachable!("validation must not read the asset")
                }
            }

            impl Signer for $wallet {
                fn sign<'a>(&'a self, _: SignRequest) -> SignFuture<'a> {
                    unreachable!("validation must not sign")
                }
            }
        };
    }

    inert_wallet!(Probe);
    inert_wallet!(Other);

    #[test]
    fn origin_accepts_only_its_own_type_configuration_and_dependencies() {
        let outbox = Arc::new(());
        let origin = Origin::new((1, Dependency::of(&outbox)));
        let wallet = Probe {
            config: 1,
            outbox: outbox.clone(),
        };

        assert!(origin.validate(&wallet, Probe::fingerprint).is_ok());
        for wallet in [
            Probe {
                config: 2,
                outbox: outbox.clone(),
            },
            Probe {
                config: 1,
                outbox: Arc::new(()),
            },
        ] {
            let error = origin
                .validate(&wallet, Probe::fingerprint)
                .expect_err("mismatched wallet must be rejected");
            assert_eq!(error.kind, WalletErrorKind::Unsupported);
        }
        let error = origin
            .validate(&Other, Probe::fingerprint)
            .expect_err("foreign wallet type must be rejected");
        assert_eq!(error.kind, WalletErrorKind::Unsupported);
    }
}
