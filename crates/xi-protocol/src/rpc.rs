//! JSON-RPC 2.0 envelope and typed method traits.
//!
//! Each protocol method is a marker type implementing [`RequestMethod`] or
//! [`NotificationMethod`], which ties the method name to its parameter and
//! result types. The envelope itself stays untyped so a peer can route a
//! message before knowing its method.

use std::fmt;

use serde::de::{self, DeserializeOwned, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A method that expects a response.
pub trait RequestMethod {
    /// Wire name, for example `session/prompt`.
    const METHOD: &'static str;
    /// Parameters. `()` is sent as an absent `params` field.
    type Params: Serialize + DeserializeOwned;
    /// Successful result. `()` is sent as `null`.
    type Result: Serialize + DeserializeOwned;
}

/// A method that expects no response.
pub trait NotificationMethod {
    /// Wire name, for example `event/appended`.
    const METHOD: &'static str;
    /// Parameters.
    type Params: Serialize + DeserializeOwned;
}

/// The `"jsonrpc": "2.0"` marker. Any other value fails to deserialize.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Version;

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value == "2.0" {
            Ok(Self)
        } else {
            Err(de::Error::invalid_value(
                de::Unexpected::Str(&value),
                &"\"2.0\"",
            ))
        }
    }
}

/// A request id chosen by the sender.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric id.
    Number(i64),
    /// String id.
    String(String),
}

/// Any JSON-RPC message.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Message {
    /// A call expecting a response.
    Request(Request),
    /// The answer to a request.
    Response(Response),
    /// A call expecting no response.
    Notification(Notification),
}

impl<'de> Deserialize<'de> for Message {
    /// Classifies by field presence (`method` and `id`) before decoding, so
    /// errors name the actual problem instead of "no variant matched".
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| de::Error::custom("JSON-RPC message must be an object"))?;
        let message = match (object.contains_key("method"), object.contains_key("id")) {
            (true, true) => serde_json::from_value(value).map(Message::Request),
            (true, false) => serde_json::from_value(value).map(Message::Notification),
            (false, _) => serde_json::from_value(value).map(Message::Response),
        };
        message.map_err(de::Error::custom)
    }
}

/// A call expecting a response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Protocol marker.
    pub jsonrpc: Version,
    /// Id echoed in the response.
    pub id: RequestId,
    /// Method name.
    pub method: String,
    /// Method parameters.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl Request {
    /// Builds a request for method `M`.
    ///
    /// # Errors
    /// Fails when the parameters cannot be serialized.
    pub fn new<M: RequestMethod>(id: RequestId, params: &M::Params) -> serde_json::Result<Self> {
        Ok(Self {
            jsonrpc: Version,
            id,
            method: M::METHOD.to_owned(),
            params: serde_json::to_value(params)?,
        })
    }

    /// Decodes the parameters if this request is for method `M`. Returns
    /// `None` for any other method.
    pub fn params<M: RequestMethod>(&self) -> Option<serde_json::Result<M::Params>> {
        (self.method == M::METHOD).then(|| M::Params::deserialize(&self.params))
    }
}

/// A call expecting no response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    /// Protocol marker.
    pub jsonrpc: Version,
    /// Method name.
    pub method: String,
    /// Method parameters.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl Notification {
    /// Builds a notification for method `M`.
    ///
    /// # Errors
    /// Fails when the parameters cannot be serialized.
    pub fn new<M: NotificationMethod>(params: &M::Params) -> serde_json::Result<Self> {
        Ok(Self {
            jsonrpc: Version,
            method: M::METHOD.to_owned(),
            params: serde_json::to_value(params)?,
        })
    }

    /// Decodes the parameters if this notification is for method `M`.
    /// Returns `None` for any other method.
    pub fn params<M: NotificationMethod>(&self) -> Option<serde_json::Result<M::Params>> {
        (self.method == M::METHOD).then(|| M::Params::deserialize(&self.params))
    }
}

/// The answer to a request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Protocol marker.
    pub jsonrpc: Version,
    /// The request's id; `null` when the request was too malformed to read one.
    pub id: Option<RequestId>,
    /// Either `result` or `error`.
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// The payload of a [`Response`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    /// Success, serialized as `"result"`.
    #[serde(rename = "result")]
    Ok(Value),
    /// Failure, serialized as `"error"`.
    #[serde(rename = "error")]
    Err(RpcError),
}

impl Response {
    /// Builds a success response for method `M`.
    ///
    /// # Errors
    /// Fails when the result cannot be serialized.
    pub fn ok<M: RequestMethod>(id: RequestId, result: &M::Result) -> serde_json::Result<Self> {
        Ok(Self {
            jsonrpc: Version,
            id: Some(id),
            outcome: Outcome::Ok(serde_json::to_value(result)?),
        })
    }

    /// Builds an error response.
    #[must_use]
    pub fn err(id: Option<RequestId>, error: RpcError) -> Self {
        Self {
            jsonrpc: Version,
            id,
            outcome: Outcome::Err(error),
        }
    }

    /// Decodes the outcome as the result of method `M`. The outer error is a
    /// decoding failure; the inner one is the peer's error response.
    ///
    /// # Errors
    /// Fails when a success result does not match `M::Result`.
    pub fn decode<M: RequestMethod>(self) -> serde_json::Result<Result<M::Result, RpcError>> {
        match self.outcome {
            Outcome::Ok(value) => serde_json::from_value(value).map(Ok),
            Outcome::Err(error) => Ok(Err(error)),
        }
    }
}

/// A JSON-RPC error object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    /// Error code; see the associated constants.
    pub code: i64,
    /// Short description.
    pub message: String,
    /// Structured detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// The message was not valid JSON.
    pub const PARSE_ERROR: i64 = -32700;
    /// The JSON was not a valid request.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method does not exist.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// The parameters did not match the method.
    pub const INVALID_PARAMS: i64 = -32602;
    /// The handler failed.
    pub const INTERNAL_ERROR: i64 = -32603;

    /// Builds an error without structured data.
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl std::error::Error for RpcError {}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    struct Echo;
    impl RequestMethod for Echo {
        const METHOD: &'static str = "test/echo";
        type Params = String;
        type Result = String;
    }

    #[test]
    fn messages_are_classified_by_fields() {
        let request: Message = serde_json::from_value(
            json!({"jsonrpc": "2.0", "id": 1, "method": "test/echo", "params": "hi"}),
        )
        .unwrap();
        let Message::Request(request) = request else {
            panic!("expected request, got {request:?}");
        };
        assert_eq!(request.params::<Echo>().unwrap().unwrap(), "hi");

        let notification: Message =
            serde_json::from_value(json!({"jsonrpc": "2.0", "method": "test/echo"})).unwrap();
        assert!(matches!(notification, Message::Notification(_)));

        let error: Message = serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "bad"}
        }))
        .unwrap();
        let Message::Response(error) = error else {
            panic!("expected response, got {error:?}");
        };
        assert_eq!(error.id, None);
        assert_eq!(
            error.decode::<Echo>().unwrap().unwrap_err().code,
            RpcError::PARSE_ERROR
        );
    }

    #[test]
    fn response_wire_shape_has_result_at_top_level() {
        let response = Response::ok::<Echo>(RequestId::String("a".into()), &"hi".into()).unwrap();
        assert_eq!(
            serde_json::to_value(&response).unwrap(),
            json!({"jsonrpc": "2.0", "id": "a", "result": "hi"})
        );
    }

    #[test]
    fn rejects_other_jsonrpc_versions() {
        let result =
            serde_json::from_value::<Message>(json!({"jsonrpc": "1.0", "id": 1, "method": "x"}));
        assert!(result.is_err());
    }

    #[test]
    fn response_without_result_or_error_is_rejected() {
        let result = serde_json::from_value::<Message>(json!({"jsonrpc": "2.0", "id": 1}));
        assert!(result.is_err());
    }
}
