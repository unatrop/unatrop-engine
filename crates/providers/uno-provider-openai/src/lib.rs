//! Провайдер OpenAI-совместимого `/chat/completions` и парсер SSE-потока
//!
//! Включает [`OpenAiCompatibleProvider`] для стандартных эндпоинтов, а также
//! [`openai_request_body`] и [`send_openai_stream`] для бэкендов со своей авторизацией

mod dto;
mod openai_compat;
mod provider;
mod responses;

pub use openai_compat::{
    openai_request_body, openai_request_body_with_cache, send_openai_stream,
    stream_chat_completions, stream_chat_completions_with_cache, stream_responses,
};
pub use provider::OpenAiCompatibleProvider;
pub use responses::responses_request_body;
