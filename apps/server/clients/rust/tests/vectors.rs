//! The shared vectors of `apps/server/clients/_golden/`: scopes, and key
//! sessions signed byte for byte as the server signs them.

use std::collections::BTreeMap;
use std::time::Duration;

use hashinbox_client::scope::escape_id;
use hashinbox_client::{Issue, Scope, Scopes, Secret, Session};
use serde_json::Value;

fn vectors(name: &str) -> Value {
    let path = format!("{}/../_golden/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
    serde_json::from_str(&text).unwrap()
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap()
}

fn scopes(value: &Value) -> Scopes {
    text(value).parse().unwrap()
}

#[test]
fn scope_vectors() {
    let all = vectors("scope.json");
    for case in all["parse"].as_array().unwrap() {
        let scope: Scope = text(&case["text"]).parse().unwrap();
        assert_eq!(scope.to_string(), text(&case["display"]), "{case}");
    }
    for case in all["refuse"].as_array().unwrap() {
        assert!(text(case).parse::<Scope>().is_err(), "{case}");
    }
    for case in all["covers"].as_array().unwrap() {
        let ours: Scope = text(&case["ours"]).parse().unwrap();
        let theirs: Scope = text(&case["theirs"]).parse().unwrap();
        assert_eq!(
            ours.covers(&theirs),
            case["covers"].as_bool().unwrap(),
            "{case}"
        );
    }
    for case in all["escape"].as_array().unwrap() {
        assert_eq!(escape_id(text(&case["id"])), text(&case["text"]), "{case}");
    }
    for case in all["first_uncovered"].as_array().unwrap() {
        let (granted, asked) = (scopes(&case["granted"]), scopes(&case["asked"]));
        let first = granted.first_uncovered(&asked).map(ToString::to_string);
        assert_eq!(first.as_deref(), case["first_uncovered"].as_str(), "{case}");
    }
    for case in all["sets"].as_array().unwrap() {
        let set = scopes(&case["scopes"]);
        assert_eq!(set.to_string(), text(&case["display"]), "{case}");
        assert_eq!(
            set.compress().to_string(),
            text(&case["compressed"]),
            "{case}"
        );
        let packed: BTreeMap<String, Vec<String>> =
            serde_json::from_value(case["packed"].clone()).unwrap();
        assert_eq!(set.pack(), packed, "{case}");
        assert_eq!(Scopes::unpack(&packed).unwrap(), set.compress(), "{case}");
    }
}

#[test]
fn key_sessions_are_signed_byte_for_byte() {
    let all = vectors("session.json");
    let secret = Secret::parse(text(&all["id"]), text(&all["secret"])).unwrap();
    for case in all["sessions"].as_array().unwrap() {
        let (iat, exp) = (case["iat"].as_i64().unwrap(), case["exp"].as_i64().unwrap());
        let ttl = Duration::from_secs((exp - iat).try_into().unwrap());
        let mut issue = Issue::new()
            .scopes(scopes(&case["scopes"]))
            .ttl(ttl)
            .issued_at(iat);
        if let Some(sub) = case["sub"].as_str() {
            issue = issue.subject(sub);
        }
        let session = secret.issue(issue).unwrap();
        assert_eq!(session.token, text(&case["token"]), "{case}");
        assert_eq!(session, Session::parse(text(&case["token"])).unwrap());
        assert_eq!((session.issued_at, session.expires_at), (iat, exp));
        assert_eq!(session.issuer, text(&all["id"]));
        assert_eq!(session.subject.as_deref(), case["sub"].as_str());
        assert_eq!(session.scopes, scopes(&case["scopes"]).compress());
    }
}

#[test]
fn a_server_made_session_parses() {
    let user = &vectors("session.json")["user"];
    let session = Session::parse(text(&user["token"])).unwrap();
    assert_eq!(session.issuer, text(&user["iss"]));
    assert_eq!(session.subject.as_deref(), user["sub"].as_str());
    assert_eq!(session.issued_at, user["iat"].as_i64().unwrap());
    assert_eq!(session.expires_at, user["exp"].as_i64().unwrap());
    assert_eq!(session.scopes, scopes(&user["scopes"]));
    let amr: Vec<String> = session.amr.iter().map(ToString::to_string).collect();
    assert_eq!(serde_json::json!(amr), user["amr"]);
    assert!(session.expired());
}

#[test]
fn a_secret_is_checked_and_a_session_lasts_at_most_a_day() {
    let all = vectors("session.json");
    let (id, value) = (text(&all["id"]), text(&all["secret"]));
    let secret = Secret::parse(id, value).unwrap();
    let typo = format!("{}{}", &value[..value.len() - 1], "3");
    assert!(Secret::parse(id, typo).is_err(), "a typo");
    assert!(Secret::parse("hip/0OIl", value).is_err(), "not base 58");
    assert!(Secret::parse(id, "his/short").is_err());
    assert!(
        !format!("{secret:?}").contains(&value[4..]),
        "Debug hides it"
    );
    let day = Duration::from_secs(24 * 60 * 60);
    assert!(secret.issue(Issue::new().ttl(day)).is_ok());
    assert!(
        secret
            .issue(Issue::new().ttl(day + Duration::from_secs(1)))
            .is_err()
    );
    assert!(secret.issue(Issue::new().ttl(Duration::ZERO)).is_err());
    let fresh = secret.issue(Issue::new()).unwrap();
    assert_eq!(fresh.expires_at - fresh.issued_at, 600, "10 minutes");
    assert!(!fresh.expired());
}

#[test]
fn a_session_can_an_operation_with_its_holes_filled() {
    use hashinbox_client::operations::{WALLET_GET, WALLETS_CREATE};
    let secret = vectors("session.json");
    let secret = Secret::parse(text(&secret["id"]), text(&secret["secret"])).unwrap();
    let issue = Issue::new()
        .scope("com.hashinbox.wallets[a b].$read")
        .unwrap();
    let session = secret.issue(issue).unwrap();
    assert!(session.can(&WALLET_GET, &[("id", "a b")]));
    assert!(!session.can(&WALLET_GET, &[("id", "c")]));
    assert!(!session.can(&WALLETS_CREATE, &[]));
}
