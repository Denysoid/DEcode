## What changed / Что изменено

Describe the problem and implemented behavior. / Опишите проблему и реализованное поведение.

## Verification / Проверка

List exact commands and results. Include screenshots or gallery output for UI changes. / Укажите точные команды и результаты. Для UI приложите screenshots или gallery output.

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo check --locked --all-targets`
- [ ] `cargo clippy --locked --all-targets -- -D warnings`
- [ ] `cargo test --locked --all-targets`
- [ ] Relevant failure, cancellation, and recovery paths were tested
- [ ] User-facing strings and documentation were updated where needed
- [ ] No credentials, local configuration, sessions, logs, binaries, or generated files are included

## Safety impact / Влияние на безопасность

Explain changes to permissions, sandboxing, command execution, networking, persistence, secrets, or recovery. Write `None` when no trust boundary changes. / Опишите изменения permissions, sandbox, команд, сети, persistence, секретов или recovery. Если границы доверия не меняются, напишите `None`.
