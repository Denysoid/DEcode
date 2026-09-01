# DEcode

**Локальный AI-агент для программирования с безопасным терминальным интерфейсом.**

[English](README.md) · [Документация](docs/README.ru.md) · [Возможности](docs/FEATURES.ru.md) · [Безопасность](docs/SECURITY.ru.md) · [Участие в разработке](CONTRIBUTING.ru.md)

[![CI](https://github.com/denysoid/DEcode/actions/workflows/terminal-matrix.yml/badge.svg)](https://github.com/denysoid/DEcode/actions/workflows/terminal-matrix.yml)
[![Релиз](https://img.shields.io/github/v/release/denysoid/DEcode?display_name=tag&sort=semver)](https://github.com/denysoid/DEcode/releases)
[![Лицензия: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust: stable](https://img.shields.io/badge/Rust-stable-orange.svg)](rust-toolchain.toml)
[![Платформы](https://img.shields.io/badge/platform-Windows%20%7C%20Linux%20%7C%20macOS-lightgrey.svg)](docs/INSTALLATION.ru.md)

<p align="center">
  <img src="assets/demo.gif" alt="Терминальный интерфейс DEcode: coding session, MCP, LSP и managed connections" width="1200">
</p>

> [!IMPORTANT]
> DEcode находится на стадии ранней публичной версии. Он умеет выполнять команды и изменять файлы. Используйте Git, проверяйте подтверждения и diff, храните независимые резервные копии ценных данных.

DEcode объединяет удобный для мыши интерфейс на `ratatui` и ограниченный runtime coding-агента. Он потоково выводит ответы модели, сохраняет durable-сессии, показывает изменения, запускает инструменты через workspace capability sandbox и поддерживает несколько провайдеров, не передавая управление файлами и командами provider-specific коду.

## Что даёт DEcode

- Локальная работа в терминале с мышью и полноценным управлением с клавиатуры.
- Постоянные сессии с resume, fork, rename, pin, archive, search, pause, cancellation и восстановлением после сбоя.
- Точное ревью патчей, подтверждение команд, Git-checkpoints и conflict-safe rewind.
- Режимы Plan, Explore, Review, Goal и Deep Thinking с независимыми разрешениями.
- Рекурсивные research/writer субагенты с DAG-зависимостями, бюджетами, file claims и отдельными Git worktrees.
- MCP, LSP, индекс репозитория, необязательные embeddings, плагины, skills, hooks, пользовательские команды и GitHub pull-request workflows.
- Вложения изображений, документов, текста, аудио и видео с проверкой содержимого и возможностей провайдера.
- Двенадцать языков UI и детерминированные кроссплатформенные тесты терминальной раскладки.

Реализованное поведение и явные ограничения перечислены в [матрице возможностей](docs/FEATURES.ru.md).

## Состояние проекта

Версия `0.1.0` предназначена для технических пользователей, знакомых со сборкой Rust и настройкой провайдеров. Azure OpenAI — основной эталонный маршрут. Остальные adapters имеют локальное и интеграционное покрытие, но поведение всё равно может отличаться из-за revision модели, deployment, terminal emulator, proxy, региона и policy аккаунта.

Скомпилированные исполняемые файлы не хранятся в исходном репозитории. Соберите их локально либо используйте GitHub Release после его публикации.

## Документация

| Тема | English | Русский |
|---|---|---|
| Установка в Windows, Linux и macOS | [Installation](docs/INSTALLATION.md) | [Установка](docs/INSTALLATION.ru.md) |
| Провайдеры, ключи, endpoint и контекст | [Configuration](docs/CONFIGURATION.md) | [Настройка](docs/CONFIGURATION.ru.md) |
| Задачи, файлы, сессии, пауза и recovery | [Usage](docs/USAGE.md) | [Использование](docs/USAGE.ru.md) |
| Ошибки запуска, API, контекста и вложений | [Troubleshooting](docs/TROUBLESHOOTING.md) | [Решение проблем](docs/TROUBLESHOOTING.ru.md) |
| Управление клавиатурой и мышью | [Keymap](docs/KEYMAP.md) | [Управление](docs/KEYMAP.ru.md) |
| Границы доверия | [Security model](docs/SECURITY.md) | [Модель безопасности](docs/SECURITY.ru.md) |
| Устройство runtime | [Architecture](docs/ARCHITECTURE.md) | [Архитектура](docs/ARCHITECTURE.ru.md) |

## Требования

- Актуальный стабильный Rust через [rustup](https://rustup.rs/) с `rustfmt` и `clippy`.
- Git для checkout, checkpoints, rewind и writer worktrees.
- Терминал с UTF-8; мышь необязательна.
- Учётные данные одного поддерживаемого провайдера, кроме локального Ollama без ключа.
- Опционально: авторизованный GitHub CLI (`gh`) для PR и установленные language servers для LSP.

## Сборка и запуск

### Скачивание опубликованного релиза

Когда версия опубликована, скачайте архив для своей операционной системы из [GitHub Releases](https://github.com/denysoid/DEcode/releases) вместе с его файлом `.sha256`. До распаковки проверьте checksum через `Get-FileHash` в Windows, `sha256sum --check` в Linux или `shasum -a 256 --check` в macOS. В release archive входят нативный executable, оба README и лицензия.

### Сборка из исходного кода

Один раз клонируйте репозиторий:

```bash
git clone https://github.com/denysoid/DEcode.git
cd DEcode
cargo build --locked --release
```

### Windows PowerShell

```powershell
.\target\release\decode.exe --workspace "D:\projects\my-app"
```

### Windows CMD

```bat
target\release\decode.exe --workspace "D:\projects\my-app"
```

### Linux и macOS

```bash
./target/release/decode --workspace /home/user/projects/my-app
```

Во время разработки можно запускать через Cargo:

```bash
cargo run --locked --release -- --workspace /абсолютный/путь/к/проекту
```

`--workspace` требует путь к существующему каталогу. DEcode — терминальное приложение; в Windows запускайте EXE из PowerShell, CMD или Windows Terminal, а не двойным щелчком, чтобы видеть ошибки запуска.

Требования платформ, установка через Cargo, обновление и удаление описаны в [руководстве по установке](docs/INSTALLATION.ru.md).

## Первый запуск

Если завершённой пользовательской конфигурации нет, мастер настройки запрашивает:

1. язык интерфейса;
2. провайдера и модель/deployment;
3. endpoint и способ авторизации, если они нужны;
4. workspace и назначение агента;
5. предел контекста, transport, timeout и retry.

Секреты из мастера сохраняются в keyring операционной системы под сервисом `decode-provider`. Они не записываются в TOML, журналы сессий или logs. Мастер можно пропустить; пока завершённого пользовательского конфига нет, он откроется снова.

Для воспроизводимого запуска передавайте оба пути явно:

```powershell
.\target\release\decode.exe `
  --config "D:\decode-config\config.toml" `
  --workspace "D:\projects\my-app"
```

```bash
./target/release/decode \
  --config "$HOME/.config/decode/config.toml" \
  --workspace "$HOME/projects/my-app"
```

Для полной ручной настройки скопируйте [config.example.toml](config.example.toml). CLI flags перекрывают environment variables, те перекрывают доверенный пользовательский config, а затем действуют встроенные defaults. Файл `.decode.toml` в workspace может задавать только ограниченный project-safe набор.

## Учётные данные и провайдеры

Предпочитайте мастер настройки или переменные окружения процесса. Env file никогда не ищется автоматически: передайте его явно и храните вне workspace.

```powershell
$env:AZURE_OPENAI_API_KEY = "your-key"
.\target\release\decode.exe --workspace "D:\projects\my-app"
```

```bash
export OPENAI_API_KEY="your-key"
./target/release/decode --workspace /home/user/projects/my-app
```

```text
decode --env-file /вне/workspace/decode.env --workspace /путь/к/проекту
```

Не коммитьте API keys, bearer tokens, endpoint конкретного аккаунта, данные сессий или заполненную локальную конфигурацию. [.env.example](.env.example) показывает формат файла, а [руководство по настройке](docs/CONFIGURATION.ru.md) перечисляет переменные всех провайдеров.

| Семейство API | Поддерживаемые маршруты |
|---|---|
| Responses | Azure OpenAI, OpenAI, Bedrock Mantle и явные compatible endpoints |
| Нативные | Google Gemini, Anthropic Claude, AWS Bedrock Runtime `ConverseStream` |
| Chat Completions | OpenRouter, xAI, Groq, Mistral, DeepSeek, Together, Fireworks, Cerebras, Perplexity, NVIDIA NIM, SambaNova, Moonshot, DashScope, Hugging Face, GitHub Models, Ollama |

Смена провайдера меняет сериализацию запроса и авторизацию, но не полномочия tools. Неподдерживаемый тип вложения отклоняется до сетевой передачи, а не преобразуется или обрезается молча.

## Первая полезная задача

1. Запустите программу в нужном workspace.
2. Опишите результат, ограничения и команду проверки.
3. Добавьте файлы через `@`, вставьте изображение через `Ctrl+V` либо вставьте/перетащите абсолютные пути.
4. Проверяйте каждую запрошенную команду и patch.
5. Прочитайте финальный ответ и `git diff`.
6. Перед коммитом запустите тесты проекта.

Пример запроса:

```text
Исправь падающий тест parser без изменения публичного формата. Добавь
регрессионный тест для некорректного input, запусти точный тест и formatter,
затем перечисли изменённые файлы и оставшиеся риски.
```

## Основное управление

| Ввод | Действие |
|---|---|
| `F10` | Открыть menu bar |
| `/` в пустом composer | Открыть палитру команд |
| `@` | Прикрепить один или несколько файлов из workspace, home, Рабочего стола, Загрузок или любого корня/диска |
| `Ctrl+V` | Вставить текст или получить изображение из буфера |
| `Alt+Enter` | Добавить новую строку |
| `Ctrl+N` / `Ctrl+O` | Создать сессию / открыть менеджер сессий |
| `Ctrl+M` | Выбрать модель, reasoning effort и бюджет контекста |
| `F6` / `F8` | Поставить на паузу или продолжить / отменить приостановленный ход |
| `F7` | Покормить или разбудить Pixel во время простоя |
| `Ctrl+C` / `Esc` | Прервать активную работу или покинуть текущий экран |
| `Tab` / `Shift+Tab` | Перемещаться между интерактивными элементами |

[Полное управление](docs/KEYMAP.ru.md) описывает диалоги, tool cards, списки, агентов, usage и интерактивные терминалы.

## Вложения

В одном ходе можно отправить текст и несколько файлов. Браузер `@` поддерживает навигацию и множественный выбор. Изображение из буфера сохраняется как байты, а не отправляется временным путём. Drag and drop совместимого терминала распознаётся через абсолютные пути, созданные terminal host.

Внешние файлы копируются в content-addressed store сессии и проверяются по SHA-256. Стандартные ограничения: 16 файлов, 50 МиБ на файл и 50 МиБ суммарно на ход. Большая вставка текста становится текстовым вложением и не обрезается в editor.

## Сессии, контекст и восстановление

Журналы сессий — checksummed append-only JSONL с восстановлением torn tail. Каждая сессия запоминает свой бюджет контекста; последнее явное изменение становится default новых сессий и не переписывает старые.

Локальное сжатие сохраняет начальную задачу и новейшую полную причинную группу tools, добавляет видимое событие и отклоняет запрос, который нельзя вместить без потери обязательного состояния. Суммарный usage сессии не равен размеру последнего provider request.

Pause и interruption сохраняют последнюю подтверждённую причинную границу. Resume создаёт новый provider request из durable history; DEcode не утверждает, что продолжает закрытый stream или выполнил незавершённый tool call. Суммы времени, токенов и стоимости логического хода включают завершённую работу до восстанавливаемой паузы/retry.

## Модель безопасности

Вывод модели, содержимое репозитория, tool output, данные MCP/LSP, плагины и удалённые ответы считаются недоверенными.

- Код проекта использует `#![forbid(unsafe_code)]` и строгие Clippy gates.
- Файловый доступ модели ограничен workspace capability и запрещает traversal и symlink/reparse escape.
- Изменения используют точные patches или атомарную запись и могут требовать hunk review.
- Команды имеют null stdin, пределы времени/вывода, отмену, очистку дерева процессов и approval policies.
- Незавершённые streamed calls никогда не выполняются.
- Секреты берутся из environment или OS keyring и очищаются из ошибок/tracing.
- MCP, remote embeddings, плагины, hooks и auto-approval остаются явными решениями о доверии.

Перед привилегированными интеграциями прочитайте [модель безопасности](docs/SECURITY.ru.md). Уязвимости отправляйте по [политике безопасности](SECURITY.ru.md), а не через public issue с exploit details.

## Архитектура

Оркестратор единолично владеет API, историей, tools, сессиями, checkpoints, субагентами, MCP/LSP, индексом, плагинами и snapshots UI. Код интерфейса отправляет typed commands и отображает immutable snapshots; бизнес-операции из UI не выполняются.

```text
ввод / TUI -> OrchestratorCommand -> agent actor -> API / tools / runtimes
     ^                                    |
     +----------- snapshots/events <------+
```

Перед межмодульными изменениями прочитайте [архитектуру](docs/ARCHITECTURE.ru.md).

## Структура репозитория

```text
src/                     код приложения/библиотеки и unit tests
tests/                   integration и acceptance tests
examples/ui_gallery.rs   детерминированный offline renderer TUI
examples/configuration/  примеры agent, command, hook, instructions и privacy
examples/plugin/         минимальный пример plugin package
assets/demo.gif          демонстрация README, собранная из реального UI
scripts/                 утилиты для maintainers
docs/                    парные EN/RU руководства пользователя и разработчика
.github/                  CI, ownership, issue forms и pull-request template
config.example.toml      полный аннотированный справочник конфигурации
```

Локальная конфигурация, сессии, logs, результаты сборки, release binaries, backups и приватные dumps намеренно исключены из Git.

## Разработка

Запустите те же gates, что и CI:

```bash
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Детерминированный визуальный тест не требует credentials:

```bash
cargo run --locked --example ui_gallery -- 54 24 chat en dark
cargo run --locked --example ui_gallery -- 120 40 chat ru dark
cargo run --locked --example ui_gallery -- 160 50 mcp-add en light
```

В Windows анимация README пересобирается из этих реальных gallery scenes командой:

```powershell
.\scripts\render-readme-demo.ps1
```

CI запускает Rust-проверки в Windows, Linux и macOS, проверяет работу Unix PTY, компилирует Windows ConPTY и рендерит 288 сочетаний языка, темы, экрана и размера терминала.

Перед изменениями прочитайте [CONTRIBUTING.ru.md](CONTRIBUTING.ru.md), а пользовательские изменения смотрите в [CHANGELOG.ru.md](CHANGELOG.ru.md).

## Поддержка и лицензия

Начните с [решения проблем](docs/TROUBLESHOOTING.ru.md) и [правил поддержки](SUPPORT.ru.md), затем найдите похожие issue и откройте подходящую структурированную форму с очищенной диагностикой. Отчёты безопасности отправляются по [SECURITY.ru.md](SECURITY.ru.md), а участие регулирует [Кодекс поведения](CODE_OF_CONDUCT.ru.md).

DEcode распространяется по [лицензии MIT](LICENSE).
