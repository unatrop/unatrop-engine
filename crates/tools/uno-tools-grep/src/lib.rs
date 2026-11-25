//! Инструмент текстового поиска по рабочей области

mod search_text;
mod workspace;

pub use search_text::{
    MAX_MATCH_LINE_CHARS, MAX_MATCHES, MAX_QUERY_BYTES, SearchText, SearchTextArgs,
};
