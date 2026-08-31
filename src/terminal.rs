use std::{
    collections::BTreeMap,
    io::{Read, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{Arc, mpsc as std_mpsc},
    time::SystemTime,
};

use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use thiserror::Error;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{Duration, Instant, MissedTickBehavior},
};

use crate::config::InteractiveTerminalConfig;

const COMMAND_CAPACITY: usize = 256;
const INTERNAL_EVENT_CAPACITY: usize = 256;
const WRITER_CAPACITY: usize = 64;
const OUTPUT_CHUNK_BYTES: usize = 8 * 1024;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;
const MIN_ROWS: u16 = 2;
const MIN_COLS: u16 = 10;
const MAX_ROWS: u16 = 500;
const MAX_COLS: u16 = 1_000;
const PUBLISH_INTERVAL: Duration = Duration::from_millis(100);
const SHUTDOWN_GRACE: Duration = Duration::from_millis(1_500);

pub type TerminalSessionId = u64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminalColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalStyle {
    pub foreground: TerminalColor,
    pub background: TerminalColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalSpan {
    pub text: String,
    pub style: TerminalStyle,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalRow {
    pub spans: Arc<[TerminalSpan]>,
    pub wrapped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalFrame {
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub hide_cursor: bool,
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub alternate_screen: bool,
    pub mouse_mode: TerminalMouseMode,
    pub mouse_encoding: TerminalMouseEncoding,
    pub scrollback_offset: usize,
    pub content: Arc<[TerminalRow]>,
}

impl TerminalFrame {
    fn empty(rows: u16, cols: u16) -> Self {
        Self {
            rows,
            cols,
            cursor_row: 0,
            cursor_col: 0,
            hide_cursor: false,
            application_cursor: false,
            bracketed_paste: false,
            alternate_screen: false,
            mouse_mode: TerminalMouseMode::None,
            mouse_encoding: TerminalMouseEncoding::Default,
            scrollback_offset: 0,
            content: Arc::from([]),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminalMouseMode {
    #[default]
    None,
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminalMouseEncoding {
    #[default]
    Default,
    Utf8,
    Sgr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalStatus {
    Starting,
    Running,
    Stopping,
    Exited { code: u32, signal: Option<String> },
    Failed { failure: TerminalFailure },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalFailure {
    InputUnavailable,
    InputClosed,
    ParserResize,
    Start { detail: String },
    ParserOutput,
    Input { detail: String },
    Reap { detail: String },
    Stop { detail: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalNotice {
    Disabled,
    LimitReached {
        max_sessions: usize,
    },
    Starting {
        id: TerminalSessionId,
        cwd: PathBuf,
    },
    Missing {
        id: TerminalSessionId,
    },
    NotAcceptingInput {
        id: TerminalSessionId,
    },
    InputBackpressured {
        id: TerminalSessionId,
    },
    Closed {
        id: TerminalSessionId,
    },
    ResizeFailed {
        id: TerminalSessionId,
        detail: String,
    },
    Ready {
        id: TerminalSessionId,
    },
    StartFailed {
        id: TerminalSessionId,
        detail: String,
    },
    ParserFailed {
        id: TerminalSessionId,
    },
    OutputClosed {
        id: TerminalSessionId,
        detail: String,
    },
    InputFailed {
        id: TerminalSessionId,
    },
    Exited {
        id: TerminalSessionId,
        code: u32,
    },
    ReapFailed {
        id: TerminalSessionId,
        detail: String,
    },
    StopAfterStartup {
        id: TerminalSessionId,
    },
    StopFailed {
        id: TerminalSessionId,
        detail: String,
    },
    Stopping {
        id: TerminalSessionId,
    },
}

impl TerminalStatus {
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalSessionSnapshot {
    pub id: TerminalSessionId,
    pub title: String,
    pub cwd: PathBuf,
    pub created_at: SystemTime,
    pub status: TerminalStatus,
    pub process_id: Option<u32>,
    pub output_revision: u64,
    pub frame: Arc<TerminalFrame>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalFleetSnapshot {
    pub revision: u64,
    pub enabled: bool,
    pub max_sessions: usize,
    pub sessions: Arc<[TerminalSessionSnapshot]>,
    pub notice: Option<TerminalNotice>,
}

impl TerminalFleetSnapshot {
    #[must_use]
    pub fn empty(config: &InteractiveTerminalConfig) -> Self {
        Self {
            revision: 0,
            enabled: config.enabled,
            max_sessions: config.max_sessions,
            sessions: Arc::from([]),
            notice: None,
        }
    }
}

impl Default for TerminalFleetSnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            enabled: true,
            max_sessions: 0,
            sessions: Arc::from([]),
            notice: None,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TerminalControlError {
    #[error("terminal command queue is busy; try again")]
    Busy,
    #[error("terminal runtime is closed")]
    Closed,
    #[error("terminal input exceeds the {max_bytes}-byte safety limit")]
    InputTooLarge { max_bytes: usize },
}

#[derive(Debug, Error)]
enum TerminalRuntimeError {
    #[error("terminal spawn worker failed: {message}")]
    SpawnWorker { message: String },
    #[error("could not allocate a pseudo-terminal: {message}")]
    OpenPty { message: String },
    #[error("could not start the terminal process: {message}")]
    SpawnProcess { message: String },
    #[error("could not open the terminal output stream: {message}")]
    OutputStream { message: String },
    #[error("could not open the terminal input stream: {message}")]
    InputStream { message: String },
    #[error("could not reap the terminal process: {message}")]
    Reap { message: String },
}

#[derive(Clone, Debug)]
pub struct TerminalControl {
    tx: mpsc::Sender<TerminalCommand>,
}

impl TerminalControl {
    pub fn create(&self) -> Result<(), TerminalControlError> {
        self.try_send(TerminalCommand::Create)
    }

    pub fn input(&self, id: TerminalSessionId, bytes: Vec<u8>) -> Result<(), TerminalControlError> {
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(TerminalControlError::InputTooLarge {
                max_bytes: MAX_INPUT_BYTES,
            });
        }
        if bytes.is_empty() {
            return Ok(());
        }
        self.try_send(TerminalCommand::Input { id, bytes })
    }

    pub fn resize(
        &self,
        id: TerminalSessionId,
        rows: u16,
        cols: u16,
    ) -> Result<(), TerminalControlError> {
        self.try_send(TerminalCommand::Resize { id, rows, cols })
    }

    pub fn scroll(&self, id: TerminalSessionId, rows: i32) -> Result<(), TerminalControlError> {
        self.try_send(TerminalCommand::Scroll { id, rows })
    }

    pub fn jump_to_latest(&self, id: TerminalSessionId) -> Result<(), TerminalControlError> {
        self.try_send(TerminalCommand::JumpToLatest { id })
    }

    pub fn stop(&self, id: TerminalSessionId) -> Result<(), TerminalControlError> {
        self.try_send(TerminalCommand::Stop { id })
    }

    pub fn close(&self, id: TerminalSessionId) -> Result<(), TerminalControlError> {
        self.try_send(TerminalCommand::Close { id })
    }

    pub async fn shutdown(&self) {
        let _ = self.tx.send(TerminalCommand::Shutdown).await;
    }

    fn try_send(&self, command: TerminalCommand) -> Result<(), TerminalControlError> {
        self.tx.try_send(command).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => TerminalControlError::Busy,
            mpsc::error::TrySendError::Closed(_) => TerminalControlError::Closed,
        })
    }
}

enum TerminalCommand {
    Create,
    Input {
        id: TerminalSessionId,
        bytes: Vec<u8>,
    },
    Resize {
        id: TerminalSessionId,
        rows: u16,
        cols: u16,
    },
    Scroll {
        id: TerminalSessionId,
        rows: i32,
    },
    JumpToLatest {
        id: TerminalSessionId,
    },
    Stop {
        id: TerminalSessionId,
    },
    Close {
        id: TerminalSessionId,
    },
    Shutdown,
}

enum RuntimeEvent {
    Spawned {
        id: TerminalSessionId,
        result: Result<SpawnedPty, TerminalRuntimeError>,
    },
    Output {
        id: TerminalSessionId,
        bytes: Vec<u8>,
    },
    ReaderClosed {
        id: TerminalSessionId,
        error: Option<String>,
    },
    WriterFailed {
        id: TerminalSessionId,
        message: String,
    },
    Exited {
        id: TerminalSessionId,
        result: Result<portable_pty::ExitStatus, TerminalRuntimeError>,
    },
}

struct SpawnedPty {
    master: Box<dyn MasterPty + Send>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    process_id: Option<u32>,
}

struct RuntimeSession {
    id: TerminalSessionId,
    title: String,
    cwd: PathBuf,
    created_at: SystemTime,
    status: TerminalStatus,
    process_id: Option<u32>,
    output_revision: u64,
    parser: vt100::Parser,
    frame: Arc<TerminalFrame>,
    master: Option<Box<dyn MasterPty + Send>>,
    writer_tx: Option<std_mpsc::SyncSender<Vec<u8>>>,
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    stop_on_spawn: bool,
    close_when_exited: bool,
    awaiting_exit: bool,
    dirty: bool,
}

impl RuntimeSession {
    fn new(id: TerminalSessionId, cwd: PathBuf, scrollback_lines: usize) -> Self {
        Self {
            id,
            title: format!("Terminal {id}"),
            cwd,
            created_at: SystemTime::now(),
            status: TerminalStatus::Starting,
            process_id: None,
            output_revision: 0,
            parser: vt100::Parser::new(INITIAL_ROWS, INITIAL_COLS, scrollback_lines),
            frame: Arc::new(TerminalFrame::empty(INITIAL_ROWS, INITIAL_COLS)),
            master: None,
            writer_tx: None,
            killer: None,
            stop_on_spawn: false,
            close_when_exited: false,
            awaiting_exit: true,
            dirty: true,
        }
    }

    fn snapshot(&self) -> TerminalSessionSnapshot {
        TerminalSessionSnapshot {
            id: self.id,
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            created_at: self.created_at,
            status: self.status.clone(),
            process_id: self.process_id,
            output_revision: self.output_revision,
            frame: Arc::clone(&self.frame),
        }
    }
}

pub fn start_terminal_runtime(
    config: InteractiveTerminalConfig,
    workspace_root: PathBuf,
) -> (
    TerminalControl,
    watch::Receiver<TerminalFleetSnapshot>,
    JoinHandle<()>,
) {
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (snapshot_tx, snapshot_rx) = watch::channel(TerminalFleetSnapshot::empty(&config));
    let control = TerminalControl { tx: command_tx };
    let task = tokio::spawn(run_terminal_actor(
        config,
        workspace_root,
        command_rx,
        snapshot_tx,
    ));
    (control, snapshot_rx, task)
}

async fn run_terminal_actor(
    config: InteractiveTerminalConfig,
    workspace_root: PathBuf,
    mut command_rx: mpsc::Receiver<TerminalCommand>,
    snapshot_tx: watch::Sender<TerminalFleetSnapshot>,
) {
    let (event_tx, mut event_rx) = mpsc::channel(INTERNAL_EVENT_CAPACITY);
    let mut sessions = BTreeMap::<TerminalSessionId, RuntimeSession>::new();
    let mut next_id = 1_u64;
    let mut revision = 0_u64;
    let mut notice = None;
    let mut publish_tick = tokio::time::interval(PUBLISH_INTERVAL);
    publish_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut publish_needed = true;
    let mut shutting_down = false;
    let mut shutdown_deadline = None;

    loop {
        tokio::select! {
            command = command_rx.recv(), if !shutting_down => {
                let Some(command) = command else {
                    stop_all(&mut sessions);
                    shutting_down = true;
                    shutdown_deadline = Some(Instant::now() + SHUTDOWN_GRACE);
                    continue;
                };
                if matches!(command, TerminalCommand::Shutdown) {
                    command_rx.close();
                    stop_all(&mut sessions);
                    shutting_down = true;
                    shutdown_deadline = Some(Instant::now() + SHUTDOWN_GRACE);
                    if sessions.values().all(|session| !session.awaiting_exit) {
                        break;
                    }
                    continue;
                }
                handle_command(
                    command,
                    &config,
                    &workspace_root,
                    &mut sessions,
                    &mut next_id,
                    &event_tx,
                    &mut notice,
                );
                publish_needed = true;
            }
            event = event_rx.recv() => {
                let Some(event) = event else {
                    stop_all(&mut sessions);
                    break;
                };
                handle_runtime_event(event, &mut sessions, &event_tx, &mut notice);
                publish_needed = true;
                if shutting_down && sessions.values().all(|session| !session.awaiting_exit) {
                    break;
                }
            }
            _ = publish_tick.tick() => {
                if publish_needed {
                    refresh_frames(&mut sessions);
                    revision = revision.saturating_add(1);
                    let snapshot = TerminalFleetSnapshot {
                        revision,
                        enabled: config.enabled,
                        max_sessions: config.max_sessions,
                        sessions: Arc::from(
                            sessions.values().map(RuntimeSession::snapshot).collect::<Vec<_>>(),
                        ),
                        notice: notice.take(),
                    };
                    snapshot_tx.send_replace(snapshot);
                    publish_needed = false;
                }
                if shutting_down
                    && (sessions.values().all(|session| !session.awaiting_exit)
                        || shutdown_deadline.is_some_and(|deadline| Instant::now() >= deadline))
                {
                    break;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_command(
    command: TerminalCommand,
    config: &InteractiveTerminalConfig,
    workspace_root: &Path,
    sessions: &mut BTreeMap<TerminalSessionId, RuntimeSession>,
    next_id: &mut u64,
    event_tx: &mpsc::Sender<RuntimeEvent>,
    notice: &mut Option<TerminalNotice>,
) {
    match command {
        TerminalCommand::Create => {
            if !config.enabled {
                *notice = Some(TerminalNotice::Disabled);
                return;
            }
            if sessions.len() >= config.max_sessions {
                *notice = Some(TerminalNotice::LimitReached {
                    max_sessions: config.max_sessions,
                });
                return;
            }
            let id = *next_id;
            *next_id = next_id.saturating_add(1);
            sessions.insert(
                id,
                RuntimeSession::new(id, workspace_root.to_path_buf(), config.scrollback_lines),
            );
            spawn_pty_async(
                id,
                config.clone(),
                workspace_root.to_path_buf(),
                event_tx.clone(),
            );
            *notice = Some(TerminalNotice::Starting {
                id,
                cwd: workspace_root.to_path_buf(),
            });
        }
        TerminalCommand::Input { id, bytes } => {
            let Some(session) = sessions.get_mut(&id) else {
                *notice = Some(TerminalNotice::Missing { id });
                return;
            };
            if !matches!(session.status, TerminalStatus::Running) {
                *notice = Some(TerminalNotice::NotAcceptingInput { id });
                return;
            }
            let Some(writer_tx) = &session.writer_tx else {
                fail_session(session, TerminalFailure::InputUnavailable);
                return;
            };
            match writer_tx.try_send(bytes) {
                Ok(()) => {}
                Err(std_mpsc::TrySendError::Full(_)) => {
                    *notice = Some(TerminalNotice::InputBackpressured { id });
                }
                Err(std_mpsc::TrySendError::Disconnected(_)) => {
                    fail_session(session, TerminalFailure::InputClosed);
                }
            }
        }
        TerminalCommand::Resize { id, rows, cols } => {
            let Some(session) = sessions.get_mut(&id) else {
                return;
            };
            let rows = rows.clamp(MIN_ROWS, MAX_ROWS);
            let cols = cols.clamp(MIN_COLS, MAX_COLS);
            if session.parser.screen().size() == (rows, cols) {
                return;
            }
            if let Some(master) = &session.master
                && let Err(error) = master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
            {
                *notice = Some(TerminalNotice::ResizeFailed {
                    id,
                    detail: error.to_string(),
                });
                return;
            }
            let resized = catch_unwind(AssertUnwindSafe(|| {
                session.parser.screen_mut().set_size(rows, cols);
            }))
            .is_ok();
            if !resized {
                fail_session(session, TerminalFailure::ParserResize);
                return;
            }
            session.dirty = true;
        }
        TerminalCommand::Scroll { id, rows } => {
            let Some(session) = sessions.get_mut(&id) else {
                return;
            };
            let current = session.parser.screen().scrollback();
            let requested = if rows.is_negative() {
                current.saturating_add(rows.unsigned_abs() as usize)
            } else {
                current.saturating_sub(rows as usize)
            };
            session.parser.screen_mut().set_scrollback(requested);
            session.dirty = true;
        }
        TerminalCommand::JumpToLatest { id } => {
            if let Some(session) = sessions.get_mut(&id) {
                session.parser.screen_mut().set_scrollback(0);
                session.dirty = true;
            }
        }
        TerminalCommand::Stop { id } => {
            if let Some(session) = sessions.get_mut(&id) {
                request_stop(session, false, notice);
            }
        }
        TerminalCommand::Close { id } => {
            let remove_now = sessions
                .get(&id)
                .is_some_and(|session| !session.awaiting_exit);
            if remove_now {
                sessions.remove(&id);
                *notice = Some(TerminalNotice::Closed { id });
            } else if let Some(session) = sessions.get_mut(&id) {
                request_stop(session, true, notice);
            }
        }
        TerminalCommand::Shutdown => {}
    }
}

fn spawn_pty_async(
    id: TerminalSessionId,
    config: InteractiveTerminalConfig,
    workspace_root: PathBuf,
    event_tx: mpsc::Sender<RuntimeEvent>,
) {
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || spawn_pty(&config, &workspace_root))
            .await
            .map_err(|error| TerminalRuntimeError::SpawnWorker {
                message: error.to_string(),
            })
            .and_then(|result| result);
        let event = RuntimeEvent::Spawned { id, result };
        if let Err(error) = event_tx.send(event).await
            && let RuntimeEvent::Spawned {
                result: Ok(spawned),
                ..
            } = error.0
        {
            let _ = tokio::task::spawn_blocking(move || cleanup_spawned(spawned)).await;
        }
    });
}

fn spawn_pty(
    config: &InteractiveTerminalConfig,
    workspace_root: &Path,
) -> Result<SpawnedPty, TerminalRuntimeError> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: INITIAL_ROWS,
            cols: INITIAL_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| TerminalRuntimeError::OpenPty {
            message: error.to_string(),
        })?;
    let (program, args) = shell_command(config);
    let mut command = CommandBuilder::new(&program);
    command.args(&args);
    command.cwd(workspace_root);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "decode");
    command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    command.env("PAGER", "cat");
    command.env("GIT_PAGER", "cat");
    command.env("SYSTEMD_PAGER", "cat");
    let mut child =
        pair.slave
            .spawn_command(command)
            .map_err(|error| TerminalRuntimeError::SpawnProcess {
                message: error.to_string(),
            })?;
    let process_id = child.process_id();
    let mut killer = child.clone_killer();
    drop(pair.slave);
    // Follow portable-pty's required lifecycle order: spawn and release the
    // slave handles before cloning the reader/taking the writer. On ConPTY,
    // taking the streams before CreateProcess can leave the interactive side
    // connected but unable to deliver input.
    let reader = match pair.master.try_clone_reader() {
        Ok(reader) => reader,
        Err(error) => {
            let _ = kill_pty_child(killer.as_mut());
            let _ = child.wait();
            return Err(TerminalRuntimeError::OutputStream {
                message: error.to_string(),
            });
        }
    };
    let writer = match pair.master.take_writer() {
        Ok(writer) => writer,
        Err(error) => {
            let _ = kill_pty_child(killer.as_mut());
            let _ = child.wait();
            return Err(TerminalRuntimeError::InputStream {
                message: error.to_string(),
            });
        }
    };
    Ok(SpawnedPty {
        master: pair.master,
        reader,
        writer,
        child,
        killer,
        process_id,
    })
}

fn shell_command(config: &InteractiveTerminalConfig) -> (String, Vec<String>) {
    if let Some(program) = &config.program {
        return (program.clone(), config.args.clone());
    }
    #[cfg(windows)]
    {
        let args = if config.args.is_empty() {
            vec!["-NoLogo".to_owned()]
        } else {
            config.args.clone()
        };
        ("powershell.exe".to_owned(), args)
    }
    #[cfg(not(windows))]
    {
        let program = std::env::var("SHELL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".to_owned());
        (program, config.args.clone())
    }
}

fn handle_runtime_event(
    event: RuntimeEvent,
    sessions: &mut BTreeMap<TerminalSessionId, RuntimeSession>,
    event_tx: &mpsc::Sender<RuntimeEvent>,
    notice: &mut Option<TerminalNotice>,
) {
    match event {
        RuntimeEvent::Spawned { id, result } => match result {
            Ok(spawned) => {
                let Some(session) = sessions.get_mut(&id) else {
                    tokio::task::spawn_blocking(move || cleanup_spawned(spawned));
                    return;
                };
                let (rows, cols) = session.parser.screen().size();
                if (rows, cols) != (INITIAL_ROWS, INITIAL_COLS)
                    && let Err(error) = spawned.master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                {
                    let detail = error.to_string();
                    let remove = session.close_when_exited;
                    session.awaiting_exit = false;
                    session.status = TerminalStatus::Failed {
                        failure: TerminalFailure::Start {
                            detail: detail.clone(),
                        },
                    };
                    session.dirty = true;
                    tokio::task::spawn_blocking(move || cleanup_spawned(spawned));
                    if remove {
                        sessions.remove(&id);
                    }
                    *notice = Some(TerminalNotice::StartFailed { id, detail });
                    return;
                }
                attach_spawned(session, spawned, event_tx.clone());
                if session.stop_on_spawn {
                    request_stop(session, session.close_when_exited, notice);
                } else {
                    session.status = TerminalStatus::Running;
                    session.dirty = true;
                    *notice = Some(TerminalNotice::Ready { id });
                }
            }
            Err(error) => {
                let message = error.to_string();
                let remove = sessions
                    .get(&id)
                    .is_some_and(|session| session.close_when_exited);
                if let Some(session) = sessions.get_mut(&id) {
                    session.awaiting_exit = false;
                    fail_session(
                        session,
                        TerminalFailure::Start {
                            detail: message.clone(),
                        },
                    );
                }
                if remove {
                    sessions.remove(&id);
                }
                *notice = Some(TerminalNotice::StartFailed {
                    id,
                    detail: message,
                });
            }
        },
        RuntimeEvent::Output { id, bytes } => {
            let Some(session) = sessions.get_mut(&id) else {
                return;
            };
            let parsed = catch_unwind(AssertUnwindSafe(|| session.parser.process(&bytes))).is_ok();
            if parsed {
                session.output_revision = session.output_revision.saturating_add(1);
                session.dirty = true;
            } else {
                fail_session(session, TerminalFailure::ParserOutput);
                *notice = Some(TerminalNotice::ParserFailed { id });
            }
        }
        RuntimeEvent::ReaderClosed { id, error } => {
            if let Some(error) = error
                && let Some(session) = sessions.get(&id)
                && session.status.is_active()
            {
                *notice = Some(TerminalNotice::OutputClosed { id, detail: error });
            }
        }
        RuntimeEvent::WriterFailed { id, message } => {
            if let Some(session) = sessions.get_mut(&id) {
                fail_session(session, TerminalFailure::Input { detail: message });
                *notice = Some(TerminalNotice::InputFailed { id });
            }
        }
        RuntimeEvent::Exited { id, result } => {
            let close = sessions
                .get(&id)
                .is_some_and(|session| session.close_when_exited);
            if let Some(session) = sessions.get_mut(&id) {
                session.awaiting_exit = false;
                session.master = None;
                session.writer_tx = None;
                session.killer = None;
                match result {
                    Ok(status) => {
                        let code = status.exit_code();
                        let signal = status.signal().map(str::to_owned);
                        if !matches!(session.status, TerminalStatus::Failed { .. }) {
                            session.status = TerminalStatus::Exited { code, signal };
                        }
                        *notice = Some(TerminalNotice::Exited { id, code });
                    }
                    Err(error) => {
                        let message = error.to_string();
                        session.status = TerminalStatus::Failed {
                            failure: TerminalFailure::Reap {
                                detail: message.clone(),
                            },
                        };
                        *notice = Some(TerminalNotice::ReapFailed {
                            id,
                            detail: message,
                        });
                    }
                }
                session.dirty = true;
            }
            if close {
                sessions.remove(&id);
            }
        }
    }
}

fn attach_spawned(
    session: &mut RuntimeSession,
    spawned: SpawnedPty,
    event_tx: mpsc::Sender<RuntimeEvent>,
) {
    let SpawnedPty {
        master,
        mut reader,
        mut writer,
        mut child,
        killer,
        process_id,
    } = spawned;
    let id = session.id;
    let reader_events = event_tx.clone();
    tokio::task::spawn_blocking(move || {
        let mut buffer = vec![0_u8; OUTPUT_CHUNK_BYTES];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    let _ =
                        reader_events.blocking_send(RuntimeEvent::ReaderClosed { id, error: None });
                    break;
                }
                Ok(read) => {
                    if reader_events
                        .blocking_send(RuntimeEvent::Output {
                            id,
                            bytes: buffer[..read].to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    let _ = reader_events.blocking_send(RuntimeEvent::ReaderClosed {
                        id,
                        error: Some(error.to_string()),
                    });
                    break;
                }
            }
        }
    });

    let (writer_tx, writer_rx) = std_mpsc::sync_channel::<Vec<u8>>(WRITER_CAPACITY);
    let writer_events = event_tx.clone();
    tokio::task::spawn_blocking(move || {
        while let Ok(bytes) = writer_rx.recv() {
            if let Err(error) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
                let _ = writer_events.blocking_send(RuntimeEvent::WriterFailed {
                    id,
                    message: error.to_string(),
                });
                break;
            }
        }
    });

    tokio::task::spawn_blocking(move || {
        let result = child.wait().map_err(|error| TerminalRuntimeError::Reap {
            message: error.to_string(),
        });
        let _ = event_tx.blocking_send(RuntimeEvent::Exited { id, result });
    });

    session.master = Some(master);
    session.writer_tx = Some(writer_tx);
    session.killer = Some(killer);
    session.process_id = process_id;
}

fn request_stop(
    session: &mut RuntimeSession,
    close_when_exited: bool,
    notice: &mut Option<TerminalNotice>,
) {
    session.close_when_exited |= close_when_exited;
    match session.status {
        TerminalStatus::Starting => {
            session.stop_on_spawn = true;
            session.status = TerminalStatus::Stopping;
            *notice = Some(TerminalNotice::StopAfterStartup { id: session.id });
        }
        TerminalStatus::Running | TerminalStatus::Stopping => {
            session.status = TerminalStatus::Stopping;
            session.writer_tx = None;
            if let Some(killer) = session.killer.as_mut()
                && let Err(error) = kill_pty_child(killer.as_mut())
            {
                session.status = TerminalStatus::Failed {
                    failure: TerminalFailure::Stop {
                        detail: error.to_string(),
                    },
                };
                *notice = Some(TerminalNotice::StopFailed {
                    id: session.id,
                    detail: error.to_string(),
                });
                session.dirty = true;
                return;
            }
            *notice = Some(TerminalNotice::Stopping { id: session.id });
        }
        TerminalStatus::Failed { .. } if session.awaiting_exit => {
            session.writer_tx = None;
            if let Some(killer) = session.killer.as_mut() {
                if let Err(error) = kill_pty_child(killer.as_mut()) {
                    *notice = Some(TerminalNotice::StopFailed {
                        id: session.id,
                        detail: error.to_string(),
                    });
                } else {
                    *notice = Some(TerminalNotice::Stopping { id: session.id });
                }
            }
        }
        TerminalStatus::Exited { .. } | TerminalStatus::Failed { .. } => {}
    }
    session.dirty = true;
}

fn fail_session(session: &mut RuntimeSession, failure: TerminalFailure) {
    if let Some(killer) = session.killer.as_mut() {
        let _ = kill_pty_child(killer.as_mut());
    } else if matches!(session.status, TerminalStatus::Starting) {
        session.stop_on_spawn = true;
    }
    session.writer_tx = None;
    session.status = TerminalStatus::Failed { failure };
    session.dirty = true;
}

fn stop_all(sessions: &mut BTreeMap<TerminalSessionId, RuntimeSession>) {
    for session in sessions.values_mut() {
        session.stop_on_spawn = true;
        session.writer_tx = None;
        if session.awaiting_exit && !matches!(session.status, TerminalStatus::Failed { .. }) {
            session.status = TerminalStatus::Stopping;
        }
        if let Some(killer) = session.killer.as_mut() {
            let _ = kill_pty_child(killer.as_mut());
        }
    }
}

fn cleanup_spawned(mut spawned: SpawnedPty) {
    let _ = kill_pty_child(spawned.killer.as_mut());
    let _ = spawned.child.wait();
}

fn kill_pty_child(killer: &mut dyn ChildKiller) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        // portable-pty 0.9 performs TerminateProcess correctly but inverts the
        // Windows BOOL when constructing its Result. The authoritative exit
        // signal is the independently waited child handle, so do not turn that
        // dependency reporting bug into a false Failed state.
        let _ = killer.kill();
        Ok(())
    }
    #[cfg(not(windows))]
    {
        killer.kill()
    }
}

fn refresh_frames(sessions: &mut BTreeMap<TerminalSessionId, RuntimeSession>) {
    for session in sessions.values_mut().filter(|session| session.dirty) {
        session.frame = Arc::new(frame_from_screen(session.parser.screen()));
        session.dirty = false;
    }
}

fn frame_from_screen(screen: &vt100::Screen) -> TerminalFrame {
    // This is the terminal equivalent of sanitizer-first rendering: raw PTY
    // bytes and escape sequences never reach Ratatui. vt100 interprets them
    // into bounded cells, and the UI receives only text plus typed styles.
    let (rows, cols) = screen.size();
    let (cursor_row, cursor_col) = screen.cursor_position();
    let mut content = Vec::with_capacity(usize::from(rows));
    for row in 0..rows {
        let mut spans = Vec::<TerminalSpan>::new();
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                append_span(&mut spans, TerminalStyle::default(), " ");
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let text = if cell.has_contents() {
                cell.contents()
            } else {
                " "
            };
            let safe_text = sanitize_cell_text(text);
            append_span(&mut spans, style_from_cell(cell), &safe_text);
        }
        content.push(TerminalRow {
            spans: Arc::from(spans),
            wrapped: screen.row_wrapped(row),
        });
    }
    TerminalFrame {
        rows,
        cols,
        cursor_row,
        cursor_col,
        hide_cursor: screen.hide_cursor(),
        application_cursor: screen.application_cursor(),
        bracketed_paste: screen.bracketed_paste(),
        alternate_screen: screen.alternate_screen(),
        mouse_mode: match screen.mouse_protocol_mode() {
            vt100::MouseProtocolMode::None => TerminalMouseMode::None,
            vt100::MouseProtocolMode::Press => TerminalMouseMode::Press,
            vt100::MouseProtocolMode::PressRelease => TerminalMouseMode::PressRelease,
            vt100::MouseProtocolMode::ButtonMotion => TerminalMouseMode::ButtonMotion,
            vt100::MouseProtocolMode::AnyMotion => TerminalMouseMode::AnyMotion,
        },
        mouse_encoding: match screen.mouse_protocol_encoding() {
            vt100::MouseProtocolEncoding::Default => TerminalMouseEncoding::Default,
            vt100::MouseProtocolEncoding::Utf8 => TerminalMouseEncoding::Utf8,
            vt100::MouseProtocolEncoding::Sgr => TerminalMouseEncoding::Sgr,
        },
        scrollback_offset: screen.scrollback(),
        content: Arc::from(content),
    }
}

fn append_span(spans: &mut Vec<TerminalSpan>, style: TerminalStyle, text: &str) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.text.push_str(text);
        return;
    }
    spans.push(TerminalSpan {
        text: text.to_owned(),
        style,
    });
}

fn sanitize_cell_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

fn style_from_cell(cell: &vt100::Cell) -> TerminalStyle {
    TerminalStyle {
        foreground: color_from_vt(cell.fgcolor()),
        background: color_from_vt(cell.bgcolor()),
        bold: cell.bold(),
        dim: cell.dim(),
        italic: cell.italic(),
        underline: cell.underline(),
        inverse: cell.inverse(),
    }
}

const fn color_from_vt(color: vt100::Color) -> TerminalColor {
    match color {
        vt100::Color::Default => TerminalColor::Default,
        vt100::Color::Idx(index) => TerminalColor::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => TerminalColor::Rgb(red, green, blue),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::{Cursor, Read, Write},
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
    use tokio::sync::mpsc;

    use super::{
        INITIAL_COLS, INITIAL_ROWS, RuntimeEvent, RuntimeSession, SpawnedPty, TerminalColor,
        TerminalCommand, TerminalFailure, TerminalNotice, TerminalStatus, frame_from_screen,
        handle_command, handle_runtime_event, start_terminal_runtime,
    };
    use crate::config::InteractiveTerminalConfig;

    #[derive(Debug)]
    struct TestMaster {
        sizes: Arc<Mutex<Vec<PtySize>>>,
        fail_resize: bool,
    }

    impl MasterPty for TestMaster {
        fn resize(&self, size: PtySize) -> Result<(), anyhow::Error> {
            if self.fail_resize {
                anyhow::bail!("resize failed");
            }
            self.sizes
                .lock()
                .map_err(|_| anyhow::anyhow!("size recorder lock poisoned"))?
                .push(size);
            Ok(())
        }

        fn get_size(&self) -> Result<PtySize, anyhow::Error> {
            Ok(PtySize::default())
        }

        fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, anyhow::Error> {
            Ok(Box::new(Cursor::new(Vec::<u8>::new())))
        }

        fn take_writer(&self) -> Result<Box<dyn Write + Send>, anyhow::Error> {
            Ok(Box::new(std::io::sink()))
        }

        #[cfg(unix)]
        fn process_group_leader(&self) -> Option<i32> {
            None
        }

        #[cfg(unix)]
        fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
            None
        }

        #[cfg(unix)]
        fn tty_name(&self) -> Option<PathBuf> {
            None
        }
    }

    #[derive(Debug)]
    struct TestChild;

    impl ChildKiller for TestChild {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(Self)
        }
    }

    impl Child for TestChild {
        fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
            Ok(Some(ExitStatus::with_exit_code(0)))
        }

        fn wait(&mut self) -> std::io::Result<ExitStatus> {
            Ok(ExitStatus::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            Some(7)
        }

        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    #[derive(Debug)]
    struct RecordingKiller(Arc<AtomicUsize>);

    impl ChildKiller for RecordingKiller {
        fn kill(&mut self) -> std::io::Result<()> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(Self(Arc::clone(&self.0)))
        }
    }

    fn test_spawned(sizes: Arc<Mutex<Vec<PtySize>>>) -> SpawnedPty {
        SpawnedPty {
            master: Box::new(TestMaster {
                sizes,
                fail_resize: false,
            }),
            reader: Box::new(Cursor::new(Vec::<u8>::new())),
            writer: Box::new(std::io::sink()),
            child: Box::new(TestChild),
            killer: Box::new(TestChild),
            process_id: Some(7),
        }
    }

    #[test]
    fn frame_contains_rendered_text_and_styles_without_escape_sequences() {
        let mut parser = vt100::Parser::new(3, 12, 100);
        parser.process(b"plain \x1b[31;1mRED\x1b[0m");
        let frame = frame_from_screen(parser.screen());
        let first = &frame.content[0];
        let rendered = first
            .spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert!(rendered.starts_with("plain RED"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(first.spans.iter().any(|span| {
            span.text.contains("RED")
                && span.style.foreground == TerminalColor::Indexed(1)
                && span.style.bold
        }));
    }

    #[test]
    fn frame_replaces_bidi_controls_before_the_ui_receives_cells() {
        let mut parser = vt100::Parser::new(2, 20, 100);
        parser.process("safe\u{202e}spoof".as_bytes());
        let frame = frame_from_screen(parser.screen());
        let rendered = frame.content[0]
            .spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert!(!rendered.contains('\u{202e}'));
        assert!(rendered.contains('\u{fffd}'));
    }

    #[test]
    fn frame_exposes_bracketed_paste_and_application_cursor_modes() {
        let mut parser = vt100::Parser::new(2, 10, 100);
        parser.process(b"\x1b[?2004h\x1b[?1h\x1b[?1000h\x1b[?1006h");
        let frame = frame_from_screen(parser.screen());
        assert!(frame.bracketed_paste);
        assert!(frame.application_cursor);
        assert_eq!(frame.mouse_mode, super::TerminalMouseMode::PressRelease);
        assert_eq!(frame.mouse_encoding, super::TerminalMouseEncoding::Sgr);
    }

    #[test]
    fn failed_pty_resize_does_not_desynchronize_the_parser() {
        let config = InteractiveTerminalConfig::default();
        let mut sessions = BTreeMap::new();
        let mut session = RuntimeSession::new(1, PathBuf::from("."), 100);
        session.status = TerminalStatus::Running;
        session.master = Some(Box::new(TestMaster {
            sizes: Arc::new(Mutex::new(Vec::new())),
            fail_resize: true,
        }));
        sessions.insert(1, session);
        let mut next_id = 2;
        let (event_tx, _event_rx) = mpsc::channel(1);
        let mut notice = None;

        handle_command(
            TerminalCommand::Resize {
                id: 1,
                rows: 40,
                cols: 120,
            },
            &config,
            PathBuf::from(".").as_path(),
            &mut sessions,
            &mut next_id,
            &event_tx,
            &mut notice,
        );

        assert_eq!(
            sessions[&1].parser.screen().size(),
            (INITIAL_ROWS, INITIAL_COLS)
        );
        assert!(matches!(
            notice,
            Some(TerminalNotice::ResizeFailed { id: 1, .. })
        ));
    }

    #[tokio::test]
    async fn resize_requested_during_startup_is_applied_to_the_spawned_pty() {
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let mut session = RuntimeSession::new(1, PathBuf::from("."), 100);
        session.parser.screen_mut().set_size(40, 120);
        let mut sessions = BTreeMap::from([(1, session)]);
        let (event_tx, _event_rx) = mpsc::channel(8);
        let mut notice = None;

        handle_runtime_event(
            RuntimeEvent::Spawned {
                id: 1,
                result: Ok(test_spawned(Arc::clone(&sizes))),
            },
            &mut sessions,
            &event_tx,
            &mut notice,
        );

        assert_eq!(
            *sizes.lock().unwrap(),
            vec![PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            }]
        );
    }

    #[test]
    fn closing_a_failed_session_keeps_it_until_the_child_is_reaped() {
        let config = InteractiveTerminalConfig::default();
        let mut session = RuntimeSession::new(1, PathBuf::from("."), 100);
        session.status = TerminalStatus::Failed {
            failure: TerminalFailure::InputUnavailable,
        };
        session.awaiting_exit = true;
        let mut sessions = BTreeMap::from([(1, session)]);
        let mut next_id = 2;
        let (event_tx, _event_rx) = mpsc::channel(1);
        let mut notice = None;

        handle_command(
            TerminalCommand::Close { id: 1 },
            &config,
            PathBuf::from(".").as_path(),
            &mut sessions,
            &mut next_id,
            &event_tx,
            &mut notice,
        );

        assert!(sessions.contains_key(&1));
        assert!(sessions[&1].close_when_exited);
    }

    #[test]
    fn stop_retries_termination_for_a_failed_but_unreaped_child() {
        let config = InteractiveTerminalConfig::default();
        let kills = Arc::new(AtomicUsize::new(0));
        let mut session = RuntimeSession::new(1, PathBuf::from("."), 100);
        session.status = TerminalStatus::Failed {
            failure: TerminalFailure::InputUnavailable,
        };
        session.killer = Some(Box::new(RecordingKiller(Arc::clone(&kills))));
        let mut sessions = BTreeMap::from([(1, session)]);
        let mut next_id = 2;
        let (event_tx, _event_rx) = mpsc::channel(1);
        let mut notice = None;

        handle_command(
            TerminalCommand::Stop { id: 1 },
            &config,
            PathBuf::from(".").as_path(),
            &mut sessions,
            &mut next_id,
            &event_tx,
            &mut notice,
        );

        assert_eq!(kills.load(Ordering::Relaxed), 1);
        assert!(matches!(notice, Some(TerminalNotice::Stopping { id: 1 })));
    }

    #[tokio::test]
    async fn disabled_runtime_refuses_creation_without_spawning_a_process() {
        let workspace = tempfile::tempdir().unwrap();
        let config = InteractiveTerminalConfig {
            enabled: false,
            ..InteractiveTerminalConfig::default()
        };
        let (control, mut snapshots, task) =
            start_terminal_runtime(config, workspace.path().to_path_buf());
        control.create().unwrap();
        tokio::time::timeout(Duration::from_secs(2), snapshots.changed())
            .await
            .unwrap()
            .unwrap();
        let snapshot = snapshots.borrow_and_update().clone();
        assert!(snapshot.sessions.is_empty());
        assert!(matches!(
            snapshot.notice,
            Some(super::TerminalNotice::Disabled)
        ));
        control.shutdown().await;
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_stop_reaps_an_interactive_shell_without_waiting_for_input() {
        let workspace = tempfile::tempdir().unwrap();
        let (control, mut snapshots, task) = start_terminal_runtime(
            InteractiveTerminalConfig::default(),
            workspace.path().to_path_buf(),
        );
        control.create().unwrap();
        let id =
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    snapshots.changed().await.unwrap();
                    let snapshot = snapshots.borrow_and_update().clone();
                    if let Some(session) = snapshot
                        .sessions
                        .iter()
                        .find(|session| matches!(session.status, TerminalStatus::Running))
                    {
                        return session.id;
                    }
                    assert!(snapshot.sessions.iter().all(|session| {
                        !matches!(session.status, TerminalStatus::Failed { .. })
                    }));
                }
            })
            .await
            .unwrap();
        control.stop(id).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                snapshots.changed().await.unwrap();
                let snapshot = snapshots.borrow_and_update().clone();
                if snapshot.sessions.iter().any(|session| {
                    session.id == id
                        && matches!(
                            session.status,
                            TerminalStatus::Exited { .. } | TerminalStatus::Failed { .. }
                        )
                }) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        control.shutdown().await;
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_waits_for_active_shell_reaping_within_its_bound() {
        let workspace = tempfile::tempdir().unwrap();
        let (control, mut snapshots, task) = start_terminal_runtime(
            InteractiveTerminalConfig::default(),
            workspace.path().to_path_buf(),
        );
        control.create().unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                snapshots.changed().await.unwrap();
                if snapshots
                    .borrow_and_update()
                    .sessions
                    .iter()
                    .any(|session| matches!(session.status, TerminalStatus::Running))
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        control.shutdown().await;
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg_attr(
        windows,
        ignore = "ConPTY child I/O is unavailable in headless Windows test sessions"
    )]
    async fn runtime_captures_output_and_reaps_a_short_lived_pty() {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = InteractiveTerminalConfig::default();
        #[cfg(windows)]
        {
            config.program = Some("cmd.exe".to_owned());
            config.args = vec!["/D".to_owned()];
        }
        #[cfg(not(windows))]
        {
            config.program = Some("/bin/sh".to_owned());
            config.args = Vec::new();
        }
        let (control, mut snapshots, task) =
            start_terminal_runtime(config, workspace.path().to_path_buf());
        control.create().unwrap();
        let probe = tokio::time::timeout(Duration::from_secs(10), async {
            let mut exit_sent = false;
            loop {
                if snapshots.changed().await.is_err() {
                    return false;
                }
                let snapshot = snapshots.borrow_and_update().clone();
                if let Some(session) = snapshot.sessions.first() {
                    if matches!(session.status, TerminalStatus::Running) && !exit_sent {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        #[cfg(windows)]
                        let command = b"echo terminal-ready\r\nexit\r\n".to_vec();
                        #[cfg(not(windows))]
                        let command = b"printf terminal-ready\nexit\n".to_vec();
                        control.input(session.id, command).unwrap();
                        exit_sent = true;
                    }
                    let text = session
                        .frame
                        .content
                        .iter()
                        .flat_map(|row| row.spans.iter())
                        .map(|span| span.text.as_str())
                        .collect::<String>();
                    if text.contains("terminal-ready")
                        && matches!(
                            session.status,
                            TerminalStatus::Exited { .. } | TerminalStatus::Failed { .. }
                        )
                    {
                        return true;
                    }
                }
            }
        })
        .await;
        let last_snapshot = snapshots.borrow().clone();
        control.shutdown().await;
        let runtime_shutdown = tokio::time::timeout(Duration::from_secs(3), task).await;
        match probe {
            Ok(true) => {}
            Ok(false) => panic!(
                "ConPTY infrastructure closed the runtime before the smoke marker; last snapshot: {last_snapshot:?}"
            ),
            Err(_) => panic!(
                "ConPTY infrastructure did not deliver child I/O within 10s; last snapshot: {last_snapshot:?}"
            ),
        }
        runtime_shutdown
            .expect("terminal runtime did not shut down within 3s")
            .expect("terminal runtime task failed during ConPTY smoke test");
    }
}
