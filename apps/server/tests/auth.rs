//! Users, keys, sessions and scopes (design 096) through the router: the
//! root bootstrap, a login with two-factor, a key and the sessions it
//! signs (offline and here), the pay form's token reading one wallet,
//! revocation and invalidation; every non-public route's guard (401 with
//! no credential, 403 to a session with no scopes); a bad token refused on
//! a public route too; every OpenAPI scope
//! known to `HASHINBOX`; the settings written, checked and public in
//! `/v1/version`, the secret one never; the recovery passphrase created, replaced and removed,
//! signing in as any user (two-factor still asked); a new password ending
//! every other session.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header::AUTHORIZATION},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crypto::totp;
use http_body_util::BodyExt as _;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::support::{
    ROOT_EMAIL, ROOT_PASSWORD,
    db::seeded,
    scripted::{Script, container},
};
use crate::{
    api::{self, AppState},
    auth::HASHINBOX,
    models::user_key::KeySecret,
    models::{Client, Session, user_2fa},
};

/// The routes over the scripted network on a fresh file, no user yet; the
/// file and its directory.
async fn app() -> (Router, crate::db::Db, tempfile::TempDir) {
    let (_, db, directory) = seeded().await;
    let state = Arc::new(AppState {
        container: container(&db, Arc::new(Script::default())),
        db: db.clone(),
        queue: crate::usecase::tasks::queue(db.clone()),
        actions: Arc::new(crate::usecase::workers::trigger::actions::registry()),
        logs: crate::usecase::logs::feed(),
        candles: Default::default(),
        root_exists: Default::default(),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    (
        api::router(state, &config).expect("a router"),
        db,
        directory,
    )
}

/// `method uri` with `token` as the bearer and `body`: its status and JSON
/// body.
async fn call(
    router: &Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = body.map_or_else(Body::empty, |body| Body::from(body.to_string()));
    let response = router
        .clone()
        .oneshot(request.body(body).expect("a request"))
        .await
        .expect("a response");
    let status = response.status();
    let bytes = response.into_body().collect().await.expect("a body");
    let body = serde_json::from_slice(&bytes.to_bytes()).unwrap_or(Value::Null);
    (status, body)
}

/// The root user created on `router` (no user yet): its session.
async fn root(router: &Router) -> String {
    let body = json!({"email": ROOT_EMAIL, "name": "root", "password": ROOT_PASSWORD});
    let (status, created) = call(router, Method::POST, "/v1/users", None, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    created["token"].as_str().expect("a token").to_owned()
}

/// A login's body, with `extra` fields.
fn login(extra: Value) -> Value {
    let mut body = json!({"email": ROOT_EMAIL, "password": ROOT_PASSWORD});
    if let (Some(body), Value::Object(extra)) = (body.as_object_mut(), extra) {
        body.extend(extra);
    }
    body
}

/// `scopes` (space-separated) as a request carries them: packed.
fn packed(scopes: &str) -> Value {
    let scopes: scope::Scopes = scopes.parse().expect("scopes");
    json!(scopes.pack())
}

/// Now, Unix seconds.
fn now() -> i64 {
    crate::usecase::now().div_euclid(1000)
}

#[tokio::test(flavor = "multi_thread")]
async fn root_login_keys_pay_form_revocation_and_two_factor() {
    let (router, db, _directory) = app().await;

    // A fresh install: the root user, created with no credential.
    let (_, version) = call(&router, Method::GET, "/v1/version", None, None).await;
    assert_eq!(version["missing_root"], true);
    let root_session = root(&router).await;
    let (_, version) = call(&router, Method::GET, "/v1/version", None, None).await;
    assert!(version.get("missing_root").is_none(), "{version}");
    let again = json!({"email": "b@example.com", "name": "b", "password": "password b"});
    let (status, _) = call(&router, Method::POST, "/v1/users", None, Some(again)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a user exists");

    // A login: a wrong password and an unknown email answer the same.
    let wrong = login(json!({"password": "not it"}));
    let (status, wrong) = call(&router, Method::POST, "/v1/session", None, Some(wrong)).await;
    assert_eq!(
        (status, &wrong["reason"]),
        (StatusCode::UNAUTHORIZED, &json!("wrong_credentials"))
    );
    let unknown = login(json!({"email": "nobody@example.com"}));
    let (status, unknown) = call(&router, Method::POST, "/v1/session", None, Some(unknown)).await;
    assert_eq!((status, &unknown), (StatusCode::UNAUTHORIZED, &wrong));
    let (status, session) = call(
        &router,
        Method::POST,
        "/v1/session",
        None,
        Some(login(json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let user_session = session["token"].as_str().expect("a token").to_owned();
    let (status, user) = call(&router, Method::GET, "/v1/user", Some(&user_session), None).await;
    assert_eq!(
        (status, &user["user"]["email"]),
        (StatusCode::OK, &json!(ROOT_EMAIL))
    );
    assert!(user["user"].get("session_secret").is_none() && user["user"]["login"] == true);

    // Two wallets, and a key over every wallet.
    let mut wallets = Vec::new();
    for address in ["alice", "bob"] {
        let body = json!({"chain": "bitcoin", "network": "regtest", "address": address});
        let root = Some(root_session.as_str());
        let (status, created) = call(&router, Method::POST, "/v1/wallets", root, Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        wallets.push(created["id"].as_str().expect("an id").to_owned());
    }
    let (one, other) = (&wallets[0], &wallets[1]);
    let new_key = json!({"name": "shop", "scopes": packed("com.hashinbox.wallets.$*")});
    let (status, created) = call(
        &router,
        Method::POST,
        "/v1/user/keys",
        Some(&root_session),
        Some(new_key),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let secret_text = created["secret"].as_str().expect("a secret").to_owned();
    let key_id = created["key"]["id"].as_str().expect("an id").to_owned();
    assert!(secret_text.starts_with("his/") && key_id.starts_with("hip/"));
    let (_, listed) = call(
        &router,
        Method::GET,
        "/v1/user/keys",
        Some(&root_session),
        None,
    )
    .await;
    assert!(
        !listed.to_string().contains(&secret_text),
        "never the secret: {listed}"
    );

    // The key's secret as the bearer: its scopes.
    let (status, page) = call(
        &router,
        Method::GET,
        "/v1/wallets",
        Some(&secret_text),
        None,
    )
    .await;
    assert_eq!(
        (status, page["wallets"].as_array().map(Vec::len)),
        (StatusCode::OK, Some(2))
    );
    let (status, denied) = call(
        &router,
        Method::GET,
        "/v1/networks",
        Some(&secret_text),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["kind"], "permission_denied");
    assert_eq!(denied["reason"], "missing_scope");
    assert_eq!(denied["metadata"]["scope"], "com.hashinbox.networks.$read");

    // The pay form's token, signed offline by the key's secret (HS256, the
    // `his/…` text as the key): one wallet, read.
    let secret = secret_text.as_bytes();
    let read_one = format!("com.hashinbox.wallets[{one}].$read");
    let client = Client::Key(key_id.clone());
    let session = Session::new(
        client.clone(),
        Some("customer-1".into()),
        read_one.parse().unwrap(),
        now(),
        600,
    );
    let pay_form = session.to_jwt(secret);
    let read = |id: &str| format!("/v1/wallets/{id}");
    let (status, _) = call(&router, Method::GET, &read(one), Some(&pay_form), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &router,
        Method::GET,
        &format!("{}?token={pay_form}", read(one)),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "`?token=` on a GET");
    let (status, _) = call(&router, Method::GET, &read(other), Some(&pay_form), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&router, Method::GET, "/v1/wallets", Some(&pay_form), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a list needs every wallet");
    let index = json!({"asset": "native"});
    let uri = format!("{}/indexes", read(one));
    let (status, _) = call(&router, Method::POST, &uri, Some(&pay_form), Some(index)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "read only");

    // A third party's token, signed as the Node.js example does:
    // `jwt.sign({ sub, scopes }, "his/…", { algorithm: "HS256", keyid:
    // "hip/…", issuer: "hip/…", expiresIn: "15m" })`.
    let header = Header {
        kid: Some(key_id.clone()),
        ..Header::new(Algorithm::HS256)
    };
    let claims = json!({
        "sub": "customer-3",
        "scopes": packed(&read_one),
        "iat": now(),
        "exp": now() + 900,
        "iss": key_id,
    });
    let third_party =
        jsonwebtoken::encode(&header, &claims, &EncodingKey::from_secret(secret)).unwrap();
    let (status, _) = call(&router, Method::GET, &read(one), Some(&third_party), None).await;
    assert_eq!(status, StatusCode::OK, "a third party's HS256");

    // Refused: `alg: none`, an `EdDSA` header, another key's secret.
    let part = |value: Value| URL_SAFE_NO_PAD.encode(value.to_string());
    let claims_part = part(claims.clone());
    let none = part(json!({"alg": "none", "typ": "JWT", "kid": key_id}));
    let eddsa = part(json!({"alg": "EdDSA", "typ": "JWT", "kid": key_id}));
    let signature = third_party.rsplit_once('.').expect("a signature").1;
    let other_secret = KeySecret::generate().unwrap();
    let other_key = EncodingKey::from_secret(other_secret.reveal().as_bytes());
    for (refused, what) in [
        (format!("{none}.{claims_part}."), "alg: none"),
        (format!("{eddsa}.{claims_part}.{signature}"), "EdDSA"),
        (
            jsonwebtoken::encode(&header, &claims, &other_key).unwrap(),
            "another key's secret",
        ),
    ] {
        let (status, error) = call(&router, Method::GET, &read(one), Some(&refused), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{what}");
        assert!(
            error.get("reason").is_none(),
            "{what}: not genuine, {error}"
        );
    }

    // The same token signed here, for a caller that cannot sign.
    let body = json!({"sub": "customer-2", "scopes": packed(&read_one), "ttl": 600});
    let (status, minted) = call(
        &router,
        Method::POST,
        "/v1/session",
        Some(&secret_text),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{minted}");
    let online = minted["token"].as_str().expect("a token").to_owned();
    let (status, _) = call(&router, Method::GET, &read(one), Some(&online), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, decoded) = call(&router, Method::GET, "/v1/session", Some(&online), None).await;
    assert_eq!(
        (status, &decoded["kind"], &decoded["sub"]),
        (StatusCode::OK, &json!("key"), &json!("customer-2"))
    );

    // Narrowing only: wider than the key is refused, issued or forged.
    let wider = json!({"scopes": packed("com.hashinbox.networks.$read")});
    let (status, _) = call(
        &router,
        Method::POST,
        "/v1/session",
        Some(&secret_text),
        Some(wider),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let forged = Session::new(
        client.clone(),
        None,
        "com.hashinbox.*.$*".parse().unwrap(),
        now(),
        600,
    );
    let forged = forged.to_jwt(secret);
    let (status, _) = call(&router, Method::GET, "/v1/networks", Some(&forged), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "wider than its key");
    let unsigned = session.to_jwt(other_secret.reveal().as_bytes());
    let (status, _) = call(&router, Method::GET, &read(one), Some(&unsigned), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "not its key's signature");
    let expired = Session::new(
        client.clone(),
        None,
        read_one.parse().unwrap(),
        now() - 120,
        60,
    );
    let expired = expired.to_jwt(secret);
    let (status, error) = call(&router, Method::GET, &read(one), Some(&expired), None).await;
    assert_eq!(
        (status, &error["reason"]),
        (StatusCode::UNAUTHORIZED, &json!("session_expired")),
        "expired"
    );

    // Rotating the key refuses what it signed; the key keeps working.
    let rotate = format!("/v1/user/keys/{}/rotate", key_id.trim_start_matches("hip/"));
    let (status, _) = call(&router, Method::POST, &rotate, Some(&root_session), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, error) = call(&router, Method::GET, &read(one), Some(&pay_form), None).await;
    assert_eq!(
        (status, &error["reason"]),
        (StatusCode::UNAUTHORIZED, &json!("session_expired")),
        "invalidated"
    );
    let (status, _) = call(&router, Method::GET, &read(one), Some(&secret_text), None).await;
    assert_eq!(status, StatusCode::OK);

    // Revoking it refuses the key itself.
    let revoke = format!("/v1/user/keys/{}", key_id.trim_start_matches("hip/"));
    let (status, _) = call(&router, Method::DELETE, &revoke, Some(&root_session), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&router, Method::GET, &read(one), Some(&secret_text), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "revoked");

    // Logging out everywhere refuses every session the user issued.
    let (status, _) = call(
        &router,
        Method::DELETE,
        "/v1/session?all=true",
        Some(&user_session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, error) = call(&router, Method::GET, "/v1/user", Some(&user_session), None).await;
    assert_eq!(
        (status, &error["reason"]),
        (StatusCode::UNAUTHORIZED, &json!("session_expired")),
        "logged out everywhere"
    );
    let (status, _) = call(&router, Method::GET, "/v1/user", Some(&root_session), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the root session too");

    // Two-factor: on once confirmed; then a login needs a fresh code.
    // The sessions above are refused until the next second.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (_, session) = call(
        &router,
        Method::POST,
        "/v1/session",
        None,
        Some(login(json!({}))),
    )
    .await;
    let session = session["token"].as_str().expect("a token").to_owned();
    let (status, started) = call(&router, Method::POST, "/v1/user/2fa", Some(&session), None).await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    assert!(
        started["uri"]
            .as_str()
            .is_some_and(|uri| uri.starts_with("otpauth://totp/"))
    );
    let id = user["user"]["id"].as_str().expect("an id").to_owned();
    let row = db
        .read(move |connection| Ok(user_2fa::db::find(connection, &id)?))
        .await
        .unwrap()
        .expect("a two-factor row");
    let step = now() / totp::STEP;
    let confirm = json!({"code": totp::code(&row.secret, step)});
    let (status, _) = call(
        &router,
        Method::POST,
        "/v1/user/2fa/confirm",
        Some(&session),
        Some(confirm.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, required) = call(
        &router,
        Method::POST,
        "/v1/session",
        None,
        Some(login(json!({}))),
    )
    .await;
    assert_eq!(
        (status, &required["kind"], &required["reason"]),
        (
            StatusCode::UNAUTHORIZED,
            &json!("unauthenticated"),
            &json!("totp_required")
        )
    );
    let replayed = login(json!({"totp": confirm["code"]}));
    let (status, replayed) = call(&router, Method::POST, "/v1/session", None, Some(replayed)).await;
    assert_eq!(
        (status, &replayed),
        (StatusCode::UNAUTHORIZED, &wrong),
        "a code once"
    );
    let fresh = login(json!({"totp": totp::code(&row.secret, step + 1)}));
    let (status, _) = call(&router, Method::POST, "/v1/session", None, Some(fresh)).await;
    assert_eq!(status, StatusCode::CREATED);
}

/// `method uri` as `token`'s, expecting `status`: the body.
async fn expect(
    router: &Router,
    (method, uri): (Method, &str),
    token: Option<&str>,
    body: Option<Value>,
    status: StatusCode,
) -> Value {
    let (got, body) = call(router, method.clone(), uri, token, body).await;
    assert_eq!(got, status, "{method} {uri}: {body}");
    body
}

/// The session `body` logs in with (expecting 201): its token and its
/// `amr`, as `GET /v1/session` reads it back.
async fn session_of(router: &Router, body: Value) -> (String, Value) {
    let created = expect(
        router,
        (Method::POST, "/v1/session"),
        None,
        Some(body),
        StatusCode::CREATED,
    );
    let token = created.await["token"].as_str().expect("a token").to_owned();
    let read = expect(
        router,
        (Method::GET, "/v1/session"),
        Some(&token),
        None,
        StatusCode::OK,
    );
    let amr = read.await["amr"].clone();
    (token, amr)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_recovery_passphrase_its_logins_and_a_new_password() {
    let (router, db, _directory) = app().await;
    let root_session = root(&router).await;
    let root = Some(root_session.as_str());
    let version = (Method::GET, "/v1/version");
    let setup = expect(&router, version, None, None, StatusCode::OK).await;
    assert!(setup.get("missing_recovery").is_none(), "{setup}");
    let status = (Method::GET, "/v1/recovery");
    let unset = expect(&router, status.clone(), root, None, StatusCode::OK).await;
    assert_eq!(unset, json!({"set": false, "created_at": null}));

    // The secret setting: listed as set or not, never its value; never
    // written through the API.
    let settings = (Method::GET, "/v1/settings");
    let secret_of = |listed: &Value| listed["settings"][1].clone();
    let listed = expect(&router, settings.clone(), root, None, StatusCode::OK).await;
    let secret = secret_of(&listed);
    assert_eq!(
        (&secret["key"], &secret["set"]),
        (&json!("recovery_passphrase"), &json!(false))
    );
    assert!(
        secret.get("value").is_none() && secret.get("default").is_none(),
        "{secret}"
    );

    // Created, then replaced: the old words and their sessions end.
    let recovery = (Method::POST, "/v1/recovery");
    let created = expect(&router, recovery.clone(), root, None, StatusCode::CREATED).await;
    let old = created["passphrase"].as_str().expect("words").to_owned();
    assert_eq!(old.split(' ').count(), 12, "{old}");
    let by_old = json!({"email": ROOT_EMAIL, "passphrase": old});
    let (old_session, _) = session_of(&router, by_old.clone()).await;
    let replaced = expect(&router, recovery.clone(), root, None, StatusCode::CREATED).await;
    let words = replaced["passphrase"].as_str().expect("words").to_owned();
    assert_ne!(words, old);
    let read = (Method::GET, "/v1/user");
    let unauthorized = StatusCode::UNAUTHORIZED;
    let ended = expect(
        &router,
        read.clone(),
        Some(&old_session),
        None,
        unauthorized,
    )
    .await;
    assert_eq!(ended["reason"], json!("session_expired"));
    let login_at = (Method::POST, "/v1/session");
    let refused = expect(&router, login_at.clone(), None, Some(by_old), unauthorized).await;
    assert_eq!(refused["reason"], json!("wrong_credentials"));
    expect(&router, read.clone(), root, None, StatusCode::OK).await;
    let shown = expect(&router, status.clone(), root, None, StatusCode::OK).await;
    assert_eq!(shown["set"], json!(true));
    assert!(
        shown["created_at"].as_i64().is_some_and(|at| at > 0),
        "{shown}"
    );
    let keys: Vec<&String> = shown.as_object().expect("an object").keys().collect();
    assert_eq!(keys, ["created_at", "set"], "never the words: {shown}");

    // Removed: passphrase logins refused like wrong words, their sessions
    // ended; then created again.
    let by_new = json!({"email": ROOT_EMAIL, "passphrase": words});
    let (new_session, _) = session_of(&router, by_new.clone()).await;
    let remove = (Method::DELETE, "/v1/recovery");
    expect(&router, remove, root, None, StatusCode::NO_CONTENT).await;
    let ended = expect(&router, read, Some(&new_session), None, unauthorized).await;
    assert_eq!(ended["reason"], json!("session_expired"));
    let off = expect(&router, login_at, None, Some(by_new), unauthorized).await;
    assert_eq!(off["reason"], json!("wrong_credentials"));
    let unset = expect(&router, status, root, None, StatusCode::OK).await;
    assert_eq!(unset, json!({"set": false, "created_at": null}));
    let created = expect(&router, recovery, root, None, StatusCode::CREATED).await;
    let words = created["passphrase"].as_str().expect("words").to_owned();
    let listed = expect(&router, settings, root, None, StatusCode::OK).await;
    assert_eq!(secret_of(&listed)["set"], json!(true));
    assert!(
        !listed.to_string().contains("$argon2id$"),
        "never the hash: {listed}"
    );
    let put = (Method::PUT, "/v1/settings/recovery_passphrase");
    let value = Some(json!({"value": "$argon2id$v=19$forged"}));
    let refused = expect(&router, put, root, value, StatusCode::FORBIDDEN).await;
    assert_eq!(refused["kind"], json!("permission_denied"));

    // It signs in as any user, typed any case and spacing.
    let other = json!({"email": "b@example.com", "name": "b", "password": "password b"});
    expect(
        &router,
        (Method::POST, "/v1/users"),
        root,
        Some(other),
        StatusCode::CREATED,
    )
    .await;
    let b = json!({"email": "b@example.com", "password": "password b"});
    let (b_session, _) = session_of(&router, b).await;
    for method in [Method::GET, Method::POST, Method::DELETE] {
        let route = (method, "/v1/recovery");
        let denied = expect(
            &router,
            route,
            Some(&b_session),
            None,
            StatusCode::FORBIDDEN,
        )
        .await;
        assert_eq!(denied["reason"], json!("missing_scope"), "not the root");
    }
    let typed = format!(
        "  {}  ",
        words.to_uppercase().replace(
            ' ', "
 "
        )
    );
    let by_words = json!({"email": "b@example.com", "passphrase": typed});
    let (_, amr) = session_of(&router, by_words).await;
    assert_eq!(amr, json!(["passphrase"]));
    let (_, amr) = session_of(&router, login(json!({}))).await;
    assert_eq!(amr, json!(["pwd"]));

    // Wrong words, unknown email, both credentials: refused alike.
    let wrong = login(json!({"password": "not it"}));
    let session = (Method::POST, "/v1/session");
    let wrong = expect(&router, session.clone(), None, Some(wrong), unauthorized).await;
    assert_eq!(wrong["reason"], json!("wrong_credentials"));
    let valid_other = "legal winner thank year wave sausage worth useful legal winner thank yellow";
    for (email, passphrase) in [
        (ROOT_EMAIL, valid_other),
        (ROOT_EMAIL, "not twelve words"),
        ("nobody@example.com", words.as_str()),
    ] {
        let body = json!({"email": email, "passphrase": passphrase});
        let refused = expect(&router, session.clone(), None, Some(body), unauthorized).await;
        assert_eq!(refused, wrong, "{email} {passphrase}");
    }
    let both = login(json!({"passphrase": words}));
    expect(
        &router,
        session.clone(),
        None,
        Some(both),
        StatusCode::BAD_REQUEST,
    )
    .await;

    // Two-factor still applies to it.
    let two_factor = (Method::POST, "/v1/user/2fa");
    expect(&router, two_factor, root, None, StatusCode::CREATED).await;
    let user = expect(
        &router,
        (Method::GET, "/v1/user"),
        root,
        None,
        StatusCode::OK,
    )
    .await;
    let id = user["user"]["id"].as_str().expect("an id").to_owned();
    let row = db
        .read(move |connection| Ok(user_2fa::db::find(connection, &id)?))
        .await
        .unwrap()
        .expect("a two-factor row");
    let step = now() / totp::STEP;
    let confirm = Some(json!({"code": totp::code(&row.secret, step)}));
    let confirmed = (Method::POST, "/v1/user/2fa/confirm");
    expect(&router, confirmed, root, confirm, StatusCode::NO_CONTENT).await;
    let by_words = json!({"email": ROOT_EMAIL, "passphrase": words});
    let required = expect(&router, session, None, Some(by_words.clone()), unauthorized).await;
    assert_eq!(required["reason"], json!("totp_required"));
    let mut coded = by_words;
    coded["totp"] = json!(totp::code(&row.secret, step + 1));
    let (recovered, amr) = session_of(&router, coded).await;
    assert_eq!(amr, json!(["passphrase", "otp"]));

    // A new password, the current one not asked: every other session ends,
    // the answer is a fresh one.
    let update = (Method::PUT, "/v1/user");
    let short = Some(json!({"password": "short"}));
    let recovered = Some(recovered.as_str());
    expect(
        &router,
        update.clone(),
        recovered,
        short,
        StatusCode::BAD_REQUEST,
    )
    .await;
    let new = Some(json!({"password": "a new password"}));
    let changed = expect(&router, update, recovered, new, StatusCode::OK).await;
    let changed = changed["token"].as_str().expect("a token").to_owned();
    let read = (Method::GET, "/v1/user");
    for ended in [root, recovered] {
        let error = expect(&router, read.clone(), ended, None, unauthorized).await;
        assert_eq!(error["reason"], json!("session_expired"));
    }
    expect(&router, read, Some(&changed), None, StatusCode::OK).await;
    let session = (Method::POST, "/v1/session");
    let old = expect(
        &router,
        session.clone(),
        None,
        Some(login(json!({}))),
        unauthorized,
    )
    .await;
    assert_eq!(old["reason"], json!("wrong_credentials"));
    let new = login(json!({"password": "a new password"}));
    let right = expect(&router, session, None, Some(new), unauthorized).await;
    assert_eq!(
        right["reason"],
        json!("totp_required"),
        "the new password is right"
    );
}

/// The routes anyone may call.
const PUBLIC: [(&str, &str); 5] = [
    ("get", "/health"),
    ("get", "/openapi.json"),
    ("get", "/v1/version"),
    ("post", "/v1/session"),
    ("post", "/v1/users"),
];

/// Every operation of the OpenAPI document: its method, path and
/// security scopes (`None`: no `security`).
fn operations() -> Vec<(String, String, Option<Vec<String>>)> {
    let document = serde_json::to_value(api::openapi()).expect("the document");
    let paths = document["paths"].as_object().cloned().unwrap_or_default();
    let mut operations = Vec::new();
    for (path, item) in paths {
        for (method, operation) in item.as_object().cloned().unwrap_or_default() {
            let security = operation["security"].as_array().map(|security| {
                let scopes = security.iter().filter_map(|each| each["bearer"].as_array());
                let scopes = scopes.flatten().filter_map(Value::as_str);
                scopes.map(str::to_owned).collect()
            });
            operations.push((method, path.clone(), security));
        }
    }
    operations
}

#[tokio::test(flavor = "multi_thread")]
async fn every_route_but_the_public_ones_is_guarded() {
    let (router, _db, _directory) = app().await;
    root(&router).await;
    let body = login(json!({"scopes": {}}));
    let (status, none) = call(&router, Method::POST, "/v1/session", None, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{none}");
    let none = none["token"].as_str().expect("a token").to_owned();
    let mut guarded = 0;
    for (method, path, security) in operations() {
        // Every path parameter `1`, which each of them parses.
        let steps = path.split('/');
        let steps: Vec<&str> = steps
            .map(|step| if step.starts_with('{') { "1" } else { step })
            .collect();
        let uri = steps.join("/");
        let method_of = Method::from_bytes(method.to_uppercase().as_bytes()).expect("a method");
        let public = PUBLIC.contains(&(method.as_str(), path.as_str()));
        if public && path != "/v1/users" {
            continue;
        }
        let security = security.unwrap_or_else(|| panic!("{method} {path}: no security"));
        let (status, body) = call(&router, method_of.clone(), &uri, None, None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} unauthenticated: {body}"
        );
        let (status, body) = call(&router, method_of, &uri, Some(&none), None).await;
        if security.is_empty() {
            assert!(
                ![StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN].contains(&status),
                "{method} {path}: any session: {status} {body}"
            );
        } else {
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{method} {path} without scopes: {body}"
            );
        }
        guarded += 1;
    }
    assert!(guarded > 40, "{guarded} guarded routes");
    let paths: Vec<(String, String)> = operations()
        .into_iter()
        .map(|(method, path, _)| (method, path))
        .collect();
    for new in [
        ("get", "/v1/recovery"),
        ("post", "/v1/recovery"),
        ("delete", "/v1/recovery"),
        ("put", "/v1/user"),
    ] {
        let new = (new.0.to_owned(), new.1.to_owned());
        assert!(paths.contains(&new), "{new:?} is guarded");
    }
    assert!(
        !paths.iter().any(|(_, path)| path == "/v1/user/password"),
        "removed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_public_route_refuses_a_bad_token_and_answers_without_one() {
    let (router, _db, _directory) = app().await;
    let (status, body) = call(&router, Method::GET, "/v1/version", None, None).await;
    assert_eq!(status, StatusCode::OK, "no token: {body}");
    for (method, path) in PUBLIC {
        let method = Method::from_bytes(method.to_uppercase().as_bytes()).expect("a method");
        let (status, body) = call(&router, method, path, Some("not-a-token"), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}: {body}");
    }
}

/// `template` (a `require!`'s) with each `{placeholder}` a sample id.
fn sample(template: &str) -> String {
    let mut filled = String::new();
    let mut rest = template;
    while let Some((head, tail)) = rest.split_once('{') {
        filled.push_str(head);
        rest = tail.split_once('}').map_or("", |(_, after)| after);
        filled.push_str("ethereum.mainnet.0xabc");
    }
    filled + rest
}

#[test]
fn every_openapi_scope_is_known() {
    for (method, path, security) in operations() {
        for scope in security.unwrap_or_default() {
            let concrete = sample(&scope);
            let parsed: scope::Scope = concrete
                .parse()
                .unwrap_or_else(|error| panic!("{scope}: {error}"));
            assert!(
                HASHINBOX.knows(&parsed),
                "{method} {path}: {scope} is not known"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_setting_round_trips_is_checked_and_shows_in_the_version() {
    let (router, _db, _directory) = app().await;
    let (status, version) = call(&router, Method::GET, "/v1/version", None, None).await;
    assert_eq!(status, StatusCode::OK, "{version}");
    assert_eq!(version["missing_root"], json!(true));
    assert_eq!(version["settings"], json!({"instance": "hashinbox"}));
    let token = root(&router).await;
    let token = Some(token.as_str());

    let (status, listed) = call(&router, Method::GET, "/v1/settings", token, None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let instance = &listed["settings"][0];
    assert_eq!(instance["key"], json!("instance"));
    assert_eq!(instance["value"], json!("hashinbox"));
    assert_eq!(instance["default"], json!("hashinbox"));
    assert_eq!(instance["public"], json!(true));
    assert_eq!(instance["type"], json!("String"));
    assert!(
        instance["description"]
            .as_str()
            .is_some_and(|text| text.starts_with("The instance's name")),
        "{instance}"
    );

    let put = |value: Value| Some(json!({ "value": value }));
    let uri = "/v1/settings/instance";
    let (status, written) = call(&router, Method::PUT, uri, token, put(json!("acme"))).await;
    assert_eq!(status, StatusCode::OK, "{written}");
    assert_eq!(written["value"], json!("acme"));
    assert_eq!(written["default"], json!("hashinbox"));
    let (_, listed) = call(&router, Method::GET, "/v1/settings", token, None).await;
    assert_eq!(listed["settings"][0]["value"], json!("acme"));
    let (_, version) = call(&router, Method::GET, "/v1/version", None, None).await;
    assert_eq!(version["settings"], json!({"instance": "acme"}));
    assert_eq!(version.get("missing_root"), None, "{version}");

    for refused in [json!(""), json!("x".repeat(65)), json!(5)] {
        let (status, body) = call(&router, Method::PUT, uri, token, put(refused.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}: {body}");
    }
    let unknown = "/v1/settings/nope";
    let (status, body) = call(&router, Method::PUT, unknown, token, put(json!("a"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("no setting nope"), "{body}");
    let (_, listed) = call(&router, Method::GET, "/v1/settings", token, None).await;
    assert_eq!(
        listed["settings"][0]["value"],
        json!("acme"),
        "kept after refusals"
    );
}
