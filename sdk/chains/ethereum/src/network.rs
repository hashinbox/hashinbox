/// An EVM chain's numeric identity and stable application network name.
///
/// The numeric ID is verified against RPC. The slug identifies persisted scopes
/// and must remain stable across restarts; neither value is derived from the other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub chain_id: u64,
    pub slug: String,
}

impl Network {
    #[must_use]
    pub fn new(chain_id: u64, slug: impl Into<String>) -> Self {
        Self {
            chain_id,
            slug: slug.into(),
        }
    }
}
