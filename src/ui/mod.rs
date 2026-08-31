pub mod actions;
pub mod agents;
pub mod app;
pub mod approval_center;
pub mod automation;
pub mod code_index;
pub mod confirm;
pub mod connections;
pub mod eta;
pub mod followups;
pub mod github;
pub mod i18n;
pub mod input;
pub mod instructions;
pub mod language;
pub mod lsp;
pub mod mascot;
pub mod mcp;
pub mod modes;
pub mod notifications;
pub mod onboarding;
pub mod palette;
pub mod patch_review;
pub mod permissions;
pub mod plugins;
pub mod privacy;
pub mod render;
pub mod review;
pub mod rewind;
pub mod runtime;
pub mod sessions;
pub mod shell;
pub mod side_chat;
pub mod skills;
mod syntax;
pub mod terminal;
pub mod usage;
pub mod whip;

use crate::{config::AppConfig, error::AppError};

pub async fn run(config: AppConfig) -> Result<(), AppError> {
    app::run_app(config).await
}

pub async fn run_onboarding() -> Result<onboarding::WizardOutcome, AppError> {
    app::run_onboarding().await
}
