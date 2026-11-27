//! `scope::require!` templates, and the OpenAPI security `#[scope::utoipa]`
//! reads from them.

use futures_executor::block_on;
use scope::{Denied, Scopes};
use utoipa::OpenApi;

/// A route's error: any with `From<Denied>` works.
#[derive(Debug, PartialEq, Eq)]
enum Error {
    /// `403`.
    Forbidden(String),
}

impl From<Denied> for Error {
    fn from(denied: Denied) -> Self {
        Self::Forbidden(denied.needed().to_owned())
    }
}

/// A session: a caller's type implementing `Granted`.
struct Session(Scopes);

impl scope::Granted for Session {
    fn scopes(&self) -> &Scopes {
        &self.0
    }
}

/// One wallet: `{id}` captures `id`.
#[scope::utoipa(get, path = "/v1/wallets/{id}", responses((status = 200, body = String)))]
async fn read(session: Session, id: String) -> Result<String, Error> {
    scope::require!(session, "com.hashinbox.wallets[{id}]:read");
    Ok(id)
}

/// A transfer: the path's wallet, then each wallet of the body, several
/// `require!`s.
#[scope::utoipa(post, path = "/v1/wallets/{id}/transfers")]
async fn transfer(granted: Scopes, id: &str, transfer: Transfer) -> Result<(), Error> {
    scope::require!(granted, "com.hashinbox.wallets[{id}]");
    scope::require!(
        &granted,
        "com.hashinbox.wallets[{from}]",
        from = transfer.from
    );
    scope::require!(&granted, "com.hashinbox.wallets[{to}]", to = transfer.to);
    for wallet in transfer.notify {
        scope::require!(granted, "com.hashinbox.wallets[{wallet}]:read");
    }
    scope::require!(granted, "com.hashinbox.wallets[{id}]");
    Ok(())
}

/// A transfer's body.
struct Transfer {
    /// The wallet it takes from.
    from: &'static str,
    /// The wallet it pays.
    to: &'static str,
    /// Wallets told of it.
    notify: Vec<&'static str>,
}

#[derive(OpenApi)]
#[openapi(paths(read, transfer))]
struct Api;

fn session(text: &str) -> Session {
    Session(text.parse().unwrap())
}

/// The security scopes of `method` on `path` in the OpenAPI document.
fn security(path: &str, method: &str) -> serde_json::Value {
    let doc = serde_json::to_value(Api::openapi()).unwrap();
    doc["paths"][path][method]["security"].clone()
}

#[test]
fn openapi_security_is_every_template() {
    assert_eq!(
        security("/v1/wallets/{id}", "get"),
        serde_json::json!([{"bearer": ["com.hashinbox.wallets[{id}]:read"]}])
    );
    assert_eq!(
        security("/v1/wallets/{id}/transfers", "post"),
        serde_json::json!([{"bearer": [
            "com.hashinbox.wallets[{id}]",
            "com.hashinbox.wallets[{from}]",
            "com.hashinbox.wallets[{to}]",
            "com.hashinbox.wallets[{wallet}]:read",
        ]}])
    );
}

#[test]
fn other_args_pass_through() {
    let doc = serde_json::to_value(Api::openapi()).unwrap();
    let responses = &doc["paths"]["/v1/wallets/{id}"]["get"]["responses"];
    assert!(responses.get("200").is_some(), "{responses}");
}

#[test]
fn a_captured_variable_fills_its_placeholder() {
    let read = |held: &str, id: &str| block_on(read(session(held), id.to_owned()));
    assert_eq!(
        read("com.hashinbox.wallets[w1]:read", "w1"),
        Ok("w1".to_owned())
    );
    assert_eq!(read("com.hashinbox", "w2"), Ok("w2".to_owned()));
    assert_eq!(
        read("com.hashinbox.wallets[w1]:read", "w2"),
        Err(Error::Forbidden(
            "com.hashinbox.wallets[w2]:read".to_owned()
        ))
    );
    assert!(read("com.hashinbox.wallets.tags", "w1").is_err());
}

#[test]
fn a_hostile_id_is_escaped_not_structure() {
    let read = |held: &str, id: &str| block_on(read(session(held), id.to_owned()));
    // Unescaped, `w2]:read com.hashinbox.wallets[w1` would break out.
    let held = "com.hashinbox.wallets[w1]:read";
    assert_eq!(
        read(held, "w2].x[w1"),
        Err(Error::Forbidden(
            "com.hashinbox.wallets[w2%5D.x%5Bw1]:read".to_owned()
        ))
    );
    assert!(read(held, "w1 x").is_err());
    assert!(read(held, "w1%").is_err());
    let odd = "com.hashinbox.wallets[a%5Db]:read";
    assert_eq!(read(odd, "a]b"), Ok("a]b".to_owned()));
}

#[test]
fn several_require_and_named_args_each_count() {
    let call = |held: &str, notify: Vec<&'static str>| {
        let body = Transfer {
            from: "a",
            to: "b",
            notify,
        };
        block_on(transfer(held.parse().unwrap(), "a", body))
    };
    let both = "com.hashinbox.wallets[a] com.hashinbox.wallets[b]";
    assert_eq!(call(both, vec![]), Ok(()));
    assert_eq!(
        call("com.hashinbox.wallets[a]", vec![]),
        Err(Error::Forbidden("com.hashinbox.wallets[b]".to_owned()))
    );
    let and_read = format!("{both} com.hashinbox.wallets[c]:read");
    assert_eq!(call(&and_read, vec!["c"]), Ok(()));
    assert_eq!(
        call(&and_read, vec!["c", "d"]),
        Err(Error::Forbidden("com.hashinbox.wallets[d]:read".to_owned()))
    );
}

#[test]
fn a_template_that_does_not_parse_is_denied() {
    fn bad(granted: &Scopes, name: &str) -> Result<(), Denied> {
        scope::require!(granted, "com.hashinbox.{name}:read");
        Ok(())
    }
    let all: Scopes = "com.hashinbox".parse().unwrap();
    assert_eq!(
        bad(&all, "wal lets").unwrap_err().needed(),
        "com.hashinbox.wal%20lets:read"
    );
    assert!(bad(&all, "wallets").is_ok());
}
