//! Google Protobuf (`prost`) wire encoding, decoding, and core domain conversions.

use bytes::Bytes;
use prost::Message as ProstMessage;
use serde_json::Value;
use uno_harness::{
    RunState,
    plan::{PlanStep, PlanStepStatus},
};
use uno_protocol::{
    Content, ContentPart, FinishReason, Message, Role, StreamEvent, ThinkingLevel, ToolCall, Usage,
};
use uno_provider::ContextProfile;
use uno_tools_core::ApprovalMode;

pub const CONTENT_TYPE_PROTOBUF: &str = "application/x-protobuf";

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/uno.daemon.v1.rs"));
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoDecodeError {
    #[error("protobuf decode error: {0}")]
    Prost(#[from] prost::DecodeError),
    #[error("missing required field: {0}")]
    MissingField(&'static str),
}

pub fn encode_proto<T: ProstMessage>(msg: &T) -> Vec<u8> {
    msg.encode_to_vec()
}

pub fn encode_proto_bytes<T: ProstMessage>(msg: &T) -> Bytes {
    Bytes::from(msg.encode_to_vec())
}

pub fn decode_proto<T: ProstMessage + Default>(bytes: &[u8]) -> Result<T, prost::DecodeError> {
    T::decode(bytes)
}

pub fn parse_json_args(raw: &str) -> Value {
    if raw.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
    }
}

pub fn stringify_json_args(val: &Value) -> String {
    if val.is_null() {
        String::new()
    } else {
        serde_json::to_string(val).unwrap_or_default()
    }
}

impl From<ApprovalMode> for pb::ApprovalMode {
    fn from(v: ApprovalMode) -> Self {
        match v {
            ApprovalMode::Default => Self::Default,
            ApprovalMode::ReadOnly => Self::ReadOnly,
            ApprovalMode::AutoApproveWorkspace => Self::AutoApproveWorkspace,
            ApprovalMode::FullAuto => Self::FullAuto,
        }
    }
}

impl From<pb::ApprovalMode> for ApprovalMode {
    fn from(v: pb::ApprovalMode) -> Self {
        match v {
            pb::ApprovalMode::ReadOnly => Self::ReadOnly,
            pb::ApprovalMode::AutoApproveWorkspace => Self::AutoApproveWorkspace,
            pb::ApprovalMode::FullAuto => Self::FullAuto,
            pb::ApprovalMode::Unspecified | pb::ApprovalMode::Default => Self::Default,
        }
    }
}

impl From<ThinkingLevel> for pb::ThinkingLevel {
    fn from(v: ThinkingLevel) -> Self {
        match v {
            ThinkingLevel::Off => Self::Off,
            ThinkingLevel::Minimal => Self::Minimal,
            ThinkingLevel::Low => Self::Low,
            ThinkingLevel::Medium => Self::Medium,
            ThinkingLevel::High => Self::High,
            ThinkingLevel::XHigh => Self::Xhigh,
        }
    }
}

impl From<pb::ThinkingLevel> for ThinkingLevel {
    fn from(v: pb::ThinkingLevel) -> Self {
        match v {
            pb::ThinkingLevel::Off | pb::ThinkingLevel::Unspecified => Self::Off,
            pb::ThinkingLevel::Minimal => Self::Minimal,
            pb::ThinkingLevel::Low => Self::Low,
            pb::ThinkingLevel::Medium => Self::Medium,
            pb::ThinkingLevel::High => Self::High,
            pb::ThinkingLevel::Xhigh => Self::XHigh,
        }
    }
}

impl From<RunState> for pb::RunState {
    fn from(v: RunState) -> Self {
        match v {
            RunState::WaitingForModel => Self::WaitingForModel,
            RunState::StreamingResponse => Self::StreamingResponse,
            RunState::HandlingToolCalls => Self::HandlingToolCalls,
            RunState::Completed => Self::Completed,
        }
    }
}

impl From<pb::RunState> for RunState {
    fn from(v: pb::RunState) -> Self {
        match v {
            pb::RunState::Unspecified | pb::RunState::WaitingForModel => Self::WaitingForModel,
            pb::RunState::StreamingResponse => Self::StreamingResponse,
            pb::RunState::HandlingToolCalls => Self::HandlingToolCalls,
            pb::RunState::Completed => Self::Completed,
        }
    }
}

impl From<FinishReason> for pb::FinishReason {
    fn from(v: FinishReason) -> Self {
        match v {
            FinishReason::Stop => Self::Stop,
            FinishReason::ToolCall => Self::ToolCall,
            FinishReason::Length => Self::Length,
            FinishReason::ContentFilter => Self::ContentFilter,
            FinishReason::Other => Self::Other,
        }
    }
}

impl From<pb::FinishReason> for FinishReason {
    fn from(v: pb::FinishReason) -> Self {
        match v {
            pb::FinishReason::Stop => Self::Stop,
            pb::FinishReason::ToolCall => Self::ToolCall,
            pb::FinishReason::Length => Self::Length,
            pb::FinishReason::ContentFilter => Self::ContentFilter,
            pb::FinishReason::Unspecified | pb::FinishReason::Other => Self::Other,
        }
    }
}

impl From<Role> for pb::Role {
    fn from(v: Role) -> Self {
        match v {
            Role::System => Self::System,
            Role::Developer => Self::Developer,
            Role::User => Self::User,
            Role::Assistant => Self::Assistant,
            Role::Tool => Self::Tool,
        }
    }
}

impl From<pb::Role> for Role {
    fn from(v: pb::Role) -> Self {
        match v {
            pb::Role::System => Self::System,
            pb::Role::Developer => Self::Developer,
            pb::Role::Unspecified | pb::Role::User => Self::User,
            pb::Role::Assistant => Self::Assistant,
            pb::Role::Tool => Self::Tool,
        }
    }
}

impl From<ContextProfile> for pb::ContextProfile {
    fn from(v: ContextProfile) -> Self {
        Self {
            max_input_tokens: v.max_input_tokens,
            max_output_tokens: v.max_output_tokens,
            compact_at_tokens: v.compact_at_tokens,
        }
    }
}

impl From<pb::ContextProfile> for ContextProfile {
    fn from(v: pb::ContextProfile) -> Self {
        Self {
            max_input_tokens: v.max_input_tokens,
            max_output_tokens: v.max_output_tokens,
            compact_at_tokens: v.compact_at_tokens,
        }
    }
}

impl From<&ContentPart> for pb::ContentPart {
    fn from(v: &ContentPart) -> Self {
        let kind = match v {
            ContentPart::Text { text } => Some(pb::content_part::Kind::Text(text.clone())),
            ContentPart::Reasoning { text } => {
                Some(pb::content_part::Kind::Reasoning(text.clone()))
            }
            ContentPart::ImageUrl { url, detail } => {
                Some(pb::content_part::Kind::ImageUrl(pb::ImageUrl {
                    url: url.clone(),
                    detail: detail.clone(),
                }))
            }
        };
        Self { kind }
    }
}

impl From<pb::ContentPart> for ContentPart {
    fn from(v: pb::ContentPart) -> Self {
        match v.kind {
            Some(pb::content_part::Kind::Text(text)) => Self::Text { text },
            Some(pb::content_part::Kind::Reasoning(text)) => Self::Reasoning { text },
            Some(pb::content_part::Kind::ImageUrl(img)) => Self::ImageUrl {
                url: img.url,
                detail: img.detail,
            },
            None => Self::Text {
                text: String::new(),
            },
        }
    }
}

impl From<&ToolCall> for pb::ToolCall {
    fn from(v: &ToolCall) -> Self {
        Self {
            id: v.id.clone(),
            name: v.name.clone(),
            arguments_json: stringify_json_args(&v.arguments),
            signature: v.signature.clone(),
        }
    }
}

impl From<pb::ToolCall> for ToolCall {
    fn from(v: pb::ToolCall) -> Self {
        Self {
            id: v.id,
            name: v.name,
            arguments: parse_json_args(&v.arguments_json),
            signature: v.signature,
        }
    }
}

impl From<&Message> for pb::Message {
    fn from(v: &Message) -> Self {
        Self {
            role: pb::Role::from(v.role) as i32,
            content: v.content.0.iter().map(pb::ContentPart::from).collect(),
            tool_calls: v.tool_calls.iter().map(pb::ToolCall::from).collect(),
            tool_call_id: v.tool_call_id.clone(),
        }
    }
}

impl From<pb::Message> for Message {
    fn from(v: pb::Message) -> Self {
        let role = pb::Role::try_from(v.role).unwrap_or(pb::Role::User);
        Self {
            role: role.into(),
            content: Content(v.content.into_iter().map(ContentPart::from).collect()),
            tool_calls: v.tool_calls.into_iter().map(ToolCall::from).collect(),
            tool_call_id: v.tool_call_id,
        }
    }
}

impl From<&Usage> for pb::Usage {
    fn from(v: &Usage) -> Self {
        Self {
            input_tokens: v.input_tokens,
            output_tokens: v.output_tokens,
            cached_tokens: v.cached_tokens,
        }
    }
}

impl From<pb::Usage> for Usage {
    fn from(v: pb::Usage) -> Self {
        Self {
            input_tokens: v.input_tokens,
            output_tokens: v.output_tokens,
            cached_tokens: v.cached_tokens,
        }
    }
}

impl From<&StreamEvent> for pb::StreamEvent {
    fn from(v: &StreamEvent) -> Self {
        let event = match v {
            StreamEvent::TextDelta { delta } => pb::stream_event::Event::TextDelta(pb::TextDelta {
                delta: delta.clone(),
            }),
            StreamEvent::ReasoningDelta { delta } => {
                pb::stream_event::Event::ReasoningDelta(pb::ReasoningDelta {
                    delta: delta.clone(),
                })
            }
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => pb::stream_event::Event::ToolCallDelta(pb::ToolCallDelta {
                index: *index as u64,
                id: id.clone(),
                name: name.clone(),
                arguments_delta: arguments_delta.clone(),
            }),
            StreamEvent::ThoughtSignature { index, signature } => {
                pb::stream_event::Event::ThoughtSignature(pb::ThoughtSignature {
                    index: *index as u64,
                    signature: signature.clone(),
                })
            }
            StreamEvent::Finished { reason, usage } => {
                pb::stream_event::Event::Finished(pb::StreamFinished {
                    reason: pb::FinishReason::from(*reason) as i32,
                    usage: usage.as_ref().map(pb::Usage::from),
                })
            }
            StreamEvent::Error { message, retryable } => {
                pb::stream_event::Event::Error(pb::StreamError {
                    message: message.clone(),
                    retryable: *retryable,
                })
            }
        };
        Self { event: Some(event) }
    }
}

impl TryFrom<pb::StreamEvent> for StreamEvent {
    type Error = ProtoDecodeError;
    fn try_from(v: pb::StreamEvent) -> Result<Self, ProtoDecodeError> {
        match v
            .event
            .ok_or(ProtoDecodeError::MissingField("stream_event.event"))?
        {
            pb::stream_event::Event::TextDelta(e) => Ok(Self::TextDelta { delta: e.delta }),
            pb::stream_event::Event::ReasoningDelta(e) => {
                Ok(Self::ReasoningDelta { delta: e.delta })
            }
            pb::stream_event::Event::ToolCallDelta(e) => Ok(Self::ToolCallDelta {
                index: e.index as usize,
                id: e.id,
                name: e.name,
                arguments_delta: e.arguments_delta,
            }),
            pb::stream_event::Event::ThoughtSignature(e) => Ok(Self::ThoughtSignature {
                index: e.index as usize,
                signature: e.signature,
            }),
            pb::stream_event::Event::Finished(e) => Ok(Self::Finished {
                reason: pb::FinishReason::try_from(e.reason)
                    .unwrap_or(pb::FinishReason::Other)
                    .into(),
                usage: e.usage.map(Usage::from),
            }),
            pb::stream_event::Event::Error(e) => Ok(Self::Error {
                message: e.message,
                retryable: e.retryable,
            }),
        }
    }
}

impl From<&PlanStep> for pb::PlanStep {
    fn from(v: &PlanStep) -> Self {
        let status = match v.status {
            PlanStepStatus::Pending => pb::PlanStepStatus::Pending,
            PlanStepStatus::InProgress => pb::PlanStepStatus::InProgress,
            PlanStepStatus::Completed => pb::PlanStepStatus::Completed,
        };
        Self {
            step: v.step.clone(),
            status: status as i32,
        }
    }
}

impl From<pb::PlanStep> for PlanStep {
    fn from(v: pb::PlanStep) -> Self {
        let status =
            match pb::PlanStepStatus::try_from(v.status).unwrap_or(pb::PlanStepStatus::Pending) {
                pb::PlanStepStatus::Unspecified | pb::PlanStepStatus::Pending => {
                    PlanStepStatus::Pending
                }
                pb::PlanStepStatus::InProgress => PlanStepStatus::InProgress,
                pb::PlanStepStatus::Completed => PlanStepStatus::Completed,
            };
        Self {
            step: v.step,
            status,
        }
    }
}
