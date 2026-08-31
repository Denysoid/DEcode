use std::time::{Duration, Instant};

use super::actions::ClickRegionRegistry;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
};
use ratatui_interact::{
    components::{
        Button, ButtonState, ButtonStyle, ButtonVariant, DialogConfig, DialogState, ListPicker,
        ListPickerState, ListPickerStyle, PopupDialog,
    },
    state::FocusManager,
};
use thiserror::Error;

use crate::usage::{
    CostCoverage, DeploymentPricing, DeploymentUsageSnapshot, PricingError, UsageSnapshot,
    format_microusd,
};

use super::{
    i18n::{Text, text},
    render::{sanitize_for_display, truncate_for_display},
};

const ANIMATION_STEP: Duration = Duration::from_millis(160);

#[derive(Debug, Error)]
#[error("{message}")]
pub struct UsagePricingInputError {
    message: String,
}

impl UsagePricingInputError {
    fn localized(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UsageFocus {
    Deployments,
    Edit,
    Close,
    InputRate,
    CachedRate,
    OutputRate,
    LongThreshold,
    LongInputRate,
    LongCachedRate,
    LongOutputRate,
    Save,
    Remove,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UsageHit {
    Deployment(usize),
    Edit,
    Close,
    InputRate,
    CachedRate,
    OutputRate,
    LongThreshold,
    LongInputRate,
    LongCachedRate,
    LongOutputRate,
    Save,
    Remove,
    Cancel,
}

#[derive(Debug, Clone)]
pub struct UsageUiState {
    open: bool,
    dialog: DialogState<()>,
    picker: ListPickerState,
    focus: FocusManager<UsageFocus>,
    clicks: ClickRegionRegistry<UsageHit>,
    editing: bool,
    deployment_names: Vec<String>,
    editing_deployment: Option<String>,
    input_rate: String,
    cached_rate: String,
    output_rate: String,
    long_threshold: String,
    long_input_rate: String,
    long_cached_rate: String,
    long_output_rate: String,
    editor_error: Option<String>,
    animation_frame: usize,
    last_animation_at: Instant,
}

impl UsageUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        focus.register(UsageFocus::Deployments);
        focus.register(UsageFocus::Edit);
        focus.register(UsageFocus::Close);
        focus.register(UsageFocus::InputRate);
        focus.register(UsageFocus::CachedRate);
        focus.register(UsageFocus::OutputRate);
        focus.register(UsageFocus::LongThreshold);
        focus.register(UsageFocus::LongInputRate);
        focus.register(UsageFocus::LongCachedRate);
        focus.register(UsageFocus::LongOutputRate);
        focus.register(UsageFocus::Save);
        focus.register(UsageFocus::Remove);
        focus.register(UsageFocus::Cancel);
        focus.set(UsageFocus::Deployments);
        Self {
            open: false,
            dialog: DialogState::new(()),
            picker: ListPickerState::new(0),
            focus,
            clicks: ClickRegionRegistry::new(),
            editing: false,
            deployment_names: Vec::new(),
            editing_deployment: None,
            input_rate: String::new(),
            cached_rate: String::new(),
            output_rate: String::new(),
            long_threshold: String::new(),
            long_input_rate: String::new(),
            long_cached_rate: String::new(),
            long_output_rate: String::new(),
            editor_error: None,
            animation_frame: 0,
            last_animation_at: Instant::now(),
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, deployments: usize) {
        self.open = true;
        self.editing = false;
        self.editing_deployment = None;
        self.editor_error = None;
        self.set_total(deployments);
        self.focus.set(UsageFocus::Deployments);
        self.dialog.show();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.dialog.hide();
        self.clicks.clear();
        self.editing = false;
        self.editing_deployment = None;
    }

    pub fn begin_frame(&mut self) {
        self.clicks.clear();
    }

    pub fn tick(&mut self, now: Instant) {
        if now.saturating_duration_since(self.last_animation_at) >= ANIMATION_STEP {
            self.animation_frame = self.animation_frame.wrapping_add(1);
            self.last_animation_at = now;
        }
    }

    pub fn set_total(&mut self, total: usize) {
        self.picker.set_total(total);
        if total > 0 && self.picker.selected_index >= total {
            self.picker.select(total.saturating_sub(1));
        }
        self.clicks.clear();
    }

    pub fn sync(&mut self, usage: &UsageSnapshot) {
        let selected_deployment = self
            .deployment_names
            .get(self.picker.selected_index)
            .cloned();
        self.set_total(usage.deployments.len());
        if let Some(index) = selected_deployment.and_then(|deployment| {
            usage
                .deployments
                .iter()
                .position(|item| item.deployment == deployment)
        }) {
            self.picker.select(index);
        }
        self.deployment_names = usage
            .deployments
            .iter()
            .map(|item| item.deployment.clone())
            .collect();
    }

    #[must_use]
    pub const fn selected(&self) -> usize {
        self.picker.selected_index
    }

    pub fn select(&mut self, index: usize) {
        self.picker.select(index);
        self.focus.set(UsageFocus::Deployments);
    }

    pub fn next_item(&mut self) {
        self.picker.select_next();
    }

    pub fn previous_item(&mut self) {
        self.picker.select_prev();
    }

    pub fn next_focus(&mut self) {
        let next = if self.editing {
            match self.focused() {
                Some(UsageFocus::InputRate) => UsageFocus::CachedRate,
                Some(UsageFocus::CachedRate) => UsageFocus::OutputRate,
                Some(UsageFocus::LongThreshold) => UsageFocus::LongInputRate,
                Some(UsageFocus::LongInputRate) => UsageFocus::LongCachedRate,
                Some(UsageFocus::LongCachedRate) => UsageFocus::LongOutputRate,
                Some(UsageFocus::LongOutputRate) => UsageFocus::Save,
                Some(UsageFocus::OutputRate) => UsageFocus::LongThreshold,
                Some(UsageFocus::Save) => UsageFocus::Remove,
                Some(UsageFocus::Remove) => UsageFocus::Cancel,
                _ => UsageFocus::InputRate,
            }
        } else {
            match self.focused() {
                Some(UsageFocus::Deployments) => UsageFocus::Edit,
                Some(UsageFocus::Edit) => UsageFocus::Close,
                _ => UsageFocus::Deployments,
            }
        };
        self.focus.set(next);
    }

    pub fn previous_focus(&mut self) {
        let previous = if self.editing {
            match self.focused() {
                Some(UsageFocus::InputRate) => UsageFocus::Cancel,
                Some(UsageFocus::CachedRate) => UsageFocus::InputRate,
                Some(UsageFocus::OutputRate) => UsageFocus::CachedRate,
                Some(UsageFocus::LongThreshold) => UsageFocus::OutputRate,
                Some(UsageFocus::LongInputRate) => UsageFocus::LongThreshold,
                Some(UsageFocus::LongCachedRate) => UsageFocus::LongInputRate,
                Some(UsageFocus::LongOutputRate) => UsageFocus::LongCachedRate,
                Some(UsageFocus::Save) => UsageFocus::LongOutputRate,
                Some(UsageFocus::Remove) => UsageFocus::Save,
                _ => UsageFocus::Remove,
            }
        } else {
            match self.focused() {
                Some(UsageFocus::Deployments) => UsageFocus::Close,
                Some(UsageFocus::Edit) => UsageFocus::Deployments,
                _ => UsageFocus::Edit,
            }
        };
        self.focus.set(previous);
    }

    #[must_use]
    pub fn focused(&self) -> Option<UsageFocus> {
        self.focus.current().copied()
    }

    pub fn focus(&mut self, focus: UsageFocus) {
        self.focus.set(focus);
    }

    #[must_use]
    pub const fn is_editing(&self) -> bool {
        self.editing
    }

    pub fn begin_edit(&mut self, item: &DeploymentUsageSnapshot) {
        self.editing = true;
        self.editing_deployment = Some(item.deployment.clone());
        self.editor_error = None;
        if let Some(rate) = item.pricing {
            self.input_rate = format_rate(rate.input_usd_per_million());
            self.cached_rate = format_rate(rate.cached_input_usd_per_million());
            self.output_rate = format_rate(rate.output_usd_per_million());
        } else {
            self.input_rate.clear();
            self.cached_rate.clear();
            self.output_rate.clear();
        }
        if let Some(long) = item.long_context_pricing {
            self.long_threshold = long.threshold_tokens.to_string();
            self.long_input_rate = format_rate(long.rate.input_usd_per_million());
            self.long_cached_rate = format_rate(long.rate.cached_input_usd_per_million());
            self.long_output_rate = format_rate(long.rate.output_usd_per_million());
        } else {
            self.long_threshold.clear();
            self.long_input_rate.clear();
            self.long_cached_rate.clear();
            self.long_output_rate.clear();
        }
        self.focus.set(UsageFocus::InputRate);
    }

    pub fn cancel_edit(&mut self) {
        self.editing = false;
        self.editing_deployment = None;
        self.editor_error = None;
        self.focus.set(UsageFocus::Edit);
    }

    pub fn push_rate_char(&mut self, value: char) {
        if !value.is_ascii_digit() && value != '.' {
            return;
        }
        let threshold = self.focused() == Some(UsageFocus::LongThreshold);
        if threshold && value == '.' {
            return;
        }
        let target = self.focused_rate_mut();
        if let Some(target) = target
            && target.len() < 24
            && (value != '.' || !target.contains('.'))
        {
            target.push(value);
            self.editor_error = None;
        }
    }

    pub fn pop_rate_char(&mut self) {
        if let Some(target) = self.focused_rate_mut() {
            target.pop();
            self.editor_error = None;
        }
    }

    pub fn push_rate_text(&mut self, value: &str) {
        for character in value.chars() {
            self.push_rate_char(character);
        }
    }

    #[must_use]
    pub fn selected_deployment(&self) -> Option<&str> {
        self.editing_deployment.as_deref().or_else(|| {
            self.deployment_names
                .get(self.picker.selected_index)
                .map(String::as_str)
        })
    }

    pub fn build_pricing(
        &mut self,
        deployment: &str,
    ) -> Result<DeploymentPricing, UsagePricingInputError> {
        let parse = |field: &str, value: &str| {
            value.parse::<f64>().map_err(|_| {
                UsagePricingInputError::localized(format!(
                    "{field}: {}",
                    text(Text::DecimalRatePerMillion)
                ))
            })
        };
        let input = parse(text(Text::Input), &self.input_rate)?;
        let cached = parse(text(Text::CachedInput), &self.cached_rate)?;
        let output = parse(text(Text::OutputLabel), &self.output_rate)?;
        let mut pricing = DeploymentPricing::from_usd_per_million(
            deployment.to_owned(),
            input,
            Some(cached),
            output,
        )
        .map_err(|_| UsagePricingInputError::localized(text(Text::InvalidPricingValue)))?;
        let long_values = [
            self.long_threshold.as_str(),
            self.long_input_rate.as_str(),
            self.long_cached_rate.as_str(),
            self.long_output_rate.as_str(),
        ];
        if long_values.iter().any(|value| !value.is_empty()) {
            if long_values.iter().any(|value| value.is_empty()) {
                return Err(UsagePricingInputError::localized(text(
                    Text::CompleteLongContextFields,
                )));
            }
            let threshold = self
                .long_threshold
                .parse::<u64>()
                .map_err(|_| UsagePricingInputError::localized(text(Text::WholeTokenThreshold)))?;
            let long_input = parse(text(Text::Input), &self.long_input_rate)?;
            let long_cached = parse(text(Text::CachedInput), &self.long_cached_rate)?;
            let long_output = parse(text(Text::OutputLabel), &self.long_output_rate)?;
            pricing = pricing
                .with_long_context_tier(threshold, long_input, Some(long_cached), long_output)
                .map_err(|_: PricingError| {
                    UsagePricingInputError::localized(text(Text::InvalidPricingValue))
                })?;
        }
        Ok(pricing.as_user_override())
    }

    pub fn set_editor_error(&mut self, message: String) {
        self.editor_error = Some(message);
    }

    fn focused_rate_mut(&mut self) -> Option<&mut String> {
        match self.focus.current().copied() {
            Some(UsageFocus::InputRate) => Some(&mut self.input_rate),
            Some(UsageFocus::CachedRate) => Some(&mut self.cached_rate),
            Some(UsageFocus::OutputRate) => Some(&mut self.output_rate),
            Some(UsageFocus::LongThreshold) => Some(&mut self.long_threshold),
            Some(UsageFocus::LongInputRate) => Some(&mut self.long_input_rate),
            Some(UsageFocus::LongCachedRate) => Some(&mut self.long_cached_rate),
            Some(UsageFocus::LongOutputRate) => Some(&mut self.long_output_rate),
            _ => None,
        }
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<UsageHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame<'_>,
        usage: &UsageSnapshot,
        context_budget: u32,
        editable: bool,
    ) {
        if !self.open {
            return;
        }
        self.sync(usage);
        let focused = self.focus.current().copied();
        let selected = self.picker.selected_index;
        let animation_frame = self.animation_frame;
        let editing = self.editing;
        let input_rate = self.input_rate.as_str();
        let cached_rate = self.cached_rate.as_str();
        let output_rate = self.output_rate.as_str();
        let long_threshold = self.long_threshold.as_str();
        let long_input_rate = self.long_input_rate.as_str();
        let long_cached_rate = self.long_cached_rate.as_str();
        let long_output_rate = self.long_output_rate.as_str();
        let editor_error = self.editor_error.as_deref();
        let editing_deployment = self.editing_deployment.as_deref();
        let picker = &mut self.picker;
        let clicks = &mut self.clicks;
        let config = DialogConfig::new(text(Text::UsageDialogTitle))
            .width_percent(78)
            .height_percent(76)
            .min_size(72, 25)
            .max_size(138, 50)
            .border_color(Color::Blue)
            .focused_border_color(Color::LightCyan)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            if editing {
                draw_pricing_editor(
                    frame,
                    area,
                    editing_deployment.and_then(|deployment| {
                        usage
                            .deployments
                            .iter()
                            .find(|item| item.deployment == deployment)
                    }),
                    focused,
                    input_rate,
                    cached_rate,
                    output_rate,
                    long_threshold,
                    long_input_rate,
                    long_cached_rate,
                    long_output_rate,
                    editor_error,
                    editable,
                    clicks,
                );
            } else {
                draw_usage(
                    frame,
                    area,
                    usage,
                    context_budget,
                    selected,
                    focused,
                    animation_frame,
                    editable,
                    picker,
                    clicks,
                );
            }
        });
        popup.render(frame);
    }
}

fn format_rate(value: f64) -> String {
    let value = format!("{value:.6}");
    value.trim_end_matches('0').trim_end_matches('.').to_owned()
}

impl Default for UsageUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_usage(
    frame: &mut Frame<'_>,
    area: Rect,
    usage: &UsageSnapshot,
    context_budget: u32,
    selected: usize,
    focused: Option<UsageFocus>,
    animation_frame: usize,
    editable: bool,
    picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<UsageHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(8),
        Constraint::Length(3),
    ])
    .split(area);
    let pulse = ["·", "•", "●", "•"][animation_frame % 4];
    let cost = match usage.cost_coverage() {
        CostCoverage::NoUsage => text(Text::UsageNoBilled).to_owned(),
        CostCoverage::Unpriced => text(Text::UsageTariffMissing).to_owned(),
        CostCoverage::Partial => format!(
            "{} + {}",
            format_microusd(usage.estimated_cost_microusd),
            text(Text::Unpriced)
        ),
        CostCoverage::Complete => format_microusd(usage.estimated_cost_microusd),
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("{pulse} {}", text(Text::UsageDialogTitle)),
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "{} {} ({} {}) · {} {} · {} {}",
                text(Text::Input),
                usage.usage.input_tokens,
                text(Text::CachedInput),
                usage.usage.cached_input_tokens,
                text(Text::OutputLabel),
                usage.usage.output_tokens,
                text(Text::TotalLabel),
                usage.usage.total_tokens
            )),
            Line::from(format!("{}: {cost}", text(Text::EstimatedCost))),
            Line::from(text(Text::ExactUsagePricingHelp)),
        ])
        .wrap(Wrap { trim: false }),
        rows[0],
    );

    let last = usage.last_response_tokens.unwrap_or(0);
    let ratio = if context_budget == 0 {
        0.0
    } else {
        (last as f64 / f64::from(context_budget)).clamp(0.0, 1.0)
    };
    frame.render_widget(
        Gauge::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", text(Text::LastResponseContextBudget))),
            )
            .gauge_style(Style::default().fg(if ratio >= 0.9 {
                Color::Red
            } else if ratio >= 0.7 {
                Color::Yellow
            } else {
                Color::Green
            }))
            .ratio(ratio)
            .label(format!(
                "{last} / {context_budget} {}",
                text(Text::TokensUnit)
            )),
        rows[1],
    );

    draw_deployments(frame, rows[2], usage, selected, focused, picker, clicks);
    draw_detail(frame, rows[3], usage.deployments.get(selected));

    let button_areas = Layout::horizontal([
        Constraint::Length(24),
        Constraint::Length(2),
        Constraint::Length(18),
        Constraint::Fill(1),
    ])
    .split(rows[4]);
    let edit_enabled = editable && usage.deployments.get(selected).is_some();
    let mut edit_state = if edit_enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    edit_state.set_focused(focused == Some(UsageFocus::Edit));
    let region = Button::new(text(Text::SetExactTariff), &edit_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default())
        .render_stateful(button_areas[0], frame.buffer_mut());
    if edit_enabled {
        clicks.register(region.area, UsageHit::Edit);
    }
    let mut close_state = ButtonState::enabled();
    close_state.set_focused(focused == Some(UsageFocus::Close));
    let region = Button::new(text(Text::CloseEsc), &close_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default())
        .render_stateful(button_areas[2], frame.buffer_mut());
    clicks.register(region.area, UsageHit::Close);
}

#[allow(clippy::too_many_arguments)]
fn draw_pricing_editor(
    frame: &mut Frame<'_>,
    area: Rect,
    item: Option<&DeploymentUsageSnapshot>,
    focused: Option<UsageFocus>,
    input_rate: &str,
    cached_rate: &str,
    output_rate: &str,
    long_threshold: &str,
    long_input_rate: &str,
    long_cached_rate: &str,
    long_output_rate: &str,
    error: Option<&str>,
    editable: bool,
    clicks: &mut ClickRegionRegistry<UsageHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .split(area);
    let deployment = item.map_or(text(Text::NoDeploymentSelected), |item| {
        item.deployment.as_str()
    });
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                sanitize_for_display(deployment),
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(text(Text::ExactPricePerMillionHelp)),
            Line::from(text(Text::LocalOverridePriorityHelp)),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", text(Text::ExactTariff))),
        ),
        rows[0],
    );
    let base_fields = Layout::horizontal([
        Constraint::Percentage(33),
        Constraint::Percentage(34),
        Constraint::Percentage(33),
    ])
    .split(rows[1]);
    for (area, title, value, focus, hit) in [
        (
            base_fields[0],
            format!("{} / 1M", text(Text::Input)),
            input_rate,
            UsageFocus::InputRate,
            UsageHit::InputRate,
        ),
        (
            base_fields[1],
            format!("{} / 1M", text(Text::CachedInput)),
            cached_rate,
            UsageFocus::CachedRate,
            UsageHit::CachedRate,
        ),
        (
            base_fields[2],
            format!("{} / 1M", text(Text::OutputLabel)),
            output_rate,
            UsageFocus::OutputRate,
            UsageHit::OutputRate,
        ),
    ] {
        draw_rate_field(
            frame,
            area,
            &title,
            value,
            focused == Some(focus),
            hit,
            clicks,
        );
    }
    let long_fields = Layout::horizontal([
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
    ])
    .split(rows[2]);
    for (area, title, value, focus, hit) in [
        (
            long_fields[0],
            format!("{} / {}", text(Text::LongContext), text(Text::TokensUnit)),
            long_threshold,
            UsageFocus::LongThreshold,
            UsageHit::LongThreshold,
        ),
        (
            long_fields[1],
            format!("{} / {}", text(Text::LongContext), text(Text::Input)),
            long_input_rate,
            UsageFocus::LongInputRate,
            UsageHit::LongInputRate,
        ),
        (
            long_fields[2],
            format!("{} / {}", text(Text::LongContext), text(Text::CachedInput)),
            long_cached_rate,
            UsageFocus::LongCachedRate,
            UsageHit::LongCachedRate,
        ),
        (
            long_fields[3],
            format!("{} / {}", text(Text::LongContext), text(Text::OutputLabel)),
            long_output_rate,
            UsageFocus::LongOutputRate,
            UsageHit::LongOutputRate,
        ),
    ] {
        draw_rate_field(
            frame,
            area,
            &title,
            value,
            focused == Some(focus),
            hit,
            clicks,
        );
    }
    frame.render_widget(
        Paragraph::new(error.map_or_else(|| text(Text::TabHint).to_owned(), sanitize_for_display))
            .style(Style::default().fg(if error.is_some() {
                Color::LightRed
            } else {
                Color::Gray
            }))
            .wrap(Wrap { trim: false }),
        rows[3],
    );
    let buttons = Layout::horizontal([
        Constraint::Length(22),
        Constraint::Length(2),
        Constraint::Length(20),
        Constraint::Length(2),
        Constraint::Length(18),
        Constraint::Fill(1),
    ])
    .split(rows[4]);
    let save_enabled = editable && item.is_some();
    let mut save_state = if save_enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    save_state.set_focused(focused == Some(UsageFocus::Save));
    let save = Button::new(text(Text::SaveRecalculate), &save_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default())
        .render_stateful(buttons[0], frame.buffer_mut());
    if save_enabled {
        clicks.register(save.area, UsageHit::Save);
    }
    let removable = editable
        && item.is_some_and(|item| {
            item.pricing_provenance.as_ref().is_some_and(|provenance| {
                provenance.source == crate::usage::PricingSource::UserOverride
            })
        });
    let mut remove_state = if removable {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    remove_state.set_focused(focused == Some(UsageFocus::Remove));
    let remove = Button::new(text(Text::RemoveOverride), &remove_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default())
        .render_stateful(buttons[2], frame.buffer_mut());
    if removable {
        clicks.register(remove.area, UsageHit::Remove);
    }
    let mut cancel_state = ButtonState::enabled();
    cancel_state.set_focused(focused == Some(UsageFocus::Cancel));
    let cancel = Button::new(text(Text::CancelEsc), &cancel_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default())
        .render_stateful(buttons[4], frame.buffer_mut());
    clicks.register(cancel.area, UsageHit::Cancel);
}

fn draw_rate_field(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    value: &str,
    focused: bool,
    hit: UsageHit,
    clicks: &mut ClickRegionRegistry<UsageHit>,
) {
    frame.render_widget(
        Paragraph::new(sanitize_for_display(value)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if focused {
                    Color::LightCyan
                } else {
                    Color::DarkGray
                }))
                .title(format!(" {title} ")),
        ),
        area,
    );
    clicks.register(area, hit);
}

fn draw_deployments(
    frame: &mut Frame<'_>,
    area: Rect,
    usage: &UsageSnapshot,
    _selected: usize,
    focused: Option<UsageFocus>,
    picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<UsageHit>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused == Some(UsageFocus::Deployments) {
            Style::default().fg(Color::LightCyan)
        } else {
            Style::default().fg(Color::Gray)
        })
        .title(format!(
            " {} ({}) ",
            text(Text::DeploymentBreakdown),
            usage.deployments.len()
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let labels = usage
        .deployments
        .iter()
        .map(|item| {
            let price = item
                .cost_microusd
                .map_or_else(|| text(Text::Unpriced).to_owned(), format_microusd);
            sanitize_for_display(&format!(
                "{} · {} {} · {}",
                item.deployment,
                item.usage.total_tokens,
                text(Text::TokensUnit),
                price
            ))
        })
        .collect::<Vec<_>>();
    let viewport = usize::from(inner.height);
    picker.ensure_visible(viewport);
    frame.render_widget(
        ListPicker::new(&labels, picker).style(ListPickerStyle::bracket().bordered(false)),
        inner,
    );
    for row in 0..viewport {
        let index = usize::from(picker.scroll).saturating_add(row);
        if index >= usage.deployments.len() {
            break;
        }
        clicks.register(
            Rect::new(inner.x, inner.y.saturating_add(row as u16), inner.width, 1),
            UsageHit::Deployment(index),
        );
    }
}

fn draw_detail(frame: &mut Frame<'_>, area: Rect, item: Option<&DeploymentUsageSnapshot>) {
    let lines = item.map_or_else(
        || vec![Line::from(text(Text::UsageNoBilled))],
        |item| {
            let provenance = item.pricing_provenance.as_ref().map_or_else(
                || format!("{}: {}", text(Text::SourceLabel), text(Text::Unavailable)),
                |provenance| {
                    let accuracy = if provenance.is_approximate() {
                        text(Text::ApproximateLabel)
                    } else {
                        text(Text::CatalogExact)
                    };
                    let updated = provenance
                        .updated_at
                        .as_deref()
                        .unwrap_or_else(|| text(Text::DateUnavailable));
                    format!(
                        "{}: {} · {accuracy} · {updated}",
                        text(Text::SourceLabel),
                        provenance.label
                    )
                },
            );
            let long_context = item.long_context_pricing.map_or_else(
                || format!("{}: {}", text(Text::LongContext), text(Text::BaseTariff)),
                |long| {
                    format!(
                        "{} > {}: ${:.4}/${:.4}/${:.4} / 1M",
                        text(Text::LongContext),
                        long.threshold_tokens,
                        long.rate.input_usd_per_million(),
                        long.rate.cached_input_usd_per_million(),
                        long.rate.output_usd_per_million()
                    )
                },
            );
            vec![
                Line::from(Span::styled(
                    truncate_for_display(&sanitize_for_display(&item.deployment), 512),
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(format!(
                    "{} {} · {} {} · {} {}",
                    text(Text::Input),
                    item.usage.input_tokens,
                    text(Text::CachedInput),
                    item.usage.cached_input_tokens,
                    text(Text::UncachedInput),
                    item.usage
                        .input_tokens
                        .saturating_sub(item.usage.cached_input_tokens)
                )),
                Line::from(format!(
                    "{} {} · {} {}",
                    text(Text::OutputLabel),
                    item.usage.output_tokens,
                    text(Text::TotalLabel),
                    item.usage.total_tokens
                )),
                Line::from(format!(
                    "{} {}",
                    text(Text::Cost),
                    item.cost_microusd
                        .map_or_else(|| text(Text::Unpriced).to_owned(), format_microusd)
                )),
                Line::from(sanitize_for_display(&provenance)),
                Line::from(sanitize_for_display(&long_context)),
            ]
        },
    );
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", text(Text::SelectedDeployment))),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::usage::{DeploymentUsageSnapshot, PricingProvenance, PricingSource, TokenUsage};

    fn deployment(name: &str) -> DeploymentUsageSnapshot {
        DeploymentUsageSnapshot {
            deployment: name.to_owned(),
            usage: TokenUsage::default(),
            cost_microusd: None,
            pricing: None,
            long_context_pricing: None,
            pricing_provenance: None,
        }
    }

    #[test]
    fn popup_exposes_real_deployment_and_close_mouse_regions()
    -> Result<(), Box<dyn std::error::Error>> {
        let usage = UsageSnapshot {
            usage: TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 40,
                output_tokens: 20,
                total_tokens: 120,
            },
            last_response_tokens: Some(120),
            estimated_cost_microusd: 42,
            has_unpriced_usage: false,
            pricing_configured: true,
            deployments: Arc::from([DeploymentUsageSnapshot {
                deployment: "prod".to_owned(),
                usage: TokenUsage {
                    input_tokens: 100,
                    cached_input_tokens: 40,
                    output_tokens: 20,
                    total_tokens: 120,
                },
                cost_microusd: Some(42),
                pricing: None,
                long_context_pricing: None,
                pricing_provenance: Some(PricingProvenance {
                    source: PricingSource::UserOverride,
                    label: "local exact override".to_owned(),
                    updated_at: Some("2026-08-28T00:00:00Z".to_owned()),
                }),
            }]),
        };
        let mut ui = UsageUiState::new();
        ui.open(1);
        let mut terminal = Terminal::new(TestBackend::new(110, 36))?;
        terminal.draw(|frame| ui.draw(frame, &usage, 1_000, true))?;
        let deployment = (0..36).any(|row| {
            (0..110).any(|column| ui.clicked(column, row) == Some(UsageHit::Deployment(0)))
        });
        let close = (0..36)
            .any(|row| (0..110).any(|column| ui.clicked(column, row) == Some(UsageHit::Close)));
        let edit = (0..36)
            .any(|row| (0..110).any(|column| ui.clicked(column, row) == Some(UsageHit::Edit)));
        assert!(deployment);
        assert!(close);
        assert!(edit);

        ui.begin_edit(&usage.deployments[0]);
        terminal.draw(|frame| ui.draw(frame, &usage, 1_000, true))?;
        for hit in [
            UsageHit::InputRate,
            UsageHit::CachedRate,
            UsageHit::OutputRate,
            UsageHit::LongThreshold,
            UsageHit::LongInputRate,
            UsageHit::LongCachedRate,
            UsageHit::LongOutputRate,
            UsageHit::Save,
            UsageHit::Remove,
            UsageHit::Cancel,
        ] {
            assert!(
                (0..36).any(|row| { (0..110).any(|column| ui.clicked(column, row) == Some(hit)) })
            );
        }
        for (focus, value) in [
            (UsageFocus::InputRate, "3.25"),
            (UsageFocus::CachedRate, "0.75"),
            (UsageFocus::OutputRate, "12.5"),
        ] {
            ui.focus(focus);
            for character in value.chars() {
                ui.push_rate_char(character);
            }
        }
        let pricing = ui.build_pricing("prod").map_err(std::io::Error::other)?;
        assert_eq!(pricing.rate_snapshot().output_usd_per_million(), 12.5);
        Ok(())
    }

    #[test]
    fn editor_builds_an_explicit_long_context_tier() -> Result<(), Box<dyn std::error::Error>> {
        let mut ui = UsageUiState::new();
        ui.input_rate = "2".to_owned();
        ui.cached_rate = "0.2".to_owned();
        ui.output_rate = "10".to_owned();
        ui.long_threshold = "200000".to_owned();
        ui.long_input_rate = "4".to_owned();
        ui.long_cached_rate = "0.4".to_owned();
        ui.long_output_rate = "15".to_owned();

        let pricing = ui.build_pricing("model").map_err(std::io::Error::other)?;
        let long = pricing
            .long_context_snapshot()
            .ok_or_else(|| std::io::Error::other("missing long-context tier"))?;
        assert_eq!(long.threshold_tokens, 200_000);
        assert_eq!(long.rate.output_usd_per_million(), 15.0);
        assert_eq!(pricing.provenance().source, PricingSource::UserOverride);
        Ok(())
    }

    #[test]
    fn selection_follows_the_deployment_when_usage_order_changes()
    -> Result<(), Box<dyn std::error::Error>> {
        let first = UsageSnapshot {
            deployments: Arc::from([deployment("first"), deployment("second")]),
            ..UsageSnapshot::default()
        };
        let second = UsageSnapshot {
            deployments: Arc::from([deployment("second"), deployment("first")]),
            ..UsageSnapshot::default()
        };
        let mut ui = UsageUiState::new();
        ui.open(first.deployments.len());
        let mut terminal = Terminal::new(TestBackend::new(110, 36))?;
        terminal.draw(|frame| ui.draw(frame, &first, 1_000, true))?;
        ui.select(1);

        terminal.draw(|frame| ui.draw(frame, &second, 1_000, true))?;

        assert_eq!(ui.selected(), 0);
        Ok(())
    }

    #[test]
    fn pricing_editor_stays_bound_to_its_deployment_after_reordering()
    -> Result<(), Box<dyn std::error::Error>> {
        let first = UsageSnapshot {
            deployments: Arc::from([deployment("first"), deployment("second")]),
            ..UsageSnapshot::default()
        };
        let second = UsageSnapshot {
            deployments: Arc::from([deployment("second"), deployment("first")]),
            ..UsageSnapshot::default()
        };
        let mut ui = UsageUiState::new();
        ui.open(first.deployments.len());
        ui.begin_edit(&first.deployments[0]);
        let mut terminal = Terminal::new(TestBackend::new(110, 36))?;

        terminal.draw(|frame| ui.draw(frame, &second, 1_000, true))?;

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("first"));
        assert!(!rendered.contains("second"));
        Ok(())
    }

    #[test]
    fn disabled_editor_actions_do_not_have_click_regions() -> Result<(), Box<dyn std::error::Error>>
    {
        let usage = UsageSnapshot::default();
        let mut ui = UsageUiState::new();
        ui.open(0);
        let mut terminal = Terminal::new(TestBackend::new(110, 36))?;

        terminal.draw(|frame| ui.draw(frame, &usage, 1_000, true))?;

        assert!(
            !(0..36).any(|row| {
                (0..110).any(|column| ui.clicked(column, row) == Some(UsageHit::Edit))
            })
        );
        Ok(())
    }
}
