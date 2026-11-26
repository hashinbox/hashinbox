//! A smoke test against a running server: skipped unless `HASHINBOX_URL`
//! is set; with `HASHINBOX_KEY_ID` and `HASHINBOX_SECRET` too, a key's
//! signed session is read back.
#![cfg(feature = "reqwest")]

use hashinbox_client::operations::{
    SessionGetRequest, VersionGetRequest, session_get, version_get,
};
use hashinbox_client::{Client, Secret};

#[tokio::test]
async fn a_live_server_answers() {
    let Ok(url) = std::env::var("HASHINBOX_URL") else {
        eprintln!("skipped: HASHINBOX_URL is not set");
        return;
    };
    let public = Client::builder(&url).build().unwrap();
    let version = version_get(&public, VersionGetRequest).await.unwrap();
    assert!(!version.version.is_empty());
    let (Ok(id), Ok(value)) = (
        std::env::var("HASHINBOX_KEY_ID"),
        std::env::var("HASHINBOX_SECRET"),
    ) else {
        return;
    };
    let secret = Secret::parse(id, value).unwrap();
    let client = Client::builder(url).secret(secret.clone()).build().unwrap();
    let read = session_get(&client, SessionGetRequest).await.unwrap();
    assert_eq!(read.key.map(|key| key.id).as_deref(), Some(secret.id()));
}
