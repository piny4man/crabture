#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AreaSelection {
    pub rect: (u32, u32, u32, u32),
    pub surface_size: (u32, u32),
    pub output_name: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureMode {
    Area,
    Window,
    FullScreen,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionCommand {
    SetMode(CaptureMode),
    CaptureArea(AreaSelection),
    Capture,
    Cancel,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionOutcome {
    Continue,
    Cancelled,
    CaptureAreaToClipboard(AreaSelection),
    Unsupported(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureSession {
    mode: CaptureMode,
}

impl Default for CaptureSession {
    fn default() -> Self {
        Self {
            mode: CaptureMode::Area,
        }
    }
}

impl CaptureSession {
    pub fn mode(&self) -> CaptureMode {
        self.mode
    }

    #[cfg(test)]
    pub fn controls() -> [&'static str; 5] {
        ["Area", "Window", "Full Screen", "Capture", "Cancel"]
    }

    pub fn handle(&mut self, command: SessionCommand) -> SessionOutcome {
        match command {
            SessionCommand::SetMode(mode) => {
                self.mode = mode;
                SessionOutcome::Continue
            }
            SessionCommand::CaptureArea(selection) if self.mode == CaptureMode::Area => {
                SessionOutcome::CaptureAreaToClipboard(selection)
            }
            SessionCommand::CaptureArea(_) => {
                SessionOutcome::Unsupported(self.unsupported_message())
            }
            SessionCommand::Cancel => SessionOutcome::Cancelled,
            SessionCommand::Capture if self.mode == CaptureMode::Area => {
                SessionOutcome::Unsupported("Draw an area before capturing.".to_string())
            }
            SessionCommand::Capture => SessionOutcome::Unsupported(self.unsupported_message()),
        }
    }

    fn unsupported_message(&self) -> String {
        match self.mode {
            CaptureMode::Area => "Area capture from the graphical toolbar is not implemented yet. Use --select for direct area capture.",
            CaptureMode::Window => "Window capture from the graphical toolbar is not implemented yet.",
            CaptureMode::FullScreen => "Full screen capture from the graphical toolbar is not implemented yet. Use --instant for direct full-screen capture.",
        }
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_phase_one_toolbar_controls() {
        assert_eq!(
            CaptureSession::controls(),
            ["Area", "Window", "Full Screen", "Capture", "Cancel"]
        );
    }

    #[test]
    fn mode_changes_continue_session() {
        let mut session = CaptureSession::default();

        assert_eq!(
            session.handle(SessionCommand::SetMode(CaptureMode::Window)),
            SessionOutcome::Continue
        );
        assert_eq!(session.mode(), CaptureMode::Window);
    }

    #[test]
    fn area_capture_requests_clipboard_output() {
        let mut session = CaptureSession::default();
        let selection = AreaSelection {
            rect: (10, 20, 30, 40),
            surface_size: (800, 600),
            output_name: Some("eDP-1".to_string()),
        };

        assert_eq!(
            session.handle(SessionCommand::CaptureArea(selection.clone())),
            SessionOutcome::CaptureAreaToClipboard(selection)
        );
    }

    #[test]
    fn area_capture_requires_a_drawn_selection() {
        let mut session = CaptureSession::default();

        assert_eq!(
            session.handle(SessionCommand::Capture),
            SessionOutcome::Unsupported("Draw an area before capturing.".to_string())
        );
    }

    #[test]
    fn unsupported_modes_still_report_helpful_feedback() {
        let mut session = CaptureSession::default();

        session.handle(SessionCommand::SetMode(CaptureMode::Window));
        assert_eq!(
            session.handle(SessionCommand::Capture),
            SessionOutcome::Unsupported(
                "Window capture from the graphical toolbar is not implemented yet.".to_string()
            )
        );
    }
}
