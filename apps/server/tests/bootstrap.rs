//! `--config` (`usecase::bootstrap`): inline JSON or a file seeds an empty
//! database with the root user and settings, the passphrase then signing
//! in; a second start changes nothing; every bad config stops the start,
//! naming the field, never echoing the password or the words, and writes
//! nothing.

use crypto::mnemonic::Mnemonic;
use serde_json::{Value, json};

use super::support::db::fresh;
use crate::{
    auth,
    db::{Db, Page},
    models::{setting, user},
    usecase::{
        bootstrap::bootstrap,
        sessions::{self, Credential, Login},
    },
};

const EMAIL: &str = "ops@shop.io";
const PASSWORD: &str = "a secret password";

/// A preset of `EMAIL`, `PASSWORD`, `instance` and `words`.
fn preset(instance: &str, words: &str) -> Value {
    json!({
        "root": {"email": EMAIL, "name": "Ops", "password": PASSWORD},
        "settings": {"instance": instance, "recovery_passphrase": words},
    })
}

/// Whether `credential` signs in as `EMAIL`.
async fn signs_in(db: &Db, credential: Credential) -> bool {
    let login = Login {
        email: EMAIL.to_owned(),
        credential,
        totp: None,
        scopes: None,
        ttl: None,
    };
    sessions::login(db, login).await.is_ok()
}

/// The users kept and the `instance` setting.
async fn kept(db: &Db) -> (Option<user::User>, String, bool) {
    db.read(|connection| {
        Ok((
            user::db::find_by_email(connection, EMAIL)?,
            setting::instance(connection)?,
            setting::recovery_passphrase(connection)?.is_some(),
        ))
    })
    .await
    .expect("a read")
}

/// `config` seeds `db`: the root with every scope, `instance` "Shop", the
/// passphrase `words` and the password signing in.
async fn seeds(db: &Db, config: &str, words: &str) {
    assert!(bootstrap(db, Some(config)).await.expect("bootstrapped"));
    let (root, instance, recovery) = kept(db).await;
    let root = root.expect("the root user");
    assert_eq!(root.name, "Ops");
    assert_eq!(root.scopes, auth::everything());
    assert_eq!(instance, "Shop");
    assert!(recovery);
    assert!(signs_in(db, Credential::Password(PASSWORD.to_owned())).await);
    assert!(signs_in(db, Credential::Passphrase(words.to_owned())).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn inline_json_and_a_file_seed_the_root_and_settings() {
    let words = Mnemonic::generate().expect("words").to_string();
    let (db, _directory) = fresh().await;
    seeds(&db, &format!("  {}", preset("Shop", &words)), &words).await;

    let (db, directory) = fresh().await;
    let path = directory.path().join("bootstrap.json");
    std::fs::write(&path, preset("Shop", &words).to_string()).expect("written");
    seeds(&db, path.to_str().expect("a path"), &words).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_start_changes_nothing_and_no_config_nothing() {
    let words = Mnemonic::generate().expect("words").to_string();
    let (db, _directory) = fresh().await;
    assert!(!bootstrap(&db, None).await.expect("no config"));
    seeds(&db, &preset("Shop", &words).to_string(), &words).await;

    let other = Mnemonic::generate().expect("words").to_string();
    let mut changed = preset("Changed", &other);
    changed["root"]["password"] = json!("another password");
    for config in [changed.to_string(), "/no/such/file.json".to_owned()] {
        assert!(bootstrap(&db, Some(&config)).await.expect("skipped"));
    }
    let (_, instance, _) = kept(&db).await;
    assert_eq!(instance, "Shop");
    assert!(signs_in(&db, Credential::Passphrase(words)).await);
    assert!(!signs_in(&db, Credential::Passphrase(other)).await);
    let password = Credential::Password("another password".to_owned());
    assert!(!signs_in(&db, password).await);
    let page = Page {
        after: None,
        limit: 10,
    };
    let users = db.read(|connection| Ok(user::db::find_page(connection, page)?));
    assert_eq!(users.await.expect("a read").len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_config_stops_the_start_naming_the_field() {
    let words = Mnemonic::generate().expect("words").to_string();
    let good = preset("Shop", &words);
    let with = |pointer: &str, value: Value| {
        let mut config = good.clone();
        *config.pointer_mut(pointer).expect("a field") = value;
        config.to_string()
    };
    let mut unknown = good.clone();
    unknown["root"]["role"] = json!("admin");
    let mut top = good.clone();
    top["users"] = json!([]);
    let mut colour = good.clone();
    colour["settings"]["colour"] = json!("red");
    let abandon = ["abandon"; 12].join(" ");
    let cases = [
        ("/no/such/bootstrap.json".to_owned(), "config file"),
        (r#"{"root": {"email": "#.to_owned(), "config"),
        (unknown.to_string(), "`role`"),
        (top.to_string(), "`users`"),
        (with("/root/password", json!("seven77")), "password"),
        (with("/root/password", json!(12_345_678)), "root.password"),
        (with("/root/email", json!("nope")), "email"),
        (with("/settings/instance", json!("")), "settings.instance"),
        (with("/settings/instance", json!(5)), "settings.instance"),
        (colour.to_string(), "settings.colour"),
        (
            with("/settings/recovery_passphrase", json!(abandon)),
            "settings.recovery_passphrase",
        ),
        (
            with("/settings/recovery_passphrase", json!(5)),
            "settings.recovery_passphrase",
        ),
        (
            json!({"settings": {"instance": "Shop"}}).to_string(),
            "config root",
        ),
    ];
    for (config, field) in cases {
        let (db, _directory) = fresh().await;
        let error = bootstrap(&db, Some(&config)).await.err();
        let error = format!("{:#}", error.expect("refused"));
        assert!(error.contains(field), "{field}: {error}");
        for secret in [PASSWORD, &words, "seven77", "12345678", "abandon"] {
            assert!(!error.contains(secret), "{error}");
        }
        let (root, instance, recovery) = kept(&db).await;
        assert!(
            root.is_none() && instance == "hashinbox" && !recovery,
            "{error}"
        );
    }
}
