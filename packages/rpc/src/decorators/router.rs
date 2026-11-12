//! [`Router`]: listed methods to their own rotation of clients, everything
//! else to the default.

use std::{
    collections::BTreeSet,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::{Answer, Batch, BoxFuture, Client, Error, ErrorKind, RawJson, Repetition, Request};

/// A default client, plus routes sending listed methods to their own clients.
///
/// A routed request starts at the route's next client, round robin, and
/// moves through the rest of the rotation on a retryable failure
/// ([`Error::is_retryable`]), then to the default. Any other error, such as
/// an error object that is no rate limit, is the answer. A batch whose calls all name one routed
/// method is routed as one unit; any other batch goes to the default.
/// [`Repetition::Forbidden`] work reaches the picked client only, once.
/// A broadcast always goes to the default, so a route never narrows it.
///
/// `Debug` shows the route count and nothing else.
pub struct Router {
    default: Arc<dyn Client>,
    routes: Vec<Route>,
}

/// Methods and the clients that take turns serving them.
struct Route {
    methods: BTreeSet<String>,
    clients: Vec<Arc<dyn Client>>,
    next: AtomicUsize,
}

impl Router {
    /// Every call reaches `default`, until routes are added.
    #[must_use]
    pub fn new(default: impl Client + 'static) -> Self {
        Self {
            default: Arc::new(default),
            routes: Vec::new(),
        }
    }

    /// The same, `methods` going to `clients`, round robin.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] for no methods, no clients, or a
    /// method another route already lists.
    pub fn route(
        mut self,
        methods: impl IntoIterator<Item = impl Into<String>>,
        clients: Vec<Arc<dyn Client>>,
    ) -> Result<Self, Error> {
        let methods: BTreeSet<String> = methods.into_iter().map(Into::into).collect();
        let invalid = |message| Err(Error::transport(ErrorKind::InvalidConfiguration, message));
        if methods.is_empty() {
            return invalid("a JSON-RPC route lists no methods");
        }
        if clients.is_empty() {
            return invalid("a JSON-RPC route lists no clients");
        }
        if methods.iter().any(|method| self.find(method).is_some()) {
            return invalid("a JSON-RPC method is routed twice");
        }
        self.routes.push(Route {
            methods,
            clients,
            next: AtomicUsize::new(0),
        });
        Ok(self)
    }

    fn find(&self, method: &str) -> Option<&Route> {
        self.routes
            .iter()
            .find(|route| route.methods.contains(method))
    }

    /// `send` to the route's next client; unless the work is once-only, on
    /// through its rotation past retryable failures, then to the default.
    /// No route: the default.
    async fn walk<'a, T>(
        &'a self,
        route: Option<&'a Route>,
        repetition: Repetition,
        send: impl Fn(&'a dyn Client) -> BoxFuture<'a, Result<T, Error>>,
    ) -> Result<T, Error> {
        let Some(route) = route else {
            return send(&*self.default).await;
        };
        let mut rotation = route.rotation();
        if repetition == Repetition::Forbidden {
            let picked = rotation.next().expect("a route lists a client");
            return send(picked).await;
        }
        for client in rotation {
            match send(client).await {
                Err(error) if error.is_retryable() => {}
                outcome => return outcome,
            }
        }
        send(&*self.default).await
    }
}

impl Route {
    /// Every client once, starting one further than the last rotation did.
    fn rotation(&self) -> impl Iterator<Item = &dyn Client> {
        let count = self.clients.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed) % count;
        (0..count).map(move |step| &*self.clients[(start + step) % count])
    }
}

impl fmt::Debug for Router {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Router")
            .field("route_count", &self.routes.len())
            .finish()
    }
}

impl Client for Router {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
        let route = self.find(request.method());
        let repetition = request.repetition();
        Box::pin(self.walk(route, repetition, move |client| {
            client.request(request.clone())
        }))
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
        let route = batch.method().and_then(|method| self.find(method));
        let repetition = batch.repetition();
        Box::pin(self.walk(route, repetition, move |client| client.batch(batch.clone())))
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
        self.default.broadcast(request)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures_executor::block_on;
    use serde_json::json;

    use super::Router;
    use crate::{
        Batch, Call, Client, Error, ErrorKind, Failure, Request,
        testing::{Fault, Faults, Reply, Script, Switch},
    };

    /// A scripted client, failing on demand.
    fn node() -> (Arc<dyn Client>, Script, Switch) {
        let script = Script::new([]);
        let faults = Faults::new(script.clone());
        let switch = faults.switch();
        (Arc::new(faults), script, switch)
    }

    fn ask(router: &Router, method: &str) -> Result<u64, Error> {
        let raw = block_on(router.request(Request::new(method.to_owned(), json!([]))))?;
        raw.deserialize()
    }

    /// A default and two route clients `a`, `b` for `m`.
    fn routed() -> (Router, [(Script, Switch); 3]) {
        let (default, d, ds) = node();
        let (a, sa, wa) = node();
        let (b, sb, wb) = node();
        let router = Router::new(default).route(["m"], vec![a, b]).unwrap();
        (router, [(d, ds), (sa, wa), (sb, wb)])
    }

    #[test]
    fn an_unrouted_method_reaches_the_default_only() {
        let (router, [(default, _), (a, _), (b, _)]) = routed();
        default.push([Reply::ok("n", json!(0))]);
        assert_eq!(ask(&router, "n").unwrap(), 0);
        for script in [default, a, b] {
            script.exhausted();
        }
    }

    #[test]
    fn a_routed_method_alternates() {
        let (router, [(default, _), (a, _), (b, _)]) = routed();
        a.push([Reply::ok("m", json!(1)), Reply::ok("m", json!(1))]);
        b.push([Reply::ok("m", json!(2)), Reply::ok("m", json!(2))]);
        let answers: Vec<u64> = (0..4).map(|_| ask(&router, "m").unwrap()).collect();
        assert_eq!(answers, [1, 2, 1, 2]);
        for script in [default, a, b] {
            script.exhausted();
        }
    }

    #[test]
    fn a_same_method_batch_is_one_unit_and_a_mixed_one_goes_to_the_default() {
        let (router, [(default, _), (a, wa), (b, _)]) = routed();
        a.push([Reply::ok("m", json!(1)), Reply::ok("m", json!(1))]);
        let same = Batch::new(vec![Call::new("m", json!([])), Call::new("m", json!([]))]);
        block_on(router.batch(same.unwrap())).unwrap();
        assert_eq!(wa.requests(), 1, "one batch, to one client");

        default.push([Reply::ok("m", json!(0)), Reply::ok("n", json!(0))]);
        let mixed = Batch::new(vec![Call::new("m", json!([])), Call::new("n", json!([]))]);
        block_on(router.batch(mixed.unwrap())).unwrap();
        for script in [default, a, b] {
            script.exhausted();
        }
    }

    #[test]
    fn a_retryable_failure_moves_on_then_to_the_default() {
        let (router, [(default, _), (a, wa), (b, wb)]) = routed();
        wa.fail_next(
            None,
            Fault::Remote(-32_005, "Rate limit exceeded".to_owned()),
        );
        b.push([Reply::ok("m", json!(2))]);
        assert_eq!(ask(&router, "m").unwrap(), 2);

        wb.down(true);
        wa.down(true);
        default.push([Reply::ok("m", json!(0))]);
        assert_eq!(ask(&router, "m").unwrap(), 0);
        for script in [default, a, b] {
            script.exhausted();
        }
    }

    #[test]
    fn an_error_object_is_the_answer() {
        let (router, [(default, _), (a, _), (b, wb)]) = routed();
        a.push([Reply::failure("m", -32_000, "execution reverted")]);
        let error = ask(&router, "m").unwrap_err();
        assert!(matches!(
            error,
            Error::Remote(Failure { code: -32_000, .. })
        ));
        assert_eq!(wb.requests(), 0);
        for script in [default, a, b] {
            script.exhausted();
        }
    }

    #[test]
    fn once_only_work_reaches_the_picked_client_only() {
        let (router, [(default, _), (a, wa), (b, wb)]) = routed();
        wa.fail_next(None, Fault::Transport(ErrorKind::Unavailable));
        let once = Request::once("m", json!([]));
        let error = block_on(router.request(once)).unwrap_err();
        assert_eq!(error.kind(), Some(ErrorKind::Unavailable));
        assert_eq!((wa.requests(), wb.requests()), (1, 0));
        for script in [default, a, b] {
            script.exhausted();
        }
    }

    #[test]
    fn a_broadcast_goes_to_the_default() {
        let (router, [(default, ds), (a, wa), (b, _)]) = routed();
        default.push([Reply::ok("m", json!("id"))]);
        block_on(router.broadcast(Request::once("m", json!([])))).unwrap();
        assert_eq!((ds.requests(), wa.requests()), (1, 0));
        for script in [default, a, b] {
            script.exhausted();
        }
    }

    #[test]
    fn a_route_must_list_methods_clients_and_new_methods() {
        let (router, _) = routed();
        let (c, _, _) = node();
        let twice = router.route(["m"], vec![c.clone()]).unwrap_err();
        assert_eq!(twice.kind(), Some(ErrorKind::InvalidConfiguration));
        let (default, _, _) = node();
        let none: [&str; 0] = [];
        assert!(Router::new(default.clone()).route(none, vec![c]).is_err());
        assert!(Router::new(default).route(["m"], Vec::new()).is_err());
    }

    #[tokio::test]
    async fn concurrent_calls_share_the_rotation_evenly() {
        let (router, [_, (a, wa), (b, wb)]) = routed();
        a.push((0..50).map(|_| Reply::ok("m", json!(1))));
        b.push((0..50).map(|_| Reply::ok("m", json!(2))));
        let router = Arc::new(router);
        let calls = (0..100).map(|_| {
            let router = router.clone();
            tokio::spawn(async move { router.request(Request::new("m", json!([]))).await })
        });
        for call in calls.collect::<Vec<_>>() {
            call.await.unwrap().unwrap();
        }
        assert_eq!((wa.calls("m"), wb.calls("m")), (50, 50));
    }
}
