use std::time::Duration;

use super::actions::ClickRegionRegistry;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use futures_util::StreamExt as _;
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use ratatui_interact::{
    components::{Button, ButtonState, ButtonStyle, ButtonVariant},
    state::FocusManager,
};
use secrecy::SecretString;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr;

use crate::{
    agent::side_chat::has_visible_text,
    config::UiLanguage,
    error::AppError,
    onboarding::{
        CONTEXT_CHOICES, OnboardingAnswers, SetupProvider, validate_https_endpoint,
        validate_provider_endpoint,
    },
};

use super::{
    i18n::{Text, text_for},
    render::animated_d_frame,
};

const TICK_RATE: Duration = Duration::from_millis(90);
const MAX_FIELD_BYTES: usize = 4_096;
const MAX_MODEL_BYTES: usize = 256;
const MAX_ENDPOINT_BYTES: usize = 2_048;
const TRANSPORT_CHOICES: [&str; 3] = ["auto", "sse", "websocket"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Language,
    Provider,
    Details,
    Network,
    ProviderAdvanced,
    Review,
}

#[derive(Debug, Clone, Copy)]
enum Copy {
    FirstLaunch,
    Subtitle,
    MouseHint,
    ChooseLanguage,
    ChooseProvider,
    Language,
    Provider,
    Project,
    Review,
    Model,
    Endpoint,
    ApiKey,
    ProjectFolder,
    UseCase,
    ContextCompaction,
    Skip,
    Cancel,
    Back,
    SaveLaunch,
    Ready,
    KeyringSafe,
    DestructiveApproval,
    KeyboardFallback,
    ErrorModel,
    ErrorEndpoint,
    ErrorApiKey,
    ErrorWorkspace,
    ProviderLocalSuffix,
    ProviderCompatible,
    ProviderNativeSigV4,
    DefaultUseCase,
    OnboardingTooSmall,
    ErrorAdvanced,
}

#[cfg(test)]
impl Copy {
    const ALL: &'static [Self] = &[
        Self::FirstLaunch,
        Self::Subtitle,
        Self::MouseHint,
        Self::ChooseLanguage,
        Self::ChooseProvider,
        Self::Language,
        Self::Provider,
        Self::Project,
        Self::Review,
        Self::Model,
        Self::Endpoint,
        Self::ApiKey,
        Self::ProjectFolder,
        Self::UseCase,
        Self::ContextCompaction,
        Self::Skip,
        Self::Cancel,
        Self::Back,
        Self::SaveLaunch,
        Self::Ready,
        Self::KeyringSafe,
        Self::DestructiveApproval,
        Self::KeyboardFallback,
        Self::ErrorModel,
        Self::ErrorEndpoint,
        Self::ErrorApiKey,
        Self::ErrorWorkspace,
        Self::ProviderLocalSuffix,
        Self::ProviderCompatible,
        Self::ProviderNativeSigV4,
        Self::DefaultUseCase,
        Self::OnboardingTooSmall,
        Self::ErrorAdvanced,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Model,
    Endpoint,
    ApiKey,
    Workspace,
    UseCase,
    RequestTimeout,
    StreamIdleTimeout,
    MaxAttempts,
    RetryMinDelay,
    RetryMaxDelay,
    RetryAfterCap,
    AwsRegion,
    AwsProfile,
    AwsRoleArn,
    BedrockEndpoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Hit {
    Language(usize),
    Provider(usize),
    Field(Field),
    Context(usize),
    Transport(usize),
    Back,
    Next,
    Skip,
    Save,
    Cancel,
}

#[derive(Debug)]
pub enum WizardOutcome {
    Completed(Box<OnboardingAnswers>),
    Skipped(UiLanguage),
    Cancelled,
}

struct Wizard {
    step: Step,
    language: UiLanguage,
    provider: SetupProvider,
    model: String,
    endpoint: String,
    api_key: String,
    workspace: String,
    use_case: String,
    use_case_edited: bool,
    context_budget: u32,
    api_transport: String,
    request_timeout_secs: String,
    stream_idle_timeout_secs: String,
    max_attempts: String,
    retry_min_delay_ms: String,
    retry_max_delay_secs: String,
    retry_after_cap_secs: String,
    aws_region: String,
    aws_profile: String,
    aws_role_arn: String,
    bedrock_endpoint_url: String,
    focus: FocusManager<Hit>,
    clicks: ClickRegionRegistry<Hit>,
    frame: usize,
    error: Option<String>,
}

impl Wizard {
    fn new() -> Self {
        let answers = OnboardingAnswers::default();
        let mut wizard = Self {
            step: Step::Language,
            language: answers.language,
            provider: answers.provider,
            model: answers.model,
            endpoint: answers.endpoint,
            api_key: String::new(),
            workspace: answers.workspace.to_string_lossy().into_owned(),
            use_case: answers.use_case,
            use_case_edited: false,
            context_budget: answers.context_budget,
            api_transport: answers.api_transport,
            request_timeout_secs: answers.request_timeout_secs.to_string(),
            stream_idle_timeout_secs: answers.stream_idle_timeout_secs.to_string(),
            max_attempts: answers.max_attempts.to_string(),
            retry_min_delay_ms: answers.retry_min_delay_ms.to_string(),
            retry_max_delay_secs: answers.retry_max_delay_secs.to_string(),
            retry_after_cap_secs: answers.retry_after_cap_secs.to_string(),
            aws_region: answers.aws_region.unwrap_or_default(),
            aws_profile: answers.aws_profile.unwrap_or_default(),
            aws_role_arn: answers.aws_role_arn.unwrap_or_default(),
            bedrock_endpoint_url: answers.bedrock_endpoint_url.unwrap_or_default(),
            focus: FocusManager::new(),
            clicks: ClickRegionRegistry::new(),
            frame: 0,
            error: None,
        };
        wizard.rebuild_focus();
        wizard
    }

    fn rebuild_focus(&mut self) {
        self.focus = FocusManager::new();
        match self.step {
            Step::Language => {
                for index in 0..UiLanguage::ALL.len() {
                    self.focus.register(Hit::Language(index));
                }
                self.focus.register(Hit::Cancel);
                self.focus.set(Hit::Language(
                    UiLanguage::ALL
                        .iter()
                        .position(|language| *language == self.language)
                        .unwrap_or(0),
                ));
            }
            Step::Provider => {
                for index in 0..SetupProvider::ALL.len() {
                    self.focus.register(Hit::Provider(index));
                }
                self.focus.register(Hit::Back);
                self.focus.register(Hit::Skip);
                self.focus.register(Hit::Next);
                self.focus.set(Hit::Provider(
                    SetupProvider::ALL
                        .iter()
                        .position(|provider| *provider == self.provider)
                        .unwrap_or(0),
                ));
            }
            Step::Details => {
                for field in [
                    Field::Model,
                    Field::Endpoint,
                    Field::ApiKey,
                    Field::Workspace,
                    Field::UseCase,
                ] {
                    if (field != Field::Endpoint || self.provider.needs_endpoint())
                        && (field != Field::ApiKey || self.provider.needs_secret())
                    {
                        self.focus.register(Hit::Field(field));
                    }
                }
                for index in 0..CONTEXT_CHOICES.len() {
                    self.focus.register(Hit::Context(index));
                }
                self.focus.register(Hit::Back);
                self.focus.register(Hit::Skip);
                self.focus.register(Hit::Next);
                self.focus.set(Hit::Field(Field::Model));
            }
            Step::Network => {
                for index in 0..available_transports(self.provider).len() {
                    self.focus.register(Hit::Transport(index));
                }
                for field in [
                    Field::RequestTimeout,
                    Field::StreamIdleTimeout,
                    Field::MaxAttempts,
                    Field::RetryMinDelay,
                    Field::RetryMaxDelay,
                    Field::RetryAfterCap,
                ] {
                    self.focus.register(Hit::Field(field));
                }
                self.focus.register(Hit::Back);
                self.focus.register(Hit::Skip);
                self.focus.register(Hit::Next);
                self.focus.set(Hit::Transport(
                    available_transports(self.provider)
                        .iter()
                        .position(|transport| *transport == self.api_transport)
                        .unwrap_or(0),
                ));
            }
            Step::ProviderAdvanced => {
                for field in [
                    Field::AwsRegion,
                    Field::AwsProfile,
                    Field::AwsRoleArn,
                    Field::BedrockEndpoint,
                ] {
                    self.focus.register(Hit::Field(field));
                }
                self.focus.register(Hit::Back);
                self.focus.register(Hit::Skip);
                self.focus.register(Hit::Next);
                self.focus.set(Hit::Field(Field::AwsRegion));
            }
            Step::Review => {
                self.focus.register(Hit::Back);
                self.focus.register(Hit::Skip);
                self.focus.register(Hit::Save);
                self.focus.set(Hit::Save);
            }
        }
    }

    fn selected(&self) -> Option<Hit> {
        self.focus.current().copied()
    }

    fn activate(&mut self, hit: Hit) -> Option<WizardOutcome> {
        self.error = None;
        match hit {
            Hit::Language(index) => {
                if let Some(language) = UiLanguage::ALL.get(index).copied() {
                    self.language = language;
                    if !self.use_case_edited {
                        self.use_case = copy(language, Copy::DefaultUseCase).to_owned();
                    }
                    self.step = Step::Provider;
                    self.rebuild_focus();
                }
            }
            Hit::Provider(index) => {
                if let Some(provider) = SetupProvider::ALL.get(index).copied() {
                    if provider != self.provider {
                        self.api_key.clear();
                    }
                    self.provider = provider;
                    self.model = provider.suggested_model().to_owned();
                    if provider != SetupProvider::OpenAi && self.api_transport == "websocket" {
                        self.api_transport = "auto".to_owned();
                    }
                    self.focus.set(Hit::Provider(index));
                }
            }
            Hit::Context(index) => {
                if let Some(context) = CONTEXT_CHOICES.get(index).copied() {
                    self.context_budget = context;
                    self.focus.set(Hit::Context(index));
                }
            }
            Hit::Transport(index) => {
                if let Some(transport) = available_transports(self.provider).get(index) {
                    self.api_transport = (*transport).to_owned();
                    self.focus.set(Hit::Transport(index));
                }
            }
            Hit::Field(field) => self.focus.set(Hit::Field(field)),
            Hit::Back => {
                self.step = match self.step {
                    Step::Language => return Some(WizardOutcome::Cancelled),
                    Step::Provider => Step::Language,
                    Step::Details => Step::Provider,
                    Step::Network => Step::Details,
                    Step::ProviderAdvanced => Step::Network,
                    Step::Review if self.provider == SetupProvider::AwsBedrockRuntime => {
                        Step::ProviderAdvanced
                    }
                    Step::Review => Step::Network,
                };
                self.rebuild_focus();
            }
            Hit::Next => {
                self.step = match self.step {
                    Step::Language => Step::Provider,
                    Step::Provider => Step::Details,
                    Step::Details => {
                        if let Some(error) = self.validate_details() {
                            self.error = Some(error);
                            return None;
                        }
                        Step::Network
                    }
                    Step::Network => {
                        if let Some(error) = self.validate_advanced() {
                            self.error = Some(error);
                            return None;
                        }
                        if self.provider == SetupProvider::AwsBedrockRuntime {
                            Step::ProviderAdvanced
                        } else {
                            Step::Review
                        }
                    }
                    Step::ProviderAdvanced => {
                        if let Some(error) = self.validate_advanced() {
                            self.error = Some(error);
                            return None;
                        }
                        Step::Review
                    }
                    Step::Review => Step::Review,
                };
                self.rebuild_focus();
            }
            Hit::Save => {
                if let Some(error) = self.validate_details().or_else(|| self.validate_advanced()) {
                    self.error = Some(error);
                    self.step = Step::Details;
                    self.rebuild_focus();
                    return None;
                }
                return Some(WizardOutcome::Completed(Box::new(OnboardingAnswers {
                    language: self.language,
                    provider: self.provider,
                    model: self.model.trim().to_owned(),
                    endpoint: self.endpoint.trim().to_owned(),
                    api_key: SecretString::new(std::mem::take(&mut self.api_key).into()),
                    workspace: self.workspace.trim().into(),
                    context_budget: self.context_budget,
                    use_case: self.use_case.trim().to_owned(),
                    api_transport: self.api_transport.clone(),
                    request_timeout_secs: parse_u64(&self.request_timeout_secs, 120),
                    stream_idle_timeout_secs: parse_u64(&self.stream_idle_timeout_secs, 180),
                    max_attempts: parse_u32(&self.max_attempts, 5),
                    retry_min_delay_ms: parse_u64(&self.retry_min_delay_ms, 500),
                    retry_max_delay_secs: parse_u64(&self.retry_max_delay_secs, 30),
                    retry_after_cap_secs: parse_u64(&self.retry_after_cap_secs, 120),
                    aws_region: optional_value(&self.aws_region),
                    aws_profile: optional_value(&self.aws_profile),
                    aws_role_arn: optional_value(&self.aws_role_arn),
                    bedrock_endpoint_url: optional_value(&self.bedrock_endpoint_url),
                })));
            }
            Hit::Skip => return Some(WizardOutcome::Skipped(self.language)),
            Hit::Cancel => return Some(WizardOutcome::Cancelled),
        }
        None
    }

    fn validate_details(&self) -> Option<String> {
        if !has_visible_text(&self.model) || self.model.len() > MAX_MODEL_BYTES {
            return Some(copy(self.language, Copy::ErrorModel).to_owned());
        }
        if self.provider.needs_endpoint()
            && (!has_visible_text(&self.endpoint)
                || self.endpoint.len() > MAX_ENDPOINT_BYTES
                || validate_provider_endpoint(self.provider, "endpoint", self.endpoint.trim())
                    .is_err())
        {
            return Some(copy(self.language, Copy::ErrorEndpoint).to_owned());
        }
        if self.provider.needs_secret() && self.api_key.trim().is_empty() {
            return Some(copy(self.language, Copy::ErrorApiKey).to_owned());
        }
        let workspace = std::path::Path::new(self.workspace.trim());
        if !workspace.is_absolute() || !workspace.is_dir() {
            return Some(copy(self.language, Copy::ErrorWorkspace).to_owned());
        }
        None
    }

    fn validate_advanced(&self) -> Option<String> {
        if !available_transports(self.provider).contains(&self.api_transport.as_str()) {
            return Some("api.transport: auto | sse | websocket".to_owned());
        }
        for (name, value, maximum) in [
            (
                "api.request_timeout_secs",
                self.request_timeout_secs.as_str(),
                86_400,
            ),
            (
                "api.stream_idle_timeout_secs",
                self.stream_idle_timeout_secs.as_str(),
                86_400,
            ),
            ("api.max_attempts", self.max_attempts.as_str(), 5),
            (
                "api.retry_min_delay_ms",
                self.retry_min_delay_ms.as_str(),
                30_000,
            ),
            (
                "api.retry_max_delay_secs",
                self.retry_max_delay_secs.as_str(),
                30,
            ),
            (
                "api.retry_after_cap_secs",
                self.retry_after_cap_secs.as_str(),
                120,
            ),
        ] {
            if !valid_positive_u64(value, maximum) {
                return Some(format!("{name}: 1..={maximum}"));
            }
        }
        let retry_min = parse_u64(&self.retry_min_delay_ms, 500);
        let retry_max = parse_u64(&self.retry_max_delay_secs, 30);
        if retry_min > retry_max.saturating_mul(1_000) {
            return Some("api.retry_min_delay_ms > api.retry_max_delay_secs".to_owned());
        }
        for (name, value, limit) in [
            ("api.aws_region", self.aws_region.as_str(), 64),
            ("api.aws_profile", self.aws_profile.as_str(), 128),
            ("api.aws_role_arn", self.aws_role_arn.as_str(), 2_048),
            (
                "api.bedrock_endpoint_url",
                self.bedrock_endpoint_url.as_str(),
                2_048,
            ),
        ] {
            if (!value.is_empty() && value.trim().is_empty())
                || value.len() > limit
                || value.chars().any(char::is_control)
            {
                return Some(format!(
                    "{} ({name})",
                    copy(self.language, Copy::ErrorAdvanced)
                ));
            }
        }
        if !self.bedrock_endpoint_url.is_empty()
            && validate_https_endpoint("api.bedrock_endpoint_url", self.bedrock_endpoint_url.trim())
                .is_err()
        {
            return Some(format!(
                "{} (api.bedrock_endpoint_url)",
                copy(self.language, Copy::ErrorAdvanced)
            ));
        }
        None
    }

    fn edit_key(&mut self, code: KeyCode) {
        let Some(Hit::Field(field)) = self.selected() else {
            return;
        };
        if field == Field::UseCase {
            self.use_case_edited = true;
        }
        let buffer = match field {
            Field::Model => &mut self.model,
            Field::Endpoint => &mut self.endpoint,
            Field::ApiKey => &mut self.api_key,
            Field::Workspace => &mut self.workspace,
            Field::UseCase => &mut self.use_case,
            Field::RequestTimeout => &mut self.request_timeout_secs,
            Field::StreamIdleTimeout => &mut self.stream_idle_timeout_secs,
            Field::MaxAttempts => &mut self.max_attempts,
            Field::RetryMinDelay => &mut self.retry_min_delay_ms,
            Field::RetryMaxDelay => &mut self.retry_max_delay_secs,
            Field::RetryAfterCap => &mut self.retry_after_cap_secs,
            Field::AwsRegion => &mut self.aws_region,
            Field::AwsProfile => &mut self.aws_profile,
            Field::AwsRoleArn => &mut self.aws_role_arn,
            Field::BedrockEndpoint => &mut self.bedrock_endpoint_url,
        };
        match code {
            KeyCode::Char(character)
                if !character.is_control()
                    && buffer.len().saturating_add(character.len_utf8()) <= MAX_FIELD_BYTES =>
            {
                buffer.push(character);
            }
            KeyCode::Backspace => {
                if let Some((index, _)) = buffer.grapheme_indices(true).next_back() {
                    buffer.truncate(index);
                }
            }
            KeyCode::Delete => buffer.clear(),
            _ => {}
        }
    }

    fn paste(&mut self, value: &str) {
        let Some(Hit::Field(field)) = self.selected() else {
            return;
        };
        if field == Field::UseCase {
            self.use_case_edited = true;
        }
        let buffer = match field {
            Field::Model => &mut self.model,
            Field::Endpoint => &mut self.endpoint,
            Field::ApiKey => &mut self.api_key,
            Field::Workspace => &mut self.workspace,
            Field::UseCase => &mut self.use_case,
            Field::RequestTimeout => &mut self.request_timeout_secs,
            Field::StreamIdleTimeout => &mut self.stream_idle_timeout_secs,
            Field::MaxAttempts => &mut self.max_attempts,
            Field::RetryMinDelay => &mut self.retry_min_delay_ms,
            Field::RetryMaxDelay => &mut self.retry_max_delay_secs,
            Field::RetryAfterCap => &mut self.retry_after_cap_secs,
            Field::AwsRegion => &mut self.aws_region,
            Field::AwsProfile => &mut self.aws_profile,
            Field::AwsRoleArn => &mut self.aws_role_arn,
            Field::BedrockEndpoint => &mut self.bedrock_endpoint_url,
        };
        for character in value.chars() {
            if buffer.len().saturating_add(character.len_utf8()) > MAX_FIELD_BYTES {
                break;
            }
            if !character.is_control() || character == ' ' {
                buffer.push(character);
            }
        }
    }
}

pub async fn run<B: Backend>(terminal: &mut Terminal<B>) -> Result<WizardOutcome, AppError> {
    let mut wizard = Wizard::new();
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(TICK_RATE);
    loop {
        terminal
            .draw(|frame| draw(frame, &mut wizard))
            .map_err(|error| AppError::Terminal(error.to_string()))?;
        tokio::select! {
            _ = ticker.tick() => wizard.frame = wizard.frame.wrapping_add(1),
            event = events.next() => {
                let Some(event) = event else { return Ok(WizardOutcome::Cancelled); };
                let event = event.map_err(|error| AppError::Terminal(error.to_string()))?;
                match event {
                    Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                            return Ok(WizardOutcome::Cancelled);
                        }
                        match key.code {
                            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => wizard.focus.prev(),
                            KeyCode::Tab | KeyCode::Down => wizard.focus.next(),
                            KeyCode::BackTab | KeyCode::Up => wizard.focus.prev(),
                            KeyCode::Enter => {
                                if let Some(hit) = wizard.selected()
                                    && let Some(outcome) = wizard.activate(hit)
                                { return Ok(outcome); }
                            }
                            KeyCode::Esc => {
                                let hit = if wizard.step == Step::Language { Hit::Cancel } else { Hit::Back };
                                if let Some(outcome) = wizard.activate(hit) { return Ok(outcome); }
                            }
                            code => wizard.edit_key(code),
                        }
                    }
                    Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(hit) = wizard.clicks.handle_click(mouse.column, mouse.row).copied()
                            && let Some(outcome) = wizard.activate(hit)
                        { return Ok(outcome); }
                    }
                    Event::Paste(value) => wizard.paste(&value),
                    _ => {}
                }
            }
        }
    }
}

fn draw(frame: &mut Frame<'_>, wizard: &mut Wizard) {
    wizard.clicks.clear();
    let area = frame.area();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(8, 12, 22))),
        area,
    );
    if area.width < 80 || area.height < 36 {
        frame.render_widget(
            Paragraph::new(copy(wizard.language, Copy::OnboardingTooSmall))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let width = area.width.min(104);
    let height = area.height.min(42);
    let panel = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Rgb(91, 164, 255)))
            .style(Style::default().bg(Color::Rgb(13, 19, 32)))
            .title(format!(
                " DEcode by denysoid · {} ",
                copy(wizard.language, Copy::FirstLaunch)
            )),
        panel,
    );
    let inner = panel.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    let rows = Layout::vertical([
        Constraint::Length(6),
        Constraint::Length(2),
        Constraint::Min(12),
        Constraint::Length(2),
        Constraint::Length(3),
    ])
    .split(inner);
    draw_header(frame, rows[0], wizard);
    draw_progress(frame, rows[1], wizard.step, wizard.language);
    match wizard.step {
        Step::Language => draw_languages(frame, rows[2], wizard),
        Step::Provider => draw_providers(frame, rows[2], wizard),
        Step::Details => draw_details(frame, rows[2], wizard),
        Step::Network => draw_network(frame, rows[2], wizard),
        Step::ProviderAdvanced => draw_provider_advanced(frame, rows[2], wizard),
        Step::Review => draw_review(frame, rows[2], wizard),
    }
    frame.render_widget(
        Paragraph::new(wizard.error.as_deref().unwrap_or(""))
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::LightRed)),
        rows[3],
    );
    draw_navigation(frame, rows[4], wizard);
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, wizard: &Wizard) {
    const PET: [&str; 4] = [" /ᐠ｡ꞈ｡ᐟ\\", " /ᐠ˵- ᴗ -˵ᐟ\\", " /ᐠ｡▿｡ᐟ\\", " /ᐠ > ﻌ < ᐟ\\"];
    let lines = vec![
        Line::from(vec![
            Span::styled(
                animated_d_frame(wizard.frame),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                "DEcode",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                copy(wizard.language, Copy::Subtitle),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(Span::styled(
            PET[(wizard.frame / 4) % PET.len()],
            Style::default().fg(Color::LightMagenta),
        )),
        Line::from(copy(wizard.language, Copy::MouseHint)),
    ];
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

fn draw_progress(frame: &mut Frame<'_>, area: Rect, step: Step, language: UiLanguage) {
    let current = match step {
        Step::Language => 0,
        Step::Provider => 1,
        Step::Details => 2,
        Step::Network | Step::ProviderAdvanced => 3,
        Step::Review => 4,
    };
    let names = [
        copy(language, Copy::Language),
        copy(language, Copy::Provider),
        copy(language, Copy::Project),
        text_for(language, Text::RuntimeSettings),
        copy(language, Copy::Review),
    ];
    let spans = names
        .iter()
        .enumerate()
        .flat_map(|(index, name)| {
            let style = if index == current {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if index < current {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            [
                Span::styled(format!(" {} ", name), style),
                Span::raw(if index + 1 < names.len() { "─" } else { "" }),
            ]
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        area,
    );
}

fn draw_languages(frame: &mut Frame<'_>, area: Rect, wizard: &mut Wizard) {
    let title = copy(wizard.language, Copy::ChooseLanguage);
    draw_picker(
        frame,
        area,
        title,
        UiLanguage::ALL
            .iter()
            .enumerate()
            .map(|(index, language)| {
                (
                    Hit::Language(index),
                    language.label().to_owned(),
                    *language == wizard.language,
                )
            })
            .collect(),
        wizard,
    );
}

fn draw_providers(frame: &mut Frame<'_>, area: Rect, wizard: &mut Wizard) {
    let title = copy(wizard.language, Copy::ChooseProvider);
    draw_picker(
        frame,
        area,
        title,
        SetupProvider::ALL
            .iter()
            .enumerate()
            .map(|(index, provider)| {
                (
                    Hit::Provider(index),
                    provider_label(wizard.language, *provider),
                    *provider == wizard.provider,
                )
            })
            .collect(),
        wizard,
    );
}

fn draw_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    items: Vec<(Hit, String, bool)>,
    wizard: &mut Wizard,
) {
    let outer = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    frame.render_widget(
        Paragraph::new(title)
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::Gray)),
        outer[0],
    );
    let column_count = if items.len() > 16 { 3 } else { 2 };
    let columns = Layout::horizontal(vec![
        Constraint::Ratio(1, column_count as u32);
        column_count
    ])
    .split(outer[1]);
    let rows_per_column = items.len().div_ceil(column_count);
    let row_height = if column_count == 3 { 2 } else { 3 };
    for (index, (hit, label, selected)) in items.into_iter().enumerate() {
        let column = index / rows_per_column;
        let row = index % rows_per_column;
        let button_area = Rect::new(
            columns[column].x + 1,
            columns[column].y + row as u16 * row_height,
            columns[column].width.saturating_sub(2),
            row_height,
        );
        if button_area.bottom() > columns[column].bottom() {
            continue;
        }
        draw_button(
            frame,
            button_area,
            hit,
            &format!("{} {label}", if selected { "●" } else { "○" }),
            selected,
            wizard,
        );
    }
}

fn draw_details(frame: &mut Frame<'_>, area: Rect, wizard: &mut Wizard) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(4),
    ])
    .split(area);
    let mut row = 0;
    draw_field(
        frame,
        rows[row],
        Field::Model,
        copy(wizard.language, Copy::Model),
        false,
        wizard,
    );
    row += 1;
    if wizard.provider.needs_endpoint() {
        draw_field(
            frame,
            rows[row],
            Field::Endpoint,
            copy(wizard.language, Copy::Endpoint),
            false,
            wizard,
        );
        row += 1;
    }
    if wizard.provider.needs_secret() {
        draw_field(
            frame,
            rows[row],
            Field::ApiKey,
            copy(wizard.language, Copy::ApiKey),
            true,
            wizard,
        );
        row += 1;
    }
    draw_field(
        frame,
        rows[row],
        Field::Workspace,
        copy(wizard.language, Copy::ProjectFolder),
        false,
        wizard,
    );
    row += 1;
    draw_field(
        frame,
        rows[row],
        Field::UseCase,
        copy(wizard.language, Copy::UseCase),
        false,
        wizard,
    );
    row += 1;
    let context_area = Rect::new(
        area.x,
        rows[row.min(rows.len() - 1)].y,
        area.width,
        area.bottom()
            .saturating_sub(rows[row.min(rows.len() - 1)].y),
    );
    let context_rows =
        Layout::vertical([Constraint::Length(1), Constraint::Length(3)]).split(context_area);
    frame.render_widget(
        Paragraph::new(copy(wizard.language, Copy::ContextCompaction))
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::Gray)),
        context_rows[0],
    );
    let buttons = Layout::horizontal(
        CONTEXT_CHOICES.map(|_| Constraint::Ratio(1, CONTEXT_CHOICES.len() as u32)),
    )
    .split(context_rows[1]);
    for (index, value) in CONTEXT_CHOICES.iter().enumerate() {
        let label = if *value >= 1_000_000 {
            format!("{}M", *value / 1_000_000)
        } else {
            format!("{}k", *value / 1_000)
        };
        draw_button(
            frame,
            buttons[index],
            Hit::Context(index),
            &label,
            wizard.context_budget == *value,
            wizard,
        );
    }
}

fn draw_network(frame: &mut Frame<'_>, area: Rect, wizard: &mut Wizard) {
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(text_for(wizard.language, Text::AdvancedFieldsHelp))
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::Gray)),
        rows[0],
    );
    let transports = available_transports(wizard.provider);
    let columns = Layout::horizontal(vec![
        Constraint::Ratio(1, transports.len() as u32);
        transports.len()
    ])
    .split(rows[1]);
    for (index, transport) in transports.iter().enumerate() {
        draw_button(
            frame,
            columns[index],
            Hit::Transport(index),
            transport,
            wizard.api_transport == *transport,
            wizard,
        );
    }
    let fields = [
        (Field::RequestTimeout, "api.request_timeout_secs"),
        (Field::StreamIdleTimeout, "api.stream_idle_timeout_secs"),
        (Field::MaxAttempts, "api.max_attempts"),
        (Field::RetryMinDelay, "api.retry_min_delay_ms"),
        (Field::RetryMaxDelay, "api.retry_max_delay_secs"),
        (Field::RetryAfterCap, "api.retry_after_cap_secs"),
    ];
    for (row, pair) in fields.as_chunks::<2>().0.iter().enumerate() {
        let columns = Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
            .split(rows[row + 2]);
        for (column, (field, label)) in pair.iter().enumerate() {
            draw_field(frame, columns[column], *field, label, false, wizard);
        }
    }
}

fn draw_provider_advanced(frame: &mut Frame<'_>, area: Rect, wizard: &mut Wizard) {
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(text_for(wizard.language, Text::AdvancedSecuritySettings))
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::Gray)),
        rows[0],
    );
    for (row, (field, label)) in [
        (Field::AwsRegion, "AWS_REGION"),
        (Field::AwsProfile, "AWS_PROFILE"),
        (Field::AwsRoleArn, "AWS_ROLE_ARN"),
        (Field::BedrockEndpoint, "api.bedrock_endpoint_url"),
    ]
    .into_iter()
    .enumerate()
    {
        draw_field(frame, rows[row + 1], field, label, false, wizard);
    }
}

fn draw_field(
    frame: &mut Frame<'_>,
    area: Rect,
    field: Field,
    title: &str,
    secret: bool,
    wizard: &mut Wizard,
) {
    let focused = wizard.selected() == Some(Hit::Field(field));
    let value = match field {
        Field::Model => &wizard.model,
        Field::Endpoint => &wizard.endpoint,
        Field::ApiKey => &wizard.api_key,
        Field::Workspace => &wizard.workspace,
        Field::UseCase => &wizard.use_case,
        Field::RequestTimeout => &wizard.request_timeout_secs,
        Field::StreamIdleTimeout => &wizard.stream_idle_timeout_secs,
        Field::MaxAttempts => &wizard.max_attempts,
        Field::RetryMinDelay => &wizard.retry_min_delay_ms,
        Field::RetryMaxDelay => &wizard.retry_max_delay_secs,
        Field::RetryAfterCap => &wizard.retry_after_cap_secs,
        Field::AwsRegion => &wizard.aws_region,
        Field::AwsProfile => &wizard.aws_profile,
        Field::AwsRoleArn => &wizard.aws_role_arn,
        Field::BedrockEndpoint => &wizard.bedrock_endpoint_url,
    };
    let shown = if secret {
        "•".repeat(value.chars().count().min(48))
    } else {
        value.to_owned()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
        .border_style(if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        });
    frame.render_widget(Paragraph::new(shown).block(block), area);
    wizard.clicks.register(area, Hit::Field(field));
}

fn draw_review(frame: &mut Frame<'_>, area: Rect, wizard: &Wizard) {
    let context = if wizard.context_budget >= 1_000_000 {
        format!("{}M", wizard.context_budget / 1_000_000)
    } else {
        format!("{}k", wizard.context_budget / 1_000)
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{}  ", copy(wizard.language, Copy::Language)),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(wizard.language.label()),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{}  ", copy(wizard.language, Copy::Provider)),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(provider_label(wizard.language, wizard.provider)),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{}  ", text_for(wizard.language, Text::Model)),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(&wizard.model),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{}  ", copy(wizard.language, Copy::ProjectFolder)),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(&wizard.workspace),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{}  ", text_for(wizard.language, Text::Context)),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(context),
        ]),
        Line::from(format!(
            "{}  {} · {}s/{}s · retry {}",
            text_for(wizard.language, Text::TransportLabel),
            wizard.api_transport,
            wizard.request_timeout_secs,
            wizard.stream_idle_timeout_secs,
            wizard.max_attempts
        )),
    ];
    if wizard.provider == SetupProvider::AwsBedrockRuntime {
        lines.push(Line::from(format!(
            "AWS  region={} · profile={} · role={}",
            display_optional(&wizard.aws_region),
            display_optional(&wizard.aws_profile),
            display_optional(&wizard.aws_role_arn)
        )));
    }
    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            copy(wizard.language, Copy::KeyringSafe),
            Style::default().fg(Color::Green),
        )),
        Line::from(Span::styled(
            copy(wizard.language, Copy::DestructiveApproval),
            Style::default().fg(Color::Green),
        )),
        Line::from(Span::styled(
            copy(wizard.language, Copy::KeyboardFallback),
            Style::default().fg(Color::Green),
        )),
    ]);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", copy(wizard.language, Copy::Ready))),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_navigation(frame: &mut Frame<'_>, area: Rect, wizard: &mut Wizard) {
    let left = if wizard.step == Step::Language {
        Hit::Cancel
    } else {
        Hit::Back
    };
    let left_label = if wizard.step == Step::Language {
        copy(wizard.language, Copy::Cancel)
    } else {
        copy(wizard.language, Copy::Back)
    };
    let (right, right_label) = if wizard.step == Step::Review {
        (Hit::Save, copy(wizard.language, Copy::SaveLaunch))
    } else {
        (Hit::Next, text_for(wizard.language, Text::Continue))
    };
    let skip_label = (wizard.step != Step::Language).then(|| copy(wizard.language, Copy::Skip));
    let widths = navigation_widths(left_label, skip_label, right_label);
    let gap = u16::from(skip_label.is_some()) * 2;
    let chunks = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(widths[0]),
        Constraint::Length(gap),
        Constraint::Length(widths[1]),
        Constraint::Length(gap),
        Constraint::Length(widths[2]),
        Constraint::Fill(1),
    ])
    .split(area);

    draw_button(frame, chunks[1], left, left_label, false, wizard);
    if let Some(skip_label) = skip_label {
        draw_button(frame, chunks[3], Hit::Skip, skip_label, false, wizard);
    }
    draw_button(
        frame,
        chunks[5],
        right,
        right_label,
        right == Hit::Save,
        wizard,
    );
}

fn navigation_widths(left: &str, skip: Option<&str>, right: &str) -> [u16; 3] {
    let width = |label: &str| {
        u16::try_from(UnicodeWidthStr::width(label).saturating_add(2)).unwrap_or(u16::MAX)
    };
    [width(left), skip.map_or(0, width), width(right)]
}

fn draw_button(
    frame: &mut Frame<'_>,
    area: Rect,
    hit: Hit,
    label: &str,
    selected: bool,
    wizard: &mut Wizard,
) {
    let mut state = ButtonState::enabled();
    state.set_focused(wizard.selected() == Some(hit));
    let style = if selected {
        ButtonStyle::success()
    } else {
        ButtonStyle::primary()
    };
    let button = Button::new(label, &state)
        .variant(ButtonVariant::Block)
        .style(style);
    let region = button.render_stateful(area, frame.buffer_mut());
    wizard.clicks.register(region.area, hit);
}

fn available_transports(provider: SetupProvider) -> &'static [&'static str] {
    if provider == SetupProvider::OpenAi {
        &TRANSPORT_CHOICES
    } else {
        &TRANSPORT_CHOICES[..2]
    }
}

fn valid_positive_u64(value: &str, maximum: u64) -> bool {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| (1..=maximum).contains(value))
        .is_some()
}

fn parse_u64(value: &str, default: u64) -> u64 {
    value.parse().unwrap_or(default)
}

fn parse_u32(value: &str, default: u32) -> u32 {
    value.parse().unwrap_or(default)
}

fn optional_value(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn display_optional(value: &str) -> &str {
    let value = value.trim();
    if value.is_empty() { "—" } else { value }
}

fn provider_label(language: UiLanguage, provider: SetupProvider) -> String {
    match provider {
        SetupProvider::AwsBedrockRuntime => format!(
            "AWS Bedrock Runtime ({})",
            copy(language, Copy::ProviderNativeSigV4)
        ),
        SetupProvider::Ollama => {
            format!("Ollama ({})", copy(language, Copy::ProviderLocalSuffix))
        }
        SetupProvider::Compatible => copy(language, Copy::ProviderCompatible).to_owned(),
        _ => provider.label().to_owned(),
    }
}

fn copy(language: UiLanguage, key: Copy) -> &'static str {
    let values = match key {
        Copy::FirstLaunch => [
            "first launch",
            "первый запуск",
            "перший запуск",
            "primer inicio",
            "erster Start",
            "premier démarrage",
            "pierwsze uruchomienie",
            "primeiro início",
            "首次启动",
            "初回起動",
            "첫 실행",
            "ilk çalıştırma",
        ],
        Copy::Subtitle => [
            " — a calm, visual coding agent",
            " — спокойный визуальный coding-агент",
            " — спокійний візуальний coding-агент",
            " — un agente visual y claro",
            " — ein ruhiger visueller Coding-Agent",
            " — un agent de code visuel et serein",
            " — spokojny, wizualny agent kodowania",
            " — um agente visual e tranquilo",
            " — 清晰、沉稳的编程智能体",
            " — 落ち着いた視覚的コーディングエージェント",
            " — 차분한 시각적 코딩 에이전트",
            " — sakin ve görsel bir kodlama ajanı",
        ],
        Copy::MouseHint => [
            "Click with the mouse or use Tab / Shift+Tab · no numeric prompts",
            "Кликайте мышью или используйте Tab / Shift+Tab · без ввода номеров",
            "Клацайте мишею або використовуйте Tab / Shift+Tab · без введення номерів",
            "Haz clic o usa Tab / Shift+Tab · sin introducir números",
            "Klicken oder Tab / Shift+Tab verwenden · keine Nummerneingabe",
            "Cliquez ou utilisez Tab / Maj+Tab · aucun numéro à saisir",
            "Klikaj lub użyj Tab / Shift+Tab · bez wpisywania numerów",
            "Clique ou use Tab / Shift+Tab · sem digitar números",
            "鼠标点击或使用 Tab / Shift+Tab · 无需输入编号",
            "クリックまたは Tab / Shift+Tab · 番号入力は不要",
            "마우스 클릭 또는 Tab / Shift+Tab · 번호 입력 없음",
            "Tıklayın veya Tab / Shift+Tab kullanın · numara girmeyin",
        ],
        Copy::ChooseLanguage => [
            "First choose the language for the whole interface",
            "Сначала выберите язык всего интерфейса",
            "Спочатку виберіть мову всього інтерфейсу",
            "Primero elige el idioma de toda la interfaz",
            "Wählen Sie zuerst die Sprache der Oberfläche",
            "Choisissez d’abord la langue de toute l’interface",
            "Najpierw wybierz język całego interfejsu",
            "Primeiro escolha o idioma de toda a interface",
            "先选择整个界面的语言",
            "最初にインターフェース全体の言語を選択",
            "먼저 전체 인터페이스 언어를 선택하세요",
            "Önce tüm arayüzün dilini seçin",
        ],
        Copy::ChooseProvider => [
            "Choose the primary provider · you can switch later",
            "Выберите основной провайдер · его можно сменить позже",
            "Оберіть основного провайдера · його можна змінити пізніше",
            "Elige el proveedor principal · podrás cambiarlo después",
            "Primären Anbieter wählen · später änderbar",
            "Choisissez le fournisseur principal · modifiable plus tard",
            "Wybierz głównego dostawcę · można go zmienić później",
            "Escolha o provedor principal · pode mudar depois",
            "选择主要提供商 · 以后可以更改",
            "主要プロバイダーを選択 · 後で変更可能",
            "기본 제공자를 선택하세요 · 나중에 변경 가능",
            "Ana sağlayıcıyı seçin · sonra değiştirilebilir",
        ],
        Copy::Language => [
            "Language", "Язык", "Мова", "Idioma", "Sprache", "Langue", "Język", "Idioma", "语言",
            "言語", "언어", "Dil",
        ],
        Copy::Provider => [
            "Provider",
            "Провайдер",
            "Провайдер",
            "Proveedor",
            "Anbieter",
            "Fournisseur",
            "Dostawca",
            "Provedor",
            "提供商",
            "プロバイダー",
            "제공자",
            "Sağlayıcı",
        ],
        Copy::Project => [
            "Project",
            "Проект",
            "Проєкт",
            "Proyecto",
            "Projekt",
            "Projet",
            "Projekt",
            "Projeto",
            "项目",
            "プロジェクト",
            "프로젝트",
            "Proje",
        ],
        Copy::Review => [
            "Review",
            "Проверка",
            "Перевірка",
            "Revisión",
            "Prüfung",
            "Vérification",
            "Podsumowanie",
            "Revisão",
            "检查",
            "確認",
            "검토",
            "İnceleme",
        ],
        Copy::Model => [
            "Model / deployment",
            "Модель / deployment",
            "Модель / deployment",
            "Modelo / deployment",
            "Modell / Deployment",
            "Modèle / deployment",
            "Model / deployment",
            "Modelo / deployment",
            "模型 / deployment",
            "モデル / deployment",
            "모델 / deployment",
            "Model / deployment",
        ],
        Copy::Endpoint => [
            "HTTPS endpoint",
            "HTTPS endpoint",
            "HTTPS endpoint",
            "Endpoint HTTPS",
            "HTTPS-Endpunkt",
            "Endpoint HTTPS",
            "Endpoint HTTPS",
            "Endpoint HTTPS",
            "HTTPS 端点",
            "HTTPS エンドポイント",
            "HTTPS 엔드포인트",
            "HTTPS uç noktası",
        ],
        Copy::ApiKey => [
            "API key · stored only in OS keyring",
            "API-ключ · только в системном хранилище",
            "API-ключ · лише в системному сховищі",
            "Clave API · solo en el almacén del sistema",
            "API-Schlüssel · nur im System-Schlüsselbund",
            "Clé API · uniquement dans le trousseau système",
            "Klucz API · tylko w magazynie systemowym",
            "Chave API · apenas no cofre do sistema",
            "API 密钥 · 仅存入系统密钥库",
            "API キー · OS キーリングのみに保存",
            "API 키 · OS 키링에만 저장",
            "API anahtarı · yalnızca işletim sistemi kasasında",
        ],
        Copy::ProjectFolder => [
            "Project folder",
            "Папка проекта",
            "Папка проєкту",
            "Carpeta del proyecto",
            "Projektordner",
            "Dossier du projet",
            "Folder projektu",
            "Pasta do projeto",
            "项目文件夹",
            "プロジェクトフォルダー",
            "프로젝트 폴더",
            "Proje klasörü",
        ],
        Copy::UseCase => [
            "What will you use DEcode for?",
            "Для чего вы будете использовать DEcode?",
            "Для чого ви використовуватимете DEcode?",
            "¿Para qué usarás DEcode?",
            "Wofür verwenden Sie DEcode?",
            "À quoi servira DEcode ?",
            "Do czego użyjesz DEcode?",
            "Para que você usará o DEcode?",
            "您将用 DEcode 做什么？",
            "DEcode を何に使いますか？",
            "DEcode를 어디에 사용하나요?",
            "DEcode'u ne için kullanacaksınız?",
        ],
        Copy::ContextCompaction => [
            "Context kept before smart compaction",
            "Контекст до умного сжатия",
            "Контекст до розумного стиснення",
            "Contexto antes de la compactación inteligente",
            "Kontext vor intelligenter Komprimierung",
            "Contexte avant compression intelligente",
            "Kontekst przed inteligentną kompresją",
            "Contexto antes da compactação inteligente",
            "智能压缩前保留的上下文",
            "スマート圧縮前に保持するコンテキスト",
            "스마트 압축 전 보관할 컨텍스트",
            "Akıllı sıkıştırmadan önce tutulacak bağlam",
        ],
        Copy::Skip => [
            "Skip setup",
            "Пропустить настройку",
            "Пропустити налаштування",
            "Omitir configuración",
            "Einrichtung überspringen",
            "Ignorer la configuration",
            "Pomiń konfigurację",
            "Pular configuração",
            "跳过设置",
            "セットアップをスキップ",
            "설정 건너뛰기",
            "Kurulumu atla",
        ],
        Copy::Cancel => [
            "Cancel (Esc)",
            "Отмена (Esc)",
            "Скасувати (Esc)",
            "Cancelar (Esc)",
            "Abbrechen (Esc)",
            "Annuler (Esc)",
            "Anuluj (Esc)",
            "Cancelar (Esc)",
            "取消 (Esc)",
            "キャンセル (Esc)",
            "취소 (Esc)",
            "İptal (Esc)",
        ],
        Copy::Back => [
            "← Back (Esc)",
            "← Назад (Esc)",
            "← Назад (Esc)",
            "← Atrás (Esc)",
            "← Zurück (Esc)",
            "← Retour (Esc)",
            "← Wstecz (Esc)",
            "← Voltar (Esc)",
            "← 返回 (Esc)",
            "← 戻る (Esc)",
            "← 뒤로 (Esc)",
            "← Geri (Esc)",
        ],
        Copy::SaveLaunch => [
            "Save & launch ✓",
            "Сохранить и запустить ✓",
            "Зберегти й запустити ✓",
            "Guardar e iniciar ✓",
            "Speichern und starten ✓",
            "Enregistrer et lancer ✓",
            "Zapisz i uruchom ✓",
            "Salvar e iniciar ✓",
            "保存并启动 ✓",
            "保存して起動 ✓",
            "저장하고 실행 ✓",
            "Kaydet ve başlat ✓",
        ],
        Copy::Ready => [
            "Ready",
            "Готово",
            "Готово",
            "Listo",
            "Bereit",
            "Prêt",
            "Gotowe",
            "Pronto",
            "就绪",
            "準備完了",
            "준비됨",
            "Hazır",
        ],
        Copy::KeyringSafe => [
            "✓ API key goes to the OS keyring, never config.toml",
            "✓ API-ключ попадёт в системное хранилище, не в config.toml",
            "✓ API-ключ буде в системному сховищі, не в config.toml",
            "✓ La clave API va al almacén del sistema, nunca a config.toml",
            "✓ API-Schlüssel nur im System-Schlüsselbund, nie in config.toml",
            "✓ La clé API va au trousseau système, jamais dans config.toml",
            "✓ Klucz API trafia do magazynu systemowego, nigdy do config.toml",
            "✓ A chave API vai para o cofre do sistema, nunca para config.toml",
            "✓ API 密钥进入系统密钥库，不写入 config.toml",
            "✓ API キーは OS キーリングに保存し config.toml には書きません",
            "✓ API 키는 OS 키링에만 저장되며 config.toml에는 기록되지 않습니다",
            "✓ API anahtarı sistem kasasına gider, config.toml'a yazılmaz",
        ],
        Copy::DestructiveApproval => [
            "✓ Destructive actions still require explicit approval",
            "✓ Опасные действия по-прежнему требуют подтверждения",
            "✓ Небезпечні дії й надалі потребують підтвердження",
            "✓ Las acciones destructivas siguen requiriendo aprobación",
            "✓ Destruktive Aktionen brauchen weiterhin Bestätigung",
            "✓ Les actions destructives exigent toujours une confirmation",
            "✓ Działania destrukcyjne nadal wymagają potwierdzenia",
            "✓ Ações destrutivas ainda exigem aprovação",
            "✓ 破坏性操作仍需明确批准",
            "✓ 破壊的操作には引き続き明示的な承認が必要",
            "✓ 파괴적 작업은 계속 명시적 승인이 필요합니다",
            "✓ Yıkıcı işlemler açık onay gerektirir",
        ],
        Copy::KeyboardFallback => [
            "✓ Mouse controls always have a Tab/keyboard fallback",
            "✓ У мыши всегда есть дублирование через Tab/клавиатуру",
            "✓ Керування мишею завжди дублюється Tab/клавіатурою",
            "✓ El ratón siempre tiene alternativa con Tab/teclado",
            "✓ Maussteuerung hat immer eine Tab-/Tastatur-Alternative",
            "✓ La souris a toujours une alternative Tab/clavier",
            "✓ Mysz zawsze ma odpowiednik Tab/klawiatura",
            "✓ O mouse sempre tem alternativa por Tab/teclado",
            "✓ 鼠标操作始终有 Tab/键盘备选",
            "✓ マウス操作には常に Tab/キーボード操作があります",
            "✓ 마우스 조작은 항상 Tab/키보드로 대체할 수 있습니다",
            "✓ Fare denetimlerinin her zaman Tab/klavye karşılığı vardır",
        ],
        Copy::ErrorModel => [
            "Enter a model or deployment name",
            "Введите модель или имя deployment",
            "Введіть модель або назву deployment",
            "Introduce un modelo o deployment",
            "Modell- oder Deployment-Namen eingeben",
            "Saisissez un modèle ou un deployment",
            "Podaj model lub nazwę deployment",
            "Informe um modelo ou deployment",
            "输入模型或 deployment 名称",
            "モデルまたは deployment 名を入力",
            "모델 또는 deployment 이름을 입력하세요",
            "Model veya deployment adı girin",
        ],
        Copy::ErrorEndpoint => [
            "This provider requires an endpoint",
            "Для этого провайдера нужен endpoint",
            "Цьому провайдеру потрібен endpoint",
            "Este proveedor requiere un endpoint",
            "Dieser Anbieter benötigt einen Endpunkt",
            "Ce fournisseur exige un endpoint",
            "Ten dostawca wymaga endpointu",
            "Este provedor exige um endpoint",
            "此提供商需要端点",
            "このプロバイダーにはエンドポイントが必要です",
            "이 제공자는 엔드포인트가 필요합니다",
            "Bu sağlayıcı bir uç nokta gerektirir",
        ],
        Copy::ErrorApiKey => [
            "Enter the provider API key",
            "Введите API-ключ провайдера",
            "Введіть API-ключ провайдера",
            "Introduce la clave API",
            "API-Schlüssel des Anbieters eingeben",
            "Saisissez la clé API du fournisseur",
            "Podaj klucz API dostawcy",
            "Informe a chave API do provedor",
            "输入提供商 API 密钥",
            "プロバイダーの API キーを入力",
            "제공자 API 키를 입력하세요",
            "Sağlayıcı API anahtarını girin",
        ],
        Copy::ErrorWorkspace => [
            "The workspace must be an existing absolute directory",
            "Папка проекта должна существовать и быть абсолютной",
            "Папка проєкту має існувати й бути абсолютною",
            "La carpeta debe existir y tener una ruta absoluta",
            "Der Projektordner muss existieren und absolut sein",
            "Le dossier doit exister et avoir un chemin absolu",
            "Folder projektu musi istnieć i mieć ścieżkę bezwzględną",
            "A pasta deve existir e ter caminho absoluto",
            "工作区必须是存在的绝对目录",
            "ワークスペースは存在する絶対パスのディレクトリである必要があります",
            "작업 공간은 존재하는 절대 경로여야 합니다",
            "Çalışma alanı var olan mutlak bir dizin olmalıdır",
        ],
        Copy::ProviderLocalSuffix => [
            "local",
            "локально",
            "локально",
            "local",
            "lokal",
            "local",
            "lokalnie",
            "local",
            "本地",
            "ローカル",
            "로컬",
            "yerel",
        ],
        Copy::ProviderCompatible => [
            "Custom compatible provider",
            "Свой совместимый провайдер",
            "Власний сумісний провайдер",
            "Proveedor compatible personalizado",
            "Eigener kompatibler Anbieter",
            "Fournisseur compatible personnalisé",
            "Własny zgodny dostawca",
            "Provedor compatível personalizado",
            "自定义兼容提供商",
            "カスタム互換プロバイダー",
            "사용자 지정 호환 제공자",
            "Özel uyumlu sağlayıcı",
        ],
        Copy::ProviderNativeSigV4 => [
            "native SigV4",
            "нативный SigV4",
            "нативний SigV4",
            "SigV4 nativo",
            "natives SigV4",
            "SigV4 natif",
            "natywny SigV4",
            "SigV4 nativo",
            "原生 SigV4",
            "ネイティブ SigV4",
            "네이티브 SigV4",
            "yerel SigV4",
        ],
        Copy::DefaultUseCase => [
            "Coding, review, tests, and project maintenance",
            "Разработка, ревью, тесты и сопровождение проекта",
            "Розробка, рев'ю, тести й супровід проєкту",
            "Código, revisión, pruebas y mantenimiento del proyecto",
            "Entwicklung, Review, Tests und Projektpflege",
            "Développement, revue, tests et maintenance du projet",
            "Programowanie, przegląd, testy i utrzymanie projektu",
            "Código, revisão, testes e manutenção do projeto",
            "编码、审查、测试和项目维护",
            "開発、レビュー、テスト、プロジェクト保守",
            "개발, 리뷰, 테스트 및 프로젝트 유지보수",
            "Kodlama, inceleme, testler ve proje bakımı",
        ],
        Copy::OnboardingTooSmall => [
            "Terminal too small for setup · resize to at least 80×36 · Esc cancels",
            "Терминал мал для настройки · увеличьте до 80×36 · Esc отменяет",
            "Термінал замалий для налаштування · збільште до 80×36 · Esc скасовує",
            "Terminal demasiado pequeña · amplíela a 80×36 · Esc cancela",
            "Terminal für die Einrichtung zu klein · auf 80×36 vergrößern · Esc bricht ab",
            "Terminal trop petit pour la configuration · passez à 80×36 · Échap annule",
            "Terminal za mały do konfiguracji · zwiększ do 80×36 · Esc anuluje",
            "Terminal pequeno para configurar · aumente para 80×36 · Esc cancela",
            "终端过小 · 请调整到至少 80×36 · Esc 取消",
            "セットアップには端末が小さすぎます · 80×36 以上に変更 · Esc で中止",
            "설정하기에는 터미널이 너무 작습니다 · 80×36 이상으로 조정 · Esc 취소",
            "Terminal kurulum için küçük · en az 80×36 yapın · Esc iptal eder",
        ],
        Copy::ErrorAdvanced => [
            "Invalid advanced setting",
            "Недопустимая расширенная настройка",
            "Неприпустиме розширене налаштування",
            "Configuración avanzada no válida",
            "Ungültige erweiterte Einstellung",
            "Paramètre avancé invalide",
            "Nieprawidłowe ustawienie zaawansowane",
            "Configuração avançada inválida",
            "高级设置无效",
            "詳細設定が無効です",
            "고급 설정이 올바르지 않습니다",
            "Gelişmiş ayar geçersiz",
        ],
    };
    let index = UiLanguage::ALL
        .iter()
        .position(|candidate| *candidate == language)
        .unwrap_or(0);
    values[index.min(values.len() - 1)]
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn infallible<T>(result: Result<T, Infallible>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => match error {},
        }
    }

    #[test]
    fn every_language_has_localized_onboarding_copy() {
        for language in UiLanguage::ALL {
            for key in Copy::ALL {
                assert!(!copy(language, *key).trim().is_empty());
            }
        }
    }

    #[test]
    fn language_picker_is_clickable_and_moves_to_the_provider_step() -> Result<(), std::io::Error> {
        let mut wizard = Wizard::new();
        let mut terminal = infallible(Terminal::new(TestBackend::new(110, 42)));
        infallible(terminal.draw(|frame| draw(frame, &mut wizard)));
        let area = terminal.backend().buffer().area;
        let russian = UiLanguage::ALL
            .iter()
            .position(|language| *language == UiLanguage::Russian)
            .ok_or_else(|| std::io::Error::other("Russian language is missing"))?;
        let point = (0..area.height).find_map(|row| {
            (0..area.width)
                .find(|column| {
                    wizard.clicks.handle_click(*column, row).copied()
                        == Some(Hit::Language(russian))
                })
                .map(|column| (column, row))
        });
        let (column, row) =
            point.ok_or_else(|| std::io::Error::other("language has no click region"))?;
        let hit = wizard
            .clicks
            .handle_click(column, row)
            .copied()
            .ok_or_else(|| std::io::Error::other("language click did not resolve"))?;
        assert!(wizard.activate(hit).is_none());
        assert_eq!(wizard.language, UiLanguage::Russian);
        assert_eq!(wizard.step, Step::Provider);
        assert_eq!(
            wizard.use_case,
            copy(UiLanguage::Russian, Copy::DefaultUseCase)
        );
        Ok(())
    }

    #[test]
    fn provider_qualifiers_are_localized() {
        for language in UiLanguage::ALL {
            let compatible = provider_label(language, SetupProvider::Compatible);
            let local = provider_label(language, SetupProvider::Ollama);
            let runtime = provider_label(language, SetupProvider::AwsBedrockRuntime);
            assert_eq!(compatible, copy(language, Copy::ProviderCompatible));
            assert!(local.contains(copy(language, Copy::ProviderLocalSuffix)));
            assert!(runtime.contains(copy(language, Copy::ProviderNativeSigV4)));
        }
    }

    #[test]
    fn every_provider_is_visible_clickable_and_keyboard_reachable_at_minimum_size() {
        let mut wizard = Wizard::new();
        wizard.step = Step::Provider;
        wizard.rebuild_focus();
        let mut terminal = infallible(Terminal::new(TestBackend::new(80, 36)));
        infallible(terminal.draw(|frame| draw(frame, &mut wizard)));

        let bindings = wizard.clicks.bindings();
        let mut keyboard_actions = std::collections::HashSet::new();
        for _ in 0..SetupProvider::ALL.len() + 2 {
            if let Some(action) = wizard.selected() {
                keyboard_actions.insert(action);
            }
            wizard.focus.next();
        }
        for index in 0..SetupProvider::ALL.len() {
            assert!(
                bindings
                    .iter()
                    .any(|binding| binding.action == Hit::Provider(index)),
                "provider {index} has no visible mouse region"
            );
            assert!(keyboard_actions.contains(&Hit::Provider(index)));
        }
        assert!(wizard.clicks.is_complete());
    }

    #[test]
    fn setup_can_be_skipped_after_language_selection_without_losing_the_language()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut wizard = Wizard::new();
        wizard.language = UiLanguage::Ukrainian;
        wizard.step = Step::Provider;
        wizard.rebuild_focus();
        let mut terminal = infallible(Terminal::new(TestBackend::new(100, 38)));
        infallible(terminal.draw(|frame| draw(frame, &mut wizard)));
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains(copy(UiLanguage::Ukrainian, Copy::Skip)));
        assert!(
            wizard
                .clicks
                .bindings()
                .iter()
                .any(|binding| binding.action == Hit::Skip)
        );
        assert!(matches!(
            wizard.activate(Hit::Skip),
            Some(WizardOutcome::Skipped(UiLanguage::Ukrainian))
        ));
        Ok(())
    }

    #[test]
    fn localized_navigation_labels_fit_the_minimum_setup_width() {
        for language in UiLanguage::ALL {
            let ordinary = navigation_widths(
                copy(language, Copy::Back),
                Some(copy(language, Copy::Skip)),
                text_for(language, Text::Continue),
            );
            let review = navigation_widths(
                copy(language, Copy::Back),
                Some(copy(language, Copy::Skip)),
                copy(language, Copy::SaveLaunch),
            );
            assert!(ordinary.into_iter().sum::<u16>() + 4 <= 80);
            assert!(review.into_iter().sum::<u16>() + 4 <= 80);
        }
    }

    #[test]
    fn advanced_network_controls_have_mouse_and_keyboard_paths() {
        let mut wizard = Wizard::new();
        wizard.provider = SetupProvider::OpenAi;
        wizard.step = Step::Network;
        wizard.rebuild_focus();
        let mut terminal = infallible(Terminal::new(TestBackend::new(110, 42)));
        infallible(terminal.draw(|frame| draw(frame, &mut wizard)));

        let bindings = wizard.clicks.bindings();
        for expected in [
            Hit::Transport(0),
            Hit::Transport(1),
            Hit::Transport(2),
            Hit::Field(Field::RequestTimeout),
            Hit::Field(Field::StreamIdleTimeout),
            Hit::Field(Field::MaxAttempts),
            Hit::Field(Field::RetryMinDelay),
            Hit::Field(Field::RetryMaxDelay),
            Hit::Field(Field::RetryAfterCap),
            Hit::Back,
            Hit::Next,
        ] {
            assert!(bindings.iter().any(|binding| binding.action == expected));
        }
        assert!(wizard.clicks.is_complete());
    }

    #[test]
    fn bedrock_runtime_fields_have_mouse_and_keyboard_paths() {
        let mut wizard = Wizard::new();
        wizard.provider = SetupProvider::AwsBedrockRuntime;
        wizard.step = Step::ProviderAdvanced;
        wizard.rebuild_focus();
        let mut terminal = infallible(Terminal::new(TestBackend::new(110, 42)));
        infallible(terminal.draw(|frame| draw(frame, &mut wizard)));

        for expected in [
            Hit::Field(Field::AwsRegion),
            Hit::Field(Field::AwsProfile),
            Hit::Field(Field::AwsRoleArn),
            Hit::Field(Field::BedrockEndpoint),
            Hit::Back,
            Hit::Next,
        ] {
            assert!(
                wizard
                    .clicks
                    .bindings()
                    .iter()
                    .any(|binding| binding.action == expected)
            );
        }
    }

    #[test]
    fn websocket_is_not_offered_to_unsupported_providers() {
        assert_eq!(
            available_transports(SetupProvider::OpenAi),
            &["auto", "sse", "websocket"]
        );
        assert_eq!(available_transports(SetupProvider::Azure), &["auto", "sse"]);
    }

    #[test]
    fn narrow_setup_never_registers_clipped_actions() {
        let mut wizard = Wizard::new();
        let mut terminal = infallible(Terminal::new(TestBackend::new(54, 24)));
        infallible(terminal.draw(|frame| draw(frame, &mut wizard)));
        assert!(wizard.clicks.bindings().is_empty());
    }

    #[test]
    fn api_key_is_masked_in_the_rendered_details_step() {
        let mut wizard = Wizard::new();
        wizard.step = Step::Details;
        wizard.provider = SetupProvider::Google;
        wizard.api_key = "never-render-this-secret".to_owned();
        wizard.rebuild_focus();
        let mut terminal = infallible(Terminal::new(TestBackend::new(110, 42)));
        infallible(terminal.draw(|frame| draw(frame, &mut wizard)));
        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for row in 0..buffer.area.height {
            for column in 0..buffer.area.width {
                rendered.push_str(buffer[(column, row)].symbol());
            }
        }
        assert!(!rendered.contains("never-render-this-secret"));
        assert!(rendered.contains('•'));
    }

    #[test]
    fn field_editor_never_exceeds_its_utf8_byte_limit() {
        let mut wizard = Wizard::new();
        wizard.step = Step::Details;
        wizard.rebuild_focus();
        wizard.model = "a".repeat(MAX_FIELD_BYTES - 1);

        wizard.edit_key(KeyCode::Char('é'));

        assert_eq!(wizard.model.len(), MAX_FIELD_BYTES - 1);
    }

    #[test]
    fn field_backspace_removes_one_grapheme() {
        let mut wizard = Wizard::new();
        wizard.step = Step::Details;
        wizard.rebuild_focus();
        wizard.model = "e\u{301}".to_owned();

        wizard.edit_key(KeyCode::Backspace);

        assert!(wizard.model.is_empty());
    }

    #[test]
    fn required_fields_reject_invisible_or_blank_values() {
        let mut wizard = Wizard::new();
        wizard.model = "\u{200b}\u{200d}".to_owned();
        assert_eq!(
            wizard.validate_details().as_deref(),
            Some(copy(wizard.language, Copy::ErrorModel))
        );

        wizard.model = "model".to_owned();
        wizard.provider = SetupProvider::Google;
        wizard.api_key = "   ".to_owned();
        assert_eq!(
            wizard.validate_details().as_deref(),
            Some(copy(wizard.language, Copy::ErrorApiKey))
        );
    }

    #[test]
    fn detail_validation_matches_persistence_size_limits() {
        let mut wizard = Wizard::new();
        wizard.model = "a".repeat(257);
        assert_eq!(
            wizard.validate_details().as_deref(),
            Some(copy(wizard.language, Copy::ErrorModel))
        );

        wizard.model = "model".to_owned();
        wizard.endpoint = "a".repeat(2_049);
        assert_eq!(
            wizard.validate_details().as_deref(),
            Some(copy(wizard.language, Copy::ErrorEndpoint))
        );

        wizard.endpoint = "http://example.test/responses".to_owned();
        wizard.api_key = "secret".to_owned();
        assert_eq!(
            wizard.validate_details().as_deref(),
            Some(copy(wizard.language, Copy::ErrorEndpoint))
        );
    }

    #[test]
    fn advanced_validation_rejects_an_unsafe_bedrock_endpoint() {
        let mut wizard = Wizard::new();
        wizard.provider = SetupProvider::AwsBedrockRuntime;
        wizard.bedrock_endpoint_url = "file:///tmp/credentials".to_owned();

        assert!(wizard.validate_advanced().is_some());
    }

    #[test]
    fn switching_provider_discards_the_previous_provider_secret()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut wizard = Wizard::new();
        wizard.provider = SetupProvider::Google;
        wizard.api_key = "google-secret".to_owned();
        let Some(anthropic) = SetupProvider::ALL
            .iter()
            .position(|provider| *provider == SetupProvider::Anthropic)
        else {
            return Err("Anthropic provider is missing".into());
        };

        wizard.activate(Hit::Provider(anthropic));

        assert!(wizard.api_key.is_empty());
        Ok(())
    }
}
