//! Детерминированный сборщик системного промпта с именованными слотами и числовыми весами.
//! Стабильный порядок секций сохраняет префикс запроса для KV-кэша провайдера.

use std::collections::HashMap;

/// Стандартные веса секций системного промпта
pub mod section_order {
    pub const HARNESS_IDENTITY: i32 = -1_000;
    pub const PERSONA_PREFIX: i32 = 0;
    pub const PLAN_POLICY: i32 = 500;
    pub const TEAM_POLICY: i32 = 600;
    pub const WORKSPACE_CONTEXT: i32 = 900;
    pub const SANDBOX_POLICY: i32 = 1_000;
    pub const TOOLS_GUIDANCE: i32 = 2_000;
    pub const PERSONA_SUFFIX: i32 = 10_200;
}

#[derive(Debug, Clone)]
struct PromptSection {
    order: i32,
    seq: usize,
    content: String,
}

/// Сборщик системного промпта из именованных секций с детерминированной сортировкой
#[derive(Debug, Default, Clone)]
pub struct SystemPromptBuilder {
    sections: HashMap<String, PromptSection>,
    next_seq: usize,
}

impl SystemPromptBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Добавляет или заменяет именованную секцию. Пустой контент удаляет слот.
    pub fn set_section(
        &mut self,
        id: impl Into<String>,
        order: i32,
        content: impl Into<String>,
    ) -> &mut Self {
        let id = id.into();
        let content = content.into();
        if content.trim().is_empty() {
            self.sections.remove(&id);
            return self;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.sections.insert(
            id,
            PromptSection {
                order,
                seq,
                content,
            },
        );
        self
    }

    /// Собирает итоговый системный промпт в строгом порядке `(order, seq)`
    pub fn build(&self) -> String {
        let mut entries: Vec<&PromptSection> = self.sections.values().collect();
        entries.sort_by_key(|s| (s.order, s.seq));
        entries
            .into_iter()
            .map(|s| s.content.trim())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}
