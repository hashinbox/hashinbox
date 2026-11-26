//! `scope!` and `schema!`.

/// A [`Scope`](crate::Scope) from its path and action, written as code:
/// `scope!(com.hashinbox.wallets[id] => read)` is
/// `com.hashinbox.wallets[<id>].$read`. A step is a name or `*`; an id in
/// `[ ]` is an expression (anything `ToString`); the action after `=>` is
/// a name or `*`.
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
    ($($step:tt $([$id:expr])?).+ => $action:tt) => {
        $crate::Scope::__from_parts(
            ::std::vec![$(
                $crate::Segment::__from_token(
                    ::core::stringify!($step),
                    $crate::__id!($($id)?),
                )
            ),+],
            ::core::stringify!($action),
        )
    };
}

/// The id of one `scope!` step: `None`, or the `[ ]`'s.
#[doc(hidden)]
#[macro_export]
macro_rules! __id {
    () => {
        ::core::option::Option::None
    };
    ($id:expr) => {
        ::core::option::Option::Some(::std::string::ToString::to_string(&$id))
    };
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
