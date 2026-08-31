use crate::agent::orchestrator::WhipKind;
use std::time::{Duration, Instant};

const FRAME_TIME: Duration = Duration::from_millis(90);
const FLASH_TIME: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhipDisplayKind {
    Soft,
    Hard,
}

#[derive(Debug, Clone)]
pub struct WhipController {
    pub requests_sent: u64,
    pub acknowledgements: u64,
    pub last_acknowledgement: Option<WhipDisplayKind>,
    animation_started: Option<Instant>,
    flash_until: Option<Instant>,
}

impl WhipController {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            requests_sent: 0,
            acknowledgements: 0,
            last_acknowledgement: None,
            animation_started: None,
            flash_until: None,
        }
    }

    pub fn record_request(&mut self) {
        self.requests_sent = self.requests_sent.saturating_add(1);
        self.animation_started = Some(Instant::now());
    }

    pub fn acknowledge(&mut self, kind: &WhipKind) {
        self.acknowledgements = self.acknowledgements.saturating_add(1);
        self.last_acknowledgement = Some(match kind {
            WhipKind::Soft => WhipDisplayKind::Soft,
            WhipKind::Hard => WhipDisplayKind::Hard,
        });
        self.flash_until = Some(Instant::now() + FLASH_TIME);
    }

    /// Returns one of three animation frames without ever blocking the event loop.
    #[must_use]
    pub fn frame(&self, now: Instant) -> usize {
        let Some(started) = self.animation_started else {
            return 0;
        };
        let frame =
            (now.saturating_duration_since(started).as_millis() / FRAME_TIME.as_millis()) as usize;
        frame.min(2)
    }

    #[must_use]
    pub fn is_flashing(&self, now: Instant) -> bool {
        self.flash_until.is_some_and(|until| now < until)
    }

    pub fn tick(&mut self, now: Instant) {
        if self.animation_started.is_some_and(|started| {
            now.saturating_duration_since(started) >= FRAME_TIME.saturating_mul(3)
        }) {
            self.animation_started = None;
        }
        if self.flash_until.is_some_and(|until| now >= until) {
            self.flash_until = None;
        }
    }
}

impl Default for WhipController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{FRAME_TIME, WhipController};
    use std::time::Instant;

    #[test]
    fn animation_advances_through_three_frames_without_blocking() {
        let started = Instant::now();
        let mut whip = WhipController::new();
        whip.animation_started = Some(started);

        assert_eq!(whip.frame(started), 0);
        assert_eq!(whip.frame(started + FRAME_TIME), 1);
        assert_eq!(whip.frame(started + FRAME_TIME.saturating_mul(2)), 2);
        whip.tick(started + FRAME_TIME.saturating_mul(3));
        assert_eq!(whip.frame(started + FRAME_TIME.saturating_mul(3)), 0);
    }
}
