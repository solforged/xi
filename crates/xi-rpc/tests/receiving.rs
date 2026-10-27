//! Marker-directed receiving codecs and their JSON-RPC error codes.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use xi_protocol::SessionId;
use xi_protocol::methods::client::{SessionCancel, SessionRename};
use xi_protocol::methods::worker::ModelRelease;
use xi_protocol::rpc::{RequestMethod, RpcError};
use xi_rpc::{decode_notification, decode_request, encode_result};

#[test]
fn request_parameters_and_result_follow_the_marker() {
    let session = SessionId::generate();
    let params = decode_request::<SessionRename>(json!({
        "session": session,
        "title": "renamed",
    }))
    .unwrap();
    assert_eq!(params.session, session);
    assert_eq!(params.title, "renamed");
    assert_eq!(encode_result::<SessionRename>(()).unwrap(), Value::Null);
}

#[test]
fn invalid_request_parameters_keep_the_invalid_params_code() {
    for params in [Value::Null, json!({}), json!({ "session": 42 })] {
        let error = decode_request::<SessionCancel>(params).unwrap_err();
        assert_eq!(error.code, RpcError::INVALID_PARAMS);
        assert!(error.data.is_none());
    }
}

#[test]
fn notification_parameters_follow_the_marker() {
    let params = decode_notification::<ModelRelease>(json!({ "endpoint": "local" })).unwrap();
    assert_eq!(params.endpoint, "local");
    let error = decode_notification::<ModelRelease>(json!({ "endpoint": false })).unwrap_err();
    assert_eq!(error.code, RpcError::INVALID_PARAMS);
    assert!(error.data.is_none());
}

struct InvalidResult;

impl RequestMethod for InvalidResult {
    const METHOD: &'static str = "test/invalid_result";
    type Params = ();
    type Result = BTreeMap<(u32, u32), u32>;
}

#[test]
fn invalid_results_keep_the_internal_error_code() {
    let error = encode_result::<InvalidResult>(BTreeMap::from([((1, 2), 3)])).unwrap_err();
    assert_eq!(error.code, RpcError::INTERNAL_ERROR);
    assert!(error.data.is_none());
}
