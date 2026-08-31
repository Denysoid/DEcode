#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![recursion_limit = "256"]

use std::process::ExitCode;

use decode::{config::AppConfig, error::AppError, onboarding, telemetry, ui};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkippedSetupAction {
    Continue,
    UseLocalFallback,
}

fn skipped_setup_action(has_loaded_config: bool) -> SkippedSetupAction {
    if has_loaded_config {
        SkippedSetupAction::Continue
    } else {
        SkippedSetupAction::UseLocalFallback
    }
}

enum SetupResult {
    Completed(Box<AppConfig>),
    Skipped(decode::config::UiLanguage),
    Cancelled,
}

#[tokio::main]
async fn main() -> ExitCode {
    restore_startup_language();
    let mut config = match AppConfig::load() {
        Ok(c) => c,
        Err(error) if onboarding::should_offer(&error) => match run_setup().await {
            Ok(SetupResult::Completed(config)) => *config,
            Ok(SetupResult::Skipped(language)) => {
                if let Err(error) = finish_skipped_setup(None, language) {
                    eprintln!("{}: {error}", ui::i18n::text(ui::i18n::Text::FatalError));
                    return ExitCode::FAILURE;
                }
                match AppConfig::load() {
                    Ok(config) => config,
                    Err(error) => {
                        eprintln!("{}: {error}", ui::i18n::text(ui::i18n::Text::FatalError));
                        return ExitCode::FAILURE;
                    }
                }
            }
            Ok(SetupResult::Cancelled) => return ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{}: {error}", ui::i18n::text(ui::i18n::Text::FatalError));
                return ExitCode::FAILURE;
            }
        },
        Err(e) => {
            eprintln!("{}: {e}", ui::i18n::text(ui::i18n::Text::FatalError));
            return ExitCode::FAILURE;
        }
    };
    apply_ui_preferences(&mut config);
    if onboarding::should_launch(config.ui.onboarding_completed) {
        match run_setup().await {
            Ok(SetupResult::Completed(reloaded)) => {
                config = *reloaded;
                apply_ui_preferences(&mut config);
            }
            Ok(SetupResult::Skipped(language)) => {
                if let Err(error) = finish_skipped_setup(Some(&mut config), language) {
                    eprintln!("{}: {error}", ui::i18n::text(ui::i18n::Text::FatalError));
                    return ExitCode::FAILURE;
                }
            }
            Ok(SetupResult::Cancelled) => return ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{}: {error}", ui::i18n::text(ui::i18n::Text::FatalError));
                return ExitCode::FAILURE;
            }
        }
    }

    let _guard = match telemetry::init(&config) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("{}: {e}", ui::i18n::text(ui::i18n::Text::FatalError));
            return ExitCode::FAILURE;
        }
    };

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "DEcode by denysoid starting"
    );

    // The setup action exits the TUI cleanly, then the same process opens the wizard.
    loop {
        match ui::run(config.clone()).await {
            Ok(()) if setup_requested() => match run_setup().await {
                Ok(SetupResult::Completed(mut reloaded)) => {
                    apply_ui_preferences(&mut reloaded);
                    config = *reloaded;
                }
                Ok(SetupResult::Skipped(language)) => {
                    if let Err(error) = finish_skipped_setup(Some(&mut config), language) {
                        tracing::error!(%error, "failed to skip interactive setup");
                        eprintln!("{}: {error}", ui::i18n::text(ui::i18n::Text::FatalError));
                        return ExitCode::FAILURE;
                    }
                }
                Ok(SetupResult::Cancelled) => return ExitCode::SUCCESS,
                Err(error) => {
                    tracing::error!(%error, "interactive reconfiguration failed");
                    eprintln!("{}: {error}", ui::i18n::text(ui::i18n::Text::FatalError));
                    return ExitCode::FAILURE;
                }
            },
            Ok(()) | Err(AppError::UserExit) => return ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(error = %e, "Fatal error");
                eprintln!("{}: {e}", ui::i18n::text(ui::i18n::Text::FatalError));
                return ExitCode::FAILURE;
            }
        }
    }
}

fn setup_requested() -> bool {
    matches!(
        onboarding::load_ui_preferences(),
        Ok(Some(preferences)) if !preferences.onboarding_completed
    )
}

async fn run_setup() -> Result<SetupResult, AppError> {
    match ui::run_onboarding().await? {
        ui::onboarding::WizardOutcome::Completed(answers) => {
            onboarding::persist(&answers)?;
            Ok(SetupResult::Completed(Box::new(AppConfig::load()?)))
        }
        ui::onboarding::WizardOutcome::Skipped(language) => Ok(SetupResult::Skipped(language)),
        ui::onboarding::WizardOutcome::Cancelled => Ok(SetupResult::Cancelled),
    }
}

fn finish_skipped_setup(
    config: Option<&mut AppConfig>,
    language: decode::config::UiLanguage,
) -> Result<SkippedSetupAction, AppError> {
    let action = skipped_setup_action(config.is_some());
    if let Some(config) = config {
        onboarding::persist_ui_preferences(language, true)?;
        apply_ui_preferences(config);
    } else {
        onboarding::persist_local_fallback(language)?;
    }
    ui::i18n::set_language(language);
    Ok(action)
}

fn apply_ui_preferences(config: &mut AppConfig) {
    match onboarding::load_ui_preferences() {
        Ok(Some(preferences)) => {
            config.ui.language = preferences.language;
            config.ui.onboarding_completed = preferences.onboarding_completed;
            if let Some(mascot_enabled) = preferences.mascot_enabled {
                config.ui.mascot_enabled = mascot_enabled;
            }
            if let Some(context_budget) = preferences.default_context_budget {
                config.agent.context_budget = context_budget.min(config.agent.max_context_budget);
            }
            ui::i18n::set_language(preferences.language);
        }
        Ok(None) => {}
        Err(error) => eprintln!("{}: {error}", ui::i18n::text(ui::i18n::Text::WarningLabel)),
    }
}

fn restore_startup_language() {
    if let Ok(Some(preferences)) = onboarding::load_ui_preferences() {
        ui::i18n::set_language(preferences.language);
    }
}

#[cfg(test)]
mod tests {
    use super::{SkippedSetupAction, skipped_setup_action};

    #[test]
    fn skipping_optional_setup_keeps_a_loaded_configuration_running() {
        assert_eq!(skipped_setup_action(true), SkippedSetupAction::Continue);
    }

    #[test]
    fn skipping_required_setup_does_not_close_the_application() {
        assert_eq!(
            skipped_setup_action(false),
            SkippedSetupAction::UseLocalFallback
        );
    }
}
