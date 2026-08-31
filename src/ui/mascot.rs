use std::time::{Duration, Instant};

use chrono::{Local, Timelike as _};

use crate::agent::AgentPhase;

use super::i18n::Text;

const IDLE_NAP_AFTER: Duration = Duration::from_secs(6 * 60);
const HUNGRY_AFTER: Duration = Duration::from_secs(35 * 60);
const WAKE_GRACE: Duration = Duration::from_secs(30 * 60);
const FULLNESS_DECAY: Duration = Duration::from_secs(12 * 60);
const SPECIAL_ANIMATION: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MascotMood {
    Curious,
    Blinking,
    Waving,
    Playful,
    Pouncing,
    Grooming,
    Affectionate,
    Purring,
    Stretching,
    Dancing,
    Rolling,
    VictoryRolling,
    Chasing,
    Stargazing,
    Tongue,
    Yawning,
    Working,
    Celebrating,
    Waiting,
    Error,
    Sleeping,
    Hungry,
    Overfed,
    Burping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedReaction {
    Happy,
    Full,
    Burp,
}

impl FeedReaction {
    #[must_use]
    pub const fn status_key(self) -> Text {
        match self {
            Self::Happy => Text::PixelSnackEnjoyed,
            Self::Full => Text::PixelVeryFull,
            Self::Burp => Text::PixelBurped,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MascotState {
    enabled: bool,
    frame: usize,
    last_tick: Instant,
    last_interaction: Instant,
    last_fed: Instant,
    last_fullness_decay: Instant,
    fullness: u8,
    forced_awake_until: Option<Instant>,
    special: Option<(MascotMood, Instant)>,
}

impl MascotState {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        let now = Instant::now();
        Self {
            enabled,
            frame: 0,
            last_tick: now,
            last_interaction: now,
            last_fed: now,
            last_fullness_decay: now,
            fullness: 0,
            forced_awake_until: None,
            special: None,
        }
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if enabled {
            self.last_interaction = Instant::now();
        }
    }

    pub(crate) const fn animation_frame(&self) -> usize {
        self.frame
    }

    pub fn tick(&mut self, now: Instant) {
        if now.saturating_duration_since(self.last_tick) >= Duration::from_millis(140) {
            self.frame = self.frame.wrapping_add(1);
            self.last_tick = now;
        }
        if self.forced_awake_until.is_some_and(|until| now >= until) {
            self.forced_awake_until = None;
        }
        if self.special.is_some_and(|(_, until)| now >= until) {
            self.special = None;
        }
        let fullness_elapsed = now.saturating_duration_since(self.last_fullness_decay);
        let decay_intervals = fullness_elapsed.as_secs() / FULLNESS_DECAY.as_secs();
        if decay_intervals > 0 {
            self.fullness = self
                .fullness
                .saturating_sub(u8::try_from(decay_intervals).unwrap_or(u8::MAX));
            let completed =
                Duration::from_secs(decay_intervals.saturating_mul(FULLNESS_DECAY.as_secs()));
            self.last_fullness_decay = self
                .last_fullness_decay
                .checked_add(completed)
                .unwrap_or(now);
        }
    }

    pub fn interact(&mut self, now: Instant) {
        self.last_interaction = now;
    }

    pub fn feed(&mut self, now: Instant) -> FeedReaction {
        self.last_fed = now;
        self.last_interaction = now;
        self.last_fullness_decay = now;
        self.fullness = self.fullness.saturating_add(1).min(4);
        let (reaction, mood, duration) = match self.fullness {
            0..=2 => (
                FeedReaction::Happy,
                MascotMood::Celebrating,
                SPECIAL_ANIMATION,
            ),
            3 => (
                FeedReaction::Full,
                MascotMood::Overfed,
                Duration::from_secs(6),
            ),
            _ => {
                self.fullness = 1;
                (
                    FeedReaction::Burp,
                    MascotMood::Burping,
                    Duration::from_secs(4),
                )
            }
        };
        self.special = Some((mood, now + duration));
        reaction
    }

    pub fn wake(&mut self, now: Instant) {
        self.last_interaction = now;
        self.forced_awake_until = Some(now + WAKE_GRACE);
        self.special = Some((MascotMood::Playful, now + SPECIAL_ANIMATION));
    }

    pub fn celebrate(&mut self, now: Instant) {
        self.special = Some((MascotMood::VictoryRolling, now + Duration::from_secs(7)));
    }

    #[must_use]
    pub fn mood(&self, phase: &AgentPhase, now: Instant) -> MascotMood {
        if matches!(phase, AgentPhase::Error { .. }) {
            return MascotMood::Error;
        }
        if matches!(
            phase,
            AgentPhase::AwaitingConfirmation
                | AgentPhase::AwaitingPatchApproval
                | AgentPhase::AwaitingPlanApproval
                | AgentPhase::AwaitingContinuation
        ) {
            return MascotMood::Waiting;
        }
        if phase.is_busy() {
            return MascotMood::Working;
        }
        if let Some((mood, until)) = self.special
            && now < until
        {
            return mood;
        }
        if now.saturating_duration_since(self.last_fed) >= HUNGRY_AFTER {
            return MascotMood::Hungry;
        }
        let local_hour = Local::now().hour();
        let night = !(7..23).contains(&local_hour);
        let idle_long = now.saturating_duration_since(self.last_interaction) >= IDLE_NAP_AFTER;
        if self.forced_awake_until.is_none() && (night || idle_long) {
            return MascotMood::Sleeping;
        }
        match (self.frame / 28) % 32 {
            1 => MascotMood::Waving,
            3 => MascotMood::Blinking,
            5..=6 => MascotMood::Playful,
            8 => MascotMood::Grooming,
            10 => MascotMood::Pouncing,
            12 => MascotMood::Stretching,
            14..=15 => MascotMood::Rolling,
            17 => MascotMood::Dancing,
            19 => MascotMood::Tongue,
            21 => MascotMood::Affectionate,
            22 => MascotMood::Purring,
            25 => MascotMood::Yawning,
            27..=28 => MascotMood::Chasing,
            30 => MascotMood::Stargazing,
            _ => MascotMood::Curious,
        }
    }

    #[must_use]
    pub fn art(&self, mood: MascotMood) -> [String; 8] {
        pixel_frame(mood, self.frame % 4)
    }

    #[must_use]
    pub fn mini_face(&self, mood: MascotMood) -> &'static str {
        let alternate = (self.frame / 3).is_multiple_of(2);
        match (mood, alternate) {
            (MascotMood::Curious, true) => "(o.o)",
            (MascotMood::Curious, false) => "(o.o)?",
            (MascotMood::Blinking, _) => "(-.-)",
            (MascotMood::Waving, _) => "(o.o)/",
            (MascotMood::Playful, _) => "(^.^)",
            (MascotMood::Pouncing, _) => "(>.<)!",
            (MascotMood::Grooming, _) => "(-.-)*",
            (MascotMood::Affectionate, _) => "(^.^)<3",
            (MascotMood::Purring, true) => "(^.^)rr",
            (MascotMood::Purring, false) => "(-.-)rr",
            (MascotMood::Stretching, _) => "(^o^)",
            (MascotMood::Dancing, _) => "(^▽^)♪",
            (MascotMood::Rolling, _) => "(>.<)",
            (MascotMood::VictoryRolling, true) => "(^▽^)↻",
            (MascotMood::VictoryRolling, false) => "↺(^▽^)",
            (MascotMood::Chasing, true) => "(o.o)>",
            (MascotMood::Chasing, false) => "<(o.o)",
            (MascotMood::Stargazing, true) => "(o.o)*",
            (MascotMood::Stargazing, false) => "*(o.o)",
            (MascotMood::Tongue, _) => "(^.U)",
            (MascotMood::Yawning, _) => "(-.O)",
            (MascotMood::Working, true) => "(•̀ω•́)",
            (MascotMood::Working, false) => "(•̀.•́)",
            (MascotMood::Celebrating, true) => "(^▽^)",
            (MascotMood::Celebrating, false) => "(>▽<)",
            (MascotMood::Waiting, _) => "(•.•)?",
            (MascotMood::Error, _) => "(x_x)!",
            (MascotMood::Sleeping, true) => "(-.-)z",
            (MascotMood::Sleeping, false) => "(-.-)Z",
            (MascotMood::Hungry, _) => "(•﹃•)",
            (MascotMood::Overfed, _) => "(^.^)",
            (MascotMood::Burping, _) => "(O.o)",
        }
    }
}

const PIXEL_CANVAS_WIDTH: usize = 25;
const PIXEL_PANEL_INNER_WIDTH: usize = 15;

fn pixel_frame(mood: MascotMood, phase: usize) -> [String; 8] {
    [
        pixel_line(pixel_effect(mood, phase)),
        pixel_line("/\\_/\\"),
        pixel_line(pixel_face(mood, phase)),
        pixel_line(pixel_pose(mood, phase)),
        pixel_line(".---------------."),
        pixel_line(&pixel_status(mood, phase)),
        pixel_line("'---------------'"),
        pixel_line("Pixel"),
    ]
}

fn pixel_line(content: &str) -> String {
    use unicode_width::UnicodeWidthStr as _;

    let width = content.width();
    debug_assert!(width <= PIXEL_CANVAS_WIDTH);
    let padding = PIXEL_CANVAS_WIDTH.saturating_sub(width);
    let left = padding / 2;
    format!(
        "{}{}{}",
        " ".repeat(left),
        content,
        " ".repeat(padding - left)
    )
}

fn pixel_status(mood: MascotMood, phase: usize) -> String {
    use unicode_width::UnicodeWidthStr as _;

    let label = pixel_status_label(mood, phase);
    let width = label.width();
    debug_assert!(width <= PIXEL_PANEL_INNER_WIDTH);
    let padding = PIXEL_PANEL_INNER_WIDTH.saturating_sub(width);
    let left = padding / 2;
    format!(
        "|{}{}{}|",
        " ".repeat(left),
        label,
        " ".repeat(padding - left)
    )
}

fn pixel_effect(mood: MascotMood, phase: usize) -> &'static str {
    match (mood, phase % 4) {
        (MascotMood::Curious, 0 | 3) => "?",
        (MascotMood::Curious, _) => "?  ?",
        (MascotMood::Waving, 0 | 3) => "hello!",
        (MascotMood::Waving, _) => "* hello *",
        (MascotMood::Playful, 0) => "o . .",
        (MascotMood::Playful, 1) => ". o .",
        (MascotMood::Playful, 2) => ". . o",
        (MascotMood::Playful, _) => ". o .",
        (MascotMood::Pouncing, 0) => "ready",
        (MascotMood::Pouncing, 1) => "set",
        (MascotMood::Pouncing, 2) => "go!",
        (MascotMood::Pouncing, _) => "caught!",
        (MascotMood::Grooming, 0 | 3) => "*",
        (MascotMood::Grooming, _) => "* clean *",
        (MascotMood::Affectionate, 0 | 3) => "<3",
        (MascotMood::Affectionate, _) => "♥  ♥",
        (MascotMood::Purring, 0) => "~ purr ~",
        (MascotMood::Purring, 1) => "~~ purr ~~",
        (MascotMood::Purring, 2) => "~~~ purr ~~~",
        (MascotMood::Purring, _) => "~~ purr ~~",
        (MascotMood::Stretching, 0) => "stretch",
        (MascotMood::Stretching, 1) => "stretch.",
        (MascotMood::Stretching, 2) => "stretch..",
        (MascotMood::Stretching, _) => "stretch...",
        (MascotMood::Dancing, 0) => "♪     ♫",
        (MascotMood::Dancing, 1) => "  ♫ ♪",
        (MascotMood::Dancing, 2) => "♫     ♪",
        (MascotMood::Dancing, _) => "♪ ♫ ♪",
        (MascotMood::Rolling, 0) => "↻",
        (MascotMood::Rolling, 1) => "↻ ↻",
        (MascotMood::Rolling, 2) => "↺ ↺",
        (MascotMood::Rolling, _) => "✓",
        (MascotMood::VictoryRolling, 0) => "✦ ↻",
        (MascotMood::VictoryRolling, 1) => "↻ ✦",
        (MascotMood::VictoryRolling, 2) => "✦ ↺ ✦",
        (MascotMood::VictoryRolling, _) => "✓ ✦ ✓",
        (MascotMood::Chasing, 0) => "o · · ·",
        (MascotMood::Chasing, 1) => "· o · ·",
        (MascotMood::Chasing, 2) => "· · o ·",
        (MascotMood::Chasing, _) => "· · · o",
        (MascotMood::Stargazing, 0) => "·  *  ·",
        (MascotMood::Stargazing, 1) => "*  ·  ✦",
        (MascotMood::Stargazing, 2) => "✦  *  ·",
        (MascotMood::Stargazing, _) => "·  ✦  *",
        (MascotMood::Tongue, 0 | 3) => ":P",
        (MascotMood::Tongue, _) => ":p",
        (MascotMood::Yawning, 0 | 3) => "z",
        (MascotMood::Yawning, _) => "z  Z",
        (MascotMood::Working, 0) => "thinking",
        (MascotMood::Working, 1) => "thinking.",
        (MascotMood::Working, 2) => "thinking..",
        (MascotMood::Working, _) => "thinking...",
        (MascotMood::Celebrating, 0) => "✦     ✦",
        (MascotMood::Celebrating, 1) => "  ✦ ✦",
        (MascotMood::Celebrating, 2) => "✓  ✦  ✓",
        (MascotMood::Celebrating, _) => "✓ ✓ ✓",
        (MascotMood::Waiting, 0 | 3) => "?",
        (MascotMood::Waiting, _) => "?  ?",
        (MascotMood::Error, 0 | 3) => "!",
        (MascotMood::Error, _) => "! ! !",
        (MascotMood::Sleeping, 0) => "z",
        (MascotMood::Sleeping, 1) => "z  Z",
        (MascotMood::Sleeping, 2) => "z Z z",
        (MascotMood::Sleeping, _) => "Z  z",
        (MascotMood::Hungry, 0 | 3) => "snack?",
        (MascotMood::Hungry, _) => "snack please?",
        (MascotMood::Overfed, 0 | 3) => "♥   ♥",
        (MascotMood::Overfed, _) => "♥ ♥ ♥",
        (MascotMood::Burping, 0 | 3) => "*burp*",
        (MascotMood::Burping, _) => "* burp *",
        _ => "",
    }
}

fn pixel_face(mood: MascotMood, phase: usize) -> &'static str {
    match mood {
        MascotMood::Blinking if matches!(phase % 4, 1 | 2) => "( -.- )",
        MascotMood::Blinking | MascotMood::Curious | MascotMood::Waiting => "( o.o )",
        MascotMood::Waving
        | MascotMood::Playful
        | MascotMood::Grooming
        | MascotMood::Affectionate
        | MascotMood::Purring
        | MascotMood::Stretching
        | MascotMood::Dancing
        | MascotMood::Celebrating
        | MascotMood::Overfed => "( ^.^ )",
        MascotMood::Pouncing | MascotMood::Rolling | MascotMood::Chasing => "( >.< )",
        MascotMood::VictoryRolling => "( ^o^ )",
        MascotMood::Stargazing => "( o.o )",
        MascotMood::Tongue => "( ^.u )",
        MascotMood::Yawning => "( -.O )",
        MascotMood::Working if phase.is_multiple_of(2) => "( o.o )",
        MascotMood::Working => "( •̀.•́ )",
        MascotMood::Error => "( x_x )",
        MascotMood::Sleeping => "( -.- )",
        MascotMood::Hungry => "( •﹃• )",
        MascotMood::Burping => "( O.o )",
    }
}

fn pixel_pose(mood: MascotMood, phase: usize) -> &'static str {
    match (mood, phase % 4) {
        (MascotMood::Waving, 0 | 3) => "> ^ </",
        (MascotMood::Waving, _) => "\\> ^ <",
        (MascotMood::Pouncing, 0 | 1) => "_> ^ <_",
        (MascotMood::Pouncing, _) => "> ^ <",
        (MascotMood::Grooming, 0 | 3) => "> ^ <*",
        (MascotMood::Grooming, _) => "*> ^ <",
        (MascotMood::Affectionate, _) => "<  ♥  >",
        (MascotMood::Purring, 0 | 2) => "~ > ^ < ~",
        (MascotMood::Purring, _) => "~~ > ^ < ~~",
        (MascotMood::Stretching, 0 | 3) => "-- >^< --",
        (MascotMood::Stretching, _) => "--- >^< ---",
        (MascotMood::Dancing, 0) => "\\> ^ <",
        (MascotMood::Dancing, 1) => "> ^ </",
        (MascotMood::Dancing, 2) => "\\> ^ </",
        (MascotMood::Dancing, _) => "> ^ <",
        (MascotMood::Rolling | MascotMood::VictoryRolling, 0 | 2) => "( >^< )",
        (MascotMood::Rolling | MascotMood::VictoryRolling, _) => "( <^> )",
        (MascotMood::Chasing, 0 | 1) => "> ^ <",
        (MascotMood::Chasing, _) => "> ^ < >",
        (MascotMood::Working, _) => "_> ^ <_",
        (MascotMood::Celebrating, 0 | 3) => "\\> ^ </",
        (MascotMood::Celebrating, _) => "> ^ <",
        (MascotMood::Overfed, _) => "/ >^< \\",
        (MascotMood::Burping, _) => "> o <",
        _ => "> ^ <",
    }
}

fn pixel_status_label(mood: MascotMood, phase: usize) -> &'static str {
    match (mood, phase % 4) {
        (MascotMood::Curious, _) => "curious",
        (MascotMood::Blinking, _) => "hello",
        (MascotMood::Waving, _) => "hi!",
        (MascotMood::Playful, 0 | 2) => "play  o",
        (MascotMood::Playful, _) => "o  play",
        (MascotMood::Pouncing, 0) => "ready",
        (MascotMood::Pouncing, 1) => "set",
        (MascotMood::Pouncing, 2) => "pounce!",
        (MascotMood::Pouncing, _) => "caught!",
        (MascotMood::Grooming, _) => "tidy",
        (MascotMood::Affectionate, _) => "<3",
        (MascotMood::Purring, 0) => "~ purr",
        (MascotMood::Purring, 1) => "~~ purr",
        (MascotMood::Purring, 2) => "~~~ purr",
        (MascotMood::Purring, _) => "purr ~~",
        (MascotMood::Stretching, _) => "stretch",
        (MascotMood::Dancing, 0) => "step ←",
        (MascotMood::Dancing, 1) => "step ↑",
        (MascotMood::Dancing, 2) => "step →",
        (MascotMood::Dancing, _) => "spin!",
        (MascotMood::Rolling, 0) => "curl ↻",
        (MascotMood::Rolling, 1) => "roll ↻",
        (MascotMood::Rolling, 2) => "roll ↺",
        (MascotMood::Rolling, _) => "done ✓",
        (MascotMood::VictoryRolling, 0) => "victory ↻",
        (MascotMood::VictoryRolling, 1) => "shipped ↻",
        (MascotMood::VictoryRolling, 2) => "victory ↺",
        (MascotMood::VictoryRolling, _) => "done ✓",
        (MascotMood::Chasing, 0) => "o · · ·",
        (MascotMood::Chasing, 1) => "· o · ·",
        (MascotMood::Chasing, 2) => "· · o ·",
        (MascotMood::Chasing, _) => "· · · o",
        (MascotMood::Stargazing, 0) => "·  *  ·",
        (MascotMood::Stargazing, 1) => "*  ·  ✦",
        (MascotMood::Stargazing, 2) => "✦  *  ·",
        (MascotMood::Stargazing, _) => "·  ✦  *",
        (MascotMood::Tongue, _) => ":P",
        (MascotMood::Yawning, _) => "yawn",
        (MascotMood::Working, 0) => "[>       ]",
        (MascotMood::Working, 1) => "[==>     ]",
        (MascotMood::Working, 2) => "[====>   ]",
        (MascotMood::Working, _) => "[======> ]",
        (MascotMood::Celebrating, 0) => "shipped!",
        (MascotMood::Celebrating, 1) => "nice!",
        (MascotMood::Celebrating, 2) => "✓  ✓",
        (MascotMood::Celebrating, _) => "hooray!",
        (MascotMood::Waiting, 0 | 2) => "waiting ·",
        (MascotMood::Waiting, _) => "waiting …",
        (MascotMood::Error, _) => "try again",
        (MascotMood::Sleeping, _) => "dreaming",
        (MascotMood::Hungry, _) => "snack [F7]",
        (MascotMood::Overfed, _) => "very full",
        (MascotMood::Burping, _) => "burp",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn work_and_error_override_idle_pet_states() {
        let pet = MascotState::new(true);
        let now = Instant::now() + HUNGRY_AFTER + Duration::from_secs(1);
        assert_eq!(pet.mood(&AgentPhase::Streaming, now), MascotMood::Working);
        assert_eq!(
            pet.mood(
                &AgentPhase::Error {
                    message: "network".to_owned(),
                    recoverable: true,
                },
                now,
            ),
            MascotMood::Error
        );
    }

    #[test]
    fn feeding_progresses_from_happy_to_full_and_burp() {
        let mut pet = MascotState::new(true);
        let now = Instant::now();
        assert_eq!(pet.feed(now), FeedReaction::Happy);
        assert_eq!(pet.feed(now + Duration::from_secs(1)), FeedReaction::Happy);
        assert_eq!(pet.feed(now + Duration::from_secs(2)), FeedReaction::Full);
        assert_eq!(pet.feed(now + Duration::from_secs(3)), FeedReaction::Burp);
        assert_eq!(
            pet.mood(&AgentPhase::Idle, now + Duration::from_secs(4)),
            MascotMood::Burping
        );
    }

    #[test]
    fn fullness_catches_up_after_multiple_missed_decay_intervals() {
        let mut pet = MascotState::new(true);
        let now = Instant::now();
        pet.feed(now);
        pet.feed(now);
        pet.feed(now);

        pet.tick(now + FULLNESS_DECAY * 3);

        assert_eq!(pet.fullness, 0);
    }

    #[test]
    fn waking_forces_a_playful_awake_state() {
        let mut pet = MascotState::new(true);
        let now = Instant::now();
        pet.wake(now);
        assert_eq!(
            pet.mood(&AgentPhase::Idle, now + Duration::from_secs(1)),
            MascotMood::Playful
        );
        assert!(pet.forced_awake_until.is_some());
    }

    #[test]
    fn celebration_uses_the_four_phase_victory_roll() {
        let mut pet = MascotState::new(true);
        let now = Instant::now();
        pet.celebrate(now);
        assert_eq!(
            pet.mood(&AgentPhase::Idle, now + Duration::from_secs(1)),
            MascotMood::VictoryRolling
        );
    }

    #[test]
    fn every_pixel_frame_stays_inside_its_visual_texture() {
        let moods = [
            MascotMood::Curious,
            MascotMood::Blinking,
            MascotMood::Waving,
            MascotMood::Playful,
            MascotMood::Pouncing,
            MascotMood::Grooming,
            MascotMood::Affectionate,
            MascotMood::Purring,
            MascotMood::Stretching,
            MascotMood::Dancing,
            MascotMood::Rolling,
            MascotMood::VictoryRolling,
            MascotMood::Chasing,
            MascotMood::Stargazing,
            MascotMood::Tongue,
            MascotMood::Yawning,
            MascotMood::Working,
            MascotMood::Celebrating,
            MascotMood::Waiting,
            MascotMood::Error,
            MascotMood::Sleeping,
            MascotMood::Hungry,
            MascotMood::Overfed,
            MascotMood::Burping,
        ];
        let mut pet = MascotState::new(true);
        for frame in 0..4 {
            pet.frame = frame;
            for mood in moods {
                for line in pet.art(mood) {
                    assert_eq!(
                        UnicodeWidthStr::width(line.as_str()),
                        25,
                        "Pixel {mood:?} frame must stay on one centered 25-cell canvas: {line:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_animation_keeps_pixels_head_anchored() {
        let moods = [
            MascotMood::Curious,
            MascotMood::Blinking,
            MascotMood::Waving,
            MascotMood::Playful,
            MascotMood::Pouncing,
            MascotMood::Grooming,
            MascotMood::Affectionate,
            MascotMood::Purring,
            MascotMood::Stretching,
            MascotMood::Dancing,
            MascotMood::Rolling,
            MascotMood::VictoryRolling,
            MascotMood::Chasing,
            MascotMood::Stargazing,
            MascotMood::Tongue,
            MascotMood::Yawning,
            MascotMood::Working,
            MascotMood::Celebrating,
            MascotMood::Waiting,
            MascotMood::Error,
            MascotMood::Sleeping,
            MascotMood::Hungry,
            MascotMood::Overfed,
            MascotMood::Burping,
        ];
        let mut pet = MascotState::new(true);
        for mood in moods {
            let mut positions = Vec::new();
            for frame in 0..4 {
                pet.frame = frame;
                let art = pet.art(mood);
                let position = art.iter().enumerate().find_map(|(row, line)| {
                    line.find("/\\_/\\")
                        .map(|byte| (row, UnicodeWidthStr::width(&line[..byte])))
                });
                assert!(
                    position.is_some(),
                    "{mood:?} frame {frame} lost Pixel's head"
                );
                if let Some(position) = position {
                    positions.push(position);
                }
            }
            let min_row = positions.iter().map(|(row, _)| *row).min().unwrap_or(0);
            let max_row = positions.iter().map(|(row, _)| *row).max().unwrap_or(0);
            let min_col = positions.iter().map(|(_, col)| *col).min().unwrap_or(0);
            let max_col = positions.iter().map(|(_, col)| *col).max().unwrap_or(0);
            assert!(
                max_row.saturating_sub(min_row) <= 1 && max_col.saturating_sub(min_col) <= 1,
                "{mood:?} jerks Pixel around the texture: {positions:?}"
            );
        }
    }

    #[test]
    fn every_status_panel_keeps_its_edges_aligned() {
        let moods = [
            MascotMood::Curious,
            MascotMood::Blinking,
            MascotMood::Waving,
            MascotMood::Playful,
            MascotMood::Pouncing,
            MascotMood::Grooming,
            MascotMood::Affectionate,
            MascotMood::Purring,
            MascotMood::Stretching,
            MascotMood::Dancing,
            MascotMood::Rolling,
            MascotMood::VictoryRolling,
            MascotMood::Chasing,
            MascotMood::Stargazing,
            MascotMood::Tongue,
            MascotMood::Yawning,
            MascotMood::Working,
            MascotMood::Celebrating,
            MascotMood::Waiting,
            MascotMood::Error,
            MascotMood::Sleeping,
            MascotMood::Hungry,
            MascotMood::Overfed,
            MascotMood::Burping,
        ];
        let mut pet = MascotState::new(true);
        for mood in moods {
            for frame in 0..4 {
                pet.frame = frame;
                let art = pet.art(mood);
                let panel = [&art[4], &art[5], &art[6]];
                let left_edges = panel.map(|line| line.len() - line.trim_start().len());
                let widths = panel.map(|line| UnicodeWidthStr::width(line.trim()));
                assert_eq!(
                    left_edges, [4; 3],
                    "{mood:?} frame {frame} panel is horizontally misaligned"
                );
                assert_eq!(
                    widths, [17; 3],
                    "{mood:?} frame {frame} panel has uneven edges"
                );
            }
        }
    }

    #[test]
    fn complex_pixel_animations_each_have_four_distinct_frames() {
        use std::collections::BTreeSet;

        let mut pet = MascotState::new(true);
        for mood in [
            MascotMood::Working,
            MascotMood::Dancing,
            MascotMood::Rolling,
            MascotMood::Purring,
            MascotMood::VictoryRolling,
            MascotMood::Chasing,
            MascotMood::Stargazing,
        ] {
            let mut frames = BTreeSet::new();
            for frame in 0..4 {
                pet.frame = frame;
                frames.insert(pet.art(mood).join("\n"));
            }
            assert_eq!(
                frames.len(),
                4,
                "{mood:?} did not animate through four phases"
            );
        }
    }
}
