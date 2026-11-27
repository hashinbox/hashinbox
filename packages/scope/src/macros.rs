//! `schema!`.

/// Declares a [`Schema`](crate::Schema) static: its namespace, then each
/// resource's path below it, a child after its parent.
///
/// ```
/// scope::schema! {
///     /// Every hashinbox scope.
///     pub static HASHINBOX = com.hashinbox {
///         wallets,
///         wallets.tags,
///         triggers,
///     }
/// }
///
/// assert_eq!(HASHINBOX.resources().len(), 3);
/// ```
#[macro_export]
macro_rules! schema {
    (
        $(#[$meta:meta])*
        $vis:vis static $name:ident = $($namespace:ident).+ {
            $($($path:ident).+),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis static $name: $crate::Schema = $crate::Schema::new(
            &[$(::core::stringify!($namespace)),+],
            &[$(&[$(::core::stringify!($path)),+]),*],
        );
    };
}
