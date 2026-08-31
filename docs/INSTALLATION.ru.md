# Установка DEcode

[English](INSTALLATION.md) · [README](../README.ru.md) · [Настройка](CONFIGURATION.ru.md) · [Решение проблем](TROUBLESHOOTING.ru.md)

Сейчас DEcode распространяется в виде исходного кода. Собранные бинарники не хранятся в репозитории, поэтому надёжный способ установки — собрать зафиксированный набор зависимостей через Cargo.

## Поддерживаемые платформы

| Платформа | Проверки CI | Примечания |
|---|---|---|
| Windows 10/11 | Сборка, Clippy, тесты и жизненный цикл ConPTY | Запуск поддерживается из PowerShell и CMD |
| Linux | Сборка, Clippy, тесты, PTY и галерея UI | Для изображений из буфера Wayland используется `wl-paste`, X11 — `xclip` |
| macOS | Сборка, Clippy, тесты и жизненный цикл PTY | Для изображений из буфера используется встроенный `osascript` |

Другие Rust-платформы могут собираться, но не входят в матрицу CI проекта.

## Общие требования

Установите:

- [Git](https://git-scm.com/downloads);
- [Rust через rustup](https://rustup.rs/);
- учётные данные одного поддерживаемого провайдера моделей;
- терминал с поддержкой UTF-8.

Проверьте инструменты:

```text
git --version
rustc --version
cargo --version
```

Файл `rust-toolchain.toml` выбирает стабильный канал Rust и устанавливает `rustfmt` и `clippy` через rustup.

После клонирования не изменяйте `Cargo.lock` и используйте `--locked`. Тогда Cargo отклонит случайное изменение dependency graph, а не соберёт незаметно другой набор версий.

## Windows

### Требования

1. Установите Git for Windows.
2. Установите Rust через `rustup-init.exe` и выберите стандартный MSVC toolchain.
3. Если не найден linker, установите Visual Studio Build Tools с компонентами **Desktop development with C++** и Windows SDK.
4. Перезапустите терминал, чтобы `%USERPROFILE%\.cargo\bin` появился в `PATH`.

### Сборка через PowerShell

```powershell
git clone https://github.com/denysoid/DEcode.git
Set-Location DEcode
cargo build --locked --release
.\target\release\decode.exe --workspace "D:\путь\к\проекту"
```

### Сборка через CMD

```bat
git clone https://github.com/denysoid/DEcode.git
cd /d DEcode
cargo build --locked --release
target\release\decode.exe --workspace "D:\путь\к\проекту"
```

DEcode — терминальное приложение. При запуске `decode.exe` двойным щелчком окно может сразу закрыться из-за ошибки конфигурации. Запускайте его из PowerShell, CMD или Windows Terminal, чтобы увидеть сообщение об ошибке.

## Linux

Установите C toolchain и Git через пакетный менеджер дистрибутива.

Debian или Ubuntu:

```bash
sudo apt update
sudo apt install build-essential git
```

Fedora:

```bash
sudo dnf group install "Development Tools"
sudo dnf install git
```

Arch Linux:

```bash
sudo pacman -S --needed base-devel git
```

Установите Rust с [rustup.rs](https://rustup.rs/), загрузите окружение Cargo (либо откройте новый shell), затем выполните:

```bash
. "$HOME/.cargo/env"
git clone https://github.com/denysoid/DEcode.git
cd DEcode
cargo build --locked --release
./target/release/decode --workspace /путь/к/проекту
```

Вставка изображения из буфера необязательна. Установите `wl-clipboard` для Wayland или `xclip` для X11, если `Ctrl+V` должен читать изображения. Обычная вставка текста от этих программ не зависит.

## macOS

Установите инструменты командной строки Apple:

```bash
xcode-select --install
```

Установите Rust с [rustup.rs](https://rustup.rs/), откройте новый shell, затем выполните:

```bash
git clone https://github.com/denysoid/DEcode.git
cd DEcode
cargo build --locked --release
./target/release/decode --workspace /путь/к/проекту
```

Нативная вставка изображений использует системный `osascript`; дополнительные пакеты для буфера не нужны.

## Установка в каталог бинарников Cargo

Чтобы запускать `decode` без пути `target/release`, установите локальный checkout:

```bash
cargo install --locked --path .
```

Исполняемый файл попадёт в каталог Cargo: обычно `%USERPROFILE%\.cargo\bin` в Windows и `$HOME/.cargo/bin` в Linux/macOS. Если этот путь только что добавлен в `PATH`, перезапустите терминал.

После этого запускайте:

```text
decode --workspace /абсолютный/путь/к/проекту
```

В Windows операционная система принимает оба вида разделителей, но нативный путь в кавычках читается проще:

```bat
decode.exe --workspace "D:\projects\my-app"
```

## Запуск без установки

Аргументы Cargo заканчиваются перед `--`, а после него идут аргументы DEcode:

```bash
cargo run --locked --release -- --workspace /путь/к/проекту
```

Точный список параметров конкретной сборки выводит команда:

```bash
decode --help
```

## Обновление существующей копии

Сначала закоммитьте или временно сохраните локальные изменения, затем обновитесь без merge-коммита:

```bash
git pull --ff-only
cargo build --locked --release
```

Если DEcode был установлен через `cargo install --path .`, переустановите обновлённый checkout:

```bash
cargo install --locked --path . --force
```

## Удаление локальных результатов сборки

Результат сборки Cargo воспроизводим и не коммитится:

```bash
cargo clean
```

Если программа установлена через Cargo:

```bash
cargo uninstall decode
```

Эти команды не удаляют конфигурацию DEcode, записи keyring, сессии или каталог с исходным кодом.

## Проверка сборки из исходников

Запустите те же проверки корректности, которые выполняет CI:

```bash
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

После установки перейдите к [настройке](CONFIGURATION.ru.md), затем к [использованию](USAGE.ru.md).
