use std::{error, fmt};

/// Which configuration rule an [`Error`] broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The endpoint list is empty or one of its URLs is not a usable JSON-RPC
    /// endpoint.
    Endpoints,
    /// The repetition policy's bounds contradict each other.
    Repeat,
    /// A cadence bound is zero.
    Cadence,
    /// A configuration document could not be read or decoded; the message
    /// carries the decoder's own wording.
    Document,
    /// One named section is not a section any registered chain understands,
    /// or the chain that owns it refused it.
    Section,
    /// A candidate node list built and parsed, and its node did not prove the
    /// network its section configures ([`crate::config::ChainNodes::verify`]).
    Unverified,
}

/// A configuration value the deployment cannot run on.
///
/// The message never contains an endpoint URL or its credentials: an operator reads it from a log, and a bad URL is identified by
/// what is wrong with it rather than by quoting it back.
///
/// [`Self::section`] names the part of the document at fault when there is
/// one. A document has one section per chain and every chain parses its own,
/// so "which section" is the first thing an operator needs and the last thing
/// a chain's own error can say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// What went wrong, for callers to branch on.
    pub kind: ErrorKind,
    /// The named section this failure belongs to, when it belongs to one.
    pub section: Option<String>,
    /// Human-readable context.
    pub message: String,
}

impl Error {
    /// A failure that belongs to the document as a whole.
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            section: None,
            message: message.into(),
        }
    }

    /// Whatever a chain refused, as this crate reports it. A chain crate calls
    /// this from its own registration hook; [`Registry`](crate::config::Registry)
    /// names the section afterwards, because a chain does not know what it was
    /// registered as.
    #[must_use]
    pub fn refused(message: impl fmt::Display) -> Self {
        Self::new(ErrorKind::Section, message.to_string())
    }

    /// The same failure, attributed to `section`. Applied once, by the
    /// registry, so a chain never has to repeat its own name.
    #[must_use]
    pub fn in_section(mut self, section: &str) -> Self {
        self.section = Some(section.to_owned());
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.section {
            Some(section) => write!(formatter, "{section}: {}", self.message),
            None => formatter.write_str(&self.message),
        }
    }
}

impl error::Error for Error {}

impl From<json_rpc::Error> for Error {
    /// The transport's own wording is kept verbatim: it already names the
    /// parameter or the scheme at fault, and it already omits the URL. Every
    /// failure the transport can raise while a value is still being checked
    /// or composed is about the endpoint list, so one kind covers them.
    fn from(error: json_rpc::Error) -> Self {
        Self::new(ErrorKind::Endpoints, error.message)
    }
}

impl From<wallets::WalletError> for Error {
    /// A chain refused to compose itself over the storage it was handed. The
    /// section is attached by whatever was iterating chains at the time.
    fn from(error: wallets::WalletError) -> Self {
        Self::new(ErrorKind::Section, error.message)
    }
}

impl From<indexing::IndexError> for Error {
    /// A chain's index or the composition over every chain refused: two
    /// chains claiming one network, or a store that failed while a chain was
    /// composed over it. The index's own wording is kept verbatim.
    fn from(error: indexing::IndexError) -> Self {
        Self::new(ErrorKind::Section, error.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_section_name_is_attached_once_and_shown_before_the_message() {
        let bare = Error::refused("Solana requires exactly one RPC endpoint");
        assert_eq!(bare.section, None);
        assert_eq!(bare.to_string(), "Solana requires exactly one RPC endpoint");

        let named = bare.in_section("solana");
        assert_eq!(named.kind, ErrorKind::Section);
        assert_eq!(named.section.as_deref(), Some("solana"));
        assert_eq!(
            named.to_string(),
            "solana: Solana requires exactly one RPC endpoint"
        );
    }
}
