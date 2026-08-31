use super::actions::ClickRegionRegistry;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_interact::{
    components::{
        Button, ButtonState, ButtonStyle, ButtonVariant, DialogConfig, DialogState, ListPicker,
        ListPickerState, ListPickerStyle, PopupDialog,
    },
    state::FocusManager,
};

use crate::api::ReasoningEffort;

use super::{
    i18n::{Text, text},
    render::sanitize_for_display,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStage {
    Closed,
    Model,
    Effort,
    Context,
    Confirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeFocus {
    Back,
    Primary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeHit {
    Item(usize),
    Back,
    Primary,
}

#[derive(Debug, Clone)]
pub struct RuntimeUiState {
    stage: RuntimeStage,
    dialog: DialogState<()>,
    model_picker: ListPickerState,
    effort_picker: ListPickerState,
    context_picker: ListPickerState,
    context_options: Vec<u32>,
    model_choices: Vec<String>,
    confirmed_model: Option<String>,
    focus: FocusManager<RuntimeFocus>,
    clicks: ClickRegionRegistry<RuntimeHit>,
}

impl RuntimeUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        focus.register(RuntimeFocus::Back);
        focus.register(RuntimeFocus::Primary);
        focus.set(RuntimeFocus::Primary);
        Self {
            stage: RuntimeStage::Closed,
            dialog: DialogState::new(()),
            model_picker: ListPickerState::new(0),
            effort_picker: ListPickerState::new(5),
            context_picker: ListPickerState::new(0),
            context_options: Vec::new(),
            model_choices: Vec::new(),
            confirmed_model: None,
            focus,
            clicks: ClickRegionRegistry::new(),
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        !matches!(self.stage, RuntimeStage::Closed)
    }

    #[must_use]
    pub const fn stage(&self) -> RuntimeStage {
        self.stage
    }

    #[must_use]
    pub fn focused(&self) -> Option<RuntimeFocus> {
        self.focus.current().copied()
    }

    pub fn begin_frame(&mut self) {
        self.clicks.clear();
    }

    pub fn open(
        &mut self,
        models: &[String],
        current: &str,
        effort: ReasoningEffort,
        deep_thinking: bool,
        context_budget: u32,
        max_context_budget: u32,
    ) {
        self.model_picker.set_total(models.len());
        self.model_choices = models.to_vec();
        self.confirmed_model = None;
        let model_index = models
            .iter()
            .position(|model| model == current)
            .unwrap_or(0);
        self.model_picker.select(model_index);
        self.effort_picker.set_total(6);
        self.effort_picker
            .select(effort_index(effort, deep_thinking));
        self.context_options = build_context_options(context_budget, max_context_budget);
        self.context_picker.set_total(self.context_options.len());
        self.context_picker.select(
            self.context_options
                .iter()
                .position(|value| *value == context_budget)
                .unwrap_or(0),
        );
        self.stage = RuntimeStage::Model;
        self.focus.set(RuntimeFocus::Primary);
        self.dialog.show();
    }

    pub fn close(&mut self) {
        self.stage = RuntimeStage::Closed;
        self.confirmed_model = None;
        self.dialog.hide();
        self.clicks.clear();
    }

    pub fn next(&mut self) {
        match self.stage {
            RuntimeStage::Model => self.model_picker.select_next(),
            RuntimeStage::Effort => self.effort_picker.select_next(),
            RuntimeStage::Context => self.context_picker.select_next(),
            RuntimeStage::Closed | RuntimeStage::Confirm => {}
        }
    }

    pub fn previous(&mut self) {
        match self.stage {
            RuntimeStage::Model => self.model_picker.select_prev(),
            RuntimeStage::Effort => self.effort_picker.select_prev(),
            RuntimeStage::Context => self.context_picker.select_prev(),
            RuntimeStage::Closed | RuntimeStage::Confirm => {}
        }
    }

    pub fn first(&mut self) {
        match self.stage {
            RuntimeStage::Model => self.model_picker.select_first(),
            RuntimeStage::Effort => self.effort_picker.select_first(),
            RuntimeStage::Context => self.context_picker.select_first(),
            RuntimeStage::Closed | RuntimeStage::Confirm => {}
        }
    }

    pub fn last(&mut self) {
        match self.stage {
            RuntimeStage::Model => self.model_picker.select_last(),
            RuntimeStage::Effort => self.effort_picker.select_last(),
            RuntimeStage::Context => self.context_picker.select_last(),
            RuntimeStage::Closed | RuntimeStage::Confirm => {}
        }
    }

    pub fn select(&mut self, index: usize) {
        match self.stage {
            RuntimeStage::Model => self.model_picker.select(index),
            RuntimeStage::Effort => self.effort_picker.select(index),
            RuntimeStage::Context => self.context_picker.select(index),
            RuntimeStage::Closed | RuntimeStage::Confirm => {}
        }
    }

    pub fn next_focus(&mut self) {
        self.focus.next();
    }

    pub fn previous_focus(&mut self) {
        self.focus.prev();
    }

    pub fn focus(&mut self, focus: RuntimeFocus) {
        self.focus.set(focus);
    }

    pub fn advance(&mut self) {
        self.stage = match self.stage {
            RuntimeStage::Model if self.selected_model().is_some() => RuntimeStage::Effort,
            RuntimeStage::Model => RuntimeStage::Model,
            RuntimeStage::Effort => RuntimeStage::Context,
            RuntimeStage::Context if self.selected_model().is_some() => {
                self.confirmed_model = self.selected_model().map(str::to_owned);
                RuntimeStage::Confirm
            }
            RuntimeStage::Context => RuntimeStage::Context,
            other => other,
        };
        self.focus.set(RuntimeFocus::Primary);
    }

    pub fn back(&mut self) {
        if matches!(self.stage, RuntimeStage::Confirm) {
            self.confirmed_model = None;
        }
        self.stage = match self.stage {
            RuntimeStage::Effort => RuntimeStage::Model,
            RuntimeStage::Context => RuntimeStage::Effort,
            RuntimeStage::Confirm => RuntimeStage::Context,
            RuntimeStage::Model | RuntimeStage::Closed => RuntimeStage::Closed,
        };
        if matches!(self.stage, RuntimeStage::Closed) {
            self.dialog.hide();
        }
        self.focus.set(RuntimeFocus::Primary);
    }

    #[must_use]
    pub const fn selected_model_index(&self) -> usize {
        self.model_picker.selected_index
    }

    #[must_use]
    pub fn selected_model(&self) -> Option<&str> {
        self.confirmed_model.as_deref().or_else(|| {
            self.model_choices
                .get(self.model_picker.selected_index)
                .map(String::as_str)
        })
    }

    #[must_use]
    pub const fn selected_effort(&self) -> ReasoningEffort {
        match self.effort_picker.selected_index {
            0 => ReasoningEffort::Low,
            2 => ReasoningEffort::High,
            3 => ReasoningEffort::XHigh,
            4 | 5 => ReasoningEffort::Max,
            _ => ReasoningEffort::Medium,
        }
    }

    #[must_use]
    pub const fn selected_ultra_profile(&self) -> bool {
        self.effort_picker.selected_index == 5
    }

    #[must_use]
    pub fn selected_context_budget(&self) -> u32 {
        self.context_options
            .get(self.context_picker.selected_index)
            .copied()
            .unwrap_or_default()
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<RuntimeHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, models: &[String], editable: bool) {
        if !self.is_open() {
            return;
        }
        let stage = self.stage;
        if !matches!(stage, RuntimeStage::Confirm) {
            self.sync_models(models);
        }
        let focused = self.focus.current().copied();
        let controls = RuntimeControls { focused, editable };
        let selected_model = self.selected_model().map(str::to_owned);
        let selected_effort = self.selected_effort();
        let selected_ultra = self.selected_ultra_profile();
        let selected_context_budget = self.selected_context_budget();
        let config = DialogConfig::new(match stage {
            RuntimeStage::Model => text(Text::ChooseModelDeployment),
            RuntimeStage::Effort => text(Text::ChooseReasoningEffort),
            RuntimeStage::Context => text(Text::ChooseContextBudget),
            RuntimeStage::Confirm => text(Text::ApplyRuntimeSettings),
            RuntimeStage::Closed => text(Text::RuntimeSettings),
        })
        .width_percent(68)
        .height_percent(60)
        .min_size(56, 15)
        .max_size(120, 38)
        .border_color(Color::Cyan)
        .focused_border_color(Color::LightCyan)
        .close_on_escape(false)
        .close_on_outside_click(false)
        .no_buttons();
        let model_picker = &mut self.model_picker;
        let effort_picker = &mut self.effort_picker;
        let context_picker = &mut self.context_picker;
        let context_values = self
            .context_options
            .iter()
            .map(|tokens| format_context_budget(*tokens))
            .collect::<Vec<_>>();
        let clicks = &mut self.clicks;
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| match stage {
            RuntimeStage::Model => draw_picker(
                frame,
                area,
                models,
                model_picker,
                text(Text::TrustedDeploymentHelp),
                controls,
                clicks,
            ),
            RuntimeStage::Effort => draw_picker(
                frame,
                area,
                &[
                    text(Text::Low).to_owned(),
                    text(Text::Medium).to_owned(),
                    text(Text::High).to_owned(),
                    "XHigh".to_owned(),
                    text(Text::MaxApiMaximum).to_owned(),
                    text(Text::UltraProfile).to_owned(),
                ],
                effort_picker,
                text(Text::UltraProfileHelp),
                controls,
                clicks,
            ),
            RuntimeStage::Context => draw_picker(
                frame,
                area,
                &context_values,
                context_picker,
                text(Text::ContextBudgetHelp),
                controls,
                clicks,
            ),
            RuntimeStage::Confirm => draw_confirm(
                frame,
                area,
                RuntimeSummary {
                    model: selected_model.as_deref(),
                    effort: selected_effort,
                    ultra_profile: selected_ultra,
                    context_budget: selected_context_budget,
                },
                controls,
                clicks,
            ),
            RuntimeStage::Closed => {}
        });
        popup.render(frame);
    }

    fn sync_models(&mut self, models: &[String]) {
        let selected = self
            .model_choices
            .get(self.model_picker.selected_index)
            .map(String::as_str);
        self.model_picker.set_total(models.len());
        if let Some(index) =
            selected.and_then(|selected| models.iter().position(|candidate| candidate == selected))
        {
            self.model_picker.select(index);
        } else if models.is_empty() {
            self.model_picker.select_first();
        }
        self.model_choices = models.to_vec();
        self.clicks.clear();
    }
}

impl Default for RuntimeUiState {
    fn default() -> Self {
        Self::new()
    }
}

fn draw_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    values: &[String],
    picker: &mut ListPickerState,
    help: &str,
    controls: RuntimeControls,
    clicks: &mut ClickRegionRegistry<RuntimeHit>,
) {
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(3),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(help).wrap(Wrap { trim: false }), chunks[0]);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::Select)));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);
    let safe_values = values
        .iter()
        .map(|value| sanitize_for_display(value))
        .collect::<Vec<_>>();
    let viewport = usize::from(inner.height);
    picker.ensure_visible(viewport);
    frame.render_widget(
        ListPicker::new(&safe_values, picker).style(ListPickerStyle::bracket().bordered(false)),
        inner,
    );
    for visible_row in 0..viewport {
        let index = usize::from(picker.scroll).saturating_add(visible_row);
        if index >= values.len() {
            break;
        }
        clicks.register(
            Rect::new(
                inner.x,
                inner.y.saturating_add(visible_row as u16),
                inner.width,
                1,
            ),
            RuntimeHit::Item(index),
        );
    }
    draw_buttons(
        frame,
        chunks[2],
        text(Text::CancelBack),
        text(Text::Next),
        controls.focused,
        controls.editable && !values.is_empty(),
        clicks,
    );
}

#[derive(Clone, Copy)]
struct RuntimeSummary<'a> {
    model: Option<&'a str>,
    effort: ReasoningEffort,
    ultra_profile: bool,
    context_budget: u32,
}

#[derive(Clone, Copy)]
struct RuntimeControls {
    focused: Option<RuntimeFocus>,
    editable: bool,
}

fn draw_confirm(
    frame: &mut Frame<'_>,
    area: Rect,
    summary: RuntimeSummary<'_>,
    controls: RuntimeControls,
    clicks: &mut ClickRegionRegistry<RuntimeHit>,
) {
    let chunks = Layout::vertical([Constraint::Min(5), Constraint::Length(3)]).split(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                text(Text::ApplyNextRequest),
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "{}: {}",
                text(Text::Deployment),
                sanitize_for_display(summary.model.unwrap_or(text(Text::Unavailable)))
            )),
            Line::from(format!(
                "{}:  {}",
                text(Text::Reasoning),
                if summary.ultra_profile {
                    text(Text::UltraProfile).to_owned()
                } else {
                    summary.effort.to_string()
                }
            )),
            Line::from(format!(
                "{}:    {}",
                text(Text::Context),
                format_context_budget(summary.context_budget)
            )),
            Line::from(text(Text::NoMidstreamSwitch)),
        ])
        .wrap(Wrap { trim: false }),
        chunks[0],
    );
    draw_buttons(
        frame,
        chunks[1],
        text(Text::Back),
        text(Text::Apply),
        controls.focused,
        controls.editable && summary.model.is_some() && summary.context_budget > 0,
        clicks,
    );
}

fn draw_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    back_label: &str,
    primary_label: &str,
    focused: Option<RuntimeFocus>,
    primary_enabled: bool,
    clicks: &mut ClickRegionRegistry<RuntimeHit>,
) {
    let columns = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(22),
        Constraint::Length(2),
        Constraint::Length(24),
        Constraint::Fill(1),
    ])
    .split(area);
    let mut back_state = ButtonState::enabled();
    back_state.set_focused(focused == Some(RuntimeFocus::Back));
    let back = Button::new(back_label, &back_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default());
    let region = back.render_stateful(columns[1], frame.buffer_mut());
    clicks.register(region.area, RuntimeHit::Back);

    let mut primary_state = if primary_enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    primary_state.set_focused(focused == Some(RuntimeFocus::Primary));
    let primary = Button::new(primary_label, &primary_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::primary());
    let region = primary.render_stateful(columns[3], frame.buffer_mut());
    if primary_enabled {
        clicks.register(region.area, RuntimeHit::Primary);
    }
}

const fn effort_index(effort: ReasoningEffort, deep_thinking: bool) -> usize {
    if deep_thinking {
        return 5;
    }
    match effort {
        ReasoningEffort::Low => 0,
        ReasoningEffort::Medium => 1,
        ReasoningEffort::High => 2,
        ReasoningEffort::XHigh => 3,
        ReasoningEffort::Max => 4,
    }
}

fn build_context_options(current: u32, maximum: u32) -> Vec<u32> {
    let maximum = maximum.clamp(1, crate::config::MAX_CONTEXT_BUDGET);
    let mut values = Vec::new();
    if current > 0 && current <= maximum {
        values.push(current);
    }
    let mut candidate = 100_000;
    while candidate <= maximum {
        values.push(candidate);
        candidate = candidate.saturating_add(100_000);
        if candidate == u32::MAX {
            break;
        }
    }
    values.push(maximum);
    values.sort_unstable();
    values.dedup();
    values
}

fn format_context_budget(tokens: u32) -> String {
    if tokens < 1_000 {
        format!("{tokens} {}", text(Text::TokensUnit))
    } else if tokens >= 1_000_000 && tokens.is_multiple_of(1_000_000) {
        format!("{}M {}", tokens / 1_000_000, text(Text::TokensUnit))
    } else {
        format!("{}K {}", tokens / 1_000, text(Text::TokensUnit))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use ratatui::{Terminal, backend::TestBackend};

    use crate::api::ReasoningEffort;

    use super::{
        RuntimeFocus, RuntimeHit, RuntimeStage, RuntimeUiState, build_context_options,
        format_context_budget,
    };

    #[test]
    fn context_picker_exposes_requested_large_budgets_through_trusted_ceiling() {
        let values = build_context_options(120_000, 2_000_000);
        assert!(values.contains(&500_000));
        assert!(values.contains(&1_000_000));
        assert!(values.contains(&2_000_000));
        assert!(values.iter().all(|value| *value <= 2_000_000));
    }

    #[test]
    fn runtime_picker_requires_model_effort_and_confirmation_stages() {
        let models = vec!["primary".to_owned(), "fast".to_owned()];
        let mut ui = RuntimeUiState::new();
        ui.open(
            &models,
            "fast",
            ReasoningEffort::High,
            false,
            200_000,
            2_000_000,
        );
        assert_eq!(ui.selected_model_index(), 1);
        assert_eq!(ui.stage(), RuntimeStage::Model);
        ui.advance();
        assert_eq!(ui.stage(), RuntimeStage::Effort);
        ui.advance();
        assert_eq!(ui.stage(), RuntimeStage::Context);
        assert_eq!(ui.selected_context_budget(), 200_000);
        ui.advance();
        assert_eq!(ui.stage(), RuntimeStage::Confirm);
        assert_eq!(ui.selected_effort(), ReasoningEffort::High);
    }

    #[test]
    fn context_budget_rows_have_real_mouse_hit_regions() -> Result<(), Box<dyn std::error::Error>> {
        let models = vec!["primary".to_owned()];
        let mut ui = RuntimeUiState::new();
        ui.open(
            &models,
            "primary",
            ReasoningEffort::XHigh,
            false,
            100_000,
            2_000_000,
        );
        ui.advance();
        ui.advance();
        assert_eq!(ui.stage(), RuntimeStage::Context);

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| {
            ui.begin_frame();
            ui.draw(frame, &models, true);
        })?;

        let mut row = None;
        for y in 0..30 {
            for x in 0..100 {
                if let Some(RuntimeHit::Item(index)) = ui.clicked(x, y) {
                    row = Some(index);
                    break;
                }
            }
            if row.is_some() {
                break;
            }
        }
        assert_eq!(row, Some(0));
        Ok(())
    }

    #[test]
    fn ultra_profile_maps_to_supported_max_plus_deep_selection() {
        let models = vec!["primary".to_owned()];
        let mut ui = RuntimeUiState::new();
        ui.open(
            &models,
            "primary",
            ReasoningEffort::Max,
            true,
            500_000,
            2_000_000,
        );
        ui.advance();
        assert_eq!(ui.selected_effort(), ReasoningEffort::Max);
        assert!(ui.selected_ultra_profile());
    }

    #[test]
    fn confirmation_keeps_the_selected_model_when_choices_reorder()
    -> Result<(), Box<dyn std::error::Error>> {
        let models = vec!["primary".to_owned(), "fast".to_owned()];
        let mut ui = RuntimeUiState::new();
        ui.open(
            &models,
            "fast",
            ReasoningEffort::High,
            false,
            200_000,
            2_000_000,
        );
        ui.advance();
        ui.advance();
        ui.advance();
        let reordered = vec!["fast".to_owned(), "primary".to_owned()];
        let mut terminal = Terminal::new(TestBackend::new(100, 30))?;

        terminal.draw(|frame| ui.draw(frame, &reordered, true))?;

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("fast"));
        assert!(!rendered.contains("primary"));
        Ok(())
    }

    #[test]
    fn sub_thousand_context_budget_is_not_displayed_as_zero() {
        let label = format_context_budget(999);
        assert!(label.starts_with("999 "));
        assert!(!label.starts_with("0K"));
    }

    #[test]
    fn every_runtime_stage_exposes_mouse_and_focus_actions()
    -> Result<(), Box<dyn std::error::Error>> {
        let models = vec!["primary".to_owned(), "fast".to_owned()];
        let mut ui = RuntimeUiState::new();
        ui.open(
            &models,
            "primary",
            ReasoningEffort::Medium,
            false,
            120_000,
            2_000_000,
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 30))?;

        for stage in [
            RuntimeStage::Model,
            RuntimeStage::Effort,
            RuntimeStage::Context,
            RuntimeStage::Confirm,
        ] {
            assert_eq!(ui.stage(), stage);
            terminal.draw(|frame| {
                ui.begin_frame();
                ui.draw(frame, &models, true);
            })?;
            let mut hits = HashSet::new();
            for row in 0..30 {
                for column in 0..100 {
                    if let Some(hit) = ui.clicked(column, row) {
                        hits.insert(hit);
                    }
                }
            }
            assert!(hits.contains(&RuntimeHit::Back));
            assert!(hits.contains(&RuntimeHit::Primary));
            if stage != RuntimeStage::Confirm {
                assert!(hits.contains(&RuntimeHit::Item(0)));
            }
            if stage != RuntimeStage::Confirm {
                ui.advance();
            }
        }

        assert_eq!(ui.focused(), Some(RuntimeFocus::Primary));
        ui.next_focus();
        assert_eq!(ui.focused(), Some(RuntimeFocus::Back));
        ui.previous_focus();
        assert_eq!(ui.focused(), Some(RuntimeFocus::Primary));
        Ok(())
    }
}
