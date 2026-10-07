//! Latency shown in the toolbar of a terminal, next to its kind.
//!
//! - **SSH from this computer**: the round trip of an SSH request on the
//!   open connection (`Connection::latency`), through the jump hosts if any.
//! - **Server sessions** (own, shared or joined with a link): the round trip
//!   to the Termoak server (`ping` / `pong` on the session's WebSocket); the
//!   server does not report the one from it to the host.
//!
//! It is measured every [`INTERVAL`] while the terminal is connected and on
//! screen: the view asks for a measurement when it renders and is due, and
//! renders again [`INTERVAL`] after the answer. A hidden tab does not render,
//! so it does not measure either.

use std::time::{Duration, Instant};

/// Time between measurements.
pub const INTERVAL: Duration = Duration::from_secs(5);
/// Longest wait for an answer (then the latency is unknown).
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// Below this it is shown as normal.
const FAIR_FROM: Duration = Duration::from_millis(150);
/// From this on it is shown in red.
const POOR_FROM: Duration = Duration::from_millis(400);

/// How good a latency is (its color).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Unknown (not measured yet or no answer).
    Unknown,
    Good,
    /// Amber.
    Fair,
    /// Red.
    Poor,
}

pub fn level(rtt: Option<Duration>) -> Level {
    match rtt {
        None => Level::Unknown,
        Some(d) if d < FAIR_FROM => Level::Good,
        Some(d) if d < POOR_FROM => Level::Fair,
        Some(_) => Level::Poor,
    }
}

/// Text of the badge: `42 ms`, `<1 ms` or `—` while unknown.
pub fn format(rtt: Option<Duration>) -> String {
    match rtt {
        None => "—".to_string(),
        Some(d) if d < Duration::from_millis(1) => "<1 ms".to_string(),
        Some(d) => format!("{} ms", d.as_millis()),
    }
}

/// Name of a Termoak server for the tooltip: its host (and port, if not
/// the default one).
pub fn server_name(base_url: &str) -> String {
    url::Url::parse(base_url)
        .ok()
        .and_then(|u| {
            let host = u.host_str()?.to_string();
            Some(match u.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        })
        .unwrap_or_else(|| base_url.to_string())
}

/// When to measure and the last result.
#[derive(Debug, Default)]
pub struct Probe {
    last: Option<Duration>,
    in_flight: bool,
    next_at: Option<Instant>,
    /// Changes on every [`Probe::stop`]: answers to older requests are
    /// ignored (a measurement of a previous connection).
    generation: u64,
}

impl Probe {
    /// Last measurement (`None`: unknown).
    pub fn value(&self) -> Option<Duration> {
        self.last
    }

    /// A measurement should start now.
    pub fn due(&self, now: Instant, connected: bool) -> bool {
        connected && !self.in_flight && self.next_at.is_none_or(|t| now >= t)
    }

    /// A measurement starts; its answer goes to [`Probe::finish`] with the
    /// returned value.
    pub fn start(&mut self) -> u64 {
        self.in_flight = true;
        self.generation
    }

    /// Answer of a measurement (`None`: no answer). Returns `false` if it was
    /// for an earlier connection and it is ignored.
    pub fn finish(&mut self, generation: u64, rtt: Option<Duration>, now: Instant) -> bool {
        if generation != self.generation {
            return false;
        }
        self.in_flight = false;
        self.last = rtt;
        self.next_at = Some(now + INTERVAL);
        true
    }

    /// Disconnected or reconnecting: forgets the value and starts over when
    /// connected again.
    pub fn stop(&mut self) {
        if self.in_flight || self.last.is_some() || self.next_at.is_some() {
            self.generation = self.generation.wrapping_add(1);
        }
        self.in_flight = false;
        self.last = None;
        self.next_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn levels_and_text() {
        assert_eq!(level(None), Level::Unknown);
        assert_eq!(level(Some(ms(0))), Level::Good);
        assert_eq!(level(Some(ms(149))), Level::Good);
        assert_eq!(level(Some(ms(150))), Level::Fair);
        assert_eq!(level(Some(ms(399))), Level::Fair);
        assert_eq!(level(Some(ms(400))), Level::Poor);
        assert_eq!(level(Some(ms(5000))), Level::Poor);

        assert_eq!(format(None), "—");
        assert_eq!(format(Some(Duration::from_micros(300))), "<1 ms");
        assert_eq!(format(Some(Duration::from_micros(42_700))), "42 ms");
        assert_eq!(format(Some(ms(1250))), "1250 ms");
    }

    #[test]
    fn server_names() {
        assert_eq!(server_name("https://termoak.com"), "termoak.com");
        assert_eq!(server_name("https://termoak.com:443/"), "termoak.com");
        assert_eq!(server_name("http://192.168.1.5:7733"), "192.168.1.5:7733");
        assert_eq!(server_name("not a url"), "not a url");
    }

    #[test]
    fn measures_every_interval_while_connected() {
        let t0 = Instant::now();
        let mut p = Probe::default();
        assert!(!p.due(t0, false));
        assert!(p.due(t0, true));
        let g = p.start();
        // One at a time.
        assert!(!p.due(t0, true));
        assert!(p.finish(g, Some(ms(42)), t0));
        assert_eq!(p.value(), Some(ms(42)));
        assert!(!p.due(t0 + ms(4999), true));
        assert!(p.due(t0 + INTERVAL, true));
        // No answer: unknown, and it tries again later.
        let g = p.start();
        assert!(p.finish(g, None, t0 + INTERVAL));
        assert_eq!(p.value(), None);
        assert!(!p.due(t0 + INTERVAL, true));
        assert!(p.due(t0 + INTERVAL * 2, true));
        // Paused while disconnected.
        assert!(!p.due(t0 + INTERVAL * 2, false));
    }

    #[test]
    fn answers_from_before_a_reconnect_are_ignored() {
        let t0 = Instant::now();
        let mut p = Probe::default();
        let g = p.start();
        p.stop();
        assert!(!p.finish(g, Some(ms(900)), t0));
        assert_eq!(p.value(), None);
        // Connected again: measures at once.
        assert!(p.due(t0, true));
        let g2 = p.start();
        assert_ne!(g, g2);
        assert!(p.finish(g2, Some(ms(30)), t0));
        assert_eq!(p.value(), Some(ms(30)));
        p.stop();
        assert_eq!(p.value(), None);
        assert!(p.due(t0, true));
        // Stopping again changes nothing.
        let g3 = p.start();
        p.stop();
        p.stop();
        assert!(!p.finish(g3, Some(ms(1)), t0));
    }
}
