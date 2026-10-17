//! Ordering for the HUD open path.
//!
//! The freeze capture and the window enumeration run on background threads
//! while the main thread loads the appearance and sets up the Wayland
//! surfaces.  The overlay must not show anything until the capture has
//! returned, or the toolbar could end up in the screenshot.  [`OpenGate`]
//! enforces that: attaching an overlay buffer takes a [`CaptureDone`] proof,
//! and the only way to get one is [`OpenGate::wait`].

use std::sync::mpsc;
use std::thread::JoinHandle;

/// A window as a global logical rectangle.
pub type WindowRect = (i32, i32, u32, u32);

/// Proof that the freeze capture has returned successfully.  Only
/// [`OpenGate::wait`] creates one.
#[derive(Clone, Copy, Debug)]
pub struct CaptureDone(());

/// Reports the end of the freeze capture to its [`OpenGate`].  Dropping it
/// without [`finish`](Self::finish) (e.g. the capture thread panicked) counts
/// as a failed capture.
pub struct CaptureSignal(mpsc::Sender<bool>);

impl CaptureSignal {
    pub fn finish(self, ok: bool) {
        let _ = self.0.send(ok);
    }
}

enum Capture {
    Pending(mpsc::Receiver<bool>),
    Done,
    Failed,
}

enum WindowRects {
    Pending(JoinHandle<Vec<WindowRect>>),
    Ready(Vec<WindowRect>),
    Taken,
}

/// Holds the overlay's first commit until the open path's background work
/// has finished.
pub struct OpenGate {
    capture: Capture,
    window_rects: WindowRects,
}

impl OpenGate {
    /// A gate that opens once the returned signal finishes.
    pub fn new() -> (Self, CaptureSignal) {
        let (tx, rx) = mpsc::channel();
        (
            Self {
                capture: Capture::Pending(rx),
                window_rects: WindowRects::Ready(Vec::new()),
            },
            CaptureSignal(tx),
        )
    }

    /// A gate with no capture to wait for (the legacy `--select` overlay).
    pub fn open(window_rects: Vec<WindowRect>) -> Self {
        Self {
            capture: Capture::Done,
            window_rects: WindowRects::Ready(window_rects),
        }
    }

    /// Hand the gate the window enumeration running in the background; it is
    /// joined when the gate opens.
    pub fn set_window_rects(&mut self, handle: JoinHandle<Vec<WindowRect>>) {
        self.window_rects = WindowRects::Pending(handle);
    }

    /// Block until the capture has returned.  `None` when it failed; the
    /// overlay must then exit without showing anything.
    #[hotpath::measure(impl_type = "OpenGate")]
    pub fn wait(&mut self) -> Option<CaptureDone> {
        if let Capture::Pending(rx) = &self.capture {
            self.capture = match rx.recv() {
                Ok(true) => Capture::Done,
                Ok(false) | Err(_) => Capture::Failed,
            };
        }
        if !matches!(self.capture, Capture::Done) {
            return None;
        }
        if let WindowRects::Pending(_) = self.window_rects {
            let WindowRects::Pending(handle) =
                std::mem::replace(&mut self.window_rects, WindowRects::Taken)
            else {
                unreachable!()
            };
            // Best-effort, like the enumeration itself: no windows, no highlight.
            self.window_rects = WindowRects::Ready(handle.join().unwrap_or_default());
        }
        Some(CaptureDone(()))
    }

    /// True once [`wait`](Self::wait) has seen the capture fail.
    pub fn failed(&self) -> bool {
        matches!(self.capture, Capture::Failed)
    }

    /// The enumerated windows, once, after the gate has opened.
    pub fn take_window_rects(&mut self) -> Option<Vec<WindowRect>> {
        if !matches!(self.capture, Capture::Done)
            || matches!(self.window_rects, WindowRects::Pending(_))
        {
            return None;
        }
        match std::mem::replace(&mut self.window_rects, WindowRects::Taken) {
            WindowRects::Ready(rects) => Some(rects),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn wait_blocks_until_the_capture_returns() {
        let (mut gate, signal) = OpenGate::new();
        let start = Instant::now();
        let capture = thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            signal.finish(true);
        });
        assert!(gate.wait().is_some());
        assert!(start.elapsed() >= Duration::from_millis(60));
        assert!(!gate.failed());
        capture.join().unwrap();
        // Later frames do not wait again.
        assert!(gate.wait().is_some());
    }

    #[test]
    fn a_failed_capture_never_opens_the_gate() {
        let (mut gate, signal) = OpenGate::new();
        signal.finish(false);
        assert!(gate.wait().is_none());
        assert!(gate.failed());
        assert!(gate.wait().is_none());
    }

    #[test]
    fn a_capture_thread_that_dies_counts_as_failed() {
        let (mut gate, signal) = OpenGate::new();
        drop(signal);
        assert!(gate.wait().is_none());
        assert!(gate.failed());
    }

    #[test]
    fn window_rects_are_handed_over_once_the_gate_opens() {
        let (mut gate, signal) = OpenGate::new();
        gate.set_window_rects(thread::spawn(|| vec![(1, 2, 3, 4)]));
        assert_eq!(gate.take_window_rects(), None, "not before the gate opens");
        signal.finish(true);
        gate.wait().unwrap();
        assert_eq!(gate.take_window_rects(), Some(vec![(1, 2, 3, 4)]));
        assert_eq!(gate.take_window_rects(), None);
    }

    #[test]
    fn an_open_gate_does_not_wait() {
        let mut gate = OpenGate::open(vec![(0, 0, 10, 10)]);
        assert!(gate.wait().is_some());
        assert_eq!(gate.take_window_rects(), Some(vec![(0, 0, 10, 10)]));
    }
}
