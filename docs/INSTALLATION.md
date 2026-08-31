# Installing DEcode

[Русская версия](INSTALLATION.ru.md) · [README](../README.md) · [Configuration](CONFIGURATION.md) · [Troubleshooting](TROUBLESHOOTING.md)

DEcode is currently distributed as source code. The repository does not track compiled binaries, so the reliable installation path is to build the pinned dependency set with Cargo.

## Supported platforms

| Platform | CI coverage | Notes |
|---|---|---|
| Windows 10/11 | Build, Clippy, tests, and ConPTY lifecycle | PowerShell and CMD are supported launch shells |
| Linux | Build, Clippy, tests, PTY, and UI gallery | Wayland clipboard images use `wl-paste`; X11 uses `xclip` |
| macOS | Build, Clippy, tests, and PTY lifecycle | Clipboard images use the built-in `osascript` runtime |

Other Rust targets may compile, but they are not part of the project CI matrix.

## Common prerequisites

Install:

- [Git](https://git-scm.com/downloads);
- [Rust through rustup](https://rustup.rs/);
- credentials for one supported model provider;
- a UTF-8 terminal.

Verify the toolchain:

```text
git --version
rustc --version
cargo --version
```

The checked-in `rust-toolchain.toml` selects the stable channel and installs `rustfmt` and `clippy` through rustup.

After cloning, keep `Cargo.lock` unchanged and use `--locked`. This makes Cargo reject an accidental dependency-resolution change instead of silently building a different graph.

## Windows

### Prerequisites

1. Install Git for Windows.
2. Install Rust with `rustup-init.exe` and select the default MSVC toolchain.
3. If the linker is missing, install Visual Studio Build Tools with **Desktop development with C++** and a Windows SDK.
4. Restart the terminal after installation so `%USERPROFILE%\.cargo\bin` is available in `PATH`.

### Build with PowerShell

```powershell
git clone https://github.com/denysoid/DEcode.git
Set-Location DEcode
cargo build --locked --release
.\target\release\decode.exe --workspace "D:\path\to\project"
```

### Build with CMD

```bat
git clone https://github.com/denysoid/DEcode.git
cd /d DEcode
cargo build --locked --release
target\release\decode.exe --workspace "D:\path\to\project"
```

DEcode is a terminal application. Starting `decode.exe` by double-clicking can close the console immediately when configuration is invalid. Launch it from PowerShell, CMD, or Windows Terminal so the error remains visible.

## Linux

Install a C toolchain and Git with the package manager for your distribution.

Debian or Ubuntu:

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

Install Rust from [rustup.rs](https://rustup.rs/), load Cargo's environment (or open a new shell), then build:

```bash
. "$HOME/.cargo/env"
git clone https://github.com/denysoid/DEcode.git
cd DEcode
cargo build --locked --release
./target/release/decode --workspace /path/to/project
```

Clipboard image paste is optional. Install `wl-clipboard` for Wayland or `xclip` for X11 if `Ctrl+V` should read bitmap images. Text paste does not depend on these helpers.

## macOS

Install Apple's command-line tools:

```bash
xcode-select --install
```

Install Rust from [rustup.rs](https://rustup.rs/), open a new shell, then build:

```bash
git clone https://github.com/denysoid/DEcode.git
cd DEcode
cargo build --locked --release
./target/release/decode --workspace /path/to/project
```

The native image clipboard path uses macOS `osascript`; no additional clipboard package is required.

## Install into Cargo's binary directory

To run `decode` without referencing `target/release`, install the local checkout:

```bash
cargo install --locked --path .
```

The executable is placed in Cargo's binary directory, normally `%USERPROFILE%\.cargo\bin` on Windows and `$HOME/.cargo/bin` on Linux/macOS. Reopen the shell if that directory was just added to `PATH`.

Then run:

```text
decode --workspace /absolute/path/to/project
```

On Windows, both slash styles are accepted by the operating system, but quoted native paths are easier to read:

```bat
decode.exe --workspace "D:\projects\my-app"
```

## Run without installing

Cargo arguments end before `--`; DEcode arguments follow it:

```bash
cargo run --locked --release -- --workspace /path/to/project
```

Use `--help` to view the CLI accepted by the exact build:

```bash
decode --help
```

## Update an existing checkout

Commit or stash local edits first, then update without creating a merge commit:

```bash
git pull --ff-only
cargo build --locked --release
```

If DEcode was installed with `cargo install --path .`, reinstall the updated checkout:

```bash
cargo install --locked --path . --force
```

## Remove local build output

Cargo build output is reproducible and is not committed:

```bash
cargo clean
```

If installed through Cargo:

```bash
cargo uninstall decode
```

This does not remove DEcode configuration, keyring entries, sessions, or the source checkout.

## Verify a source build

Run the same correctness gates used by CI:

```bash
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Continue with [Configuration](CONFIGURATION.md), then [Usage](USAGE.md).
