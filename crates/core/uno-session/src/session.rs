use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};

use async_stream::stream;
use futures_util::StreamExt;
use harness::{Harness, RunEvent, RunStream};
use protocol::{CompletionRequest, Message};

use crate::{SessionError, repair_history};

/// Сессия диалога с историей сообщений поверх [`Harness`]
pub struct Session {
    harness: Arc<Harness>,
    history: Arc<Mutex<Vec<Message>>>,
    running: Arc<AtomicBool>,
}

impl Session {
    pub fn new(harness: Arc<Harness>, history: Vec<Message>) -> Self {
        Self {
            harness,
            history: Arc::new(Mutex::new(history)),
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Возвращает копию текущей истории сессии
    pub fn history(&self) -> Result<Vec<Message>, SessionError> {
        self.history
            .lock()
            .map(|history| history.clone())
            .map_err(|_| SessionError::HistoryPoisoned)
    }

    /// Заменяет историю текущей сессии (только когда нет активного запуска)
    pub fn set_history(&self, mut messages: Vec<Message>) -> Result<(), SessionError> {
        if self.running.load(Ordering::Acquire) {
            return Err(SessionError::RunAlreadyActive);
        }
        let mut history = self
            .history
            .lock()
            .map_err(|_| SessionError::HistoryPoisoned)?;
        if self.running.load(Ordering::Acquire) {
            return Err(SessionError::RunAlreadyActive);
        }
        repair_history(&mut messages);
        *history = messages;
        Ok(())
    }

    /// Запускает один ход: `request.messages` добавляются к накопленной истории сессии
    ///
    /// Сброс потока отменяет запуск и чинит незавершённые вызовы инструментов в истории
    pub fn run(&self, request: CompletionRequest) -> Result<RunStream<'static>, SessionError> {
        self.run_with_system_hint(request, None)
    }

    /// Запускает один ход с временной системной подсказкой, не сохраняя её в историю сессии
    pub fn run_with_system_hint(
        &self,
        mut request: CompletionRequest,
        system_hint: Option<String>,
    ) -> Result<RunStream<'static>, SessionError> {
        self.running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| SessionError::RunAlreadyActive)?;

        let Ok(mut history) = self.history.lock() else {
            self.running.store(false, Ordering::Release);
            return Err(SessionError::HistoryPoisoned);
        };
        repair_history(&mut history);
        history.append(&mut request.messages);
        request.messages = history.clone();
        drop(history);

        let mut inserted_system = false;
        let mut original_first: Option<Message> = None;
        if let Some(hint) = system_hint.filter(|h| !h.trim().is_empty()) {
            if request
                .messages
                .first()
                .is_some_and(|m| m.role == protocol::Role::System)
            {
                original_first = Some(request.messages[0].clone());
                request.messages[0]
                    .content
                    .0
                    .push(protocol::ContentPart::text(format!("\n\n{hint}")));
            } else {
                request.messages.insert(0, Message::system(hint));
                inserted_system = true;
            }
        }

        let guard = RepairOnDrop {
            history: Arc::clone(&self.history),
            running: Arc::clone(&self.running),
            completed: false,
            released: false,
        };
        let history = Arc::clone(&self.history);
        let mut run = Arc::clone(&self.harness).run_owned(request);

        Ok(Box::pin(stream! {
            let mut guard = guard;
            while let Some(event) = run.next().await {
                let yielded = match event {
                    Ok(RunEvent::MessageAppended(message)) => {
                        lock_history_recover(&history).push(message.clone());
                        Ok(RunEvent::MessageAppended(message))
                    }
                    Ok(RunEvent::Completed {
                        mut messages,
                        finish_reason,
                        usage,
                    }) => {
                        if inserted_system {
                            if messages
                                .first()
                                .is_some_and(|m| m.role == protocol::Role::System)
                            {
                                messages.remove(0);
                            }
                        } else if let Some(ref orig) = original_first
                            && let Some(first) = messages.first_mut()
                        {
                            *first = orig.clone();
                        }
                        *lock_history_recover(&history) = messages.clone();
                        guard.complete();
                        Ok(RunEvent::Completed {
                            messages,
                            finish_reason,
                            usage,
                        })
                    }
                    Err(err) => {
                        guard.repair();
                        Err(err)
                    }
                    other => other,
                };

                yield yielded;
            }
        }))
    }
}

struct RepairOnDrop {
    history: Arc<Mutex<Vec<Message>>>,
    running: Arc<AtomicBool>,
    completed: bool,
    released: bool,
}

impl RepairOnDrop {
    fn complete(&mut self) {
        self.completed = true;
        self.release();
    }

    fn repair(&mut self) {
        if !self.completed {
            repair_history(&mut lock_history_recover(&self.history));
            self.completed = true;
        }
        self.release();
    }

    fn release(&mut self) {
        if !self.released {
            self.running.store(false, Ordering::Release);
            self.released = true;
        }
    }
}

impl Drop for RepairOnDrop {
    fn drop(&mut self) {
        if !self.completed {
            self.repair();
        } else {
            self.release();
        }
    }
}

fn lock_history_recover(history: &Mutex<Vec<Message>>) -> MutexGuard<'_, Vec<Message>> {
    history
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
