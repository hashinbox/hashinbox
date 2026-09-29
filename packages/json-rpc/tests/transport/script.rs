//! The scripted node (`json_rpc::testing`) is a transport too, and the
//! script is the expectation: order, exact parameters, exhaustion, and the
//! method a chain pins to exactly one execution.

use json_rpc::{
    Batch, Call, Client, ErrorKind, Repetition, Request,
    testing::{Reply, Script},
};
use serde_json::json;

fn drive<T>(future: impl Future<Output = T>) -> T {
    futures_executor::block_on(future)
}

#[test]
fn replies_in_order_and_records_every_call() {
    let script = Script::new([
        Reply::ok("first", json!(2)).expecting(json!([1])),
        Reply::failure("second", -32_601, "provider prose"),
        Reply::raw("third", b"[3]".to_vec()),
    ]);

    let first = drive(script.request(Request::new("first", json!([1]))))
        .expect("scripted")
        .expect("a result");
    assert_eq!(first.deserialize::<u8>().expect("a number"), 2);

    let failure = drive(script.request(Request::new("second", json!({}))))
        .expect("scripted")
        .expect_err("a server error object");
    assert_eq!(failure.code, -32_601);

    let batch = Batch::new(vec![Call::new("third", json!([]))]).expect("one call");
    let third = drive(script.batch(batch)).expect("scripted");
    assert_eq!(third[0].as_ref().expect("a result").as_bytes(), b"[3]");

    script.exhausted();
    assert_eq!(script.methods(), ["first", "second", "third"]);
    assert_eq!(
        script.requests()[0],
        ("first".to_owned(), json!([1])),
        "the recorded call carries its parameters"
    );
}

#[test]
fn a_clone_shares_one_script_and_a_push_extends_it() {
    let script = Script::new([]);
    let handle = script.clone();
    script.push([Reply::ok("late", json!(null))]);
    drive(handle.request(Request::new("late", json!([]))))
        .expect("the clone sees what the original scripted")
        .expect("a result");
    handle.exhausted();
    script.exhausted();
}

#[test]
#[should_panic(expected = "unexpected RPC extra")]
fn a_call_the_script_does_not_expect_fails_the_test() {
    let script = Script::new([]);
    let _ = drive(script.request(Request::new("extra", json!([]))));
}

#[test]
#[should_panic(expected = "RPC method order")]
fn a_call_out_of_order_fails_the_test() {
    let script = Script::new([Reply::ok("first", json!(1)), Reply::ok("second", json!(2))]);
    let _ = drive(script.request(Request::new("second", json!([]))));
}

#[test]
#[should_panic(expected = "getblock parameters")]
fn parameters_that_differ_from_the_scripted_ones_fail_the_test() {
    let script = Script::new([Reply::ok("getblock", json!(1)).expecting(json!(["a"]))]);
    let _ = drive(script.request(Request::new("getblock", json!(["b"]))));
}

#[test]
#[should_panic(expected = "missing RPC")]
fn an_unconsumed_reply_fails_the_test() {
    Script::new([Reply::ok("never", json!(null))]).exhausted();
}

#[test]
fn a_pinned_method_accepts_a_request_that_may_not_repeat() {
    let script = Script::new([Reply::ok("submit", json!("id"))]).once_only("submit");
    drive(script.request(Request::once("submit", json!([]))))
        .expect("scripted")
        .expect("a result");
    script.exhausted();
}

#[test]
#[should_panic(expected = "submit must be once-only")]
fn a_pinned_method_rejects_a_repeatable_request() {
    let script = Script::new([Reply::ok("submit", json!("id"))]).once_only("submit");
    let _ = drive(script.request(Request::new("submit", json!([]))));
}

#[test]
fn an_unpinned_method_may_repeat_and_a_transport_failure_is_scriptable() {
    let error = json_rpc::Error {
        kind: ErrorKind::Timeout,
        message: "timed out".to_owned(),
    };
    let script = Script::new([Reply::result("read", Err(error.clone()))]).once_only("submit");
    let request = Request::new("read", json!([]));
    assert_eq!(request.repetition(), Repetition::Permitted);
    assert_eq!(
        drive(script.request(request)).expect_err("the scripted transport failure"),
        error
    );
    script.exhausted();
}
