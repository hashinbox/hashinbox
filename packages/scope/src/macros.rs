//! `scope!`, `require!` and `schema!`.

/// A [`Scope`](crate::Scope) from its path and action, written as code:
/// `scope!(com.hashinbox.wallets[id] => read)` is
/// `com.hashinbox.wallets[<id>].$read`. A step is a name or `*`; ids in
/// `[ ]` are expressions (anything `ToString`), several split by `,`; the
/// action after `=>` is a name or `*`.
///
/// ```
/// let id = "ethereum.mainnet.0xabc";
/// let one = scope::scope!(com.hashinbox.wallets[id] => read);
/// assert_eq!(one.to_string(), "com.hashinbox.wallets[ethereum.mainnet.0xabc].$read");
/// let all = scope::scope!(com.hashinbox.wallets.* => *);
/// assert_eq!(all.to_string(), "com.hashinbox.wallets.*.$*");
/// ```
#[macro_export]
macro_rules! scope {
    ($($step:tt $([$($id:expr),+ $(,)?])?).+ => $action:tt) => {
        $crate::Scope::__from_parts(
            ::std::vec![$(
                $crate::Segment::__from_token(
                    ::core::stringify!($step),
                    $crate::__ids!($([$($id),+])?),
                )
            ),+],
            ::core::stringify!($action),
        )
    };
}

/// The ids of one `scope!` step: `None`, or the set of `[ ]`'s.
#[doc(hidden)]
#[macro_export]
macro_rules! __ids {
    () => {
        ::core::option::Option::None
    };
    ([$($id:expr),+]) => {
        ::core::option::Option::Some(
            [$(::std::string::ToString::to_string(&$id)),+]
                .into_iter()
                .collect::<::std::collections::BTreeSet<::std::string::String>>(),
        )
    };
}

/// Denies the request unless `granted` (a [`Scopes`](crate::Scopes))
/// covers the scope written after it, as [`scope!`] takes it: returns
/// `Err(Denied.into())` from the function it is in, so any error type
/// with `From<Denied>` works.
///
/// ```
/// use scope::{Denied, Scopes, require};
///
/// fn balance(granted: &Scopes, wallet: &str) -> Result<&'static str, Denied> {
///     require!(granted, com.hashinbox.wallets[wallet] => read);
///     Ok("1.5 BTC")
/// }
///
/// let token: Scopes = "com.hashinbox.wallets[w1].$read".parse().unwrap();
/// assert!(balance(&token, "w1").is_ok());
/// let denied = balance(&token, "w2").unwrap_err();
/// assert_eq!(denied.to_string(), "missing scope com.hashinbox.wallets[w2].$read");
/// ```
#[macro_export]
macro_rules! require {
    ($granted:expr, $($scope:tt)+) => {{
        let needed = $crate::scope!($($scope)+);
        if !$granted.covers(&needed) {
            return ::core::result::Result::Err($crate::Denied::new(needed).into());
        }
    }};
}

/// Declares a [`Schema`](crate::Schema) static: its namespace, then each
/// resource's path (below the namespace) and actions.
///
/// ```
/// scope::schema! {
///     /// Every hashinbox scope.
///     pub static HASHINBOX = com.hashinbox {
///         wallets => [read, write],
///         wallets.legs => [read],
///         markets => [read],
///     }
/// }
///
/// assert_eq!(HASHINBOX.scopes().len(), 4);
/// ```
#[macro_export]
macro_rules! schema {
    (
        $(#[$meta:meta])*
        $vis:vis static $name:ident = $($namespace:ident).+ {
            $($($path:ident).+ => [$($action:ident),+ $(,)?]),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis static $name: $crate::Schema = $crate::Schema::new(
            &[$(::core::stringify!($namespace)),+],
            &[$($crate::Resource {
                path: &[$(::core::stringify!($path)),+],
                actions: &[$(::core::stringify!($action)),+],
            }),*],
        );
    };
}
