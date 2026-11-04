use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Outer frame for every LDM JSON message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub v: u32,
    pub id: u64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default = "empty_obj")]
    pub payload: Value,
}

fn empty_obj() -> Value {
    json!({})
}

#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported protocol version {0}")]
    Version(u32),
    #[error("payload must be an object")]
    Payload,
    #[error("empty type")]
    EmptyType,
}

impl Envelope {
    pub fn new(id: u64, kind: impl Into<String>, payload: Value) -> Self {
        Self { v: crate::PROTOCOL_VERSION, id, kind: kind.into(), payload }
    }

    /// Build a reply that carries the same id as the request.
    pub fn reply(&self, kind: impl Into<String>, payload: Value) -> Self {
        Self::new(self.id, kind, payload)
    }

    pub fn error(id: u64, code: &str, msg: impl Into<String>) -> Self {
        Self::new(id, "Error", json!({ "error": msg.into(), "code": code }))
    }

    /// Parse and validate an envelope.
    pub fn parse(bytes: &[u8]) -> Result<Self, EnvelopeError> {
        let env: Envelope = serde_json::from_slice(bytes)?;
        env.validate()?;
        Ok(env)
    }

    pub fn validate(&self) -> Result<(), EnvelopeError> {
        if self.v != crate::PROTOCOL_VERSION {
            return Err(EnvelopeError::Version(self.v));
        }
        if !self.payload.is_object() {
            return Err(EnvelopeError::Payload);
        }
        if self.kind.is_empty() {
            return Err(EnvelopeError::EmptyType);
        }
        Ok(())
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("envelope serializes")
    }

    /// Deserialize the payload into a typed struct.
    pub fn payload_as<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_validation() {
        let e = Envelope::new(42, "DownloadAdd", json!({"url": "http://x/a.bin"}));
        let s = e.to_json();
        assert!(s.contains("\"type\":\"DownloadAdd\""));
        assert_eq!(Envelope::parse(s.as_bytes()).unwrap(), e);
        assert!(Envelope::parse(br#"{"v":2,"id":1,"type":"Ping","payload":{}}"#).is_err());
        assert!(Envelope::parse(br#"{"v":1,"id":1,"type":"Ping","payload":[]}"#).is_err());
        assert!(Envelope::parse(br#"{"v":1,"id":1,"type":"Ping","payload":{},"x":1}"#).is_err());
        // payload may be omitted
        let p = Envelope::parse(br#"{"v":1,"id":3,"type":"Ping"}"#).unwrap();
        assert!(p.payload.is_object());
    }
}
