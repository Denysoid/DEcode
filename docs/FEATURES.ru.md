# Матрица возможностей DEcode

[English](FEATURES.md) · [Документация](README.ru.md) · [Использование](USAGE.ru.md) · [Безопасность](SECURITY.ru.md)

Эта матрица отделяет реализованное поведение от явных границ. Она описывает текущее дерево исходников, но не обещает одинаковых возможностей у каждой сторонней модели, deployment, terminal или учётной записи.

## Основные возможности

| Область | Реализовано | Явная граница |
|---|---|---|
| API моделей | Azure/OpenAI Responses; нативные Gemini и Anthropic; Bedrock Mantle bearer Responses; нативный Bedrock Runtime `ConverseStream`; явные Chat Completions adapters для OpenRouter, xAI, Groq, Mistral, DeepSeek, Together, Fireworks, Cerebras, Perplexity, NVIDIA NIM, SambaNova, Moonshot, DashScope, Hugging Face, GitHub Models и Ollama | Имена моделей, доступность API, окна контекста и policy аккаунта определяет провайдер; нестандартные поля требуют явной настройки |
| Streaming | Типизированные SSE/WebSocket events, live preview, stream-idle timeout, конечные retry/backoff и авторитетный parsing полного хода | Закрытый stream нельзя продолжить со следующего токена; частичный tool call никогда не выполняется |
| Вложения | Изображения, документы, текст, аудио и видео; файловый браузер `@`; множественный выбор; вставка изображения из системного буфера; вставка/drop путей; content-addressed SHA-256 хранилище сессии | Реальные modality зависят от провайдера/модели; по умолчанию 16 файлов и 50 МиБ на файл/ход; неподдерживаемый input отклоняется до запроса |
| Большие вставки | Слишком большой текст composer становится durable текстовым вложением | Ввод больше предела вложения отклоняется, а не обрезается молча |
| Usage и стоимость | API token ledger, cached-token accounting, long-context tiers, встроенные тарифы, ограниченное обновление provider catalog, локальные overrides и исторический пересчёт | Динамическая цена аккаунта/региона/SKU требует точного локального тарифа; показанная стоимость является оценкой |
| Терминальный UI | Mouse hit regions, полные клавиатурные пути, menu bar, шесть вкладок, cards, context menus, palettes, адаптивные sidebars, themes и анимация | Это terminal-native, а не desktop/browser UI; поведение host terminal всё равно влияет |
| Локализация | Двенадцать locale интерфейса и детерминированные renders всех языков | Документация репозитория напрямую поддерживается на английском и русском; остальные переводы документации не поставляются |
| Первый запуск | Язык, провайдер, модель/deployment, endpoint, workspace, назначение, контекст, transport, retry и нужные AWS settings; секрет сохраняется в keyring | Мастер не может создать доступ у провайдера или угадать недокументированный deployment |
| Конфигурация | Типизированный TOML, приоритет environment и CLI; ограниченный `.decode.toml`; явные instructions и env files | Конфигурация репозитория не может выбирать секреты, привилегированные endpoints, исполняемые hooks, MCP-серверы или расширенные разрешения |
| Сессии | JSONL resume, fork, rename, pin, archive, search, checksums и repair torn tail | Незавершённые сетевые данные отменяются и не считаются durable-полученными |
| Пауза и recovery | Срочная отмена, durable-состояние `paused`, resume из подтверждённой истории, суммирование usage логического хода и явная отмена | Resume создаёт новый запрос; неизвестный внешний эффект требует проверки вместо blind replay |
| Контекст | Бюджет отдельной сессии, сохранённый default новых сессий, stateless/stateful режимы и видимое причинное сжатие | Настроенный потолок не может увеличить реальный предел модели; обязательное недавнее состояние не удаляется молча ради запроса |
| Checkpoints | Rewind диалога и относимых файлов с Git conflict checks | Нужна Git-база; нет repository-wide hard reset или перезаписи конфликтующих ручных изменений |
| Диагностика | Structured JSON tracing жизненных циклов session, turn, provider, tool, MCP, sub-agent и checkpoint | Prompts, bodies, headers, credentials, hook output и child stderr исключены |

## Работа агента и интеграции

| Область | Реализовано | Явная граница |
|---|---|---|
| Режимы | Независимо комбинируемые Plan, Explore, Review, Goal, Deep Thinking и reasoning effort | Prompt режима не обходит границу tool или permission |
| Подтверждения | Решения для точной команды, revisioned session grants, review patch hunks и центр auto-approval | Forced, destructive, ambiguous и model-requested границы остаются ручными |
| Инструменты | Ограниченные read/search, точные patches, атомарная запись, запуск процессов, отмена и пределы вывода | Доступ к файлам остаётся workspace-capability scoped; интерактивный shell не является обходом для модели |
| Субагенты | Рекурсивное ограниченное дерево, роли research/writer, DAG-зависимости, иерархические worktrees, file claims, сообщения, review и WAL recovery | Глубина, fan-out, параллельность, время, output и token budgets обязательны |
| MCP | STDIO/HTTP, OAuth/keyring, permissions, enable/disable, UI добавления/редактирования и opt-in субагентов | Секреты остаются в keyring/environment references; субагенты не наследуют доступ автоматически |
| LSP | Жизненный цикл пользовательского сервера, diagnostics, symbols, definition, references и hover | DEcode не скачивает и не устанавливает language-server binaries |
| Индекс репозитория | Инкрементальный lexical/symbol search с необязательными hybrid embeddings и cache | Remote embeddings включаются явно и получают только ограниченные privacy-filtered chunks |
| Плагины | `plugin.json`, ZIP packages, marketplaces, digest verification, update/enable/remove и атомарная активация | Нет загрузчика произвольного native code; материализуются только известные ограниченные contribution types |
| Расширение | Skills, slash commands, agent profiles, user hooks, иерархические `AGENTS.md` и ограниченные includes | Project hooks не исполняются; project profiles могут только сужать доверие |
| GitHub | List, open, checkout и создание draft pull request через авторизованный `gh` | Нет unattended merge, force-push или неограниченного push |
| Интерактивные терминалы | Пользовательские PTY/ConPTY tabs с create, input, stop, close, paste, resize и mouse forwarding | Отделены от model tool execution и его approval system |
| Уведомления | Ограниченный in-app inbox и terminal bell по классам | Нет встроенного сервиса внешних push-уведомлений |
| Pixel | Сохраняемый необязательный mascot, idle interaction, moods и анимация из runtime state | Только отображение; не меняет ответы модели, стоимость, разрешения или scheduling |

## Лимиты субагентов

Стандартные значения `[agent.subagents]`:

```toml
max_parallel = 4
max_per_session = 16
max_depth = 3
max_children_per_agent = 4
max_tokens_per_agent = 150000
max_total_tokens_per_session = 500000
```

Корень имеет глубину 0. Параллельные запросы резервируют оценённый input и максимальный output до запуска. При достижении budget guard вкладка Agents предлагает явные решения **Raise budget +50K** и **Stop branch**. Конфигурация дополнительно ограничена скомпилированными hard caps.

Writer isolation, durable acknowledgement, scheduling file claims, DAG failure propagation, review descendants и unknown-effect recovery делают эту систему сильнее простого плоского worker pool. Но они не позволяют категорично утверждать, что DEcode превосходит каждый hosted coding agent: production infrastructure, закрытые evals, операционный масштаб и актуальное hosted-поведение невозможно воспроизвести из этого репозитория.

## Проверяемая поверхность

CI запускает formatting, compilation, Clippy с запретом warnings и все targets в Windows, Linux и macOS. Отдельные jobs проверяют Unix PTY, Windows ConPTY и 288 детерминированных UI renders для трёх экранов, двенадцати locale, двух themes и четырёх размеров терминала.

Такое покрытие уменьшает число regressions, но не является доказательством для каждого terminal emulator, proxy, revision модели, filesystem, сочетания SSH/multiplexer или сбоя провайдера.

## Намеренные нецели

- Скрывать ограничения провайдера за преобразованием с потерями или выдуманной capability.
- Выполнять незавершённый streamed output.
- Считать утверждение модели разрешением пользователя.
- Искать секреты или привилегированную конфигурацию в недоверенном репозитории.
- Автоматически устанавливать произвольные executables для LSP, MCP или plugins.
- Заменять Git review, тесты проекта, резервные копии или человеческое решение.
