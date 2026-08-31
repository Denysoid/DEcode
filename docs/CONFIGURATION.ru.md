# Настройка DEcode

[English](CONFIGURATION.md) · [README](../README.ru.md) · [Установка](INSTALLATION.ru.md) · [Использование](USAGE.ru.md)

DEcode настраивается через интерактивный мастер, доверенный TOML-файл, переменные окружения или параметры командной строки. Начните с мастера, если вам не нужна воспроизводимая конфигурация конкретного deployment.

## Приоритет настроек

Для обычных параметров действует следующий приоритет:

1. аргументы командной строки;
2. соответствующие переменные окружения процесса;
3. явно выбранный или пользовательский `config.toml`;
4. встроенные значения по умолчанию.

Явный файл выбирается через `--config PATH` или `DECODE_CONFIG_FILE`. Без него DEcode ищет конфигурацию только в пользовательском системном каталоге. Произвольный `config.toml` из текущего репозитория не получает доверие автоматически.

Необязательный `<workspace>/.decode.toml` считается менее доверенным проектным файлом. Он может задавать только ограниченные безопасные настройки проекта, например контекст, Pixel и параметры UI. Через него нельзя внедрить ключи, endpoint провайдера, исполняемые hooks, MCP-серверы или расширенные разрешения.

## Интерактивная настройка

При первом запуске мастер запрашивает язык, провайдера, модель/deployment, endpoint, рабочую папку, предел контекста и параметры подключения. Секрет провайдера сохраняется в системном keyring под сервисом `decode-provider`. В созданный TOML записывается только имя учётной записи keyring.

Мастер можно пропустить. Если завершённой пользовательской конфигурации нет, он снова откроется при следующем запуске DEcode.

## Явный файл конфигурации

Скопируйте снабжённый пояснениями шаблон за пределы репозитория:

```powershell
New-Item -ItemType Directory -Force D:\decode-config
Copy-Item .\config.example.toml D:\decode-config\config.toml
Copy-Item .\examples\configuration\instructions.md D:\decode-config\instructions.md
```

```bash
mkdir -p "$HOME/.config/decode"
cp config.example.toml "$HOME/.config/decode/config.toml"
cp examples/configuration/instructions.md "$HOME/.config/decode/instructions.md"
```

Перед использованием замените все примеры путей, endpoint, deployment и тарифов. Затем передайте путь явно:

```text
decode --config /абсолютный/путь/config.toml --workspace /абсолютный/путь/к/проекту
```

Полный справочник полей находится в [config.example.toml](../config.example.toml). Точные параметры конкретной сборки выводит `decode --help`.

## Учётные данные

Порядок поиска учётных данных:

1. переменная окружения процесса для выбранного провайдера;
2. ключ выбранного провайдера в явно переданном `--env-file`;
3. учётная запись keyring, созданная мастером настройки.

AWS Bedrock Runtime использует стандартную цепочку учётных данных AWS SDK вместо API-ключа. Локальный Ollama может работать без ключа.

| Провайдер | Поддерживаемые переменные окружения |
|---|---|
| Azure OpenAI | `AZURE_OPENAI_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Google Gemini | `GEMINI_API_KEY`, `GOOGLE_API_KEY` |
| Anthropic | `ANTHROPIC_API_KEY` |
| Bedrock Mantle | `AWS_BEARER_TOKEN_BEDROCK` |
| Bedrock Runtime | Цепочка AWS SDK, дополнительно `AWS_REGION` и `AWS_PROFILE` |
| OpenRouter | `OPENROUTER_API_KEY` |
| xAI | `XAI_API_KEY` |
| Groq | `GROQ_API_KEY` |
| Mistral | `MISTRAL_API_KEY` |
| DeepSeek | `DEEPSEEK_API_KEY` |
| Together | `TOGETHER_API_KEY` |
| Fireworks | `FIREWORKS_API_KEY` |
| Cerebras | `CEREBRAS_API_KEY` |
| Perplexity | `PERPLEXITY_API_KEY`, `PPLX_API_KEY` |
| NVIDIA NIM | `NVIDIA_API_KEY`, `NVIDIA_NIM_API_KEY` |
| SambaNova | `SAMBANOVA_API_KEY` |
| Moonshot | `MOONSHOT_API_KEY` |
| Alibaba DashScope | `DASHSCOPE_API_KEY` |
| Hugging Face | `HF_TOKEN`, `HUGGINGFACE_API_KEY` |
| GitHub Models | `GITHUB_TOKEN`, `GH_TOKEN` |
| Ollama | необязательные `OLLAMA_API_KEY`, `DECODE_PROVIDER_API_KEY` |
| Пользовательский совместимый endpoint | `DECODE_PROVIDER_API_KEY` |

Задавайте временную переменную процесса, а не записывайте секрет в репозиторий:

```powershell
$env:OPENAI_API_KEY = "your-key"
.\target\release\decode.exe --provider openai --model "your-model" --workspace "D:\project"
```

```bash
export OPENAI_API_KEY="your-key"
./target/release/decode --provider openai --model "your-model" --workspace /path/to/project
```

Env-файл никогда не ищется автоматически. Передавайте его явно, храните за пределами workspace и оставляйте в нём только ключ выбранного провайдера:

```text
decode --env-file /вне/workspace/decode.env --workspace /путь/к/проекту
```

## Правила провайдера и endpoint

- Для Azure нужен либо полный `responses_url` с маршрутом Responses, либо `azure_base_url`; одновременное указание запрещено.
- OpenAI по умолчанию использует официальный Responses endpoint.
- Google и Anthropic используют нативные протоколы и стандартные endpoint.
- OpenAI-совместимые провайдеры используют явные адаптеры и стандартные HTTPS endpoint.
- Для `compatible` и Bedrock Mantle требуется явный endpoint.
- Обычный HTTP запрещён, кроме явно разрешённого loopback-адреса, прежде всего для локальной разработки с Ollama.
- URL должен быть абсолютным, содержать host и не содержать встроенные учётные данные или fragment.

Пример запуска Azure:

```powershell
$env:AZURE_OPENAI_API_KEY = "your-key"
.\target\release\decode.exe `
  --provider azure `
  --azure-base-url "https://YOUR-RESOURCE.openai.azure.com/openai/v1" `
  --deployment "YOUR-DEPLOYMENT" `
  --workspace "D:\project"
```

Пример запуска локального Ollama:

```bash
decode \
  --provider ollama \
  --model "your-installed-model" \
  --allow-insecure-loopback true \
  --workspace /path/to/project
```

Названия моделей и deployment, пределы контекста, поддерживаемые типы вложений и доступность API определяет выбранный провайдер. DEcode проверяет собственную маршрутизацию, но не может сделать неподдерживаемую комбинацию провайдера и модели поддерживаемой.

## Настройки контекста

`agent.context_budget` — активный бюджет отдельной сессии. `agent.max_context_budget` — доверенный верхний предел, доступный в runtime picker. Указывайте документированный размер контекста реальной модели или deployment: большее число не увеличивает возможности провайдера.

Каждая сессия запоминает выбранный бюджет. Изменение бюджета также задаёт значение по умолчанию для новых сессий, но не переписывает старые.

Режимы контекста:

- `stateless`: DEcode при каждом запросе отправляет ограниченную реконструкцию необходимого состояния диалога;
- `stateful`: DEcode использует состояние ответов у провайдера, когда оно поддерживается, и сохраняет локальные данные восстановления.

Локальное сжатие сохраняет исходную задачу и новейшую полную причинную группу инструментов. Если даже последняя обязательная группа не помещается, DEcode отклоняет запрос вместо скрытого удаления необходимого контекста.

## Workspace, сессии и инструкции

- `--workspace` должен получать путь существующего каталога.
- `agent.workspace_root` задаёт значение по умолчанию, когда CLI-флаг отсутствует.
- `agent.session_dir` задаёт постоянное хранилище сессий; по умолчанию используется системный каталог данных.
- `agent.instructions_file` должен быть явно указанным абсолютным обычным UTF-8-файлом. Он не обнаруживается автоматически внутри репозитория.
- Инструкции репозитория загружаются из иерархических `AGENTS.md` и ограниченных директив `@include`.

В качестве безопасной основы используйте [examples/configuration/instructions.md](../examples/configuration/instructions.md).

## Команды и подтверждения

Оставляйте `agent.shell.confirmation_mode = "always"`, пока полностью не разберётесь с моделью доверия. Режим `strict_allowlist` пропускает подтверждение только для точных прямых read-only наборов аргументов из доверенной пользовательской конфигурации. Shell-префикс, интерпретатор, инструмент сборки или утверждение модели не считаются доказательством безопасности.

Правила timeout сопоставляются с настроенными префиксами команд. Ограничение вывода, отмена и завершение дерева процессов работают в любом режиме.

Примеры находятся в файлах:

- [agent-profile.toml](../examples/configuration/agent-profile.toml);
- [command.toml](../examples/configuration/command.toml);
- [hook.toml](../examples/configuration/hook.toml);
- [privacy.ignore](../examples/configuration/privacy.ignore).

## Необязательные интеграции

- MCP-серверы относятся к доверенной пользовательской конфигурации. Проектные файлы не могут их добавлять.
- LSP-серверы должны быть установлены заранее; DEcode не скачивает исполняемые файлы.
- Операции GitHub вызывают авторизованный `gh`, а изменяющие действия требуют подтверждения.
- Удалённые embeddings по умолчанию отключены, потому что отправляют отфильтрованные фрагменты кода настроенному провайдеру.
- Плагины следует устанавливать только из проверенных источников с подтверждённым digest.

## Логи и диагностика

Настройте `[logging].level` и `[logging].dir` либо используйте `--log-level` и `--log-dir`. Логи TUI пишутся в файлы, а не stdout, чтобы не повреждать экран терминала. Prompts, тела ответов, headers, учётные данные, вывод hooks и stderr дочерних процессов исключены из структурированного tracing.

## Проверка безопасности конфигурации

- Храните секреты за пределами репозитория.
- Используйте HTTPS, кроме намеренно выбранного loopback-сервиса.
- Проверьте точные модель/deployment и реальный предел контекста.
- Не отключайте подтверждение команд без необходимости.
- Сначала оставьте MCP, удалённые embeddings и исполняемые hooks выключенными.
- Храните сессии и worktree субагентов вне каталога исходного кода.
- После обновления запускайте `decode --help`: исполняемый файл — окончательный источник доступных параметров.
