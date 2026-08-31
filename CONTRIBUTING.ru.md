# Участие в разработке DEcode

[English](CONTRIBUTING.md) · [Документация](docs/README.ru.md) · [Политика безопасности](SECURITY.ru.md) · [Кодекс поведения](CODE_OF_CONDUCT.ru.md)

DEcode — чувствительный к безопасности coding-агент. Вклад приветствуется, но корректность, восстановление, минимальные полномочия и удобный терминальный интерфейс важнее количества функций.

## Выбор канала

- Воспроизводимый дефект: найдите похожие issue и используйте bug report form.
- Конкретное пользовательское улучшение: используйте feature request form.
- Эксплуатируемая ошибка границы безопасности: следуйте [SECURITY.ru.md](SECURITY.ru.md) и не публикуйте детали в public issue.
- Проблема использования или настройки: сначала прочитайте [решение проблем](docs/TROUBLESHOOTING.ru.md).

Удаляйте credentials, bearer tokens, идентификаторы аккаунта, приватные prompts, код закрытого репозитория, имена пользователей и чувствительные пути из любого публичного текста, screenshots и logs.

## Настройка окружения

Установите актуальный стабильный Rust через rustup с `rustfmt` и `clippy`, Git и нативные build tools своей платформы. Полные шаги находятся в [руководстве по установке](docs/INSTALLATION.ru.md).

```bash
git clone https://github.com/denysoid/DEcode.git
cd DEcode
cargo check --locked --all-targets
cargo test --locked --all-targets
```

Обычным тестам не нужны реальные credentials провайдера. Никогда не помещайте настоящий секрет в репозиторий, fixture output, snapshot или issue.

## Перед изменением кода

1. Для межмодульной работы прочитайте [архитектуру](docs/ARCHITECTURE.ru.md).
2. При изменении файлов, команд, провайдеров, persistence, permissions, интеграций или отображаемого текста прочитайте [модель безопасности](docs/SECURITY.ru.md).
3. Подтвердите существующее поведение точным тестом или воспроизведением.
4. Сохраните несвязанные изменения working tree.
5. Выберите минимальное целостное изменение, которое исправляет сам инвариант.

Перед большим redesign создайте issue, чтобы обсудить scope и совместимость до реализации.

## Ветки и commits

Создавайте короткоживущую ветку от актуальной `main`. Используйте понятное имя, например `fix/session-resume-usage` или `docs/provider-setup`.

Формулируйте тему commit как действие и оставляйте в одном commit одну задачу. Предпочтительный формат:

```text
type(scope): concise change
```

Для `type` используйте `feat`, `fix`, `docs`, `test`, `refactor`, `build` или `chore`; scope можно убрать, если он ничего не поясняет. Примеры: `fix(session): preserve usage across resume` и `docs: explain Azure deployment names`.

Каждый commit должен компилироваться и сохранять релевантные тесты зелёными. Не смешивайте с функциональным изменением build output, постороннее форматирование или личную конфигурацию. Приватные checkpoint refs DEcode нужны для runtime recovery, а не для истории проекта; они не должны попадать в pull request.

## Стандарты кода

- Сохраняйте `#![forbid(unsafe_code)]`.
- Используйте типизированные commands, outcomes, errors и state transitions вместо управления строками.
- Ограничивайте внешний ввод по размеру, количеству, времени, retry и cancellation.
- Считайте данные модели, репозитория, процесса, провайдера, MCP/LSP и plugin недоверенными.
- Разрешайте файловый доступ модели через workspace capability sandbox.
- Используйте точные patches или атомарную запись для изменений.
- Сохраняйте durable causal boundaries при pause, cancellation, failure и restart.
- Не допускайте panic в runtime paths; превращайте восстанавливаемые сбои в понятные ошибки.
- Не добавляйте auto-approval по shell prefix, plaintext secrets, unbounded reads или отображение raw terminal escapes.
- Оставляйте короткие комментарии только для неочевидных инвариантов и обходов.

Запускайте `cargo fmt`; не форматируйте Rust вручную в обход стандартного formatter.

## Изменения интерфейса

У каждого доступного видимого действия должны быть:

- настоящая mouse hit region;
- путь через клавиатуру/фокус;
- локализованный текст во всех поддерживаемых locale UI;
- рабочее поведение при узком и широком терминале;
- безопасный рендер недоверенного содержимого.

Обрезанный, скрытый или disabled control не должен отправлять действие. Проверяйте scrolling с выбранными строками за экраном и видимость фокуса.

Отрисуйте подходящие детерминированные сцены:

```bash
cargo run --locked --example ui_gallery -- 54 24 chat en dark
cargo run --locked --example ui_gallery -- 120 40 chat ru dark
cargo run --locked --example ui_gallery -- 160 50 mcp-add en light
```

Для новой повторно используемой сцены или ветки layout расширьте `examples/ui_gallery.rs` и CI, а не ограничивайтесь screenshot.

Если изменилась видимая демонстрация README, пересоберите её в Windows из настоящего gallery renderer:

```powershell
.\scripts\render-readme-demo.ps1
```

Перед commit просмотрите новый `assets/demo.gif`. Не заменяйте его макетом, который со временем разойдётся с реальным интерфейсом.

## Тесты

Добавляйте regression test для каждого воспроизводимого бага. Узкие unit tests размещайте рядом с модулем в `src/`; каталог `tests/` используйте для поведения, которое пересекает публичные или модульные границы. Тесты внутри `src/` являются стандартными Rust unit tests и не входят в release binary.

Проверяйте важные ошибки, а не только успешный путь:

- malformed, empty, oversized, duplicate и stale input;
- timeout и cancellation;
- torn persistence и recovery после restart;
- path traversal, symlink/reparse escape и privacy denial;
- незавершённый streamed output;
- конфликтующие ручные Git-изменения;
- узкий терминал, scrolling, клавиатуру и мышь.

Тесты должны быть детерминированными, по умолчанию offline и без личных путей или live credentials.

## Документация и совместимость

Обновляйте документацию при изменении конфигурации, CLI-флагов, key bindings, поведения провайдера, persistence, предположений безопасности или пользовательского workflow.

- Сохраняйте структурное соответствие английских и русских документов.
- Обновляйте все переводы UI для user-facing strings.
- Обновляйте `config.example.toml` для полей конфигурации.
- Добавляйте пользовательские изменения в [CHANGELOG.md](CHANGELOG.md).
- Сохраняйте совместимость session/config либо документируйте и тестируйте migration.

Не заявляйте поддержку возможности провайдера без реализации и проверки. Вместо этого указывайте явные границы.

## Обязательные проверки

Запустите те же gates, что и CI:

```bash
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Для изменений зависимостей или package также выполните:

```bash
cargo package --locked --allow-dirty --no-verify
```

Pull request не должен содержать generated build output, release binaries, локальную конфигурацию, сессии, logs, credentials, backup dumps или metadata editor/OS.

## Pull requests

Открывайте pull request в ветку `main` репозитория [denysoid/DEcode](https://github.com/denysoid/DEcode). Draft pull request подходит для раннего обсуждения архитектуры или платформы; переводите его в ready только после успешных gates и полного описания.

Pull request должен содержать:

- конкретную проблему и ожидаемое поведение;
- минимальную целостную реализацию;
- влияние на security, persistence, совместимость и платформы;
- regression tests успешного и важных ошибочных путей;
- точные команды и результаты проверки;
- screenshots или gallery output для видимых изменений UI;
- обновления документации EN/RU и переводов UI, если они нужны.

Делайте commits удобными для review и не смешивайте refactor с несвязанными изменениями поведения. Maintainer может попросить разделить большую работу. Успешный CI обязателен, но не заменяет проверку trust boundaries и failure behavior.
