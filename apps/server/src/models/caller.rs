//! [`Caller`]: who is calling (design 096), as `usecase::sessions::authenticate`
//! checked it. No table.

use scope::Scopes;

use super::{Session, User, UserKey};

/// Who is calling, and the scopes that count: the session's, already
/// checked to be covered by its key's and its user's.
#[derive(Clone, Debug)]
pub(crate) struct Caller {
    /// The user: the session's, or its key's.
    pub(crate) user: User,
    /// The key, when a key is calling (its secret, or a session it signed).
    pub(crate) key: Option<UserKey>,
    /// The session.
    pub(crate) session: Session,
}

impl scope::Granted for Caller {
    /// The scopes that count: the session's.
    fn scopes(&self) -> &Scopes {
        &self.session.scopes
    }
}
