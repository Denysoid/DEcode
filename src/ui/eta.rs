use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::agent::{ActionId, AgentPhase, TurnId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PhaseKind {
    Planning,
    Requesting,
    Streaming,
    Parsing,
    Tools,
    Approval,
    Review,
}

const RECENT_SAMPLE_LIMIT: usize = 32;
const TOOL_HISTORY_LIMIT: usize = 32;
const PROFILE_LIMIT: usize = 64;
const ETA_STORE_MAX_BYTES: u64 = 2 * 1024 * 1024;
const ETA_STORE_VERSION: u32 = 1;
const MAX_PERSISTED_SAMPLE: f64 = 7.0 * 24.0 * 3_600.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Distribution {
    mean: f64,
    absolute_deviation: f64,
    samples: u32,
    recent: VecDeque<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Backtest {
    samples: u64,
    absolute_error_sum: f64,
    covered: u64,
    recent_absolute_errors: VecDeque<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct EtaProfile {
    phase_history: BTreeMap<PhaseKind, Distribution>,
    turn_history: Option<Distribution>,
    stream_output_history: Option<Distribution>,
    tool_history: BTreeMap<String, Distribution>,
    backtest: Backtest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct EtaStore {
    version: u32,
    profiles: BTreeMap<String, EtaProfile>,
}

impl Default for EtaStore {
    fn default() -> Self {
        Self {
            version: ETA_STORE_VERSION,
            profiles: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RangeSeconds {
    low: f64,
    likely: f64,
    high: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct EtaEstimate {
    pub low: Duration,
    pub high: Duration,
    pub confidence_percent: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EtaBacktest {
    pub samples: u64,
    pub mean_absolute_error: Duration,
    pub median_absolute_error: Duration,
    pub interval_coverage_percent: u8,
}

#[derive(Debug, Clone, Copy)]
struct ActivePrediction {
    low: f64,
    likely: f64,
    high: f64,
}

#[derive(Debug, Error)]
enum EtaPersistenceError {
    #[error("ETA history path has no parent: {0}")]
    InvalidPath(PathBuf),
    #[error("ETA history I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("ETA history JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ETA history version {0} is unsupported")]
    Version(u32),
    #[error("ETA history exceeds its {ETA_STORE_MAX_BYTES} byte limit")]
    TooLarge,
    #[error("ETA history contains invalid numeric data")]
    InvalidData,
}

#[derive(Debug, Clone)]
pub struct EtaTracker {
    session_started: Instant,
    active_turn_id: Option<TurnId>,
    active_segment_started: Option<Instant>,
    accumulated_turn: Duration,
    last_turn_elapsed: Option<Duration>,
    observed_phase: AgentPhase,
    observed_since: Instant,
    phase_history: BTreeMap<PhaseKind, Distribution>,
    turn_history: Option<Distribution>,
    stream_units_at_start: usize,
    stream_units: usize,
    stream_output_history: Option<Distribution>,
    tool_history: BTreeMap<String, Distribution>,
    active_tools: BTreeMap<ActionId, (String, Instant)>,
    backtest: Backtest,
    active_prediction: Option<ActivePrediction>,
    context_key: String,
    stored_profiles: BTreeMap<String, EtaProfile>,
    store_path: Option<PathBuf>,
}

impl EtaTracker {
    #[must_use]
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            session_started: now,
            active_turn_id: None,
            active_segment_started: None,
            accumulated_turn: Duration::ZERO,
            last_turn_elapsed: None,
            observed_phase: AgentPhase::Idle,
            observed_since: now,
            phase_history: BTreeMap::new(),
            turn_history: None,
            stream_units_at_start: 0,
            stream_units: 0,
            stream_output_history: None,
            tool_history: BTreeMap::new(),
            active_tools: BTreeMap::new(),
            backtest: Backtest::default(),
            active_prediction: None,
            context_key: "default".to_owned(),
            stored_profiles: BTreeMap::new(),
            store_path: None,
        }
    }

    #[must_use]
    pub fn load(path: PathBuf) -> Self {
        let mut tracker = Self::new();
        tracker.store_path = Some(path.clone());
        match read_store(&path) {
            Ok(store) => tracker.stored_profiles = store.profiles,
            Err(error) => tracing::warn!(path = %path.display(), %error, "ETA history ignored"),
        }
        tracker
    }

    pub fn set_context(&mut self, provider: &str, model: &str, effort: &str, modes: &str) {
        let next = format!("{provider}\u{1f}{model}\u{1f}{effort}\u{1f}{modes}");
        if self.context_key == next {
            return;
        }
        self.store_current_profile();
        self.context_key = next;
        if let Some(profile) = self.stored_profiles.get(&self.context_key).cloned() {
            self.apply_profile(profile);
        } else {
            self.apply_profile(EtaProfile::default());
        }
    }

    pub fn tool_started(&mut self, kind: &str, now: Instant) {
        self.tool_action_started(0, kind, now);
    }

    pub fn tool_completed(&mut self, kind: &str, now: Instant) {
        let kind = bounded_key(kind);
        if self
            .active_tools
            .get(&0)
            .is_some_and(|(active, _)| active == &kind)
        {
            self.tool_action_completed(0, now);
        }
    }

    pub(crate) fn tool_action_started(&mut self, action_id: ActionId, kind: &str, now: Instant) {
        if let Some((previous, started)) = self
            .active_tools
            .insert(action_id, (bounded_key(kind), now))
        {
            self.record_tool_duration(previous, now.saturating_duration_since(started));
        }
    }

    pub(crate) fn tool_action_completed(&mut self, action_id: ActionId, now: Instant) {
        self.finish_active_tool(action_id, now);
    }

    #[must_use]
    pub fn backtest(&self) -> Option<EtaBacktest> {
        if self.backtest.samples == 0 {
            return None;
        }
        Some(EtaBacktest {
            samples: self.backtest.samples,
            mean_absolute_error: Duration::from_secs_f64(
                (self.backtest.absolute_error_sum / self.backtest.samples as f64).max(0.0),
            ),
            median_absolute_error: Duration::from_secs_f64(median(
                &self.backtest.recent_absolute_errors,
            )),
            interval_coverage_percent: ((self.backtest.covered.saturating_mul(100)
                / self.backtest.samples)
                .min(100)) as u8,
        })
    }

    pub fn observe(&mut self, next: &AgentPhase, turn_id: Option<TurnId>, now: Instant) {
        let turn_changed =
            next.is_busy() && self.active_turn_id.is_some() && turn_id != self.active_turn_id;
        if &self.observed_phase == next && !turn_changed {
            self.observe_turn_segment(next, turn_id, now);
            return;
        }
        let previous = self.observed_phase.clone();
        let elapsed = now.saturating_duration_since(self.observed_since);
        if let Some(kind) = phase_kind(&previous) {
            update_distribution(&mut self.phase_history, kind, elapsed);
            if kind == PhaseKind::Streaming {
                let produced = self.stream_units.saturating_sub(self.stream_units_at_start);
                if produced > 0 {
                    update_optional_distribution(&mut self.stream_output_history, produced as f64);
                }
            }
        }
        if turn_changed && let Some(previous_turn) = self.active_turn_id {
            self.complete(previous_turn, now);
        }
        if matches!(next, AgentPhase::Streaming) {
            self.stream_units_at_start = self.stream_units;
        }
        self.observe_turn_segment(next, turn_id, now);
        self.observed_phase = next.clone();
        self.observed_since = now;
    }

    /// Supplies monotonic display progress for the active streamed response.
    /// Bytes are a local rate signal only; authoritative token accounting still
    /// comes exclusively from the provider's completed usage object.
    pub fn observe_stream_progress(&mut self, units: usize) {
        if units < self.stream_units {
            self.stream_units_at_start = units;
        }
        self.stream_units = units;
    }

    fn observe_turn_segment(&mut self, next: &AgentPhase, turn_id: Option<TurnId>, now: Instant) {
        if next.is_busy() {
            let Some(turn_id) = turn_id else {
                return;
            };
            if self.active_turn_id != Some(turn_id) {
                let prediction = self.initial_turn_range(next);
                self.active_turn_id = Some(turn_id);
                self.accumulated_turn = Duration::ZERO;
                self.last_turn_elapsed = None;
                self.active_segment_started = Some(now);
                self.active_prediction = Some(ActivePrediction {
                    low: prediction.low,
                    likely: prediction.likely,
                    high: prediction.high,
                });
            } else if self.active_segment_started.is_none() {
                // Retry/Continue resumes the same logical turn without
                // charging time spent waiting on the user or an error screen.
                self.active_segment_started = Some(now);
            }
        } else {
            self.suspend(turn_id.or(self.active_turn_id), now);
        }
    }

    pub fn suspend(&mut self, turn_id: Option<TurnId>, now: Instant) {
        if turn_id.is_some() && turn_id != self.active_turn_id {
            return;
        }
        if let Some(started) = self.active_segment_started.take() {
            self.accumulated_turn = self
                .accumulated_turn
                .saturating_add(now.saturating_duration_since(started));
        }
    }

    pub fn complete(&mut self, turn_id: TurnId, now: Instant) {
        if self.active_turn_id != Some(turn_id) {
            return;
        }
        self.suspend(Some(turn_id), now);
        let elapsed = self.accumulated_turn;
        update_optional_distribution(&mut self.turn_history, elapsed.as_secs_f64());
        if let Some(prediction) = self.active_prediction.take() {
            let actual = elapsed.as_secs_f64();
            let absolute_error = (prediction.likely - actual).abs();
            self.backtest.samples = self.backtest.samples.saturating_add(1);
            self.backtest.absolute_error_sum += absolute_error;
            push_recent(&mut self.backtest.recent_absolute_errors, absolute_error);
            if actual >= prediction.low && actual <= prediction.high {
                self.backtest.covered = self.backtest.covered.saturating_add(1);
            }
        }
        self.last_turn_elapsed = Some(elapsed);
        self.active_turn_id = None;
        self.accumulated_turn = Duration::ZERO;
        self.finish_all_active_tools(now);
        self.persist();
    }

    pub fn cancel(&mut self, turn_id: TurnId) {
        if self.active_turn_id == Some(turn_id) {
            self.active_turn_id = None;
            self.active_segment_started = None;
            self.accumulated_turn = Duration::ZERO;
            self.last_turn_elapsed = None;
            self.active_prediction = None;
            self.active_tools.clear();
        }
    }

    pub fn reset_turn(&mut self) {
        self.active_turn_id = None;
        self.active_segment_started = None;
        self.accumulated_turn = Duration::ZERO;
        self.last_turn_elapsed = None;
        self.stream_units_at_start = self.stream_units;
        self.active_prediction = None;
        self.active_tools.clear();
    }

    #[must_use]
    pub fn session_elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.session_started)
    }

    #[must_use]
    pub fn turn_elapsed(&self, now: Instant) -> Option<Duration> {
        if self.active_turn_id.is_some() {
            Some(
                self.accumulated_turn.saturating_add(
                    self.active_segment_started
                        .map_or(Duration::ZERO, |started| {
                            now.saturating_duration_since(started)
                        }),
                ),
            )
        } else {
            self.last_turn_elapsed
        }
    }

    #[must_use]
    pub fn estimate(
        &self,
        phase: &AgentPhase,
        now: Instant,
        running_tools: usize,
        remaining_plan_steps: Option<usize>,
    ) -> Option<EtaEstimate> {
        if !phase.is_busy() {
            return None;
        }
        let phase_elapsed = now
            .saturating_duration_since(self.observed_since)
            .as_secs_f64();
        let current_kind = phase_kind(phase)?;
        let mut remaining = self.phase_remaining(current_kind, phase_elapsed);
        if current_kind == PhaseKind::Streaming {
            remaining = self.progress_adjusted_stream_range(remaining, phase_elapsed);
        } else if current_kind == PhaseKind::Tools
            && let Some((history, started)) = self
                .active_tools
                .values()
                .filter_map(|(kind, started)| {
                    self.tool_history
                        .get(kind)
                        .map(|history| (history, started))
                })
                .max_by_key(|(_, started)| now.saturating_duration_since(**started))
        {
            let tool_elapsed = now.saturating_duration_since(*started).as_secs_f64();
            remaining =
                remaining.blend(conditional_range(history, tool_elapsed, history.mean), 0.35);
        }
        remaining += match current_kind {
            PhaseKind::Planning => {
                self.full_phase_range(PhaseKind::Requesting)
                    + self.full_phase_range(PhaseKind::Streaming)
                    + self.full_phase_range(PhaseKind::Parsing)
            }
            PhaseKind::Requesting => {
                self.full_phase_range(PhaseKind::Streaming)
                    + self.full_phase_range(PhaseKind::Parsing)
            }
            PhaseKind::Streaming => {
                self.full_phase_range(PhaseKind::Parsing)
                    + if running_tools > 0 {
                        self.full_phase_range(PhaseKind::Tools)
                    } else {
                        RangeSeconds::zero()
                    }
            }
            PhaseKind::Parsing => {
                if running_tools > 0 {
                    self.full_phase_range(PhaseKind::Tools)
                } else {
                    RangeSeconds::point(0.6)
                }
            }
            PhaseKind::Tools => {
                self.full_phase_range(PhaseKind::Requesting)
                    + self.full_phase_range(PhaseKind::Streaming)
            }
            PhaseKind::Approval | PhaseKind::Review => RangeSeconds::zero(),
        };
        if let (Some(turn_history), Some(turn_elapsed)) =
            (&self.turn_history, self.turn_elapsed(now))
        {
            let turn_remaining =
                conditional_range(turn_history, turn_elapsed.as_secs_f64(), turn_history.mean);
            remaining = remaining.blend(turn_remaining, 0.72);
        }
        if let Some(future_steps) = remaining_plan_steps.map(|steps| steps.saturating_sub(1))
            && future_steps > 0
        {
            let seconds_per_step = self
                .turn_history
                .as_ref()
                .map_or(
                    default_seconds(PhaseKind::Requesting) + default_seconds(PhaseKind::Streaming),
                    |history| history.mean,
                )
                .clamp(1.0, 900.0);
            let step_count = future_steps.min(64) as f64;
            remaining += RangeSeconds {
                low: seconds_per_step * 0.55 * step_count,
                likely: seconds_per_step * step_count,
                high: seconds_per_step * 1.8 * step_count,
            };
        }
        let phase_samples = self
            .phase_history
            .get(&current_kind)
            .map_or(0, |average| average.samples);
        let turn_samples = self.turn_history.as_ref().map_or(0, |value| value.samples);
        let samples = phase_samples.saturating_add(turn_samples.min(phase_samples));
        let stream_bonus = u32::from(
            current_kind == PhaseKind::Streaming
                && self
                    .stream_output_history
                    .as_ref()
                    .is_some_and(|history| history.samples >= 3),
        ) * 8;
        let confidence = (20_u32
            .saturating_add(samples.saturating_mul(6))
            .saturating_add(stream_bonus))
        .min(92) as u8;
        Some(EtaEstimate {
            low: Duration::from_secs_f64(remaining.low.max(0.0)),
            high: Duration::from_secs_f64(remaining.high.max(remaining.low + 0.1)),
            confidence_percent: confidence,
        })
    }

    fn initial_turn_range(&self, phase: &AgentPhase) -> RangeSeconds {
        let Some(kind) = phase_kind(phase) else {
            return RangeSeconds::prior(default_seconds(PhaseKind::Requesting))
                + RangeSeconds::prior(default_seconds(PhaseKind::Streaming))
                + RangeSeconds::prior(default_seconds(PhaseKind::Parsing));
        };
        let mut range = self.full_phase_range(kind);
        if matches!(kind, PhaseKind::Planning) {
            range += self.full_phase_range(PhaseKind::Requesting);
        }
        if matches!(kind, PhaseKind::Planning | PhaseKind::Requesting) {
            range += self.full_phase_range(PhaseKind::Streaming);
        }
        if matches!(
            kind,
            PhaseKind::Planning | PhaseKind::Requesting | PhaseKind::Streaming
        ) {
            range += self.full_phase_range(PhaseKind::Parsing);
        }
        range
    }

    fn finish_active_tool(&mut self, action_id: ActionId, now: Instant) {
        let Some((kind, started)) = self.active_tools.remove(&action_id) else {
            return;
        };
        let elapsed = now.saturating_duration_since(started);
        self.record_tool_duration(kind, elapsed);
    }

    fn finish_all_active_tools(&mut self, now: Instant) {
        let active = std::mem::take(&mut self.active_tools);
        for (_, (kind, started)) in active {
            self.record_tool_duration(kind, now.saturating_duration_since(started));
        }
    }

    fn record_tool_duration(&mut self, kind: String, elapsed: Duration) {
        if self.tool_history.len() >= TOOL_HISTORY_LIMIT
            && !self.tool_history.contains_key(&kind)
            && let Some(first) = self.tool_history.keys().next().cloned()
        {
            self.tool_history.remove(&first);
        }
        update_distribution_by_key(&mut self.tool_history, kind, elapsed);
    }

    fn store_current_profile(&mut self) {
        if self.stored_profiles.len() >= PROFILE_LIMIT
            && !self.stored_profiles.contains_key(&self.context_key)
            && let Some(oldest_key) = self.stored_profiles.keys().next().cloned()
        {
            self.stored_profiles.remove(&oldest_key);
        }
        self.stored_profiles.insert(
            self.context_key.clone(),
            EtaProfile {
                phase_history: self.phase_history.clone(),
                turn_history: self.turn_history.clone(),
                stream_output_history: self.stream_output_history.clone(),
                tool_history: self.tool_history.clone(),
                backtest: self.backtest.clone(),
            },
        );
    }

    fn apply_profile(&mut self, profile: EtaProfile) {
        self.phase_history = profile.phase_history;
        self.turn_history = profile.turn_history;
        self.stream_output_history = profile.stream_output_history;
        self.tool_history = profile.tool_history;
        self.backtest = profile.backtest;
    }

    fn persist(&mut self) {
        let Some(path) = self.store_path.clone() else {
            return;
        };
        self.store_current_profile();
        let store = EtaStore {
            version: ETA_STORE_VERSION,
            profiles: self.stored_profiles.clone(),
        };
        if let Err(error) = write_store(&path, &store) {
            tracing::warn!(path = %path.display(), %error, "ETA history was not persisted");
        }
    }

    fn full_phase_range(&self, kind: PhaseKind) -> RangeSeconds {
        let prior = default_seconds(kind);
        self.phase_history.get(&kind).map_or_else(
            || RangeSeconds::prior(prior),
            |history| unconditional_range(history, prior),
        )
    }

    fn phase_remaining(&self, kind: PhaseKind, elapsed: f64) -> RangeSeconds {
        let prior = default_seconds(kind);
        self.phase_history.get(&kind).map_or_else(
            || prior_remaining(prior, elapsed),
            |history| conditional_range(history, elapsed, prior),
        )
    }

    fn progress_adjusted_stream_range(&self, timing: RangeSeconds, elapsed: f64) -> RangeSeconds {
        let produced = self.stream_units.saturating_sub(self.stream_units_at_start) as f64;
        let Some(history) = &self.stream_output_history else {
            return timing;
        };
        if history.samples < 3 || produced < 64.0 || elapsed < 0.25 {
            return timing;
        }
        let rate = produced / elapsed;
        if !rate.is_finite() || rate <= f64::EPSILON {
            return timing;
        }
        let output_range = unconditional_range(history, history.mean.max(produced));
        let tail_floor = (timing.likely * 0.12).clamp(0.25, 4.0);
        let from_output = RangeSeconds {
            low: ((output_range.low - produced).max(0.0) / rate).max(0.1),
            likely: ((output_range.likely - produced).max(0.0) / rate).max(tail_floor),
            high: ((output_range.high - produced).max(0.0) / rate).max(tail_floor * 1.8),
        }
        .ordered();
        timing.blend(from_output, 0.58)
    }
}

fn push_recent(values: &mut VecDeque<f64>, value: f64) {
    if !value.is_finite() || value < 0.0 {
        return;
    }
    if values.len() >= RECENT_SAMPLE_LIMIT {
        values.pop_front();
    }
    values.push_back(value);
}

fn median(values: &VecDeque<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.iter().copied().collect::<Vec<_>>();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

impl Default for EtaTracker {
    fn default() -> Self {
        Self::new()
    }
}

fn phase_kind(phase: &AgentPhase) -> Option<PhaseKind> {
    match phase {
        AgentPhase::Idle | AgentPhase::Error { .. } => None,
        AgentPhase::Planning => Some(PhaseKind::Planning),
        AgentPhase::Requesting => Some(PhaseKind::Requesting),
        AgentPhase::Streaming => Some(PhaseKind::Streaming),
        AgentPhase::Parsing => Some(PhaseKind::Parsing),
        AgentPhase::ExecutingTools => Some(PhaseKind::Tools),
        AgentPhase::AwaitingConfirmation
        | AgentPhase::AwaitingPatchApproval
        | AgentPhase::AwaitingPlanApproval
        | AgentPhase::AwaitingContinuation => Some(PhaseKind::Approval),
        AgentPhase::PreparingReview => Some(PhaseKind::Review),
    }
}

const fn default_seconds(kind: PhaseKind) -> f64 {
    match kind {
        PhaseKind::Planning => 18.0,
        PhaseKind::Requesting => 3.5,
        PhaseKind::Streaming => 28.0,
        PhaseKind::Parsing => 1.2,
        PhaseKind::Tools => 14.0,
        PhaseKind::Approval => 1.0,
        PhaseKind::Review => 4.0,
    }
}

impl RangeSeconds {
    const fn zero() -> Self {
        Self::point(0.0)
    }

    const fn point(value: f64) -> Self {
        Self {
            low: value,
            likely: value,
            high: value,
        }
    }

    fn prior(value: f64) -> Self {
        Self {
            low: value * 0.5,
            likely: value,
            high: value * 1.75,
        }
    }

    fn ordered(self) -> Self {
        let low = self.low.max(0.0);
        let likely = self.likely.max(low);
        let high = self.high.max(likely);
        Self { low, likely, high }
    }

    fn blend(self, other: Self, self_weight: f64) -> Self {
        let other_weight = 1.0 - self_weight;
        Self {
            low: self.low.mul_add(self_weight, other.low * other_weight),
            likely: self
                .likely
                .mul_add(self_weight, other.likely * other_weight),
            high: self.high.mul_add(self_weight, other.high * other_weight),
        }
        .ordered()
    }
}

impl std::ops::Add for RangeSeconds {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self {
            low: self.low + rhs.low,
            likely: self.likely + rhs.likely,
            high: self.high + rhs.high,
        }
    }
}

impl std::ops::AddAssign for RangeSeconds {
    fn add_assign(&mut self, rhs: Self) {
        self.low += rhs.low;
        self.likely += rhs.likely;
        self.high += rhs.high;
    }
}

fn unconditional_range(history: &Distribution, prior: f64) -> RangeSeconds {
    let observed_weight = f64::from(history.samples.min(12));
    let likely = (prior.mul_add(4.0, history.mean * observed_weight)) / (4.0 + observed_weight);
    if history.recent.len() < 3 {
        let spread = prior.max(history.absolute_deviation * 2.0).max(0.5);
        return RangeSeconds {
            low: (likely - spread * 0.5).max(0.0),
            likely,
            high: likely + spread,
        }
        .ordered();
    }
    let mut samples = history.recent.iter().copied().collect::<Vec<_>>();
    samples.sort_by(f64::total_cmp);
    RangeSeconds {
        low: quantile(&samples, 0.2).min(likely),
        likely,
        high: quantile(&samples, 0.85).max(likely),
    }
    .ordered()
}

fn conditional_range(history: &Distribution, elapsed: f64, prior: f64) -> RangeSeconds {
    let mut remaining = history
        .recent
        .iter()
        .copied()
        .filter(|sample| *sample > elapsed)
        .map(|sample| sample - elapsed)
        .collect::<Vec<_>>();
    if remaining.len() >= 3 {
        remaining.sort_by(f64::total_cmp);
        return RangeSeconds {
            low: quantile(&remaining, 0.15).max(0.1),
            likely: quantile(&remaining, 0.5).max(0.2),
            high: quantile(&remaining, 0.9).max(0.4),
        }
        .ordered();
    }
    let expected = unconditional_range(history, prior);
    let likely = (expected.likely - elapsed).max(overdue_tail(expected.likely, elapsed));
    let deviation = history.absolute_deviation.max(expected.likely * 0.15);
    RangeSeconds {
        low: (expected.low - elapsed).max((likely - deviation).max(0.1)),
        likely,
        high: (expected.high - elapsed).max(likely + deviation * 1.5),
    }
    .ordered()
}

fn prior_remaining(expected: f64, elapsed: f64) -> RangeSeconds {
    let likely = (expected - elapsed).max(overdue_tail(expected, elapsed));
    RangeSeconds {
        low: (expected * 0.5 - elapsed).max(likely * 0.45).max(0.1),
        likely,
        high: (expected * 1.75 - elapsed).max(likely * 1.8),
    }
    .ordered()
}

fn overdue_tail(expected: f64, elapsed: f64) -> f64 {
    let overdue = (elapsed - expected).max(0.0);
    let initial = (expected * 0.22).clamp(0.6, 30.0);
    (initial / (1.0 + overdue / expected.max(1.0))).max(0.25)
}

fn quantile(sorted: &[f64], fraction: f64) -> f64 {
    let last = sorted.len().saturating_sub(1);
    let index = ((last as f64) * fraction).round() as usize;
    sorted[index.min(last)]
}

fn update_distribution(
    histories: &mut BTreeMap<PhaseKind, Distribution>,
    kind: PhaseKind,
    duration: Duration,
) {
    let value = duration.as_secs_f64().clamp(0.01, 3_600.0);
    let history = histories.entry(kind).or_insert_with(|| Distribution {
        mean: value,
        absolute_deviation: 0.0,
        samples: 0,
        recent: VecDeque::with_capacity(RECENT_SAMPLE_LIMIT),
    });
    update_history(history, value);
}

fn update_distribution_by_key(
    histories: &mut BTreeMap<String, Distribution>,
    kind: String,
    duration: Duration,
) {
    let value = duration.as_secs_f64().clamp(0.01, 3_600.0);
    let history = histories.entry(kind).or_insert_with(|| Distribution {
        mean: value,
        absolute_deviation: 0.0,
        samples: 0,
        recent: VecDeque::with_capacity(RECENT_SAMPLE_LIMIT),
    });
    update_history(history, value);
}

fn update_optional_distribution(history: &mut Option<Distribution>, value: f64) {
    let value = value.clamp(0.01, 24.0 * 3_600.0);
    let history = history.get_or_insert_with(|| Distribution {
        mean: value,
        absolute_deviation: 0.0,
        samples: 0,
        recent: VecDeque::with_capacity(RECENT_SAMPLE_LIMIT),
    });
    update_history(history, value);
}

fn update_history(history: &mut Distribution, value: f64) {
    let previous_mean = history.mean;
    history.mean = if history.samples == 0 {
        value
    } else {
        history.mean.mul_add(0.72, value * 0.28)
    };
    let deviation = (value - previous_mean).abs();
    history.absolute_deviation = if history.samples == 0 {
        deviation
    } else {
        history.absolute_deviation.mul_add(0.78, deviation * 0.22)
    };
    history.samples = history.samples.saturating_add(1);
    if history.recent.len() == RECENT_SAMPLE_LIMIT {
        let _ = history.recent.pop_front();
    }
    history.recent.push_back(value);
}

fn bounded_key(value: &str) -> String {
    value.chars().take(96).collect()
}

fn read_store(path: &Path) -> Result<EtaStore, EtaPersistenceError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(EtaStore::default());
        }
        Err(source) => {
            return Err(EtaPersistenceError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if metadata.len() > ETA_STORE_MAX_BYTES {
        return Err(EtaPersistenceError::TooLarge);
    }
    let bytes = fs::read(path).map_err(|source| EtaPersistenceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut store: EtaStore = serde_json::from_slice(&bytes)?;
    if store.version != ETA_STORE_VERSION {
        return Err(EtaPersistenceError::Version(store.version));
    }
    if store.profiles.len() > PROFILE_LIMIT
        || store.profiles.iter().any(|(key, profile)| {
            key.len() > 512
                || !valid_profile(profile)
                || profile.tool_history.len() > TOOL_HISTORY_LIMIT
        })
    {
        return Err(EtaPersistenceError::InvalidData);
    }
    for profile in store.profiles.values_mut() {
        trim_profile(profile);
    }
    Ok(store)
}

fn valid_profile(profile: &EtaProfile) -> bool {
    profile
        .phase_history
        .values()
        .chain(profile.turn_history.iter())
        .chain(profile.stream_output_history.iter())
        .chain(profile.tool_history.values())
        .all(valid_distribution)
        && profile.backtest.absolute_error_sum.is_finite()
        && profile.backtest.absolute_error_sum >= 0.0
        && profile.backtest.samples <= 1_000_000
        && profile.backtest.covered <= profile.backtest.samples
        && profile.backtest.recent_absolute_errors.len() <= RECENT_SAMPLE_LIMIT
        && usize::try_from(profile.backtest.samples)
            .is_ok_and(|samples| samples >= profile.backtest.recent_absolute_errors.len())
        && (profile.backtest.samples > 0 || profile.backtest.absolute_error_sum == 0.0)
        && (profile.backtest.samples == 0
            || profile.backtest.absolute_error_sum / profile.backtest.samples as f64
                <= MAX_PERSISTED_SAMPLE)
        && profile
            .backtest
            .recent_absolute_errors
            .iter()
            .all(|value| value.is_finite() && (0.0..=MAX_PERSISTED_SAMPLE).contains(value))
}

fn valid_distribution(distribution: &Distribution) -> bool {
    distribution.mean.is_finite()
        && (0.0..=MAX_PERSISTED_SAMPLE).contains(&distribution.mean)
        && distribution.absolute_deviation.is_finite()
        && (0.0..=MAX_PERSISTED_SAMPLE).contains(&distribution.absolute_deviation)
        && distribution.samples <= 1_000_000
        && distribution.recent.len() <= RECENT_SAMPLE_LIMIT
        && usize::try_from(distribution.samples)
            .is_ok_and(|samples| samples >= distribution.recent.len())
        && distribution
            .recent
            .iter()
            .all(|value| value.is_finite() && (0.0..=MAX_PERSISTED_SAMPLE).contains(value))
}

fn trim_profile(profile: &mut EtaProfile) {
    profile
        .phase_history
        .values_mut()
        .for_each(trim_distribution);
    if let Some(history) = &mut profile.turn_history {
        trim_distribution(history);
    }
    if let Some(history) = &mut profile.stream_output_history {
        trim_distribution(history);
    }
    profile
        .tool_history
        .values_mut()
        .for_each(trim_distribution);
    while profile.backtest.recent_absolute_errors.len() > RECENT_SAMPLE_LIMIT {
        let _ = profile.backtest.recent_absolute_errors.pop_front();
    }
}

fn trim_distribution(distribution: &mut Distribution) {
    while distribution.recent.len() > RECENT_SAMPLE_LIMIT {
        let _ = distribution.recent.pop_front();
    }
}

fn write_store(path: &Path, store: &EtaStore) -> Result<(), EtaPersistenceError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| EtaPersistenceError::InvalidPath(path.to_path_buf()))?;
    fs::create_dir_all(parent).map_err(|source| EtaPersistenceError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|source| EtaPersistenceError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    serde_json::to_writer(&mut temporary, store)?;
    temporary
        .flush()
        .map_err(|source| EtaPersistenceError::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| EtaPersistenceError::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| EtaPersistenceError::Io {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_counts_down_without_runaway_growth_and_learns_completed_turns() {
        let mut tracker = EtaTracker::new();
        let started = Instant::now();
        tracker.session_started = started;
        tracker.observed_since = started;
        tracker.observe(
            &AgentPhase::Requesting,
            Some(7),
            started + Duration::from_secs(1),
        );

        let early = tracker
            .estimate(
                &AgentPhase::Requesting,
                started + Duration::from_secs(2),
                0,
                None,
            )
            .map(|estimate| estimate.high)
            .unwrap_or_default();
        let late = tracker
            .estimate(
                &AgentPhase::Requesting,
                started + Duration::from_secs(25),
                0,
                None,
            )
            .map(|estimate| estimate.high)
            .unwrap_or_default();
        assert!(late > Duration::ZERO);
        assert!(late <= early.saturating_add(Duration::from_secs(10)));

        tracker.observe(
            &AgentPhase::Streaming,
            Some(7),
            started + Duration::from_secs(26),
        );
        tracker.observe(
            &AgentPhase::Idle,
            Some(7),
            started + Duration::from_secs(40),
        );
        tracker.complete(7, started + Duration::from_secs(40));
        assert!(tracker.turn_history.is_some());
    }

    #[test]
    fn pause_error_and_retry_preserve_active_elapsed_but_abort_resets_it() {
        let mut tracker = EtaTracker::new();
        let started = Instant::now();
        tracker.observe(&AgentPhase::Streaming, Some(9), started);
        tracker.observe(
            &AgentPhase::Idle,
            Some(9),
            started + Duration::from_secs(12),
        );
        assert_eq!(
            tracker.turn_elapsed(started + Duration::from_secs(42)),
            Some(Duration::from_secs(12))
        );

        tracker.observe(
            &AgentPhase::Requesting,
            Some(9),
            started + Duration::from_secs(42),
        );
        assert_eq!(
            tracker.turn_elapsed(started + Duration::from_secs(47)),
            Some(Duration::from_secs(17))
        );

        tracker.observe(
            &AgentPhase::Error {
                message: "temporary".to_owned(),
                recoverable: true,
            },
            Some(9),
            started + Duration::from_secs(47),
        );
        assert_eq!(
            tracker.turn_elapsed(started + Duration::from_secs(80)),
            Some(Duration::from_secs(17))
        );
        tracker.cancel(9);
        assert_eq!(
            tracker.turn_elapsed(started + Duration::from_secs(80)),
            None
        );
    }

    #[test]
    fn live_stream_rate_refines_eta_after_enough_completed_samples() {
        let mut tracker = EtaTracker::new();
        let started = Instant::now();
        for sample in 0_u64..3 {
            let base = started + Duration::from_secs(sample * 20);
            tracker.observe_stream_progress(0);
            tracker.observe(&AgentPhase::Streaming, Some(sample + 1), base);
            tracker.observe_stream_progress(1_000);
            tracker.observe(
                &AgentPhase::Parsing,
                Some(sample + 1),
                base + Duration::from_secs(10),
            );
            tracker.observe(
                &AgentPhase::Idle,
                Some(sample + 1),
                base + Duration::from_secs(11),
            );
            tracker.complete(sample + 1, base + Duration::from_secs(11));
        }

        let base = started + Duration::from_secs(80);
        tracker.observe_stream_progress(0);
        tracker.observe(&AgentPhase::Streaming, Some(9), base);
        let without_progress = tracker
            .estimate(
                &AgentPhase::Streaming,
                base + Duration::from_secs(8),
                0,
                None,
            )
            .map(|estimate| estimate.high)
            .unwrap_or_default();
        tracker.observe_stream_progress(900);
        let with_progress = tracker
            .estimate(
                &AgentPhase::Streaming,
                base + Duration::from_secs(8),
                0,
                None,
            )
            .map(|estimate| estimate.high)
            .unwrap_or_default();

        assert!(with_progress < without_progress);
    }

    #[test]
    fn parallel_tool_timings_are_measured_independently() {
        let mut tracker = EtaTracker::new();
        let started = Instant::now();

        tracker.tool_action_started(1, "read_file", started);
        tracker.tool_action_started(2, "search", started + Duration::from_secs(1));
        tracker.tool_action_completed(2, started + Duration::from_secs(3));
        tracker.tool_action_completed(1, started + Duration::from_secs(5));

        assert_eq!(
            tracker.tool_history["read_file"].recent.back().copied(),
            Some(5.0)
        );
        assert_eq!(
            tracker.tool_history["search"].recent.back().copied(),
            Some(2.0)
        );
    }

    #[test]
    fn back_to_back_turns_in_the_same_phase_close_the_previous_sample() {
        let mut tracker = EtaTracker::new();
        let started = Instant::now();
        tracker.observed_since = started;
        tracker.observe(&AgentPhase::Requesting, Some(1), started);

        tracker.observe(
            &AgentPhase::Requesting,
            Some(2),
            started + Duration::from_secs(10),
        );

        assert_eq!(
            tracker
                .turn_history
                .as_ref()
                .and_then(|history| history.recent.back())
                .copied(),
            Some(10.0)
        );
        assert_eq!(tracker.active_turn_id, Some(2));
    }

    #[test]
    fn completed_turns_persist_per_runtime_context_with_backtest_metrics()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("eta-history.json");
        let started = Instant::now();
        let mut tracker = EtaTracker::load(path.clone());
        tracker.observed_since = started;
        tracker.set_context("azure", "gpt-5.6-sol", "xhigh", "Plan + Deep");
        tracker.observe(&AgentPhase::Requesting, Some(21), started);
        tracker.observe(
            &AgentPhase::Streaming,
            Some(21),
            started + Duration::from_secs(2),
        );
        tracker.tool_started("read_file", started + Duration::from_secs(7));
        tracker.tool_completed("read_file", started + Duration::from_secs(9));
        tracker.observe(
            &AgentPhase::Idle,
            Some(21),
            started + Duration::from_secs(10),
        );
        tracker.complete(21, started + Duration::from_secs(10));

        let first = tracker
            .backtest()
            .ok_or_else(|| std::io::Error::other("missing backtest after completed turn"))?;
        assert_eq!(first.samples, 1);
        assert_eq!(first.mean_absolute_error, first.median_absolute_error);
        assert!(path.is_file());

        let mut restored = EtaTracker::load(path);
        restored.set_context("azure", "gpt-5.6-sol", "xhigh", "Plan + Deep");
        assert_eq!(restored.backtest(), Some(first));
        assert!(restored.tool_history.contains_key("read_file"));

        restored.set_context("openai", "gpt-5.6-sol", "xhigh", "Plan + Deep");
        assert_eq!(restored.backtest(), None);
        assert!(restored.tool_history.is_empty());
        Ok(())
    }

    #[test]
    fn remaining_goal_steps_extend_the_task_eta_without_changing_elapsed_time() {
        let mut tracker = EtaTracker::new();
        let started = Instant::now();
        tracker.observe(&AgentPhase::Requesting, Some(31), started);

        let current_turn = tracker
            .estimate(
                &AgentPhase::Requesting,
                started + Duration::from_secs(1),
                0,
                Some(1),
            )
            .map(|estimate| estimate.high)
            .unwrap_or_default();
        let multi_step = tracker
            .estimate(
                &AgentPhase::Requesting,
                started + Duration::from_secs(1),
                0,
                Some(4),
            )
            .map(|estimate| estimate.high)
            .unwrap_or_default();

        assert!(multi_step > current_turn);
        assert_eq!(
            tracker.turn_elapsed(started + Duration::from_secs(1)),
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn corrupt_or_non_finite_persisted_eta_history_is_ignored()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("eta-history.json");
        fs::write(&path, br#"{"version":1,"profiles":{"bad":{"phase_history":{},"turn_history":{"mean":-1.0,"absolute_deviation":0.0,"samples":1,"recent":[-1.0]},"stream_output_history":null,"tool_history":{},"backtest":{"samples":0,"absolute_error_sum":0.0,"covered":0}}}}"#)
            ?;

        let mut tracker = EtaTracker::load(path);
        tracker.set_context("azure", "gpt-5.6-sol", "medium", "None");
        assert_eq!(tracker.backtest(), None);
        assert!(tracker.turn_history.is_none());
        Ok(())
    }

    #[test]
    fn invalid_persisted_backtest_samples_cannot_reach_duration_conversion()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("eta-history.json");
        fs::write(
            &path,
            br#"{"version":1,"profiles":{"azure":{"phase_history":{},"turn_history":null,"stream_output_history":null,"tool_history":{},"backtest":{"samples":1,"absolute_error_sum":1.0,"covered":1,"recent_absolute_errors":[-1.0]}}}}"#,
        )?;

        let mut tracker = EtaTracker::load(path);
        tracker.set_context("azure", "gpt-5.6-sol", "medium", "None");
        assert_eq!(tracker.backtest(), None);
        Ok(())
    }

    #[test]
    fn oversized_finite_eta_values_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("eta-history.json");
        fs::write(
            &path,
            br#"{"version":1,"profiles":{"azure":{"phase_history":{},"turn_history":{"mean":1e300,"absolute_deviation":0.0,"samples":1,"recent":[1e300]},"stream_output_history":null,"tool_history":{},"backtest":{"samples":0,"absolute_error_sum":0.0,"covered":0,"recent_absolute_errors":[]}}}}"#,
        )?;

        assert!(matches!(
            read_store(&path),
            Err(EtaPersistenceError::InvalidData)
        ));
        Ok(())
    }

    #[test]
    fn persisted_tool_history_stays_within_the_loader_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("eta-history.json");
        let started = Instant::now();
        let mut tracker = EtaTracker::load(path.clone());
        for index in 0_u64..=u64::try_from(RECENT_SAMPLE_LIMIT)? {
            tracker.tool_action_started(index, &format!("tool-{index:02}"), started);
            tracker.tool_action_completed(index, started + Duration::from_secs(1));
        }
        tracker.persist();

        assert!(read_store(&path).is_ok());
        Ok(())
    }
}
