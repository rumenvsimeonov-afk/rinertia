use anyhow::Result;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, Window};
use x11rb::rust_connection::RustConnection;

const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const BLOCKED_SAMPLE_LIMIT: u8 = 3;

struct X11Session {
    connection: RustConnection,
    root: Window,
}

pub struct PointerBlockDetector {
    session: Option<X11Session>,
    last_connect_attempt: Option<Instant>,
    last_position: Option<(i16, i16)>,
    pending_motion: bool,
    blocked_samples: u8,
}

impl PointerBlockDetector {
    pub fn new() -> Self {
        Self {
            session: None,
            last_connect_attempt: None,
            last_position: None,
            pending_motion: false,
            blocked_samples: 0,
        }
    }

    pub fn reset(&mut self) {
        self.pending_motion = false;
        self.blocked_samples = 0;
        self.last_position = self.query_position();
    }

    pub fn record_emission(&mut self, dx: i32, dy: i32) {
        if dx != 0 || dy != 0 {
            self.pending_motion = true;
        }
    }

    pub fn pointer_is_blocked(&mut self) -> bool {
        if !self.pending_motion {
            return false;
        }
        self.pending_motion = false;

        let Some(position) = self.query_position() else {
            self.last_position = None;
            self.blocked_samples = 0;
            return false;
        };

        self.observe_position(position)
    }

    fn observe_position(&mut self, position: (i16, i16)) -> bool {
        if self.last_position == Some(position) {
            self.blocked_samples = self.blocked_samples.saturating_add(1);
        } else {
            self.blocked_samples = 0;
        }
        self.last_position = Some(position);

        self.blocked_samples >= BLOCKED_SAMPLE_LIMIT
    }

    fn query_position(&mut self) -> Option<(i16, i16)> {
        if !self.ensure_connected() {
            return None;
        }

        let result = query_position(self.session.as_ref().expect("X11 session exists"));
        match result {
            Ok(position) => Some(position),
            Err(error) => {
                log::debug!("X11 pointer query failed: {error:#}");
                self.session = None;
                None
            }
        }
    }

    fn ensure_connected(&mut self) -> bool {
        if self.session.is_some() {
            return true;
        }

        let now = Instant::now();
        if self
            .last_connect_attempt
            .is_some_and(|last| now.duration_since(last) < RECONNECT_DELAY)
        {
            return false;
        }
        self.last_connect_attempt = Some(now);

        match x11rb::connect(None) {
            Ok((connection, screen_number)) => {
                let root = connection.setup().roots[screen_number].root;
                log::info!(
                    "X11 pointer boundary detection enabled for DISPLAY={}",
                    std::env::var("DISPLAY").unwrap_or_else(|_| "<unset>".into())
                );
                self.session = Some(X11Session { connection, root });
                true
            }
            Err(error) => {
                log::debug!("X11 pointer boundary detection unavailable: {error}");
                false
            }
        }
    }
}

fn query_position(session: &X11Session) -> Result<(i16, i16)> {
    let reply = session.connection.query_pointer(session.root)?.reply()?;
    Ok((reply.root_x, reply.root_y))
}

#[cfg(test)]
mod tests {
    use super::PointerBlockDetector;

    #[test]
    fn requires_three_unchanged_positions() {
        let mut detector = PointerBlockDetector::new();
        detector.last_position = Some((100, 200));

        assert!(!detector.observe_position((100, 200)));
        assert!(!detector.observe_position((100, 200)));
        assert!(detector.observe_position((100, 200)));
    }

    #[test]
    fn cursor_movement_resets_blocked_count() {
        let mut detector = PointerBlockDetector::new();
        detector.last_position = Some((100, 200));

        assert!(!detector.observe_position((100, 200)));
        assert!(!detector.observe_position((101, 200)));
        assert!(!detector.observe_position((101, 200)));
        assert!(!detector.observe_position((101, 200)));
        assert!(detector.observe_position((101, 200)));
    }
}
