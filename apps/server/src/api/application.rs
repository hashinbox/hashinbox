//! Request snapshots share live domain owners; configuration swaps never rebuild a sender.
use super::Payments;
use axum::extract::FromRef;
use std::sync::Arc;
use tokio::sync::watch;

/// What the router is built over: the published composition, and the runtime
/// that publishes it when one is running.
#[derive(Clone)]
pub(crate) struct Application {
    pub(crate) payments: watch::Receiver<Arc<Payments>>,
    pub(crate) runtime: Option<Arc<crate::runtime::Runtime>>,
}

impl Application {
    #[cfg(test)]
    pub(crate) fn fixed(payments: Payments) -> Self {
        Self {
            payments: watch::channel(Arc::new(payments)).1,
            runtime: None,
        }
    }
}

/// A request holds the snapshot current when it was extracted, to the end:
/// a publication landing meanwhile is seen by the next request, never by half
/// of this one.
impl FromRef<Application> for Arc<Payments> {
    fn from_ref(application: &Application) -> Self {
        application.payments.borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot taken before a publication keeps the composition it started
    /// with; the next extraction sees the new one.
    #[test]
    fn a_publication_mid_request_reaches_only_the_next_request() {
        let first = super::super::route_contract::fixture(true).payments;
        let (published, receiver) = watch::channel(Arc::new(first.clone()));
        let application = Application {
            payments: receiver,
            runtime: None,
        };
        let held = Arc::<Payments>::from_ref(&application);
        let next = Payments {
            inactive: [base::Network::new(
                base::ChainName::new("bitcoin"),
                "regtest",
            )]
            .into(),
            ..first
        };
        published.send_replace(Arc::new(next));

        assert!(held.inactive.is_empty(), "the request keeps its snapshot");
        assert_eq!(Arc::<Payments>::from_ref(&application).inactive.len(), 1);
    }
}
