## Описание

Кратко опишите цель изменений и контекст.

## Тип изменений

- [ ] Реорганизация / рефакторинг (без изменения поведения)
- [ ] Новая функциональность
- [ ] Исправление ошибки
- [ ] Документация

## Чеклист

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --lib`
- [ ] `cargo test -p uno-daemon -p uno-tools-core -p uno-tools-exec -p uno-voice -p uno-session`
- [ ] `cargo metadata --format-version 1 > /dev/null`
- [ ] Обновлена документация (если затронуты пути, API или поведение)
