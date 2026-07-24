use std::cmp::min;
use std::time::Duration;

use tokio::time::Instant;

pub struct FrameScheduler {
    dirty: bool,
    last_draw: Option<Instant>,
    next_spinner: Instant,
    min_frame_interval: Duration,
    spinner_interval: Duration,
}

impl FrameScheduler {
    pub fn new(now: Instant) -> Self {
        let spinner_interval = Duration::from_millis(80);
        Self {
            dirty: true,
            last_draw: None,
            next_spinner: now + spinner_interval,
            min_frame_interval: Duration::from_millis(16),
            spinner_interval,
        }
    }

    pub fn request_frame(&mut self) {
        self.dirty = true;
    }

    pub fn deadline(&self, now: Instant, animated: bool) -> Option<Instant> {
        let draw_deadline = self.dirty.then(|| {
            self.last_draw
                .map(|last| (last + self.min_frame_interval).max(now))
                .unwrap_or(now)
        });
        let spinner_deadline = animated.then_some(self.next_spinner);
        match (draw_deadline, spinner_deadline) {
            (Some(draw), Some(spinner)) => Some(min(draw, spinner)),
            (Some(draw), None) => Some(draw),
            (None, Some(spinner)) => Some(spinner),
            (None, None) => None,
        }
    }

    pub fn on_deadline(&mut self, now: Instant, animated: bool) -> bool {
        if !animated {
            self.next_spinner = now + self.spinner_interval;
            return false;
        }
        if now < self.next_spinner {
            return false;
        }
        while self.next_spinner <= now {
            self.next_spinner += self.spinner_interval;
        }
        true
    }

    pub fn should_draw(&self, now: Instant) -> bool {
        self.dirty
            && self
                .last_draw
                .is_none_or(|last| now >= last + self.min_frame_interval)
    }

    pub fn frame_drawn(&mut self, now: Instant) {
        self.dirty = false;
        self.last_draw = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_dirty_requests_until_the_frame_interval() {
        let start = Instant::now();
        let mut scheduler = FrameScheduler::new(start);
        assert!(scheduler.should_draw(start));
        scheduler.frame_drawn(start);
        scheduler.request_frame();

        assert!(!scheduler.should_draw(start + Duration::from_millis(10)));
        assert!(scheduler.should_draw(start + Duration::from_millis(16)));
    }

    #[test]
    fn idle_and_clean_has_no_deadline() {
        let start = Instant::now();
        let mut scheduler = FrameScheduler::new(start);
        scheduler.frame_drawn(start);

        assert_eq!(scheduler.deadline(start, false), None);
        assert_eq!(
            scheduler.deadline(start, true),
            Some(start + Duration::from_millis(80))
        );
    }
}
