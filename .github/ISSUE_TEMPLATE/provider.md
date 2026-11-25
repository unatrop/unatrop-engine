---
name: New Provider
about: Запрос или план добавления нового LLM-провайдера в crates/providers/
labels: provider
---

## Провайдер

Название и ссылка на документацию API.

## Чеклист интеграции

- [ ] Новый крейт в `crates/providers/uno-provider-<name>`
- [ ] Использование единого SSE-движка из `uno-provider`
- [ ] Отсутствие прямого обращения к ФС/процессам (проходит `core-boundary`)
- [ ] Подключение в `apps/server` (`uno-daemon`)
