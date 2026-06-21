#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AreaSelection {
    pub rect: (u32, u32, u32, u32),
    pub surface_size: (u32, u32),
    pub output_name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FullScreenSelection {
    pub output_name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowSelection {
    pub point: (u32, u32),
    pub surface_size: (u32, u32),
    pub output_name: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputDestination {
    Clipboard,
    Save,
    CopyAndSave,
}

impl OutputDestination {
    pub fn next(self) -> Self {
        match self {
            Self::Clipboard => Self::Save,
            Self::Save => Self::CopyAndSave,
            Self::CopyAndSave => Self::Clipboard,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clipboard => "clipboard",
            Self::Save => "save",
            Self::CopyAndSave => "copy_and_save",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "clipboard" => Some(Self::Clipboard),
            "save" => Some(Self::Save),
            "copy_and_save" => Some(Self::CopyAndSave),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphicalFormat {
    Png,
    Jpg,
}

impl GraphicalFormat {
    pub fn next(self) -> Self {
        match self {
            Self::Png => Self::Jpg,
            Self::Jpg => Self::Png,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpg => "jpg",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpg),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SaveLocationChoice {
    Screenshots,
    CurrentDirectory,
}

impl SaveLocationChoice {
    pub fn next(self) -> Self {
        match self {
            Self::Screenshots => Self::CurrentDirectory,
            Self::CurrentDirectory => Self::Screenshots,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Screenshots => "screenshots",
            Self::CurrentDirectory => "current_directory",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "screenshots" => Some(Self::Screenshots),
            "current_directory" => Some(Self::CurrentDirectory),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphicalPreferences {
    pub output: OutputDestination,
    pub format: GraphicalFormat,
    pub location: SaveLocationChoice,
    pub mode: CaptureMode,
}

impl Default for GraphicalPreferences {
    fn default() -> Self {
        Self {
            output: OutputDestination::Clipboard,
            format: GraphicalFormat::Png,
            location: SaveLocationChoice::Screenshots,
            mode: CaptureMode::Area,
        }
    }
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
    SetOutput(OutputDestination),
    SetFormat(GraphicalFormat),
    SetLocation(SaveLocationChoice),
    CaptureArea(AreaSelection, GraphicalPreferences),
    CaptureWindow(WindowSelection, GraphicalPreferences),
    CaptureFullScreen(FullScreenSelection, GraphicalPreferences),
    Capture,
    Cancel,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionOutcome {
    Continue,
    Cancelled,
    CaptureArea(AreaSelection, GraphicalPreferences),
    CaptureWindow(WindowSelection, GraphicalPreferences),
    CaptureFullScreen(FullScreenSelection, GraphicalPreferences),
    Unsupported(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CaptureSession {
    preferences: GraphicalPreferences,
}

impl CaptureSession {
    pub fn with_preferences(preferences: GraphicalPreferences) -> Self {
        Self { preferences }
    }

    pub fn preferences(&self) -> GraphicalPreferences {
        self.preferences
    }

    #[cfg(test)]
    pub fn controls() -> [&'static str; 8] {
        [
            "Area",
            "Window",
            "Full Screen",
            "Output",
            "Location",
            "Format",
            "Capture",
            "Cancel",
        ]
    }

    pub fn handle(&mut self, command: SessionCommand) -> SessionOutcome {
        match command {
            SessionCommand::SetMode(mode) => {
                self.preferences.mode = mode;
                SessionOutcome::Continue
            }
            SessionCommand::SetOutput(output) => {
                self.preferences.output = output;
                SessionOutcome::Continue
            }
            SessionCommand::SetFormat(format) => {
                self.preferences.format = format;
                SessionOutcome::Continue
            }
            SessionCommand::SetLocation(location) => {
                self.preferences.location = location;
                SessionOutcome::Continue
            }
            SessionCommand::CaptureArea(selection, preferences)
                if preferences.mode == CaptureMode::Area =>
            {
                self.preferences = preferences;
                SessionOutcome::CaptureArea(selection, preferences)
            }
            SessionCommand::CaptureArea(_, _) => {
                SessionOutcome::Unsupported(self.unsupported_message())
            }
            SessionCommand::CaptureWindow(selection, preferences)
                if preferences.mode == CaptureMode::Window =>
            {
                self.preferences = preferences;
                SessionOutcome::CaptureWindow(selection, preferences)
            }
            SessionCommand::CaptureWindow(_, _) => {
                SessionOutcome::Unsupported(self.unsupported_message())
            }
            SessionCommand::CaptureFullScreen(selection, preferences)
                if preferences.mode == CaptureMode::FullScreen =>
            {
                self.preferences = preferences;
                SessionOutcome::CaptureFullScreen(selection, preferences)
            }
            SessionCommand::CaptureFullScreen(_, _) => {
                SessionOutcome::Unsupported(self.unsupported_message())
            }
            SessionCommand::Cancel => SessionOutcome::Cancelled,
            SessionCommand::Capture if self.preferences.mode == CaptureMode::Area => {
                SessionOutcome::Unsupported("Draw an area before capturing.".to_string())
            }
            SessionCommand::Capture => SessionOutcome::Unsupported(self.unsupported_message()),
        }
    }

    fn unsupported_message(&self) -> String {
        match self.preferences.mode {
            CaptureMode::Area => "Area capture from the graphical toolbar is not implemented yet. Use --select for direct area capture.",
            CaptureMode::Window => "Click a window before capturing.",
            CaptureMode::FullScreen => "Full screen capture could not determine a target display.",
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
            [
                "Area",
                "Window",
                "Full Screen",
                "Output",
                "Location",
                "Format",
                "Capture",
                "Cancel",
            ]
        );
    }

    #[test]
    fn mode_changes_continue_session() {
        let mut session = CaptureSession::default();

        assert_eq!(
            session.handle(SessionCommand::SetMode(CaptureMode::Window)),
            SessionOutcome::Continue
        );
        assert_eq!(session.preferences().mode, CaptureMode::Window);
    }

    #[test]
    fn area_capture_requests_clipboard_output() {
        let mut session = CaptureSession::default();
        let selection = AreaSelection {
            rect: (10, 20, 30, 40),
            surface_size: (800, 600),
            output_name: Some("eDP-1".to_string()),
        };
        let preferences = session.preferences();

        assert_eq!(
            session.handle(SessionCommand::CaptureArea(selection.clone(), preferences)),
            SessionOutcome::CaptureArea(selection, preferences)
        );
    }

    #[test]
    fn full_screen_capture_requests_current_output() {
        let mut session = CaptureSession::with_preferences(GraphicalPreferences {
            mode: CaptureMode::FullScreen,
            ..GraphicalPreferences::default()
        });
        let selection = FullScreenSelection {
            output_name: Some("eDP-1".to_string()),
        };
        let preferences = session.preferences();

        assert_eq!(
            session.handle(SessionCommand::CaptureFullScreen(
                selection.clone(),
                preferences
            )),
            SessionOutcome::CaptureFullScreen(selection, preferences)
        );
    }

    #[test]
    fn window_capture_requests_target_point() {
        let mut session = CaptureSession::with_preferences(GraphicalPreferences {
            mode: CaptureMode::Window,
            ..GraphicalPreferences::default()
        });
        let selection = WindowSelection {
            point: (120, 80),
            surface_size: (800, 600),
            output_name: Some("eDP-1".to_string()),
        };
        let preferences = session.preferences();

        assert_eq!(
            session.handle(SessionCommand::CaptureWindow(
                selection.clone(),
                preferences
            )),
            SessionOutcome::CaptureWindow(selection, preferences)
        );
    }

    #[test]
    fn window_capture_uses_overlay_preferences_after_toolbar_mode_change() {
        let mut session = CaptureSession::default();
        let preferences = GraphicalPreferences {
            mode: CaptureMode::Window,
            ..session.preferences()
        };
        let selection = WindowSelection {
            point: (120, 80),
            surface_size: (800, 600),
            output_name: Some("eDP-1".to_string()),
        };

        assert_eq!(
            session.handle(SessionCommand::CaptureWindow(
                selection.clone(),
                preferences
            )),
            SessionOutcome::CaptureWindow(selection, preferences)
        );
    }

    #[test]
    fn full_screen_capture_uses_overlay_preferences_after_toolbar_mode_change() {
        let mut session = CaptureSession::default();
        let preferences = GraphicalPreferences {
            mode: CaptureMode::FullScreen,
            ..session.preferences()
        };
        let selection = FullScreenSelection {
            output_name: Some("eDP-1".to_string()),
        };

        assert_eq!(
            session.handle(SessionCommand::CaptureFullScreen(
                selection.clone(),
                preferences
            )),
            SessionOutcome::CaptureFullScreen(selection, preferences)
        );
    }

    #[test]
    fn output_format_and_location_changes_continue_session() {
        let mut session = CaptureSession::default();

        assert_eq!(
            session.handle(SessionCommand::SetOutput(OutputDestination::Save)),
            SessionOutcome::Continue
        );
        assert_eq!(
            session.handle(SessionCommand::SetFormat(GraphicalFormat::Jpg)),
            SessionOutcome::Continue
        );
        assert_eq!(
            session.handle(SessionCommand::SetLocation(
                SaveLocationChoice::CurrentDirectory
            )),
            SessionOutcome::Continue
        );

        assert_eq!(session.preferences().output, OutputDestination::Save);
        assert_eq!(session.preferences().format, GraphicalFormat::Jpg);
        assert_eq!(
            session.preferences().location,
            SaveLocationChoice::CurrentDirectory
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
            SessionOutcome::Unsupported("Click a window before capturing.".to_string())
        );
    }
}
