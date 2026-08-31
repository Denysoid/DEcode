use std::collections::{BTreeMap, BTreeSet};

use super::actions::ClickRegionRegistry;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_interact::components::{
    Button, ButtonState, ButtonStyle, ButtonVariant, CheckBox, CheckBoxState, CheckBoxStyle,
    DialogConfig, DialogState, PopupDialog,
};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation as _;

use crate::{
    error::ConfigError,
    lsp::LspServerConfig,
    mcp::{
        McpApprovalMode, McpOAuthConfig, McpPermissionConfig, McpServerConfig, McpTransportConfig,
    },
};

use super::{
    i18n::{Text, text},
    render::{sanitize_for_display, truncate_for_display},
};

const MAX_FIELD_BYTES: usize = 8 * 1024;

#[derive(Debug, Error)]
pub enum ConnectionValidationError {
    #[error("{message}")]
    Field { message: String },
    #[error(transparent)]
    Config(#[from] ConfigError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionKind {
    Mcp,
    Lsp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionField {
    Name,
    Target,
    Args,
    CredentialEnv,
    Mapping,
    WorkingDirectory,
    OAuth,
    OAuthClientId,
    OAuthScopes,
    OAuthCallbackPort,
    Approval,
    EnabledTools,
    DisabledTools,
    TrustedTools,
    Advanced,
    Language,
    Extensions,
    RootMarkers,
    Transport,
    Required,
    AutoStart,
    Save,
    Cancel,
}

#[derive(Debug, Clone)]
pub struct ConnectionEditor {
    open: bool,
    kind: ConnectionKind,
    focus: ConnectionField,
    dialog: DialogState<()>,
    clicks: ClickRegionRegistry<ConnectionField>,
    name: String,
    target: String,
    args: String,
    credential_env: String,
    mapping: String,
    working_directory: String,
    oauth: bool,
    oauth_client_id: String,
    oauth_scopes: String,
    oauth_callback_port: String,
    approval: McpApprovalMode,
    enabled_tools: String,
    disabled_tools: String,
    trusted_tools: String,
    advanced: bool,
    language: String,
    extensions: String,
    root_markers: String,
    http: bool,
    required: bool,
    auto_start: bool,
    error: Option<String>,
}

impl ConnectionEditor {
    #[must_use]
    pub fn new(kind: ConnectionKind) -> Self {
        Self {
            open: false,
            kind,
            focus: ConnectionField::Name,
            dialog: DialogState::new(()),
            clicks: ClickRegionRegistry::new(),
            name: String::new(),
            target: String::new(),
            args: String::new(),
            credential_env: String::new(),
            mapping: String::new(),
            working_directory: String::new(),
            oauth: false,
            oauth_client_id: String::new(),
            oauth_scopes: String::new(),
            oauth_callback_port: String::new(),
            approval: McpApprovalMode::Always,
            enabled_tools: String::new(),
            disabled_tools: String::new(),
            trusted_tools: String::new(),
            advanced: false,
            language: String::new(),
            extensions: String::new(),
            root_markers: String::new(),
            http: false,
            required: false,
            auto_start: false,
            error: None,
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    #[must_use]
    pub const fn focus(&self) -> ConnectionField {
        self.focus
    }

    pub fn open(&mut self) {
        self.clear();
        self.open = true;
        self.dialog.show();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.error = None;
        self.dialog.hide();
        self.clicks.clear();
    }

    fn clear(&mut self) {
        self.focus = ConnectionField::Name;
        self.name.clear();
        self.target.clear();
        self.args.clear();
        self.credential_env.clear();
        self.mapping.clear();
        self.working_directory.clear();
        self.oauth = false;
        self.oauth_client_id.clear();
        self.oauth_scopes.clear();
        self.oauth_callback_port.clear();
        self.approval = McpApprovalMode::Always;
        self.enabled_tools.clear();
        self.disabled_tools.clear();
        self.trusted_tools.clear();
        self.advanced = false;
        self.language.clear();
        self.extensions.clear();
        self.root_markers.clear();
        self.http = false;
        self.required = false;
        self.auto_start = false;
        self.error = None;
    }

    pub fn begin_frame(&mut self) {
        self.clicks.clear();
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<ConnectionField> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn select(&mut self, field: ConnectionField) {
        if self.fields().contains(&field) {
            self.focus = field;
        }
    }

    pub fn next(&mut self) {
        let fields = self.fields();
        let index = fields
            .iter()
            .position(|field| *field == self.focus)
            .unwrap_or(0);
        self.focus = fields[(index + 1) % fields.len()];
    }

    pub fn previous(&mut self) {
        let fields = self.fields();
        let index = fields
            .iter()
            .position(|field| *field == self.focus)
            .unwrap_or(0);
        self.focus = fields[(index + fields.len() - 1) % fields.len()];
    }

    fn fields(&self) -> Vec<ConnectionField> {
        match self.kind {
            ConnectionKind::Mcp => {
                if self.advanced {
                    let mut fields = vec![ConnectionField::Mapping];
                    if self.http {
                        fields.push(ConnectionField::OAuth);
                        if self.oauth {
                            fields.extend([
                                ConnectionField::OAuthClientId,
                                ConnectionField::OAuthScopes,
                                ConnectionField::OAuthCallbackPort,
                            ]);
                        }
                    } else {
                        fields.push(ConnectionField::WorkingDirectory);
                    }
                    fields.extend([
                        ConnectionField::Approval,
                        ConnectionField::EnabledTools,
                        ConnectionField::DisabledTools,
                        ConnectionField::TrustedTools,
                        ConnectionField::Advanced,
                        ConnectionField::Save,
                        ConnectionField::Cancel,
                    ]);
                    return fields;
                }
                let mut fields = vec![ConnectionField::Name, ConnectionField::Target];
                if self.http {
                    fields.push(ConnectionField::CredentialEnv);
                } else {
                    fields.push(ConnectionField::Args);
                }
                fields.extend([
                    ConnectionField::Transport,
                    ConnectionField::Required,
                    ConnectionField::Advanced,
                    ConnectionField::Save,
                    ConnectionField::Cancel,
                ]);
                fields
            }
            ConnectionKind::Lsp => vec![
                ConnectionField::Name,
                ConnectionField::Target,
                ConnectionField::Args,
                ConnectionField::Language,
                ConnectionField::Extensions,
                ConnectionField::RootMarkers,
                ConnectionField::Required,
                ConnectionField::AutoStart,
                ConnectionField::Save,
                ConnectionField::Cancel,
            ],
        }
    }

    pub fn toggle(&mut self, field: ConnectionField) {
        match field {
            ConnectionField::Transport if self.kind == ConnectionKind::Mcp => {
                self.http = !self.http;
                self.args.clear();
                self.credential_env.clear();
                self.mapping.clear();
                self.working_directory.clear();
                self.oauth = false;
            }
            ConnectionField::OAuth if self.kind == ConnectionKind::Mcp && self.http => {
                self.oauth = !self.oauth;
                if self.oauth {
                    self.credential_env.clear();
                }
            }
            ConnectionField::Approval if self.kind == ConnectionKind::Mcp => {
                self.approval = match self.approval {
                    McpApprovalMode::Always => McpApprovalMode::Writes,
                    McpApprovalMode::Writes => McpApprovalMode::Never,
                    McpApprovalMode::Never => McpApprovalMode::Always,
                };
            }
            ConnectionField::Advanced if self.kind == ConnectionKind::Mcp => {
                self.advanced = !self.advanced;
                self.focus = if self.advanced {
                    ConnectionField::Mapping
                } else {
                    ConnectionField::Name
                };
            }
            ConnectionField::Required => self.required = !self.required,
            ConnectionField::AutoStart if self.kind == ConnectionKind::Lsp => {
                self.auto_start = !self.auto_start;
            }
            _ => {}
        }
    }

    pub fn push(&mut self, character: char) {
        if character.is_control() || self.current_text().is_none() {
            return;
        }
        let value = self.current_text_mut();
        if let Some(value) = value
            && value.len().saturating_add(character.len_utf8()) <= MAX_FIELD_BYTES
        {
            value.push(character);
        }
    }

    pub fn push_text(&mut self, text: &str) {
        for character in text.chars() {
            if character == '\n'
                && matches!(self.focus, ConnectionField::Args | ConnectionField::Mapping)
            {
                if let Some(value) = self.current_text_mut() {
                    if value.len() >= MAX_FIELD_BYTES {
                        break;
                    }
                    value.push('\n');
                }
                continue;
            }
            if character.is_control() {
                continue;
            }
            let Some(value) = self.current_text_mut() else {
                break;
            };
            if value.len().saturating_add(character.len_utf8()) > MAX_FIELD_BYTES {
                break;
            }
            value.push(character);
        }
    }

    pub fn newline(&mut self) {
        if matches!(self.focus, ConnectionField::Args | ConnectionField::Mapping)
            && let Some(value) = self.current_text_mut()
            && value.len() < MAX_FIELD_BYTES
        {
            value.push('\n');
        }
    }

    pub fn backspace(&mut self) {
        if let Some(value) = self.current_text_mut()
            && let Some((start, _)) = value.grapheme_indices(true).next_back()
        {
            value.truncate(start);
        }
    }

    fn current_text(&self) -> Option<&str> {
        match self.focus {
            ConnectionField::Name => Some(&self.name),
            ConnectionField::Target => Some(&self.target),
            ConnectionField::Args => Some(&self.args),
            ConnectionField::CredentialEnv => Some(&self.credential_env),
            ConnectionField::Mapping => Some(&self.mapping),
            ConnectionField::WorkingDirectory => Some(&self.working_directory),
            ConnectionField::OAuthClientId => Some(&self.oauth_client_id),
            ConnectionField::OAuthScopes => Some(&self.oauth_scopes),
            ConnectionField::OAuthCallbackPort => Some(&self.oauth_callback_port),
            ConnectionField::EnabledTools => Some(&self.enabled_tools),
            ConnectionField::DisabledTools => Some(&self.disabled_tools),
            ConnectionField::TrustedTools => Some(&self.trusted_tools),
            ConnectionField::Language => Some(&self.language),
            ConnectionField::Extensions => Some(&self.extensions),
            ConnectionField::RootMarkers => Some(&self.root_markers),
            _ => None,
        }
    }

    fn current_text_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            ConnectionField::Name => Some(&mut self.name),
            ConnectionField::Target => Some(&mut self.target),
            ConnectionField::Args => Some(&mut self.args),
            ConnectionField::CredentialEnv => Some(&mut self.credential_env),
            ConnectionField::Mapping => Some(&mut self.mapping),
            ConnectionField::WorkingDirectory => Some(&mut self.working_directory),
            ConnectionField::OAuthClientId => Some(&mut self.oauth_client_id),
            ConnectionField::OAuthScopes => Some(&mut self.oauth_scopes),
            ConnectionField::OAuthCallbackPort => Some(&mut self.oauth_callback_port),
            ConnectionField::EnabledTools => Some(&mut self.enabled_tools),
            ConnectionField::DisabledTools => Some(&mut self.disabled_tools),
            ConnectionField::TrustedTools => Some(&mut self.trusted_tools),
            ConnectionField::Language => Some(&mut self.language),
            ConnectionField::Extensions => Some(&mut self.extensions),
            ConnectionField::RootMarkers => Some(&mut self.root_markers),
            _ => None,
        }
    }

    pub fn set_error(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
    }

    pub fn mcp_server(&self) -> Result<McpServerConfig, ConnectionValidationError> {
        if self.kind != ConnectionKind::Mcp {
            return Err(ConnectionValidationError::Field {
                message: text(Text::NotMcpEditor).to_owned(),
            });
        }
        let mapping = mapping_lines(&self.mapping)?;
        let transport = if self.http {
            let oauth = if self.oauth {
                Some(McpOAuthConfig {
                    client_id: non_empty(&self.oauth_client_id),
                    scopes: comma_values(&self.oauth_scopes),
                    callback_port: parse_optional_port(&self.oauth_callback_port)?,
                })
            } else {
                None
            };
            McpTransportConfig::StreamableHttp {
                url: self.target.trim().to_owned(),
                bearer_token_env: non_empty(&self.credential_env),
                headers_from: mapping,
                oauth,
            }
        } else {
            McpTransportConfig::Stdio {
                command: self.target.trim().to_owned(),
                args: lines(&self.args),
                env_from: mapping,
                working_directory: non_empty(&self.working_directory).map(Into::into),
            }
        };
        let server = McpServerConfig {
            name: self.name.trim().to_owned(),
            enabled: true,
            required: self.required,
            transport,
            permissions: McpPermissionConfig {
                approval: self.approval,
                enabled_tools: comma_values(&self.enabled_tools)
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
                disabled_tools: comma_values(&self.disabled_tools)
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
                trusted_read_only_tools: comma_values(&self.trusted_tools)
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
            },
        };
        server.validate()?;
        Ok(server)
    }

    pub fn lsp_server(&self) -> Result<LspServerConfig, ConnectionValidationError> {
        if self.kind != ConnectionKind::Lsp {
            return Err(ConnectionValidationError::Field {
                message: text(Text::NotLspEditor).to_owned(),
            });
        }
        let server = LspServerConfig {
            name: self.name.trim().to_owned(),
            enabled: true,
            required: self.required,
            auto_start: self.auto_start,
            command: self.target.trim().to_owned(),
            args: lines(&self.args),
            language_id: self.language.trim().to_owned(),
            extensions: comma_values(&self.extensions),
            root_markers: comma_values(&self.root_markers),
        };
        server.validate()?;
        Ok(server)
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        if !self.open {
            return;
        }
        let config = DialogConfig::new(match self.kind {
            ConnectionKind::Mcp => text(Text::AddMcpServer),
            ConnectionKind::Lsp => text(Text::AddLanguageServer),
        })
        .width_percent(78)
        .height_percent(if self.kind == ConnectionKind::Mcp && self.advanced {
            90
        } else if self.kind == ConnectionKind::Mcp {
            76
        } else {
            88
        })
        .min_size(
            64,
            if self.kind == ConnectionKind::Mcp && self.advanced {
                40
            } else if self.kind == ConnectionKind::Mcp {
                26
            } else {
                36
            },
        )
        .max_size(
            132,
            if self.kind == ConnectionKind::Mcp && self.advanced {
                58
            } else if self.kind == ConnectionKind::Mcp {
                42
            } else {
                54
            },
        )
        .border_color(Color::Magenta)
        .focused_border_color(Color::LightCyan)
        .close_on_escape(false)
        .close_on_outside_click(false)
        .no_buttons();
        let view = EditorView::from(&*self);
        let clicks = &mut self.clicks;
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            draw_editor(frame, area, &view, clicks);
        });
        popup.render(frame);
    }
}

#[derive(Clone)]
struct EditorView {
    kind: ConnectionKind,
    focus: ConnectionField,
    name: String,
    target: String,
    args: String,
    credential_env: String,
    mapping: String,
    working_directory: String,
    oauth: bool,
    oauth_client_id: String,
    oauth_scopes: String,
    oauth_callback_port: String,
    approval: McpApprovalMode,
    enabled_tools: String,
    disabled_tools: String,
    trusted_tools: String,
    advanced: bool,
    language: String,
    extensions: String,
    root_markers: String,
    http: bool,
    required: bool,
    auto_start: bool,
    error: Option<String>,
}

impl From<&ConnectionEditor> for EditorView {
    fn from(editor: &ConnectionEditor) -> Self {
        Self {
            kind: editor.kind,
            focus: editor.focus,
            name: editor.name.clone(),
            target: editor.target.clone(),
            args: editor.args.clone(),
            credential_env: editor.credential_env.clone(),
            mapping: editor.mapping.clone(),
            working_directory: editor.working_directory.clone(),
            oauth: editor.oauth,
            oauth_client_id: editor.oauth_client_id.clone(),
            oauth_scopes: editor.oauth_scopes.clone(),
            oauth_callback_port: editor.oauth_callback_port.clone(),
            approval: editor.approval,
            enabled_tools: editor.enabled_tools.clone(),
            disabled_tools: editor.disabled_tools.clone(),
            trusted_tools: editor.trusted_tools.clone(),
            advanced: editor.advanced,
            language: editor.language.clone(),
            extensions: editor.extensions.clone(),
            root_markers: editor.root_markers.clone(),
            http: editor.http,
            required: editor.required,
            auto_start: editor.auto_start,
            error: editor.error.clone(),
        }
    }
}

fn draw_editor(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &EditorView,
    clicks: &mut ClickRegionRegistry<ConnectionField>,
) {
    if view.kind == ConnectionKind::Mcp && view.advanced {
        draw_mcp_advanced_editor(frame, area, view, clicks);
        return;
    }
    let constraints = if view.kind == ConnectionKind::Mcp {
        vec![
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(if view.http { 3 } else { 5 }),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(2),
            Constraint::Length(3),
        ]
    } else {
        vec![
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(2),
            Constraint::Length(3),
        ]
    };
    let rows = Layout::vertical(constraints).split(area);
    frame.render_widget(
        Paragraph::new(match view.kind {
            ConnectionKind::Mcp => text(Text::McpSecretsHelp),
            ConnectionKind::Lsp => text(Text::LspNoDownloadsHelp),
        })
        .wrap(Wrap { trim: false }),
        rows[0],
    );
    draw_field(
        frame,
        rows[1],
        text(Text::NameLabel),
        &view.name,
        ConnectionField::Name,
        view.focus,
        clicks,
    );
    draw_field(
        frame,
        rows[2],
        if view.kind == ConnectionKind::Mcp && view.http {
            text(Text::HttpsUrlLabel)
        } else {
            text(Text::ExecutableLabel)
        },
        &view.target,
        ConnectionField::Target,
        view.focus,
        clicks,
    );
    if view.kind == ConnectionKind::Mcp && view.http {
        draw_field(
            frame,
            rows[3],
            text(Text::BearerTokenOptional),
            &view.credential_env,
            ConnectionField::CredentialEnv,
            view.focus,
            clicks,
        );
    } else {
        draw_field(
            frame,
            rows[3],
            text(Text::ArgumentsPerLine),
            &view.args,
            ConnectionField::Args,
            view.focus,
            clicks,
        );
    }
    let mut row = 4;
    if view.kind == ConnectionKind::Mcp {
        draw_toggles(frame, rows[row], view, clicks);
        row += 1;
        draw_advanced_button(frame, rows[row], view, clicks);
    } else {
        draw_field(
            frame,
            rows[row],
            text(Text::LanguageIdLabel),
            &view.language,
            ConnectionField::Language,
            view.focus,
            clicks,
        );
        row += 1;
        draw_field(
            frame,
            rows[row],
            text(Text::ExtensionsComma),
            &view.extensions,
            ConnectionField::Extensions,
            view.focus,
            clicks,
        );
        row += 1;
        draw_field(
            frame,
            rows[row],
            text(Text::RootMarkersComma),
            &view.root_markers,
            ConnectionField::RootMarkers,
            view.focus,
            clicks,
        );
        row += 1;
        draw_toggles(frame, rows[row], view, clicks);
    }
    row += 1;
    let error = view
        .error
        .as_deref()
        .map_or(text(Text::ConnectionFieldsHelp), |error| error);
    frame.render_widget(
        Paragraph::new(truncate_for_display(
            &sanitize_for_display(error),
            usize::from(area.width),
        ))
        .style(Style::default().fg(if view.error.is_some() {
            Color::LightRed
        } else {
            Color::DarkGray
        })),
        rows[row],
    );
    row += 1;
    draw_buttons(frame, rows[row], view.focus, clicks);
}

fn draw_mcp_advanced_editor(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &EditorView,
    clicks: &mut ClickRegionRegistry<ConnectionField>,
) {
    let mut constraints = vec![Constraint::Length(2), Constraint::Length(5)];
    if view.http {
        constraints.push(Constraint::Length(3));
        if view.oauth {
            constraints.extend([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
            ]);
        }
    } else {
        constraints.push(Constraint::Length(3));
    }
    constraints.extend([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(2),
        Constraint::Length(3),
        Constraint::Length(3),
    ]);
    let rows = Layout::vertical(constraints).split(area);
    frame.render_widget(
        Paragraph::new(text(Text::AdvancedMcpPolicyHelp)).wrap(Wrap { trim: false }),
        rows[0],
    );
    let mut row = 1;
    draw_field(
        frame,
        rows[row],
        if view.http {
            text(Text::HttpHeadersMapping)
        } else {
            text(Text::ProcessEnvironmentMapping)
        },
        &view.mapping,
        ConnectionField::Mapping,
        view.focus,
        clicks,
    );
    row += 1;
    if view.http {
        render_checkbox(
            frame,
            rows[row],
            text(Text::OAuthDisablesBearer),
            view.oauth,
            view.focus == ConnectionField::OAuth,
            ConnectionField::OAuth,
            clicks,
        );
        row += 1;
        if view.oauth {
            for (title, value, field) in [
                (
                    text(Text::OAuthClientIdOptional),
                    view.oauth_client_id.as_str(),
                    ConnectionField::OAuthClientId,
                ),
                (
                    text(Text::OAuthScopesComma),
                    view.oauth_scopes.as_str(),
                    ConnectionField::OAuthScopes,
                ),
                (
                    text(Text::CallbackPortOptional),
                    view.oauth_callback_port.as_str(),
                    ConnectionField::OAuthCallbackPort,
                ),
            ] {
                draw_field(frame, rows[row], title, value, field, view.focus, clicks);
                row += 1;
            }
        }
    } else {
        draw_field(
            frame,
            rows[row],
            text(Text::WorkingDirectoryOptional),
            &view.working_directory,
            ConnectionField::WorkingDirectory,
            view.focus,
            clicks,
        );
        row += 1;
    }
    draw_cycle_button(
        frame,
        rows[row],
        format!(
            "{}: {}",
            text(Text::ApprovalModeLabel),
            approval_label(view.approval)
        ),
        ConnectionField::Approval,
        view.focus,
        clicks,
    );
    row += 1;
    for (title, value, field) in [
        (
            text(Text::EnabledToolsAllowlist),
            view.enabled_tools.as_str(),
            ConnectionField::EnabledTools,
        ),
        (
            text(Text::DisabledToolsDenylist),
            view.disabled_tools.as_str(),
            ConnectionField::DisabledTools,
        ),
        (
            text(Text::TrustedReadOnlyTools),
            view.trusted_tools.as_str(),
            ConnectionField::TrustedTools,
        ),
    ] {
        draw_field(frame, rows[row], title, value, field, view.focus, clicks);
        row += 1;
    }
    let error = view
        .error
        .as_deref()
        .unwrap_or_else(|| text(Text::AdvancedFieldsHelp));
    frame.render_widget(
        Paragraph::new(truncate_for_display(
            &sanitize_for_display(error),
            usize::from(area.width),
        ))
        .style(Style::default().fg(if view.error.is_some() {
            Color::LightRed
        } else {
            Color::DarkGray
        })),
        rows[row],
    );
    row += 1;
    draw_advanced_button(frame, rows[row], view, clicks);
    row += 1;
    draw_buttons(frame, rows[row], view.focus, clicks);
}

fn draw_field(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    value: &str,
    field: ConnectionField,
    focus: ConnectionField,
    clicks: &mut ClickRegionRegistry<ConnectionField>,
) {
    let focused = focus == field;
    let shown = sanitize_for_display(value);
    let text = if focused {
        format!("{shown}▏")
    } else {
        shown
    };
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {title} "))
                .border_style(Style::default().fg(if focused {
                    Color::LightCyan
                } else {
                    Color::DarkGray
                })),
        ),
        area,
    );
    clicks.register(area, field);
}

fn draw_toggles(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &EditorView,
    clicks: &mut ClickRegionRegistry<ConnectionField>,
) {
    let columns =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    if view.kind == ConnectionKind::Mcp {
        render_checkbox(
            frame,
            columns[0],
            text(Text::UseHttpTransport),
            view.http,
            view.focus == ConnectionField::Transport,
            ConnectionField::Transport,
            clicks,
        );
    } else {
        render_checkbox(
            frame,
            columns[0],
            text(Text::AutoStartNextLaunch),
            view.auto_start,
            view.focus == ConnectionField::AutoStart,
            ConnectionField::AutoStart,
            clicks,
        );
    }
    render_checkbox(
        frame,
        columns[1],
        text(Text::RequiredFailClosed),
        view.required,
        view.focus == ConnectionField::Required,
        ConnectionField::Required,
        clicks,
    );
}

fn draw_advanced_button(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &EditorView,
    clicks: &mut ClickRegionRegistry<ConnectionField>,
) {
    draw_cycle_button(
        frame,
        area,
        if view.advanced {
            text(Text::BasicConnectionSettings).to_owned()
        } else {
            text(Text::AdvancedSecuritySettings).to_owned()
        },
        ConnectionField::Advanced,
        view.focus,
        clicks,
    );
}

fn draw_cycle_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: String,
    field: ConnectionField,
    focus: ConnectionField,
    clicks: &mut ClickRegionRegistry<ConnectionField>,
) {
    let mut state = ButtonState::enabled();
    state.set_focused(focus == field);
    let region = Button::new(&label, &state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default())
        .render_stateful(area, frame.buffer_mut());
    clicks.register(region.area, field);
}

fn approval_label(mode: McpApprovalMode) -> &'static str {
    match mode {
        McpApprovalMode::Always => text(Text::AlwaysAsk),
        McpApprovalMode::Writes => text(Text::TrustedReadOnlyMayRun),
        McpApprovalMode::Never => text(Text::NeverAskDenylistWins),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_checkbox(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    checked: bool,
    focused: bool,
    field: ConnectionField,
    clicks: &mut ClickRegionRegistry<ConnectionField>,
) {
    let mut state = CheckBoxState::new(checked);
    state.set_focused(focused);
    let region = CheckBox::new(label, &state)
        .style(
            CheckBoxStyle::custom(text(Text::OnLabel), text(Text::OffLabel))
                .checked_fg(Color::Green)
                .focused_fg(Color::LightCyan),
        )
        .render_stateful(area, frame.buffer_mut());
    clicks.register(region.area, field);
}

fn draw_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    focus: ConnectionField,
    clicks: &mut ClickRegionRegistry<ConnectionField>,
) {
    let columns = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(22),
        Constraint::Length(2),
        Constraint::Length(18),
        Constraint::Fill(1),
    ])
    .split(area);
    render_button(
        frame,
        columns[1],
        text(Text::SaveConnection),
        ConnectionField::Save,
        focus == ConnectionField::Save,
        ButtonStyle::primary(),
        clicks,
    );
    render_button(
        frame,
        columns[3],
        text(Text::Cancel),
        ConnectionField::Cancel,
        focus == ConnectionField::Cancel,
        ButtonStyle::default(),
        clicks,
    );
}

fn render_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    field: ConnectionField,
    focused: bool,
    style: ButtonStyle,
    clicks: &mut ClickRegionRegistry<ConnectionField>,
) {
    let mut state = ButtonState::enabled();
    state.set_focused(focused);
    let region = Button::new(label, &state)
        .variant(ButtonVariant::Block)
        .style(style)
        .render_stateful(area, frame.buffer_mut());
    clicks.register(region.area, field);
}

fn lines(value: &str) -> Vec<String> {
    value
        .lines()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn comma_values(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn mapping_lines(value: &str) -> Result<BTreeMap<String, String>, ConnectionValidationError> {
    let mut mapping = BTreeMap::new();
    for (index, line) in value.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((target, source)) = line.split_once('=') else {
            return Err(ConnectionValidationError::Field {
                message: format!("{} {}", text(Text::MappingLineMustUseEquals), index + 1),
            });
        };
        let target = target.trim();
        let source = source.trim();
        if target.is_empty() || source.is_empty() {
            return Err(ConnectionValidationError::Field {
                message: format!("{} {}", text(Text::MappingNamesNonempty), index + 1),
            });
        }
        if mapping
            .insert(target.to_owned(), source.to_owned())
            .is_some()
        {
            return Err(ConnectionValidationError::Field {
                message: format!("{}: {target:?}", text(Text::MappingTargetDuplicated)),
            });
        }
    }
    Ok(mapping)
}

fn parse_optional_port(value: &str) -> Result<u16, ConnectionValidationError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(0);
    }
    value
        .parse::<u16>()
        .map_err(|_| ConnectionValidationError::Field {
            message: text(Text::OAuthPortRange).to_owned(),
        })
        .and_then(|port| {
            (port != 0)
                .then_some(port)
                .ok_or_else(|| ConnectionValidationError::Field {
                    message: text(Text::OAuthPortZeroEmpty).to_owned(),
                })
        })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};

    use ratatui::{Terminal, backend::TestBackend};

    use super::{ConnectionEditor, ConnectionField, ConnectionKind};

    #[test]
    fn mcp_editor_defaults_to_fail_closed_approval() -> Result<(), Box<dyn std::error::Error>> {
        let mut editor = ConnectionEditor::new(ConnectionKind::Mcp);
        editor.open();
        for character in "docs".chars() {
            editor.push(character);
        }
        editor.select(ConnectionField::Target);
        for character in "docs-server".chars() {
            editor.push(character);
        }
        let server = editor.mcp_server()?;
        assert!(matches!(
            server.permissions.approval,
            crate::mcp::McpApprovalMode::Always
        ));
        Ok(())
    }

    #[test]
    fn lsp_editor_preserves_argument_boundaries() -> Result<(), Box<dyn std::error::Error>> {
        let mut editor = ConnectionEditor::new(ConnectionKind::Lsp);
        editor.open();
        for (field, text) in [
            (ConnectionField::Name, "rust-analyzer"),
            (ConnectionField::Target, "rust-analyzer"),
            (ConnectionField::Args, "--stdio\n--verbose"),
            (ConnectionField::Language, "rust"),
            (ConnectionField::Extensions, ".rs"),
            (ConnectionField::RootMarkers, "Cargo.toml"),
        ] {
            editor.select(field);
            editor.push_text(text);
        }
        assert_eq!(editor.lsp_server()?.args, ["--stdio", "--verbose"]);
        Ok(())
    }

    #[test]
    fn backspace_removes_one_grapheme() {
        let mut editor = ConnectionEditor::new(ConnectionKind::Lsp);
        editor.open();
        editor.push_text("server 👩‍💻");

        editor.backspace();

        assert_eq!(editor.name, "server ");
    }

    #[test]
    fn pasted_field_stops_before_a_character_that_exceeds_the_byte_limit() {
        let mut editor = ConnectionEditor::new(ConnectionKind::Lsp);
        editor.open();
        editor.push_text(&"a".repeat(super::MAX_FIELD_BYTES - 1));

        editor.push_text("💻b");

        assert_eq!(editor.name.len(), super::MAX_FIELD_BYTES - 1);
        assert!(editor.name.ends_with('a'));
    }

    #[test]
    fn http_editor_excludes_process_only_arguments() {
        let mut editor = ConnectionEditor::new(ConnectionKind::Mcp);
        editor.open();
        editor.toggle(ConnectionField::Transport);

        editor.select(ConnectionField::Args);
        assert_ne!(editor.focus(), ConnectionField::Args);
        editor.select(ConnectionField::CredentialEnv);
        assert_eq!(editor.focus(), ConnectionField::CredentialEnv);
    }

    #[test]
    fn mcp_advanced_editor_builds_oauth_headers_and_tool_policy()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut editor = ConnectionEditor::new(ConnectionKind::Mcp);
        editor.open();
        for (field, value) in [
            (ConnectionField::Name, "remote-docs"),
            (ConnectionField::Target, "https://example.test/mcp"),
        ] {
            editor.select(field);
            editor.push_text(value);
        }
        editor.toggle(ConnectionField::Transport);
        editor.toggle(ConnectionField::Advanced);
        for (field, value) in [
            (
                ConnectionField::Mapping,
                "X-Tenant=TENANT_ID\nX-Trace=TRACE_ID",
            ),
            (ConnectionField::OAuthClientId, "decode-client"),
            (ConnectionField::OAuthScopes, "tools.read,tools.write"),
            (ConnectionField::OAuthCallbackPort, "4242"),
            (ConnectionField::EnabledTools, "search, fetch"),
            (ConnectionField::DisabledTools, "delete"),
            (ConnectionField::TrustedTools, "search"),
        ] {
            if field == ConnectionField::OAuthClientId {
                editor.toggle(ConnectionField::OAuth);
            }
            editor.select(field);
            editor.push_text(value);
        }
        editor.toggle(ConnectionField::Approval);

        let server = editor.mcp_server()?;
        assert!(matches!(
            server.permissions.approval,
            crate::mcp::McpApprovalMode::Writes
        ));
        assert!(server.permissions.disabled_tools.contains("delete"));
        match server.transport {
            crate::mcp::McpTransportConfig::StreamableHttp {
                headers_from,
                oauth: Some(oauth),
                ..
            } => {
                assert_eq!(
                    headers_from,
                    BTreeMap::from([
                        ("X-Tenant".to_owned(), "TENANT_ID".to_owned()),
                        ("X-Trace".to_owned(), "TRACE_ID".to_owned()),
                    ])
                );
                assert_eq!(oauth.callback_port, 4242);
                assert_eq!(oauth.scopes, ["tools.read", "tools.write"]);
            }
            _ => return Err("expected OAuth HTTP transport".to_owned().into()),
        }
        Ok(())
    }

    #[test]
    fn every_advanced_mcp_control_has_a_mouse_hit_region() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut editor = ConnectionEditor::new(ConnectionKind::Mcp);
        editor.open();
        editor.toggle(ConnectionField::Transport);
        editor.toggle(ConnectionField::Advanced);
        editor.toggle(ConnectionField::OAuth);
        let mut terminal = Terminal::new(TestBackend::new(120, 60))?;
        terminal.draw(|frame| {
            editor.begin_frame();
            editor.draw(frame);
        })?;
        let mut hits = HashSet::new();
        for row in 0..60 {
            for column in 0..120 {
                if let Some(hit) = editor.clicked(column, row) {
                    hits.insert(hit);
                }
            }
        }
        for expected in [
            ConnectionField::Mapping,
            ConnectionField::OAuth,
            ConnectionField::OAuthClientId,
            ConnectionField::OAuthScopes,
            ConnectionField::OAuthCallbackPort,
            ConnectionField::Approval,
            ConnectionField::EnabledTools,
            ConnectionField::DisabledTools,
            ConnectionField::TrustedTools,
            ConnectionField::Advanced,
            ConnectionField::Save,
            ConnectionField::Cancel,
        ] {
            assert!(
                hits.contains(&expected),
                "missing hit region for {expected:?}"
            );
        }
        Ok(())
    }
}
