//! Двухступенчатый инбокс агента:
//! - `next_step`: подмешивается сразу на границе шага после завершения батча инструментов
//! - `next_turn`: запускает следующий ход, когда текущий ответ модели завершился

use std::collections::VecDeque;
use std::sync::Mutex;

use protocol::Message;

/// Потокобезопасная очередь входящих сообщений для работающего `Harness`
#[derive(Default)]
pub struct AgentInbox {
    state: Mutex<InboxState>,
}

#[derive(Default)]
struct InboxState {
    next_step: VecDeque<Message>,
    next_turn: VecDeque<Message>,
}

impl AgentInbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ставит сообщение на ближайшую границу шага (между вызовами инструментов и следующим запросом к LLM)
    pub fn push_next_step(&self, message: Message) {
        if let Ok(mut guard) = self.state.lock() {
            guard.next_step.push_back(message);
        }
    }

    /// Ставит сообщение в очередь следующего полноценного хода
    pub fn push_next_turn(&self, message: Message) {
        if let Ok(mut guard) = self.state.lock() {
            guard.next_turn.push_back(message);
        }
    }

    /// Забирает все накопившиеся `next_step` сообщения
    pub fn drain_next_step(&self) -> Vec<Message> {
        self.state
            .lock()
            .map(|mut guard| guard.next_step.drain(..).collect())
            .unwrap_or_default()
    }

    /// Забирает одно сообщение из очереди `next_turn`
    pub fn pop_next_turn(&self) -> Option<Message> {
        self.state
            .lock()
            .ok()
            .and_then(|mut guard| guard.next_turn.pop_front())
    }

    /// Очищает обе очереди (сначала `next_step`, затем `next_turn`)
    pub fn clear(&self) {
        if let Ok(mut guard) = self.state.lock() {
            guard.next_step.clear();
            guard.next_turn.clear();
        }
    }

    pub fn has_pending(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|guard| !guard.next_step.is_empty() || !guard.next_turn.is_empty())
    }
}
