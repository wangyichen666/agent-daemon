use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const JSONRPC_VERSION: &str = "2.0";
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum RequestId {
    Number(u64),
    String(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl JsonRpcRequest {
    pub fn new(id: RequestId, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            method: method.into(),
            params,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl JsonRpcResponse {
    pub fn success(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(id: RequestId, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    TurnStarted,
    TextDelta,
    ToolStarted,
    ToolFinished,
    ApprovalRequired,
    TurnCompleted,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct EventFrame {
    pub jsonrpc: String,
    pub request_id: RequestId,
    pub event: EventKind,
    pub data: Value,
}

impl EventFrame {
    pub fn new(request_id: RequestId, event: EventKind, data: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            request_id,
            event,
            data,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ServerFrame {
    Event(EventFrame),
    Response(JsonRpcResponse),
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("协议帧大小 {actual} 字节，超过 {limit} 字节限制")]
    FrameTooLarge { actual: usize, limit: usize },
    #[error("JSON-RPC 帧不是合法 JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("不支持的 JSON-RPC 版本: {0}")]
    UnsupportedVersion(String),
}

pub fn encode_frame<T: Serialize>(frame: &T) -> Result<Vec<u8>, ProtocolError> {
    let mut bytes = serde_json::to_vec(frame)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            actual: bytes.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn decode_request(bytes: &[u8]) -> Result<JsonRpcRequest, ProtocolError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            actual: bytes.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    let request: JsonRpcRequest = serde_json::from_slice(bytes)?;
    if request.jsonrpc != JSONRPC_VERSION {
        return Err(ProtocolError::UnsupportedVersion(request.jsonrpc));
    }
    Ok(request)
}

pub fn decode_server_frame(bytes: &[u8]) -> Result<ServerFrame, ProtocolError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            actual: bytes.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    let frame: ServerFrame = serde_json::from_slice(bytes)?;
    let version = match &frame {
        ServerFrame::Event(event) => &event.jsonrpc,
        ServerFrame::Response(response) => &response.jsonrpc,
    };
    if version != JSONRPC_VERSION {
        return Err(ProtocolError::UnsupportedVersion(version.clone()));
    }
    Ok(frame)
}

pub fn server_frame_request_id(frame: &ServerFrame) -> &RequestId {
    match frame {
        ServerFrame::Event(event) => &event.request_id,
        ServerFrame::Response(response) => &response.id,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn request_round_trip_uses_newline_delimited_json() {
        let request = JsonRpcRequest::new(
            RequestId::Number(7),
            "chat.send",
            json!({"message": "你好"}),
        );
        let encoded = encode_frame(&request).unwrap();

        assert_eq!(encoded.last(), Some(&b'\n'));
        assert_eq!(decode_request(&encoded).unwrap(), request);
    }

    #[test]
    fn server_frame_round_trip_preserves_request_id() {
        let frame = ServerFrame::Response(JsonRpcResponse::success(
            RequestId::String("abc".to_owned()),
            json!({"ok": true}),
        ));
        let encoded = encode_frame(&frame).unwrap();
        let decoded = decode_server_frame(&encoded).unwrap();

        assert_eq!(
            server_frame_request_id(&decoded),
            &RequestId::String("abc".to_owned())
        );
    }

    #[test]
    fn rejects_oversized_frames() {
        let bytes = vec![b'x'; MAX_FRAME_BYTES + 1];
        assert!(matches!(
            decode_request(&bytes),
            Err(ProtocolError::FrameTooLarge { .. })
        ));
    }
}
