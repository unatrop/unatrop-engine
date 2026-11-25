//! Детерминированная обрезка и ограничение вывода инструментов без вызова LLM

/// Маркер, заменяющий вырезанную середину длинного текста
pub const PRUNE_MARKER: &str = "\n\n[... tool result middle pruned ...]\n\n";

/// Бюджеты обрезки длинных текстовых результатов инструментов (в Unicode-символах)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionConfig {
    pub threshold_chars: usize,
    pub head_chars: usize,
    pub tail_chars: usize,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            threshold_chars: 8_192,
            head_chars: 4_096,
            tail_chars: 1_024,
        }
    }
}

impl RetentionConfig {
    /// Создаёт и проверяет бюджет обрезки: `head + marker + tail <= threshold`
    pub fn new(
        threshold_chars: usize,
        head_chars: usize,
        tail_chars: usize,
    ) -> Result<Self, &'static str> {
        if threshold_chars == 0 {
            return Err("threshold_chars must be > 0");
        }
        let marker_chars = PRUNE_MARKER.chars().count();
        if head_chars
            .saturating_add(marker_chars)
            .saturating_add(tail_chars)
            > threshold_chars
        {
            return Err("head_chars + marker + tail_chars must be <= threshold_chars");
        }
        Ok(Self {
            threshold_chars,
            head_chars,
            tail_chars,
        })
    }
}

/// Результат детерминированной обрезки текста
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunedText {
    pub text: String,
    pub truncated: bool,
    pub original_chars: usize,
    pub retained_chars: usize,
}

/// Обрезает середину текста по границам Unicode `char`, если он превышает `threshold_chars`.
/// Не ломает многобайтовые UTF-8 последовательности.
pub fn prune_text(text: &str, config: RetentionConfig) -> PrunedText {
    let original_chars = text.chars().count();
    if original_chars <= config.threshold_chars {
        return PrunedText {
            text: text.to_owned(),
            truncated: false,
            original_chars,
            retained_chars: original_chars,
        };
    }

    let head_end = byte_index_at_char(text, config.head_chars);
    let tail_start_char = original_chars.saturating_sub(config.tail_chars);
    let tail_start = byte_index_at_char(text, tail_start_char);

    let mut out = String::with_capacity(head_end + PRUNE_MARKER.len() + (text.len() - tail_start));
    out.push_str(&text[..head_end]);
    out.push_str(PRUNE_MARKER);
    out.push_str(&text[tail_start..]);
    let retained_chars = out.chars().count();

    PrunedText {
        text: out,
        truncated: true,
        original_chars,
        retained_chars,
    }
}

fn byte_index_at_char(text: &str, char_idx: usize) -> usize {
    if char_idx == 0 {
        return 0;
    }
    text.char_indices()
        .nth(char_idx)
        .map_or(text.len(), |(idx, _)| idx)
}

/// Ограничитель количества логических элементов (путей, grep-совпадений, результатов поиска).
/// Флаг `truncated` отражает только превышение лимита и не смешивается с ошибками I/O.
#[derive(Debug, Clone)]
pub struct ItemRetainer<T> {
    max_items: usize,
    items: Vec<T>,
    seen: usize,
    omitted_count: usize,
}

/// Итог работы [`ItemRetainer`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedItems<T> {
    pub items: Vec<T>,
    pub truncated: bool,
    pub seen: usize,
    pub omitted_count: usize,
}

impl<T> ItemRetainer<T> {
    pub fn new(max_items: usize) -> Self {
        Self {
            max_items,
            items: Vec::with_capacity(max_items.min(256)),
            seen: 0,
            omitted_count: 0,
        }
    }

    /// Предлагает очередной элемент. Возвращает `true`, если элемент сохранён.
    pub fn push(&mut self, item: T) -> bool {
        self.seen += 1;
        if self.items.len() < self.max_items {
            self.items.push(item);
            true
        } else {
            self.omitted_count += 1;
            false
        }
    }

    pub fn is_full(&self) -> bool {
        self.items.len() >= self.max_items
    }

    pub fn finish(self) -> RetainedItems<T> {
        RetainedItems {
            items: self.items,
            truncated: self.omitted_count > 0,
            seen: self.seen,
            omitted_count: self.omitted_count,
        }
    }
}
