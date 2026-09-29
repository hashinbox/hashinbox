//! The two failure shapes a caller sees: a transport [`Error`], which
//! produced no server answer, and a [`Failure`] the server returned as the
//! result of one call. [`CallError`] flattens both so one `?` reaches either.

use json_rpc::{CallError, CallResult, Error, ErrorKind, Failure, RawJson};

fn remote(code: i64) -> Failure {
    Failure {
        code,
        message: "provider secret detail".to_owned(),
        data: None,
    }
}

fn call(result: Result<CallResult, Error>) -> Result<RawJson, CallError> {
    Ok(result??)
}

#[test]
fn flattens_transport_and_remote_outcomes() {
    let value = RawJson::from_serializable(&1).expect("serializable");
    assert_eq!(call(Ok(Ok(value.clone()))), Ok(value));

    let transport = Error {
        kind: ErrorKind::Timeout,
        message: "timed out".to_owned(),
    };
    let local = call(Err(transport.clone())).expect_err("transport failure");
    assert_eq!(local, CallError::Transport(transport));
    assert_eq!(local.remote_code(), None);
    assert!(local.is_retryable());

    let failed = call(Ok(Err(remote(-32_601)))).expect_err("remote failure");
    assert_eq!(failed.remote_code(), Some(-32_601));
    assert!(!failed.is_retryable());
}

#[test]
fn remote_server_errors_are_retryable_and_display_hides_provider_text() {
    for (code, retryable) in [
        (-32_000, true),
        (-32_099, true),
        (-32_100, false),
        (-28, false),
    ] {
        let error = CallError::from(remote(code));
        assert_eq!(error.is_retryable(), retryable);
        assert_eq!(
            error.to_string(),
            format!("JSON-RPC call failed with code {code}")
        );
    }
    let invalid = CallError::from(Error {
        kind: ErrorKind::HttpStatus(400),
        message: "bad request".to_owned(),
    });
    assert!(!invalid.is_retryable());
    assert_eq!(invalid.to_string(), "bad request");
}
