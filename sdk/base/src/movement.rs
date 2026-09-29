/// Closed vocabulary of value changes shared by chain movements and indexed history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MovementKind {
    /// Value moved from one address to another (account chains).
    Transfer,
    /// A spent output (UTXO chains); the owner may be unknown.
    Input,
    /// A created output (UTXO chains); the owner may be unknown.
    Output,
    /// Value created to an address, e.g. a token mint.
    Mint,
    /// Value destroyed from an address, e.g. a token burn.
    Burn,
}
