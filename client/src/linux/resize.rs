//! Interactive-resize bookkeeping for a proxy window.
//!
//! Windows tells the client exactly when a user drag starts and ends
//! (`WM_ENTERSIZEMOVE` / `WM_EXITSIZEMOVE`). Wayland does not: the compositor just
//! sends configure events with new sizes, and interactive resize, maximize, tiling
//! and snapping all look the same. So this tracker infers the gesture:
//!
//!  * the first unexpected size change begins one (`Begin`, then `Live` at ~10Hz —
//!    protocol.md §5), during which the renderer may stretch (the one permitted
//!    exception to I-1);
//!  * once sizes stop arriving for `SETTLE`, the gesture ends with an
//!    authoritative `End`, and the host's read-back snaps the window to 1:1.
//!
//! A size we asked for ourselves (snapping to the host's actual geometry) is
//! recognised and never mistaken for a user gesture.

use crate::wire::{ResizePhase, Size};
use std::time::{Duration, Instant};

/// Client-side throttle for `Live` requests.
pub const LIVE_INTERVAL: Duration = Duration::from_millis(100);
/// No new size for this long means the drag is over.
pub const SETTLE: Duration = Duration::from_millis(150);

/// A request to send to the host.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub phase: ResizePhase,
    pub size: Size,
}

#[derive(Debug)]
pub struct ResizeTracker {
    /// Current physical client size.
    pub size: Size,
    pub resizing: bool,
    gesture_start: Size,
    last_event: Instant,
    last_live: Option<Instant>,
    /// A size we requested programmatically and expect back exactly once.
    expected: Option<Size>,
}

/// How a gesture finished.
#[derive(Debug, PartialEq, Eq)]
pub struct Finished {
    /// The size changed over the gesture, so an `End` request was issued.
    pub resized: bool,
}

impl ResizeTracker {
    pub fn new(size: Size, now: Instant) -> Self {
        Self {
            size,
            resizing: false,
            gesture_start: size,
            last_event: now,
            last_live: None,
            expected: None,
        }
    }

    /// We are about to ask the compositor for `size`; do not treat the reply as a
    /// user gesture.
    pub fn expect(&mut self, size: Size) {
        self.expected = Some(size);
    }

    /// The compositor configured a new client size. Returns the requests to send.
    pub fn on_resized(&mut self, size: Size, now: Instant) -> Vec<Request> {
        let expected = self.expected.take();
        if size == self.size {
            return vec![];
        }
        let before = self.size;
        self.size = size;
        if expected == Some(size) && !self.resizing {
            return vec![];
        }
        let mut out = vec![];
        if !self.resizing {
            self.resizing = true;
            self.gesture_start = before;
            self.last_live = None;
            out.push(Request {
                phase: ResizePhase::Begin,
                size,
            });
        }
        self.last_event = now;
        if self
            .last_live
            .map(|t| now.duration_since(t) >= LIVE_INTERVAL)
            .unwrap_or(true)
        {
            self.last_live = Some(now);
            out.push(Request {
                phase: ResizePhase::Live,
                size,
            });
        }
        out
    }

    /// Call regularly. When a gesture has gone quiet, finishes it and returns the
    /// closing request (always an `End` carrying the final size).
    pub fn tick(&mut self, now: Instant) -> Option<(Request, Finished)> {
        if !self.resizing || now.duration_since(self.last_event) < SETTLE {
            return None;
        }
        self.resizing = false;
        self.last_live = None;
        let resized = self.size != self.gesture_start;
        self.gesture_start = self.size;
        Some((
            Request {
                phase: ResizePhase::End,
                size: self.size,
            },
            Finished { resized },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(w: u32, h: u32) -> Size {
        Size { w, h }
    }
    fn ms(base: Instant, n: u64) -> Instant {
        base + Duration::from_millis(n)
    }

    #[test]
    fn a_drag_is_begin_then_throttled_live_then_end_after_it_settles() {
        let t0 = Instant::now();
        let mut t = ResizeTracker::new(s(800, 600), t0);
        let first = t.on_resized(s(810, 600), ms(t0, 10));
        assert_eq!(
            first,
            vec![
                Request { phase: ResizePhase::Begin, size: s(810, 600) },
                Request { phase: ResizePhase::Live, size: s(810, 600) },
            ]
        );
        assert!(t.resizing);
        // Within 100ms of the last Live: nothing goes on the wire.
        assert!(t.on_resized(s(820, 600), ms(t0, 40)).is_empty());
        assert!(t.on_resized(s(830, 600), ms(t0, 80)).is_empty());
        assert_eq!(
            t.on_resized(s(840, 600), ms(t0, 120)),
            vec![Request { phase: ResizePhase::Live, size: s(840, 600) }]
        );
        // Still moving: not finished.
        assert!(t.tick(ms(t0, 200)).is_none());
        let (end, fin) = t.tick(ms(t0, 300)).expect("settled");
        assert_eq!(end, Request { phase: ResizePhase::End, size: s(840, 600) });
        assert!(fin.resized);
        assert!(!t.resizing);
        assert!(t.tick(ms(t0, 900)).is_none());
    }

    #[test]
    fn the_size_we_asked_for_is_not_a_user_gesture() {
        let t0 = Instant::now();
        let mut t = ResizeTracker::new(s(800, 600), t0);
        t.expect(s(1000, 700));
        assert!(t.on_resized(s(1000, 700), ms(t0, 5)).is_empty());
        assert!(!t.resizing);
        assert_eq!(t.size, s(1000, 700));
    }

    #[test]
    fn a_different_size_than_requested_is_a_gesture_and_clears_the_expectation() {
        let t0 = Instant::now();
        let mut t = ResizeTracker::new(s(800, 600), t0);
        t.expect(s(1000, 700));
        // A tiling compositor answered with its own size.
        assert!(!t.on_resized(s(960, 540), ms(t0, 5)).is_empty());
        assert!(t.resizing);
        t.tick(ms(t0, 400));
        // The stale expectation must not swallow a later real resize.
        assert!(!t.on_resized(s(1000, 700), ms(t0, 500)).is_empty());
    }

    #[test]
    fn repeating_the_current_size_does_nothing() {
        let t0 = Instant::now();
        let mut t = ResizeTracker::new(s(800, 600), t0);
        assert!(t.on_resized(s(800, 600), ms(t0, 1)).is_empty());
        assert!(!t.resizing);
    }

    #[test]
    fn a_gesture_that_returns_to_its_start_size_is_not_a_resize() {
        let t0 = Instant::now();
        let mut t = ResizeTracker::new(s(800, 600), t0);
        t.on_resized(s(900, 600), ms(t0, 1));
        t.on_resized(s(800, 600), ms(t0, 2));
        let (_, fin) = t.tick(ms(t0, 300)).unwrap();
        assert!(!fin.resized);
    }
}
