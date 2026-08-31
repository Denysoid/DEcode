# Примеры DEcode

[English](README.md) · [Документация проекта](../docs/README.ru.md)

Эти файлы являются безопасными исходными примерами, а не активной конфигурацией. DEcode не загружает их прямо из этого каталога.

## Структура

```text
examples/
├── configuration/
│   ├── agent-profile.toml   ограниченный research/writer profile
│   ├── command.toml         пользовательская slash command
│   ├── hook.toml            доверенный пользовательский lifecycle hook
│   ├── instructions.md      явный файл пользовательских инструкций
│   └── privacy.ignore       дополнительные общесистемные privacy rules
├── plugin/
│   ├── plugin.json          минимальный manifest плагина
│   ├── commands/            slash commands из плагина
│   └── skills/              skills из плагина
└── ui_gallery.rs            детерминированный offline renderer TUI
```

## Примеры конфигурации

Перед копированием прочитайте комментарии в каждом файле. Уровень доверия зависит от размещения:

- project commands и profiles находятся в `<workspace>/.decode/` и не могут расширять полномочия пользователя;
- user commands, profiles, executable hooks и privacy rules находятся в системном configuration directory;
- основной файл инструкций задаётся явным абсолютным путём в доверенном `config.toml`;
- executable hooks должны быть обычными проверенными пользователем файлами без symlink.

Полный справочник находится в [config.example.toml](../config.example.toml). Приоритет, credentials и правила системного хранения описаны в [настройке](../docs/CONFIGURATION.ru.md).

## Пример плагина

Каталог `plugin/` показывает manifest с одним skill и одной slash command. Нативного исполняемого кода в нём нет. В распространяемом пакете `plugin.json` размещается в корне ZIP и перечисляет каждый добавляемый путь в `components`.

Перед установкой или публикацией плагина:

1. используйте стабильный reverse-domain ID;
2. оставляйте каждый component внутри package root;
3. проверяйте names, versions и relative paths;
4. создавайте ZIP без результатов сборки и секретов;
5. вычисляйте и публикуйте точный SHA-256 digest;
6. проверяйте install, enable, disable, update и removal в одноразовом профиле.

Проверенный digest подтверждает идентичность пакета, но не его безопасность. Проверяйте каждый contribution до включения.

## Галерея UI

Gallery рендерит настоящий UI в детерминированный `TestBackend`; terminal session и credentials провайдера не нужны.

```bash
cargo run --locked --example ui_gallery -- 120 40 chat en dark
cargo run --locked --example ui_gallery -- 120 40 mcp-add ru dark
cargo run --locked --example ui_gallery -- 100 32 lsp-add en light
```

Аргументы: `width height screen locale theme`. Поддерживаемые screens: `chat`, `mcp`, `mcp-add`, `lsp` и `lsp-add`; неизвестное значение рендерит состояние chat. CI проверяет поддерживаемую матрицу locale, themes, screens и размеров терминала.
