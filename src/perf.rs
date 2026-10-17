//! `CRABTURE_PERF` end-to-end latency probe.
//!
//! hotpath (see docs/hotpath-profiling.md) explains where time goes inside an
//! open or a frame; this probe measures what the user sees: process start to
//! the first presented overlay frame, and an input to the first presented
//! frame that shows its effect. With `CRABTURE_PERF=1` the overlay prints one
//! stderr line per sample:
//!
//! ```text
//! crabture-perf <metric> <surface> <milliseconds>
//! ```
//!
//! `scripts/perf-summary.sh` turns a log of these into median / p95 / max.
//! Unset (or `0`), the probe does nothing.

use std::ffi::OsStr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Which latency target an input counts toward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// Drawing, moving, or resizing the area selection in the HUD.
    AreaDrag,
    /// Drawing the selection in the legacy `--select` overlay.
    SelectDrag,
    /// Moving over windows in window mode (highlight or crosshair).
    WindowHighlight,
    /// Moving onto or off a toolbar button.
    HudHover,
    /// Clicking a toolbar button that keeps the HUD open.
    HudPress,
    /// A keyboard shortcut that keeps the HUD open (mode, output, format...).
    Key,
}

impl InputKind {
    /// The metric name this input's latency is reported under.
    pub fn metric(self) -> &'static str {
        match self {
            Self::AreaDrag => "area-drag-input-to-present",
            Self::SelectDrag => "select-drag-input-to-present",
            Self::WindowHighlight => "window-highlight-input-to-present",
            Self::HudHover => "hud-hover-input-to-present",
            Self::HudPress => "hud-press-input-to-present",
            Self::Key => "key-input-to-present",
        }
    }

    /// The metric name for an input that no visible change answered.
    pub fn unanswered_metric(self) -> &'static str {
        match self {
            Self::AreaDrag => "area-drag-input-unanswered",
            Self::SelectDrag => "select-drag-input-unanswered",
            Self::WindowHighlight => "window-highlight-input-unanswered",
            Self::HudHover => "hud-hover-input-unanswered",
            Self::HudPress => "hud-press-input-unanswered",
            Self::Key => "key-input-unanswered",
        }
    }
}

/// Whether a `CRABTURE_PERF` value turns the probe on: any non-empty value
/// other than `0`.
pub fn env_enabled(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

/// Whether the probe is on for this process (`CRABTURE_PERF`, read once).
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_enabled(std::env::var_os("CRABTURE_PERF").as_deref()))
}

static START: OnceLock<Instant> = OnceLock::new();

/// Record process start. Called first thing in `main`; later calls are
/// ignored.
pub fn mark_start() {
    START.get_or_init(Instant::now);
}

/// When the process started (or first asked, if `mark_start` never ran).
pub fn start() -> Instant {
    *START.get_or_init(Instant::now)
}

/// Format one sample line.
pub fn line(metric: &str, surface: &str, elapsed: Duration) -> String {
    format!(
        "crabture-perf {metric} {surface} {:.3}",
        elapsed.as_secs_f64() * 1000.0
    )
}

/// Print one sample line to stderr.
pub fn emit(metric: &str, surface: &str, elapsed: Duration) {
    eprintln!("{}", line(metric, surface, elapsed));
}

/// How long an input may go unanswered before the next input replaces it.
pub const INPUT_EXPIRY: Duration = Duration::from_millis(50);

/// Tracks the oldest input not yet shown on screen.
///
/// Only a committed frame with a visible change answers an input. Inputs that
/// arrive within [`INPUT_EXPIRY`] of the pending one are folded into it, so a
/// burst (pointer motion, key repeat) is measured from its first event. An
/// input that comes later replaces a pending one instead: that input changed
/// nothing on screen, and is reported as unanswered rather than timed to the
/// next input's frame.
#[derive(Debug, Default)]
pub struct InputProbe {
    pending: Option<(InputKind, Instant)>,
}

impl InputProbe {
    /// Record an input at `at`. A pending input older than [`INPUT_EXPIRY`]
    /// changed nothing on screen; it is returned with its age and this input
    /// starts a new sample.
    pub fn input(&mut self, kind: InputKind, at: Instant) -> Option<(InputKind, Duration)> {
        match self.pending {
            Some((_, since)) if at.saturating_duration_since(since) <= INPUT_EXPIRY => None,
            expired => {
                self.pending = Some((kind, at));
                expired.map(|(old, since)| (old, at.saturating_duration_since(since)))
            }
        }
    }

    /// A frame with a visible change is being committed: hand over the
    /// pending input, to be timed when that frame is presented.
    pub fn take(&mut self) -> Option<(InputKind, Instant)> {
        self.pending.take()
    }
}

/// Tracks the first presented frame, and the first one that carries the
/// overlay's content.
///
/// The HUD draws one surface per output, but only the output under the
/// pointer composites the toolbar; a tint-only frame on another output is
/// `first-present`, not content.
#[derive(Debug, Default)]
pub struct OpenProbe {
    presented: bool,
    content: bool,
}

/// What a presented frame completed, as latencies since process start.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OpenSamples {
    pub first_present: Option<Duration>,
    pub first_content_present: Option<Duration>,
}

impl OpenProbe {
    /// A frame was presented `since_start` after process start; `content`
    /// says whether it drew the overlay's content.
    pub fn presented(&mut self, content: bool, since_start: Duration) -> OpenSamples {
        let mut samples = OpenSamples::default();
        if !self.presented {
            self.presented = true;
            samples.first_present = Some(since_start);
        }
        if content && !self.content {
            self.content = true;
            samples.first_content_present = Some(since_start);
        }
        samples
    }

    /// Whether content has been presented yet.
    pub fn done(&self) -> bool {
        self.content
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [InputKind; 6] = [
        InputKind::AreaDrag,
        InputKind::SelectDrag,
        InputKind::WindowHighlight,
        InputKind::HudHover,
        InputKind::HudPress,
        InputKind::Key,
    ];

    #[test]
    fn env_value_enables_unless_empty_or_zero() {
        assert!(!env_enabled(None));
        assert!(!env_enabled(Some(OsStr::new(""))));
        assert!(!env_enabled(Some(OsStr::new("0"))));
        assert!(env_enabled(Some(OsStr::new("1"))));
        assert!(env_enabled(Some(OsStr::new("yes"))));
    }

    #[test]
    fn line_is_stable() {
        assert_eq!(
            line(
                InputKind::AreaDrag.metric(),
                "hud",
                Duration::from_micros(12_345)
            ),
            "crabture-perf area-drag-input-to-present hud 12.345"
        );
    }

    #[test]
    fn every_input_kind_has_its_own_metrics() {
        let names: Vec<_> = ALL.iter().map(|k| k.metric()).collect();
        assert_eq!(
            names,
            [
                "area-drag-input-to-present",
                "select-drag-input-to-present",
                "window-highlight-input-to-present",
                "hud-hover-input-to-present",
                "hud-press-input-to-present",
                "key-input-to-present",
            ]
        );
        for kind in ALL {
            let unanswered = kind.unanswered_metric();
            assert!(unanswered.ends_with("-input-unanswered"), "{unanswered}");
            assert_eq!(
                unanswered.trim_end_matches("-unanswered"),
                kind.metric().trim_end_matches("-to-present")
            );
        }
    }

    #[test]
    fn a_burst_is_measured_from_its_first_input() {
        let t0 = Instant::now();
        let mut probe = InputProbe::default();
        assert_eq!(probe.take(), None);

        probe.input(InputKind::AreaDrag, t0);
        probe.input(InputKind::AreaDrag, t0 + Duration::from_millis(8));
        assert_eq!(probe.take(), Some((InputKind::AreaDrag, t0)));
        // Consumed: a later frame with no new input answers nothing.
        assert_eq!(probe.take(), None);
    }

    #[test]
    fn an_unanswered_input_expires_when_the_next_one_comes_later() {
        let t0 = Instant::now();
        let mut probe = InputProbe::default();
        assert_eq!(probe.input(InputKind::Key, t0), None);

        let next = t0 + Duration::from_millis(600);
        assert_eq!(
            probe.input(InputKind::HudHover, next),
            Some((InputKind::Key, Duration::from_millis(600)))
        );
        assert_eq!(probe.take(), Some((InputKind::HudHover, next)));
    }

    #[test]
    fn inputs_within_the_expiry_still_fold() {
        let t0 = Instant::now();
        let mut probe = InputProbe::default();
        probe.input(InputKind::WindowHighlight, t0);
        assert_eq!(probe.input(InputKind::HudHover, t0 + INPUT_EXPIRY), None);
        assert_eq!(probe.take(), Some((InputKind::WindowHighlight, t0)));
    }

    #[test]
    fn open_reports_first_present_and_first_content_once_each() {
        let mut probe = OpenProbe::default();
        let ms = Duration::from_millis;

        // A tint-only frame on another output: presented, but no content.
        assert_eq!(
            probe.presented(false, ms(80)),
            OpenSamples {
                first_present: Some(ms(80)),
                first_content_present: None,
            }
        );
        assert!(!probe.done());

        assert_eq!(
            probe.presented(true, ms(85)),
            OpenSamples {
                first_present: None,
                first_content_present: Some(ms(85)),
            }
        );
        assert!(probe.done());
        assert_eq!(probe.presented(true, ms(90)), OpenSamples::default());
    }

    #[test]
    fn a_first_frame_with_content_reports_both() {
        let mut probe = OpenProbe::default();
        let at = Duration::from_millis(42);
        assert_eq!(
            probe.presented(true, at),
            OpenSamples {
                first_present: Some(at),
                first_content_present: Some(at),
            }
        );
    }
}
