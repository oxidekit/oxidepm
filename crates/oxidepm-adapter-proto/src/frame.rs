//! JSON-RPC 2.0 frames, one per line.
//!
//! Restrictions on top of JSON-RPC: no batches, `id` is an unsigned 64-bit integer, `params` and
//! `result` are objects, `"jsonrpc":"2.0"` is required, and an error response needs an id. Unknown
//! fields are ignored. Anything else is a protocol violation (a strike).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type Params = Map<String, Value>;

const MAX_METHOD_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FrameError {
    #[error("empty line")]
    Empty,
    #[error("line is not valid UTF-8")]
    InvalidUtf8,
    #[error("line contains a NUL byte")]
    Nul,
    #[error("line is not valid JSON: {0}")]
    NotJson(String),
    #[error("a frame must be a JSON object")]
    NotObject,
    #[error("batches are not supported")]
    Batch,
    #[error("\"jsonrpc\" must be the string \"2.0\"")]
    BadVersion,
    #[error("\"id\" must be an unsigned 64-bit integer")]
    BadId,
    #[error("\"method\" must be a short name of letters, digits, '.', '_' and '-'")]
    BadMethod,
    #[error("\"params\" must be an object")]
    BadParams,
    #[error("{0}")]
    BadShape(&'static str),
    #[error("the frame is {size} bytes; the limit is {max}")]
    TooLarge { size: usize, max: usize },
}

/// The `error` member of an error response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        ErrorObject {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// An error that carries the stable `E_...` name in `data.code`.
    pub fn with_name(code: i64, name: &str, message: impl Into<String>) -> Self {
        ErrorObject {
            code,
            message: message.into(),
            data: Some(serde_json::json!({ "code": name })),
        }
    }

    /// The stable name in `data.code`, if the peer sent one.
    pub fn name(&self) -> Option<&str> {
        self.data.as_ref()?.get("code")?.as_str()
    }
}

/// One message.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Request {
        id: u64,
        method: String,
        params: Params,
    },
    Notification {
        method: String,
        params: Params,
    },
    Response {
        id: u64,
        result: Params,
    },
    Error {
        id: u64,
        error: ErrorObject,
    },
}

/// Serialise a value that must become a `params` or `result` object.
pub fn to_params<T: Serialize>(value: &T) -> Result<Params, FrameError> {
    match serde_json::to_value(value) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(FrameError::BadParams),
    }
}

/// Read a `params` or `result` object into a typed value. Unknown fields are ignored.
pub fn from_params<T: DeserializeOwned>(params: &Params) -> Result<T, FrameError> {
    serde_json::from_value(Value::Object(params.clone()))
        .map_err(|e| FrameError::NotJson(e.to_string()))
}

fn valid_method(method: &str) -> bool {
    !method.is_empty()
        && method.len() <= MAX_METHOD_LEN
        && method
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl Frame {
    pub fn request<T: Serialize>(id: u64, method: &str, params: &T) -> Result<Frame, FrameError> {
        Ok(Frame::Request {
            id,
            method: method.to_string(),
            params: to_params(params)?,
        })
    }

    pub fn notification<T: Serialize>(method: &str, params: &T) -> Result<Frame, FrameError> {
        Ok(Frame::Notification {
            method: method.to_string(),
            params: to_params(params)?,
        })
    }

    pub fn response<T: Serialize>(id: u64, result: &T) -> Result<Frame, FrameError> {
        Ok(Frame::Response {
            id,
            result: to_params(result)?,
        })
    }

    pub fn error(id: u64, error: ErrorObject) -> Frame {
        Frame::Error { id, error }
    }

    /// The method, for a request or a notification.
    pub fn method(&self) -> Option<&str> {
        match self {
            Frame::Request { method, .. } | Frame::Notification { method, .. } => Some(method),
            _ => None,
        }
    }

    /// The typed `params` of a request or notification, or the `result` of a response.
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T, FrameError> {
        match self {
            Frame::Request { params, .. } | Frame::Notification { params, .. } => {
                from_params(params)
            }
            Frame::Response { result, .. } => from_params(result),
            Frame::Error { .. } => Err(FrameError::BadShape("an error response has no params")),
        }
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("jsonrpc".into(), Value::String("2.0".into()));
        match self {
            Frame::Request { id, method, params } => {
                m.insert("id".into(), Value::from(*id));
                m.insert("method".into(), Value::String(method.clone()));
                m.insert("params".into(), Value::Object(params.clone()));
            }
            Frame::Notification { method, params } => {
                m.insert("method".into(), Value::String(method.clone()));
                m.insert("params".into(), Value::Object(params.clone()));
            }
            Frame::Response { id, result } => {
                m.insert("id".into(), Value::from(*id));
                m.insert("result".into(), Value::Object(result.clone()));
            }
            Frame::Error { id, error } => {
                m.insert("id".into(), Value::from(*id));
                m.insert(
                    "error".into(),
                    serde_json::to_value(error).unwrap_or(Value::Null),
                );
            }
        }
        Value::Object(m)
    }

    /// The frame as one line, newline included. Compact JSON has no raw newline, so this is one line.
    pub fn encode(&self) -> Vec<u8> {
        let mut line = serde_json::to_vec(&self.to_value()).unwrap_or_default();
        line.push(b'\n');
        line
    }

    /// Like [`Frame::encode`], but refuses a line over `max_line` bytes (without the newline), so
    /// the daemon never buffers an oversize frame for an adapter (`E_REQUEST_TOO_LARGE`).
    pub fn encode_limited(&self, max_line: usize) -> Result<Vec<u8>, FrameError> {
        let line = self.encode();
        let size = line.len() - 1;
        if size > max_line {
            return Err(FrameError::TooLarge {
                size,
                max: max_line,
            });
        }
        Ok(line)
    }
}

/// Parse one line (without its newline) into a frame.
pub fn parse_frame(line: &[u8]) -> Result<Frame, FrameError> {
    if line.iter().all(|b| b.is_ascii_whitespace()) {
        return Err(FrameError::Empty);
    }
    if line.contains(&0) {
        return Err(FrameError::Nul);
    }
    let text = std::str::from_utf8(line).map_err(|_| FrameError::InvalidUtf8)?;
    let value: Value =
        serde_json::from_str(text).map_err(|e| FrameError::NotJson(e.to_string()))?;
    let obj = match value {
        Value::Object(o) => o,
        Value::Array(_) => return Err(FrameError::Batch),
        _ => return Err(FrameError::NotObject),
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(FrameError::BadVersion);
    }
    let id = match obj.get("id") {
        None => None,
        Some(v) => Some(v.as_u64().ok_or(FrameError::BadId)?),
    };
    let params_of = |key: &str| -> Result<Params, FrameError> {
        match obj.get(key) {
            None => Ok(Params::new()),
            Some(Value::Object(p)) => Ok(p.clone()),
            Some(_) => Err(FrameError::BadParams),
        }
    };

    if let Some(method) = obj.get("method") {
        let method = method
            .as_str()
            .filter(|m| valid_method(m))
            .ok_or(FrameError::BadMethod)?
            .to_string();
        if obj.contains_key("result") || obj.contains_key("error") {
            return Err(FrameError::BadShape(
                "a message with a method has no result or error",
            ));
        }
        let params = params_of("params")?;
        return Ok(match id {
            Some(id) => Frame::Request { id, method, params },
            None => Frame::Notification { method, params },
        });
    }

    let id = id.ok_or(FrameError::BadId)?;
    match (obj.get("result"), obj.get("error")) {
        (Some(Value::Object(result)), None) => Ok(Frame::Response {
            id,
            result: result.clone(),
        }),
        (Some(_), None) => Err(FrameError::BadParams),
        (None, Some(err)) => {
            let error: ErrorObject = serde_json::from_value(err.clone()).map_err(|_| {
                FrameError::BadShape("\"error\" needs an integer code and a string message")
            })?;
            Ok(Frame::Error { id, error })
        }
        (Some(_), Some(_)) => Err(FrameError::BadShape(
            "a response has a result or an error, not both",
        )),
        (None, None) => Err(FrameError::BadShape(
            "a message needs a method, a result or an error",
        )),
    }
}
