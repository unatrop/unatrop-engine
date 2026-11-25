//! Wire-протокол локального демона `unatropd` (Google Protobuf over HTTP + WebSocket).

pub mod proto;
pub use proto::{
    CONTENT_TYPE_PROTOBUF, ProtoDecodeError, decode_proto, encode_proto, encode_proto_bytes,
    parse_json_args, pb, stringify_json_args,
};

pub const DEFAULT_DAEMON_ADDR: &str = "127.0.0.1:7842";
pub const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:7842";
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MAX_PROVIDER_TURNS: usize = 64;
