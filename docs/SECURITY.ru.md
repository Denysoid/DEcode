# Модель безопасности DEcode

[English](SECURITY.md) · [Политика безопасности](../SECURITY.ru.md) · [Документация](README.ru.md) · [Архитектура](ARCHITECTURE.ru.md)

DEcode снижает риск разработки под управлением модели, но не может сделать безопасным произвольное выполнение кода. Пользователь отвечает за доверие провайдеру, подтверждения, Git review, резервные копии и последствия разрешённых команд.

## Модель угроз

DEcode считает недоверенными:

- ответы модели и streamed deltas;
- файлы репозитория, `AGENTS.md`, skills, profiles и project configuration;
- вывод tool, process, Git, MCP, LSP, index, plugin и hook;
- удалённые endpoints, архивы пакетов, OAuth responses и marketplace metadata;
- имена файлов, terminal text и metadata вложений.

Локальный пользователь, явные CLI/process environment и пользовательская конфигурация вне workspace имеют больше полномочий. Источник с меньшим доверием может сузить поведение, но не может выдать себе credentials, executable hooks, privileged endpoints или расширенные permissions.

## Изоляция файловой системы

Файловые операции модели проходят через workspace capability sandbox. Resolver отклоняет:

- абсолютные пути за пределами выданного root;
- traversal через `..`;
- escape через symlink, junction и reparse point;
- пути, запрещённые privacy policy;
- replacement race, обнаруженный до commit.

Чтение, обход каталогов, поиск и output ограничены. Изменяемые файлы используют точные single-match patches либо временный файл в целевом каталоге с последующим атомарным persist. Patch policy может требовать ручного review каждого hunk.

Checkpoint rewind ограничен изменениями, отнесёнными к записанному checkpoint агента. Он не выполняет разрушительный reset всего репозитория и отказывается перезаписывать конфликтующие ручные правки.

## Выполнение команд

Команды модели получают:

- фиксированный рабочий каталог;
- null stdin;
- execution и idle timeouts;
- ограниченный captured output;
- отмену и очистку дерева процессов;
- независимую границу harness confirmation.

Strict allowlist сопоставляет точные resolved argument vectors из доверенной конфигурации. Shell prefixes, interpreters, build tools, aliases или заявление модели о безопасности команды не подходят. Session grants точны, versioned, непостоянны и не обходят forced/destructive review.

Интерактивные вкладки Terminal управляются пользователем и не являются auto-approval путём для model tools.

## Streaming и вызовы инструментов

Потоковый live text — только presentation state. Tool call может выполниться лишь после получения и parsing полного авторитетного ответа. Оборванный stream, неверный envelope, отмена, timeout или parse failure не выполняют частичный вызов.

Результаты tools записываются с причинными identifiers. Resume начинается с последней durable-границы. Операция с неизвестным внешним эффектом выводится для recovery, а не повторяется молча.

## Сеть и провайдеры

- У запросов есть общий и stream-idle timeouts, конечный exponential backoff и ограниченный `Retry-After`.
- Авторизацию и wire protocol выбирает типизированный provider adapter.
- Ответы `401` и `403` автоматически не повторяются.
- Обычный HTTP запрещён, кроме явного loopback development opt-in.
- URL должны быть абсолютными и не могут содержать credentials или fragments.
- Error payloads ограничиваются по размеру и очищаются до показа/логирования.

Смена провайдера не изменяет разрешения файлов или команд. Пользовательский compatible endpoint — отдельное решение о доверии, потому что он получает prompts и выбранные вложения.

## Вложения и буфер обмена

Выбранные человеком файлы и clipboard images ограничиваются, типизируются, хешируются и копируются в content-addressed хранилище сессии. Перед сетевым использованием DEcode снова проверяет размер и SHA-256. Provider adapter проверяет requested modality до отправки данных.

MIME или расширение не делает файл исполняемым. Symlink selection отклоняется. Внешние и временные пути принимаются только через явное действие человека в UI/paste и заменяются ссылкой на сохранённый blob в durable history.

## Сессии, пауза и восстановление

Записи сессий — checksummed append-only JSONL с flush/fsync и восстановлением torn tail. Периодическое сжатие журнала записывает атомарную замену. Сбой процесса может удалить незакоммиченный хвост, но не должен переписать подтверждённые ранние записи.

Пауза отменяет активную работу и сохраняет подтверждённую причинную границу. Она не может сохранить ещё не полученный provider token или гарантировать остановку стороннего процесса до необратимого внешнего эффекта. Неизвестные эффекты остаются явными.

## Целостность контекста

Сжатие сохраняет начальную задачу и новейшую полную причинную группу. Старый восстанавливаемый output сжимается раньше обязательного недавнего состояния. Если обязательное состояние не помещается в бюджет, DEcode отклоняет запрос вместо отправки причинно неполной реконструкции.

Настроенный максимум не доказывает вместимость провайдера. Указывайте реальный документированный предел модели/deployment.

## Изоляция субагентов

Research agents получают read-only capability. Writer agents изменяют иерархические отдельные Git worktrees и планируются через DAG-зависимости и file claims. Интеграция, включая nested writer descendants, требует review.

Ограничены depth, fan-out, concurrency, iterations, transcript, output, wall time, токены агента и всей ветки. Параллельные calls атомарно резервируют бюджет. MCP субагентов выключен по умолчанию, а после включения применяет отдельные per-server tool policies и никогда не наследует shell auto-approval.

## MCP, LSP, плагины, hooks и embeddings

- MCP STDIO/HTTP connections являются доверенной пользовательской конфигурацией; OAuth использует Authorization Code с PKCE, loopback callback, state validation и keyring.
- LSP остаётся read-only на границе агента и никогда не скачивает server executable.
- Marketplace packages требуют точный SHA-256 и ограниченную ZIP validation до атомарной активации.
- Project hooks не могут исполняться; executable lifecycle hooks относятся к доверенной локальной конфигурации пользователя.
- Remote embeddings по умолчанию выключены и получают только privacy-filtered chunks с лимитами количества и размера.

Проверяйте каждый внешний сервер или пакет отдельно. Валидный digest доказывает идентичность пакета, но не безопасность его содержимого.

## Секреты и логи

API-ключи читаются из process environment, явного env file вне workspace или keyring операционной системы. Обычный TOML и session JSONL хранят ссылки keyring, а не значения секретов. Runtime secrets используют secrecy wrappers и sensitive HTTP headers.

Structured tracing записывает identifiers, outcomes и timing, но исключает prompts, response bodies, headers, credentials, hook output и child-process stderr. Ошибки provider и MCP очищаются с учётом настроенных секретов. Публичную диагностику всё равно нужно проверять вручную: пути репозитория и business data могут быть чувствительными, даже если это не credentials.

## Безопасность отображения в терминале

Текст provider, tool, process, file, MCP, LSP, GitHub и plugin очищается от опасных terminal controls и bidirectional formatting до Ratatui layout или syntax highlighting. Логи TUI пишутся в файлы, а не stdout/stderr, поэтому диагностика не нарушает владение терминалом.

## Рекомендуемая безопасная настройка

1. Работайте в Git-репозитории с известной базой и отдельной резервной копией.
2. Сначала оставьте `agent.shell.confirmation_mode = "always"`.
3. Храните секреты вне workspace и не коммитьте заполненные config/env files.
4. Используйте HTTPS; разрешайте insecure loopback только для намеренного локального сервиса.
5. Не включайте MCP субагентов, remote embeddings, плагины и executable hooks без необходимости.
6. Устанавливайте LSP и plugin executables только из проверенных источников.
7. Задавайте context/token budgets по реальным пределам провайдера.
8. Читайте каждую разрушительную команду и patch до approval.
9. Проверяйте Git status/diff и запускайте тесты проекта до commit.

## Ограничения безопасности

Ни один локальный агент не может полностью исключить:

- одобрение пользователем вредного, но синтаксически корректного действия;
- сохранение отправленных данных доверенным провайдером или интеграцией;
- эксплуатацию командой уязвимости операционной системы или зависимости;
- внешние side effects, завершённые до отмены в DEcode;
- утечку данных, уже разрешённую слишком широкими workspace или privacy rule.

Для недоверенных репозиториев и опасных команд используйте одноразовое окружение или sandbox операционной системы.

## Сообщение об уязвимости

Не создавайте public issue с exploit details, credentials, приватным кодом или чувствительными логами. Следуйте [политике безопасности](../SECURITY.ru.md) и используйте GitHub private vulnerability reporting, если оно доступно.
