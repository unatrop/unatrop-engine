# `uno-daemon` (`apps/server`)

Локальный HTTP + WebSocket демон (`unatropd`) для Desktop, Mobile и CLI клиентов.

## Структура `src/`

```text
src/
├── main.rs            # Точка входа бинарника `unatropd`
├── lib.rs             # Публичный API крейта (`DaemonState`, `build_router`, `serve*`)
├── app/               # HTTP/Axum каркас приложения
│   ├── mod.rs         # Сборка главного Router, CORS, auth middleware
│   ├── state.rs       # `DaemonState` — DI-контейнер сервисов
│   ├── middleware.rs  # Bearer auth + loopback CORS проверка
│   ├── protobuf.rs    # Экстрактор/респондер `Protobuf<T>`
│   └── error.rs       # Единый `DaemonError` -> JSON с редакцией секретов
├── modules/           # Вертикальные фича-модули (одинаковый шаблон)
│   ├── health/        # GET /health
│   ├── accounts/      # /v1/providers, /v1/accounts, /v1/accounts/oauth/{kind}/start + drivers/ (1 файл = 1 провайдер)
│   ├── sessions/      # /v1/sessions (+ /{id}/ws) и runtime/ исполнения сессий
│   ├── call/          # GET /v1/call/ws (STT + VAD + стриминг TTS)
│   ├── tts/           # POST /v1/tts
│   └── git/           # /v1/sessions/{id}/diff, /revert (`host-tools`)
└── shared/            # Общая не-HTTP инфраструктура (`config`, `prompts`, `sqlite`)
```

## Правила слоёв внутри `modules/<feature>/`

Каждый фича-модуль строится по одному шаблону:

1. **`router.rs`** — только таблица маршрутов `pub(crate) fn router() -> Router<DaemonState>`.
2. **`handlers.rs`** — тонкий транспортный слой (аналог controller): извлечение `State`, `Path`, `Json`/`Protobuf`, WebSocket upgrade и возврат `Result<_, DaemonError>`. Без бизнес-логики и состояния.
3. **`service.rs`** — доменное состояние и бизнес-логика (`*Service`). **Не импортирует `axum`** — тестируется и переиспользуется независимо от HTTP.
4. **`dto.rs`** (если нужен) — структуры запросов/ответов API.

Направление зависимостей:

```text
app::build_router -> modules::<feature>::router -> handlers -> service -> shared / crates/*
```

## Как добавить новый модуль

1. Создать папку `src/modules/<name>/` с файлами `mod.rs`, `router.rs`, `handlers.rs`, `service.rs`.
2. Объявить `pub(crate) mod <name>;` в `src/modules/mod.rs`.
3. Если у модуля есть состояние — добавить `Arc<<Name>Service>` в `DaemonState` (`src/app/state.rs`).
4. Подключить `.merge(modules::<name>::router())` в `src/app/mod.rs`.
