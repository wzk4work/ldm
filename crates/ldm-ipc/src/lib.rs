//! LDM shared protocol crate (original work, MIT).
//!
//! * [`envelope`] – the JSON envelope `{v,id,type,payload}` used between the
//!   browser extension, the native host, the WebSocket bridge, CLI/GUI and `ldmd`.
//! * [`messages`] – typed payloads for the envelope types defined in the
//!   protocol.
//! * [`framing`] – Native Messaging framing (u32 little-endian length + UTF-8 JSON)
//!   and NDJSON helpers for the Unix-socket IPC.

pub mod client;
pub mod envelope;
pub mod framing;
pub mod messages;

pub use envelope::Envelope;

/// Protocol version carried in `Envelope::v` and `HelloAck.protocolVersion`.
pub const PROTOCOL_VERSION: u32 = 1;
/// Maximum accepted message size for extension-originated messages (1 MiB).
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// Native messaging host name.
pub const NATIVE_HOST_NAME: &str = "io.github.wzk4work.ldm.browser_host";
/// Sub-protocol of the LDM JSON WebSocket bridge.
pub const WS_SUBPROTOCOL: &str = "ldm.bridge.v1";
