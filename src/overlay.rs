//! Wayland layer-shell overlay for interactive area selection.
//!
//! Uses smithay-client-toolkit to create a fullscreen overlay surface on the
//! `Overlay` layer.  A semi-transparent dark tint covers the screen; the
//! selected rectangle is shown as a clear (transparent) area with a white
//! border so the user can see the content underneath.
//! Pointer click-drag selects the area; Escape or right-click cancels.

use crate::render;
use crate::session::{
    AreaSelection, CaptureMode, FullScreenSelection, GraphicalPreferences, SessionCommand,
    WindowSelection,
};
use anyhow::{Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{
            PointerEvent, PointerEventKind, PointerHandler, cursor_shape::CursorShapeManager,
        },
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure, SurfaceKind,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    Shape, WpCursorShapeDeviceV1,
};

/// Dark overlay: premultiplied ARGB, ~60% opacity black.
/// In ARGB8888 little-endian (BGRA byte order): B=0, G=0, R=0, A=0x99.
const OVERLAY_PIXEL: u32 = 0x9900_0000;
/// Selection area: fully transparent so desktop shows through.
const CLEAR_PIXEL: u32 = 0x0000_0000;
/// Border: solid white, premultiplied.
const BORDER_PIXEL: u32 = 0xFFFF_FFFF;
/// Window-highlight fill: accent blue (10,132,255) at ~22% opacity,
/// premultiplied ARGB8888-LE.  Composited over the desktop so the hovered
/// window gets a translucent blue tint.
const HIGHLIGHT_FILL_PIXEL: u32 = 0x3802_1D38;
/// Window-highlight border: solid accent blue, premultiplied ARGB8888-LE.
const HIGHLIGHT_BORDER_PIXEL: u32 = 0xFF0A_84FF;

/// A rectangle in logical surface coordinates: (x, y, width, height).
pub type SelectionRect = (u32, u32, u32, u32);

/// Result of the overlay: an optional selection rectangle, the surface
/// dimensions `(width, height)`, and the name of the output the overlay was on.
pub type OverlayResult = (Option<SelectionRect>, (u32, u32), Option<String>);

pub fn selection_rect_from_drag(start: (f64, f64), current: (f64, f64)) -> SelectionRect {
    let x1 = start.0.min(current.0).round() as u32;
    let y1 = start.1.min(current.1).round() as u32;
    let x2 = start.0.max(current.0).round() as u32;
    let y2 = start.1.max(current.1).round() as u32;

    (x1, y1, x2.saturating_sub(x1), y2.saturating_sub(y1))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResizeHandle {
    North,
    South,
    East,
    West,
    NorthEast,
    NorthWest,
    SouthEast,
    SouthWest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AreaDrag {
    Drawing,
    Moving {
        original: SelectionRect,
        pointer_start: (i32, i32),
    },
    Resizing {
        original: SelectionRect,
        handle: ResizeHandle,
    },
}

const MIN_SELECTION_SIZE: u32 = 8;
const HANDLE_HIT_SIZE: i32 = 10;

fn clamp_selection_rect(rect: SelectionRect, surface_size: (u32, u32)) -> SelectionRect {
    let (surface_w, surface_h) = surface_size;
    let w = rect.2.max(MIN_SELECTION_SIZE).min(surface_w.max(1));
    let h = rect.3.max(MIN_SELECTION_SIZE).min(surface_h.max(1));
    let x = rect.0.min(surface_w.saturating_sub(w));
    let y = rect.1.min(surface_h.saturating_sub(h));

    (x, y, w, h)
}

fn move_selection_rect(
    rect: SelectionRect,
    delta: (i32, i32),
    surface_size: (u32, u32),
) -> SelectionRect {
    let x = (rect.0 as i32 + delta.0).clamp(0, surface_size.0.saturating_sub(rect.2) as i32) as u32;
    let y = (rect.1 as i32 + delta.1).clamp(0, surface_size.1.saturating_sub(rect.3) as i32) as u32;

    (x, y, rect.2, rect.3)
}

fn resize_selection_rect(
    rect: SelectionRect,
    handle: ResizeHandle,
    pointer: (i32, i32),
    surface_size: (u32, u32),
) -> SelectionRect {
    let left = rect.0 as i32;
    let top = rect.1 as i32;
    let right = rect.0 as i32 + rect.2 as i32;
    let bottom = rect.1 as i32 + rect.3 as i32;
    let min = MIN_SELECTION_SIZE as i32;

    let mut new_left = left;
    let mut new_top = top;
    let mut new_right = right;
    let mut new_bottom = bottom;

    match handle {
        ResizeHandle::North => new_top = pointer.1.clamp(0, bottom - min),
        ResizeHandle::South => new_bottom = pointer.1.clamp(top + min, surface_size.1 as i32),
        ResizeHandle::East => new_right = pointer.0.clamp(left + min, surface_size.0 as i32),
        ResizeHandle::West => new_left = pointer.0.clamp(0, right - min),
        ResizeHandle::NorthEast => {
            new_top = pointer.1.clamp(0, bottom - min);
            new_right = pointer.0.clamp(left + min, surface_size.0 as i32);
        }
        ResizeHandle::NorthWest => {
            new_top = pointer.1.clamp(0, bottom - min);
            new_left = pointer.0.clamp(0, right - min);
        }
        ResizeHandle::SouthEast => {
            new_bottom = pointer.1.clamp(top + min, surface_size.1 as i32);
            new_right = pointer.0.clamp(left + min, surface_size.0 as i32);
        }
        ResizeHandle::SouthWest => {
            new_bottom = pointer.1.clamp(top + min, surface_size.1 as i32);
            new_left = pointer.0.clamp(0, right - min);
        }
    }

    clamp_selection_rect(
        (
            new_left as u32,
            new_top as u32,
            (new_right - new_left) as u32,
            (new_bottom - new_top) as u32,
        ),
        surface_size,
    )
}

fn hit_resize_handle(rect: SelectionRect, point: (i32, i32)) -> Option<ResizeHandle> {
    let left = rect.0 as i32;
    let top = rect.1 as i32;
    let right = rect.0 as i32 + rect.2 as i32;
    let bottom = rect.1 as i32 + rect.3 as i32;
    let near = |value: i32, target: i32| (value - target).abs() <= HANDLE_HIT_SIZE;
    let inside_x = point.0 >= left - HANDLE_HIT_SIZE && point.0 <= right + HANDLE_HIT_SIZE;
    let inside_y = point.1 >= top - HANDLE_HIT_SIZE && point.1 <= bottom + HANDLE_HIT_SIZE;

    match (
        near(point.0, left),
        near(point.0, right),
        near(point.1, top),
        near(point.1, bottom),
    ) {
        (true, _, true, _) => Some(ResizeHandle::NorthWest),
        (_, true, true, _) => Some(ResizeHandle::NorthEast),
        (true, _, _, true) => Some(ResizeHandle::SouthWest),
        (_, true, _, true) => Some(ResizeHandle::SouthEast),
        (_, _, true, _) if inside_x => Some(ResizeHandle::North),
        (_, _, _, true) if inside_x => Some(ResizeHandle::South),
        (true, _, _, _) if inside_y => Some(ResizeHandle::West),
        (_, true, _, _) if inside_y => Some(ResizeHandle::East),
        _ => None,
    }
}

fn point_in_rect(rect: SelectionRect, point: (i32, i32)) -> bool {
    point.0 >= rect.0 as i32
        && point.0 <= (rect.0 + rect.2) as i32
        && point.1 >= rect.1 as i32
        && point.1 <= (rect.1 + rect.3) as i32
}

fn capture_area_command(
    rect: SelectionRect,
    surface_size: (u32, u32),
    output_name: Option<String>,
    preferences: GraphicalPreferences,
) -> SessionCommand {
    SessionCommand::CaptureArea(
        AreaSelection {
            rect,
            surface_size,
            output_name,
        },
        preferences,
    )
}

fn capture_full_screen_command(
    output_name: Option<String>,
    preferences: GraphicalPreferences,
) -> SessionCommand {
    SessionCommand::CaptureFullScreen(FullScreenSelection { output_name }, preferences)
}

fn capture_window_command(
    point: (u32, u32),
    surface_size: (u32, u32),
    output_name: Option<String>,
    preferences: GraphicalPreferences,
) -> SessionCommand {
    SessionCommand::CaptureWindow(
        WindowSelection {
            point,
            surface_size,
            output_name,
        },
        preferences,
    )
}

fn mode_selection_command(
    mode: CaptureMode,
    output_name: Option<String>,
    preferences: GraphicalPreferences,
) -> SessionCommand {
    let preferences = GraphicalPreferences {
        mode,
        ..preferences
    };

    match mode {
        CaptureMode::Area | CaptureMode::Window => SessionCommand::SetMode(mode),
        CaptureMode::FullScreen => capture_full_screen_command(output_name, preferences),
    }
}

fn confirmed_capture_command(
    preferences: GraphicalPreferences,
    selection: Option<SelectionRect>,
    window_target: Option<(u32, u32)>,
    surface_size: (u32, u32),
    output_name: Option<String>,
) -> SessionCommand {
    match preferences.mode {
        CaptureMode::Area => selection.map_or(SessionCommand::Capture, |rect| {
            capture_area_command(rect, surface_size, output_name, preferences)
        }),
        CaptureMode::Window => window_target.map_or(SessionCommand::Capture, |point| {
            capture_window_command(point, surface_size, output_name, preferences)
        }),
        CaptureMode::FullScreen => capture_full_screen_command(output_name, preferences),
    }
}

fn shortcut_session_command(
    keysym: Keysym,
    output_name: Option<String>,
    preferences: GraphicalPreferences,
) -> Option<SessionCommand> {
    match keysym {
        Keysym::a | Keysym::A => Some(mode_selection_command(
            CaptureMode::Area,
            output_name,
            preferences,
        )),
        Keysym::w | Keysym::W => Some(mode_selection_command(
            CaptureMode::Window,
            output_name,
            preferences,
        )),
        Keysym::f | Keysym::F => Some(mode_selection_command(
            CaptureMode::FullScreen,
            output_name,
            preferences,
        )),
        Keysym::o | Keysym::O => Some(SessionCommand::SetOutput(preferences.output.next())),
        Keysym::l | Keysym::L => Some(SessionCommand::SetLocation(preferences.location.next())),
        Keysym::p | Keysym::P => Some(SessionCommand::SetFormat(preferences.format.next())),
        _ => None,
    }
}

/// Run the fullscreen overlay and return `(selection, (surface_w, surface_h), output_name)`.
/// `selection` is `Some((x, y, w, h))` in logical surface coordinates, or
/// `None` if cancelled.  `output_name` identifies which monitor the overlay
/// appeared on (e.g. `"eDP-1"`).
pub fn run_selection_overlay() -> Result<OverlayResult> {
    let conn = Connection::connect_to_env().context("failed to connect to Wayland")?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).context("failed to initialise Wayland registry")?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("wlr-layer-shell not available")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;

    // Optional: cursor shape protocol (not all compositors support it).
    let cursor_shape_manager = CursorShapeManager::bind(&globals, &qh).ok();

    let mut state = OverlayState {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        compositor,
        layer_shell,
        keyboard: None,
        pointer: None,
        cursor_shape_manager,
        cursor_shape_device: None,
        overlays: Vec::new(),
        active_surface: None,
        window_rects: Vec::new(),
        hud_active: false,
        preferences: GraphicalPreferences::default(),
        hud_result: None,
        selection_result: None,
        exit: false,
        cancelled: false,
    };

    // Output geometry isn't available synchronously: pump the queue once so the
    // registry/xdg-output events populate OutputState before we place surfaces.
    event_queue
        .roundtrip(&mut state)
        .context("Wayland roundtrip error")?;
    state.create_overlays(&qh, "crabture-select");

    loop {
        event_queue
            .blocking_dispatch(&mut state)
            .context("Wayland dispatch error")?;
        if state.exit {
            break;
        }
    }

    let result = if state.cancelled {
        (None, (0, 0), None)
    } else {
        state
            .selection_result
            .clone()
            .unwrap_or((None, (0, 0), None))
    };
    state.teardown(&conn);
    event_queue.roundtrip(&mut state).ok();

    Ok(result)
}

pub fn run_screenshot_hud(
    default_preferences: GraphicalPreferences,
    window_rects: Vec<(i32, i32, u32, u32)>,
) -> Result<SessionCommand> {
    let conn = Connection::connect_to_env().context("failed to connect to Wayland")?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).context("failed to initialise Wayland registry")?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("wlr-layer-shell not available")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;
    let cursor_shape_manager = CursorShapeManager::bind(&globals, &qh).ok();

    let mut state = OverlayState {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        compositor,
        layer_shell,
        keyboard: None,
        pointer: None,
        cursor_shape_manager,
        cursor_shape_device: None,
        overlays: Vec::new(),
        active_surface: None,
        window_rects,
        hud_active: true,
        preferences: default_preferences,
        hud_result: None,
        selection_result: None,
        exit: false,
        cancelled: false,
    };

    // Output geometry isn't available synchronously: pump the queue once so the
    // registry/xdg-output events populate OutputState before we place surfaces.
    event_queue
        .roundtrip(&mut state)
        .context("Wayland roundtrip error")?;
    state.create_overlays(&qh, "crabture-hud");

    loop {
        event_queue
            .blocking_dispatch(&mut state)
            .context("Wayland dispatch error")?;
        if state.exit {
            break;
        }
    }

    let result = if state.cancelled {
        SessionCommand::Cancel
    } else {
        state.hud_result.clone().unwrap_or(SessionCommand::Cancel)
    };
    state.teardown(&conn);
    event_queue.roundtrip(&mut state).ok();
    Ok(result)
}

fn teardown_layer_surface(conn: &Connection, layer: &LayerSurface) {
    if let SurfaceKind::Wlr(layer_surface) = layer.kind() {
        layer_surface.destroy();
    }
    layer.wl_surface().destroy();
    conn.flush().ok();
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// A toolbar pixmap together with the inputs it was rendered for, so we can
/// reuse it across frames and only re-rasterize when something actually
/// affecting the toolbar changes.
struct ToolbarCache {
    prefs: GraphicalPreferences,
    scale: usize,
    hovered: Option<usize>,
    lw: usize,
    lh: usize,
    pixmap: tiny_skia::Pixmap,
    ox: i32,
    oy: i32,
}

/// Per-monitor overlay surface and its selection state.  One of these is
/// created for every Wayland output so the capture UI spans all monitors and
/// the user can select on any of them — not just the focused one.
struct OutputOverlay {
    layer: LayerSurface,
    /// Per-output shm pool: each surface draws its own buffer at its own
    /// physical resolution (logical × scale), so mixed-DPI setups stay crisp.
    pool: SlotPool,

    surface_w: u32,
    surface_h: u32,
    /// HiDPI buffer scale (integer).  The wl_shm buffer is allocated at
    /// `surface_size * buffer_scale` physical pixels and tagged with
    /// `set_buffer_scale` so the compositor renders it crisply.
    buffer_scale: i32,

    selecting: bool,
    start: Option<(f64, f64)>,
    current: Option<(f64, f64)>,
    selection: Option<(u32, u32, u32, u32)>,
    area_drag: Option<AreaDrag>,
    window_target: Option<(u32, u32)>,
    /// Index of the toolbar button under the pointer, for hover styling.
    hovered_button: Option<usize>,
    /// Cached rendered toolbar pixmap, rebuilt only when its inputs change
    /// (prefs / scale / hover / surface size).  Window-target and area-drag
    /// motion redraw the canvas every frame but must not re-rasterize the
    /// toolbar's SVG icons and text each time.
    toolbar_cache: Option<ToolbarCache>,
    /// Logical position of this output, used to translate the surface-local
    /// pointer into the global coordinate space `window_rects` live in.
    /// Seeded from `OutputInfo::logical_position`.
    output_origin: (i32, i32),
    /// The hovered window in surface-local logical coordinates, highlighted in
    /// window mode.  `None` when the pointer is over empty space or the toolbar.
    highlighted_window: Option<(i32, i32, u32, u32)>,
    /// Name of the output this overlay is on (e.g. "eDP-1", "HDMI-A-1").
    output_name: Option<String>,

    /// True when this surface changed and needs a redraw.
    dirty: bool,
    /// True when we've requested a frame callback and are waiting for it.
    frame_pending: bool,
    first_configure: bool,
}

struct OverlayState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    /// Kept so we can lazily create a fallback surface if no outputs resolve.
    compositor: CompositorState,
    layer_shell: LayerShell,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_shape_manager: Option<CursorShapeManager>,
    cursor_shape_device: Option<WpCursorShapeDeviceV1>,

    /// One overlay surface per monitor.
    overlays: Vec<OutputOverlay>,
    /// The surface the pointer is currently over.  The macOS-style toolbar
    /// renders on this output, and keyboard capture / full-screen act on it.
    active_surface: Option<wl_surface::WlSurface>,

    /// On-screen windows as global logical rectangles, ordered like the
    /// capture's window selection, used to highlight the hovered window in
    /// window mode.  Empty when window enumeration was unavailable.
    window_rects: Vec<(i32, i32, u32, u32)>,

    hud_active: bool,
    preferences: GraphicalPreferences,
    /// Result of the HUD session (capture command or cancel).
    hud_result: Option<SessionCommand>,
    /// Result of the legacy `--select` overlay: the completed selection, its
    /// output's surface size, and that output's name.
    selection_result: Option<OverlayResult>,
    exit: bool,
    cancelled: bool,
}

impl OverlayState {
    /// Create one layer-shell overlay surface per Wayland output so the capture
    /// UI spans every monitor.  Falls back to a single compositor-placed surface
    /// when no outputs are known (degrading to the old single-monitor behavior).
    fn create_overlays(&mut self, qh: &QueueHandle<Self>, namespace: &str) {
        let outputs: Vec<wl_output::WlOutput> = self.output_state.outputs().collect();
        if outputs.is_empty() {
            self.push_overlay(qh, namespace, None);
        } else {
            for output in outputs {
                self.push_overlay(qh, namespace, Some(output));
            }
        }
        // Seed the active surface to the first overlay so the toolbar has a home
        // before the pointer enters any monitor.
        self.active_surface = self.overlays.first().map(|o| o.layer.wl_surface().clone());
    }

    fn push_overlay(
        &mut self,
        qh: &QueueHandle<Self>,
        namespace: &str,
        output: Option<wl_output::WlOutput>,
    ) {
        let pool = match SlotPool::new(1024, &self.shm) {
            Ok(p) => p,
            Err(_) => return,
        };
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some(namespace),
            output.as_ref(),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_size(0, 0);
        layer.commit();

        // Seed geometry from the output's advertised info; refined later by
        // surface_enter / configure / scale_factor_changed.
        let (output_name, output_origin, buffer_scale) = output
            .as_ref()
            .and_then(|o| self.output_state.info(o))
            .map(|info| {
                (
                    info.name,
                    info.logical_position.unwrap_or((0, 0)),
                    info.scale_factor.max(1),
                )
            })
            .unwrap_or((None, (0, 0), 1));

        self.overlays.push(OutputOverlay {
            layer,
            pool,
            surface_w: 0,
            surface_h: 0,
            buffer_scale,
            selecting: false,
            start: None,
            current: None,
            selection: None,
            area_drag: None,
            window_target: None,
            hovered_button: None,
            toolbar_cache: None,
            output_origin,
            highlighted_window: None,
            output_name,
            dirty: false,
            frame_pending: false,
            first_configure: true,
        });
    }

    fn teardown(&self, conn: &Connection) {
        for ov in &self.overlays {
            teardown_layer_surface(conn, &ov.layer);
        }
    }

    /// Index of the overlay owning `surface`, if any.
    fn index_for_surface(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.overlays
            .iter()
            .position(|o| o.layer.wl_surface() == surface)
    }

    /// Index of the overlay owning `layer`, if any.
    fn index_for_layer(&self, layer: &LayerSurface) -> Option<usize> {
        self.overlays
            .iter()
            .position(|o| o.layer.wl_surface() == layer.wl_surface())
    }

    /// Index of the overlay the pointer is currently over (the active output).
    fn active_index(&self) -> Option<usize> {
        let surface = self.active_surface.as_ref()?;
        self.index_for_surface(surface)
    }

    /// Name of the active output (the monitor the pointer is on).
    fn active_output_name(&self) -> Option<String> {
        self.active_index()
            .and_then(|i| self.overlays[i].output_name.clone())
    }

    /// Redraw a single overlay surface.  Splits the shared, read-only session
    /// state (`shm`, prefs, window rects, active flag) out as disjoint borrows
    /// so the per-output drawing can take `&mut self.overlays[idx]`.
    fn redraw(&mut self, idx: usize, qh: &QueueHandle<Self>) {
        let is_active = self.active_index() == Some(idx);
        let shm = &self.shm;
        let preferences = self.preferences;
        let hud_active = self.hud_active;
        if let Some(ov) = self.overlays.get_mut(idx) {
            ov.draw(qh, shm, preferences, hud_active, is_active);
        }
    }

    /// Redraw every overlay surface (used when a preference that affects all
    /// monitors — mode, output, format, location — changes).
    fn redraw_all(&mut self, qh: &QueueHandle<Self>) {
        for idx in 0..self.overlays.len() {
            self.redraw(idx, qh);
        }
    }

    fn activate_mode(&mut self, mode: CaptureMode, qh: &QueueHandle<Self>) {
        match mode_selection_command(mode, self.active_output_name(), self.preferences) {
            SessionCommand::SetMode(mode) => {
                self.preferences.mode = mode;
                self.redraw_all(qh);
            }
            command @ SessionCommand::CaptureFullScreen(_, _) => {
                self.hud_result = Some(command);
                self.exit = true;
            }
            _ => unreachable!("mode selection only emits mode or full-screen capture commands"),
        }
    }

    fn apply_hud_command(&mut self, command: SessionCommand, qh: &QueueHandle<Self>) {
        match command {
            SessionCommand::SetMode(mode) => {
                self.activate_mode(mode, qh);
            }
            SessionCommand::SetOutput(output) => {
                self.preferences.output = output;
                self.redraw_all(qh);
            }
            SessionCommand::SetFormat(format) => {
                self.preferences.format = format;
                self.redraw_all(qh);
            }
            SessionCommand::SetLocation(location) => {
                self.preferences.location = location;
                self.redraw_all(qh);
            }
            SessionCommand::Capture => {
                self.hud_result = Some(self.capture_or_error_command());
                self.exit = true;
            }
            SessionCommand::Cancel => {
                self.cancelled = true;
                self.exit = true;
            }
            SessionCommand::CaptureArea(_, _)
            | SessionCommand::CaptureWindow(_, _)
            | SessionCommand::CaptureFullScreen(_, _) => {
                self.hud_result = Some(command);
                self.exit = true;
            }
        }
    }

    /// Build the capture command from the active overlay's selection / target.
    fn capture_or_error_command(&self) -> SessionCommand {
        let Some(idx) = self.active_index() else {
            return SessionCommand::Capture;
        };
        let ov = &self.overlays[idx];
        confirmed_capture_command(
            self.preferences,
            ov.selection,
            ov.window_target,
            (ov.surface_w, ov.surface_h),
            ov.output_name.clone(),
        )
    }
}

impl OutputOverlay {
    /// Recompute which toolbar button is under the pointer.  Returns `true`
    /// when the hovered button changed so the caller can trigger a redraw.
    fn update_hover(&mut self, lx: f64, ly: f64, preferences: GraphicalPreferences) -> bool {
        let layout = render::toolbar_layout(
            self.surface_w as usize,
            self.surface_h as usize,
            preferences,
        );
        let hovered = render::button_at(&layout, lx, ly);
        if hovered != self.hovered_button {
            self.hovered_button = hovered;
            true
        } else {
            false
        }
    }

    /// Recompute which window the pointer is over (window mode).  The pointer is
    /// suppressed while over the toolbar so we never highlight a window behind
    /// it.  Returns `true` when the highlight changed.
    fn update_window_highlight(
        &mut self,
        lx: f64,
        ly: f64,
        window_rects: &[(i32, i32, u32, u32)],
    ) -> bool {
        let highlight = if self.hovered_button.is_some() {
            None
        } else {
            window_rect_at_point(window_rects, self.output_origin, (lx, ly))
        };
        if highlight != self.highlighted_window {
            self.highlighted_window = highlight;
            true
        } else {
            false
        }
    }

    fn draw(
        &mut self,
        qh: &QueueHandle<OverlayState>,
        shm: &Shm,
        preferences: GraphicalPreferences,
        hud_active: bool,
        is_active: bool,
    ) {
        let scale = self.buffer_scale.max(1) as usize;
        let lw = self.surface_w as usize;
        let lh = self.surface_h as usize;
        if lw == 0 || lh == 0 {
            return;
        }

        // Allocate the buffer at physical resolution (logical × scale) so the
        // compositor renders it crisply on HiDPI outputs.
        let pw = lw * scale;
        let ph = lh * scale;

        let needed = pw * ph * 4;
        if self.pool.len() < needed {
            match SlotPool::new(needed, shm) {
                Ok(p) => self.pool = p,
                Err(_) => return,
            }
        }

        let stride = pw as i32 * 4;
        let (buffer, canvas) =
            match self
                .pool
                .create_buffer(pw as i32, ph as i32, stride, wl_shm::Format::Argb8888)
            {
                Ok(pair) => pair,
                Err(_) => return,
            };

        // Interpret canvas as u32 slice for fast fills.
        // Safety: canvas is aligned and sized as wl_shm buffer (4-byte pixels).
        let pixels: &mut [u32] =
            unsafe { std::slice::from_raw_parts_mut(canvas.as_mut_ptr() as *mut u32, pw * ph) };

        if hud_active {
            self.draw_hud(pixels, lw, lh, scale, preferences, is_active);
        } else {
            pixels.fill(OVERLAY_PIXEL);
            draw_selection(
                pixels,
                pw,
                ph,
                scaled_point(self.start, scale),
                scaled_point(self.current, scale),
                scaled_rect(self.selection, scale),
            );
        }

        self.layer.wl_surface().set_buffer_scale(scale as i32);

        self.layer
            .wl_surface()
            .damage_buffer(0, 0, pw as i32, ph as i32);

        buffer
            .attach_to(self.layer.wl_surface())
            .expect("buffer attach");

        // Throttle redraws to the compositor's frame clock: always request a
        // callback for the next frame so motion-driven redraws (HUD hover,
        // window crosshair, area drag) coalesce into one repaint per frame.
        // Without this the HUD path — where `selecting` is never set — redrew
        // and committed on every pointer-motion event, flooding the Wayland
        // connection until the compositor dropped it (Broken pipe) and lagging
        // badly.  The frame handler only redraws when `dirty`, so requesting a
        // callback unconditionally is self-terminating once motion stops.
        self.layer
            .wl_surface()
            .frame(qh, self.layer.wl_surface().clone());
        self.frame_pending = true;

        self.layer.commit();
    }

    fn draw_hud(
        &mut self,
        pixels: &mut [u32],
        lw: usize,
        lh: usize,
        scale: usize,
        preferences: GraphicalPreferences,
        is_active: bool,
    ) {
        let pw = lw * scale;
        let ph = lh * scale;
        if preferences.mode == CaptureMode::Area {
            pixels.fill(OVERLAY_PIXEL);
            draw_selection(
                pixels,
                pw,
                ph,
                scaled_point(self.start, scale),
                scaled_point(self.current, scale),
                scaled_rect(self.selection, scale),
            );
        } else if preferences.mode == CaptureMode::Window {
            pixels.fill(CLEAR_PIXEL);
            if let Some(rect) = self.highlighted_window {
                draw_window_highlight(pixels, pw, ph, rect, scale);
            } else if let Some(point) = self.window_target {
                draw_window_target(
                    pixels,
                    pw,
                    ph,
                    (point.0 as usize * scale, point.1 as usize * scale),
                    scale,
                );
            }
        } else {
            pixels.fill(CLEAR_PIXEL);
        }

        // Only the active output (the monitor the pointer is on) composites the
        // macOS-style toolbar, so it follows the cursor across displays.
        if !is_active {
            return;
        }

        // Composite the toolbar over the canvas.  Rasterizing it (SVG icons +
        // anti-aliased text) is comparatively expensive, so cache the rendered
        // pixmap and only rebuild when an input that affects the toolbar changes
        // — never on plain window-target / area-drag motion, which redraw the
        // canvas every frame.  Layout is in logical coordinates so it stays in
        // sync with the pointer hit-test; the renderer scales it to the physical
        // buffer resolution.
        let cache_hit = self.toolbar_cache.as_ref().is_some_and(|c| {
            c.prefs == preferences
                && c.scale == scale
                && c.hovered == self.hovered_button
                && c.lw == lw
                && c.lh == lh
        });
        if !cache_hit {
            let layout = render::toolbar_layout(lw, lh, preferences);
            self.toolbar_cache =
                render::render_toolbar(&layout, preferences, scale, self.hovered_button).map(
                    |(pixmap, ox, oy)| ToolbarCache {
                        prefs: preferences,
                        scale,
                        hovered: self.hovered_button,
                        lw,
                        lh,
                        pixmap,
                        ox,
                        oy,
                    },
                );
        }
        if let Some(c) = &self.toolbar_cache {
            render::blit_argb(pixels, pw, ph, &c.pixmap, c.ox, c.oy);
        }
    }
}

/// Scale an optional logical point into physical pixel coordinates.
fn scaled_point(point: Option<(f64, f64)>, scale: usize) -> Option<(f64, f64)> {
    point.map(|(x, y)| (x * scale as f64, y * scale as f64))
}

/// Scale an optional logical selection rect into physical pixel coordinates.
fn scaled_rect(rect: Option<SelectionRect>, scale: usize) -> Option<SelectionRect> {
    let s = scale as u32;
    rect.map(|(x, y, w, h)| (x * s, y * s, w * s, h * s))
}

/// Find the window under a surface-local logical `point` and return it in
/// surface-local logical coordinates.  `window_rects` are global logical rects
/// in capture-selection order; `origin` is the overlay output's logical
/// position.  Mirrors the capture's "first window containing the point" rule so
/// the highlight matches what will be captured.
fn window_rect_at_point(
    window_rects: &[(i32, i32, u32, u32)],
    origin: (i32, i32),
    point: (f64, f64),
) -> Option<(i32, i32, u32, u32)> {
    let gx = origin.0 + point.0.round() as i32;
    let gy = origin.1 + point.1.round() as i32;
    window_rects
        .iter()
        .copied()
        .find(|(x, y, w, h)| {
            *w > 0 && *h > 0 && gx >= *x && gy >= *y && gx < x + *w as i32 && gy < y + *h as i32
        })
        .map(|(x, y, w, h)| (x - origin.0, y - origin.1, w, h))
}

/// Fill the hovered window's rectangle with a translucent accent tint and a
/// solid accent border.  `rect` is in surface-local logical coordinates (its
/// origin may be negative if the window extends off the output's top/left); it
/// is scaled to physical pixels and clamped to the buffer.
fn draw_window_highlight(
    pixels: &mut [u32],
    w: usize,
    h: usize,
    rect: (i32, i32, u32, u32),
    scale: usize,
) {
    let s = scale as i32;
    let left = (rect.0 * s).clamp(0, w as i32);
    let top = (rect.1 * s).clamp(0, h as i32);
    let right = ((rect.0 + rect.2 as i32) * s).clamp(0, w as i32);
    let bottom = ((rect.1 + rect.3 as i32) * s).clamp(0, h as i32);
    if right <= left || bottom <= top {
        return;
    }
    let (left, top, right, bottom) = (left as usize, top as usize, right as usize, bottom as usize);

    for y in top..bottom {
        let row = y * w;
        for px in &mut pixels[row + left..row + right] {
            *px = HIGHLIGHT_FILL_PIXEL;
        }
    }

    let border = scale.max(1) * 2;
    for y in top..bottom {
        let row = y * w;
        let on_h_edge = y < top + border || y >= bottom.saturating_sub(border);
        if on_h_edge {
            for px in &mut pixels[row + left..row + right] {
                *px = HIGHLIGHT_BORDER_PIXEL;
            }
        } else {
            let left_edge = (left + border).min(right);
            for px in &mut pixels[row + left..row + left_edge] {
                *px = HIGHLIGHT_BORDER_PIXEL;
            }
            let right_edge = right.saturating_sub(border).max(left);
            for px in &mut pixels[row + right_edge..row + right] {
                *px = HIGHLIGHT_BORDER_PIXEL;
            }
        }
    }
}

fn draw_window_target(pixels: &mut [u32], w: usize, h: usize, point: (usize, usize), scale: usize) {
    let cx = point.0;
    let cy = point.1;
    let radius = 18usize * scale;

    if cy < h {
        let left = cx.saturating_sub(radius);
        let right = (cx + radius + 1).min(w);
        for x in left..right {
            pixels[cy * w + x] = BORDER_PIXEL;
        }
    }
    if cx < w {
        let top = cy.saturating_sub(radius);
        let bottom = (cy + radius + 1).min(h);
        for y in top..bottom {
            pixels[y * w + cx] = BORDER_PIXEL;
        }
    }
}

fn draw_selection(
    pixels: &mut [u32],
    w: usize,
    h: usize,
    start: Option<(f64, f64)>,
    current: Option<(f64, f64)>,
    selection: Option<SelectionRect>,
) {
    let rect = if let (Some(start), Some(cur)) = (start, current) {
        Some(selection_rect_from_drag(start, cur))
    } else {
        selection
    };

    if let Some((x, y, width, height)) = rect {
        let x1 = (x as usize).min(w.saturating_sub(1));
        let y1 = (y as usize).min(h.saturating_sub(1));
        let x2 = (x as usize + width as usize).min(w);
        let y2 = (y as usize + height as usize).min(h);

        if x2 > x1 && y2 > y1 {
            for row in y1..y2 {
                let start = row * w + x1;
                let end = row * w + x2;
                pixels[start..end].fill(CLEAR_PIXEL);
            }

            let top_start = y1 * w + x1;
            let top_end = y1 * w + x2;
            pixels[top_start..top_end].fill(BORDER_PIXEL);

            if y2 > y1 + 1 {
                let bot_start = (y2 - 1) * w + x1;
                let bot_end = (y2 - 1) * w + x2;
                pixels[bot_start..bot_end].fill(BORDER_PIXEL);
            }

            for row in y1..y2 {
                pixels[row * w + x1] = BORDER_PIXEL;
                if x2 > x1 + 1 {
                    pixels[row * w + x2 - 1] = BORDER_PIXEL;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Handler impls
// ---------------------------------------------------------------------------

impl CompositorHandler for OverlayState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let scale = new_factor.max(1);
        if let Some(idx) = self.index_for_surface(surface)
            && scale != self.overlays[idx].buffer_scale
        {
            self.overlays[idx].buffer_scale = scale;
            // Reallocate the buffer at the new physical resolution and redraw.
            self.redraw(idx, qh);
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        if let Some(idx) = self.index_for_surface(surface) {
            self.overlays[idx].frame_pending = false;
            if self.overlays[idx].dirty {
                self.overlays[idx].dirty = false;
                self.redraw(idx, qh);
            }
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        // Refine the overlay's output identity/geometry from the output it
        // actually landed on, and seed the buffer scale from the reported scale
        // factor as a first-frame fallback (a later `scale_factor_changed`
        // event will correct it if it differs).
        let Some(idx) = self.index_for_surface(surface) else {
            return;
        };
        if let Some(info) = self.output_state.info(output) {
            self.overlays[idx].output_name = info.name;
            // Translate window rects (global logical) into this output's local
            // space when highlighting the hovered window.
            self.overlays[idx].output_origin = info.logical_position.unwrap_or((0, 0));
            let scale = info.scale_factor.max(1);
            if scale != self.overlays[idx].buffer_scale {
                self.overlays[idx].buffer_scale = scale;
                self.redraw(idx, qh);
            }
        }
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for OverlayState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for OverlayState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.cancelled = true;
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(idx) = self.index_for_layer(layer) else {
            return;
        };

        let first = {
            let shm = &self.shm;
            let ov = &mut self.overlays[idx];
            if configure.new_size.0 > 0 {
                ov.surface_w = configure.new_size.0;
            }
            if configure.new_size.1 > 0 {
                ov.surface_h = configure.new_size.1;
            }

            // Allocate pool now that we know the surface size.  Size it for the
            // physical-resolution buffer (logical × scale, squared).
            let scale = ov.buffer_scale.max(1) as usize;
            let needed = (ov.surface_w as usize * scale) * (ov.surface_h as usize * scale) * 4;
            if ov.pool.len() < needed
                && let Ok(p) = SlotPool::new(needed, shm)
            {
                ov.pool = p;
            }

            ov.first_configure
        };

        if first {
            self.overlays[idx].first_configure = false;
            self.redraw(idx, qh);
        }
    }
}

impl SeatHandler for OverlayState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let keyboard = self
                .seat_state
                .get_keyboard(qh, &seat, None)
                .expect("failed to get keyboard");
            self.keyboard = Some(keyboard);
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            let pointer = self
                .seat_state
                .get_pointer(qh, &seat)
                .expect("failed to get pointer");
            // Create a cursor shape device so we can set a crosshair cursor.
            if let Some(ref mgr) = self.cursor_shape_manager {
                self.cursor_shape_device = Some(mgr.get_shape_device(&pointer, qh));
            }
            self.pointer = Some(pointer);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(kb) = self.keyboard.take()
        {
            kb.release();
        }
        if capability == Capability::Pointer
            && let Some(ptr) = self.pointer.take()
        {
            ptr.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for OverlayState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if !self.hud_active {
            if event.keysym == Keysym::Escape {
                self.cancelled = true;
                self.exit = true;
            }
            return;
        }

        match event.keysym {
            Keysym::Escape => {
                self.cancelled = true;
                self.exit = true;
            }
            Keysym::Return => {
                self.hud_result = Some(self.capture_or_error_command());
                self.exit = true;
            }
            Keysym::space if self.preferences.mode == CaptureMode::Area => {
                self.preferences.mode = CaptureMode::Window;
                for ov in &mut self.overlays {
                    ov.selection = None;
                    ov.start = None;
                    ov.current = None;
                    ov.area_drag = None;
                }
                self.redraw_all(qh);
            }
            _ => {
                if let Some(command) = shortcut_session_command(
                    event.keysym,
                    self.active_output_name(),
                    self.preferences,
                ) {
                    self.apply_hud_command(command, qh);
                }
            }
        }
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _event: KeyEvent,
    ) {
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
    }
}

impl PointerHandler for OverlayState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        // Deferred (frame-throttled) redraw target.  At most one surface sees
        // motion per frame, so a single index is sufficient.
        let mut pending_redraw: Option<usize> = None;

        for event in events {
            // Route each event to the overlay owning the surface it occurred on.
            let Some(idx) = self.index_for_surface(&event.surface) else {
                continue;
            };

            let lx = event.position.0;
            let ly = event.position.1;

            match event.kind {
                PointerEventKind::Enter { serial } => {
                    // Set crosshair cursor when entering the overlay surface.
                    if let Some(ref device) = self.cursor_shape_device {
                        device.set_shape(serial, Shape::Crosshair);
                    }
                    // The toolbar lives on the monitor the pointer is over, so
                    // make this surface active and repaint the old + new homes
                    // so the toolbar relocates to the cursor's display.
                    let prev = self.active_index();
                    if prev != Some(idx) {
                        self.active_surface = Some(event.surface.clone());
                        if let Some(p) = prev {
                            self.redraw(p, qh);
                        }
                        self.redraw(idx, qh);
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    if self.hud_active {
                        let mode = self.preferences.mode;
                        if button == 0x110 {
                            let mut layout = render::toolbar_layout(
                                self.overlays[idx].surface_w as usize,
                                self.overlays[idx].surface_h as usize,
                                self.preferences,
                            );
                            if let Some(button_idx) = render::button_at(&layout, lx, ly) {
                                let command = layout.swap_remove(button_idx).command;
                                self.apply_hud_command(command, qh);
                                return;
                            }
                            if mode == CaptureMode::Area {
                                let point = (lx.round() as i32, ly.round() as i32);
                                {
                                    let ov = &mut self.overlays[idx];
                                    ov.area_drag = if let Some(rect) = ov.selection {
                                        if let Some(handle) = hit_resize_handle(rect, point) {
                                            Some(AreaDrag::Resizing {
                                                original: rect,
                                                handle,
                                            })
                                        } else if point_in_rect(rect, point) {
                                            Some(AreaDrag::Moving {
                                                original: rect,
                                                pointer_start: point,
                                            })
                                        } else {
                                            ov.selection = None;
                                            ov.start = Some((lx, ly));
                                            ov.current = Some((lx, ly));
                                            Some(AreaDrag::Drawing)
                                        }
                                    } else {
                                        ov.start = Some((lx, ly));
                                        ov.current = Some((lx, ly));
                                        Some(AreaDrag::Drawing)
                                    };
                                    ov.selecting = true;
                                }
                                self.redraw(idx, qh);
                            } else if mode == CaptureMode::Window {
                                let point = (lx.round() as u32, ly.round() as u32);
                                let surface_size =
                                    (self.overlays[idx].surface_w, self.overlays[idx].surface_h);
                                let output_name = self.overlays[idx].output_name.clone();
                                self.overlays[idx].window_target = Some(point);
                                self.hud_result = Some(capture_window_command(
                                    point,
                                    surface_size,
                                    output_name,
                                    self.preferences,
                                ));
                                self.exit = true;
                            }
                        }
                        if button == 0x111 {
                            self.cancelled = true;
                            self.exit = true;
                        }
                        return;
                    }

                    if button == 0x110 {
                        self.active_surface = Some(event.surface.clone());
                        let ov = &mut self.overlays[idx];
                        ov.start = Some((lx, ly));
                        ov.current = Some((lx, ly));
                        ov.area_drag = Some(AreaDrag::Drawing);
                        ov.selecting = true;
                        pending_redraw = Some(idx);
                    }
                    if button == 0x111 {
                        self.cancelled = true;
                        self.exit = true;
                        return;
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    if button == 0x110 && self.overlays[idx].selecting {
                        {
                            let ov = &mut self.overlays[idx];
                            if matches!(ov.area_drag, Some(AreaDrag::Drawing))
                                && let (Some((sx, sy)), Some((cx, cy))) = (ov.start, ov.current)
                            {
                                let rect = clamp_selection_rect(
                                    selection_rect_from_drag((sx, sy), (cx, cy)),
                                    (ov.surface_w, ov.surface_h),
                                );
                                ov.selection = Some(rect);
                                ov.start = None;
                                ov.current = None;
                            }
                            ov.selecting = false;
                            ov.area_drag = None;
                        }
                        if !self.hud_active {
                            // Legacy `--select`: return the completed selection
                            // along with this output's size and identity.
                            let ov = &self.overlays[idx];
                            let result = (
                                ov.selection,
                                (ov.surface_w, ov.surface_h),
                                ov.output_name.clone(),
                            );
                            self.selection_result = Some(result);
                            self.exit = true;
                        } else {
                            self.redraw(idx, qh);
                        }
                        return;
                    }
                }
                PointerEventKind::Motion { .. } if self.overlays[idx].selecting => {
                    let ov = &mut self.overlays[idx];
                    let surface_size = (ov.surface_w, ov.surface_h);
                    match ov.area_drag {
                        Some(AreaDrag::Drawing) => ov.current = Some((lx, ly)),
                        Some(AreaDrag::Moving {
                            original,
                            pointer_start,
                        }) => {
                            ov.selection = Some(move_selection_rect(
                                original,
                                (
                                    lx.round() as i32 - pointer_start.0,
                                    ly.round() as i32 - pointer_start.1,
                                ),
                                surface_size,
                            ));
                        }
                        Some(AreaDrag::Resizing { original, handle }) => {
                            ov.selection = Some(resize_selection_rect(
                                original,
                                handle,
                                (lx.round() as i32, ly.round() as i32),
                                surface_size,
                            ));
                        }
                        None => {}
                    }
                    pending_redraw = Some(idx);
                }
                PointerEventKind::Motion { .. } if self.hud_active => {
                    let preferences = self.preferences;
                    let mut changed = false;
                    if self.overlays[idx].update_hover(lx, ly, preferences) {
                        changed = true;
                    }
                    if preferences.mode == CaptureMode::Window {
                        let window_rects = &self.window_rects;
                        let ov = &mut self.overlays[idx];
                        ov.window_target = Some((lx.round() as u32, ly.round() as u32));
                        ov.update_window_highlight(lx, ly, window_rects);
                        changed = true;
                    }
                    if changed {
                        pending_redraw = Some(idx);
                    }
                }
                _ => {}
            }
        }

        if let Some(idx) = pending_redraw {
            self.overlays[idx].dirty = true;
            if !self.overlays[idx].frame_pending {
                self.overlays[idx].dirty = false;
                self.redraw(idx, qh);
            }
        }
    }
}

impl ShmHandler for OverlayState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

// ---------------------------------------------------------------------------
// Delegate macros
// ---------------------------------------------------------------------------

delegate_compositor!(OverlayState);
delegate_output!(OverlayState);
delegate_shm!(OverlayState);
delegate_seat!(OverlayState);
delegate_keyboard!(OverlayState);
delegate_pointer!(OverlayState);
delegate_layer!(OverlayState);
delegate_registry!(OverlayState);

impl ProvidesRegistryState for OverlayState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{OutputDestination, SaveLocationChoice};

    #[test]
    fn normalizes_selection_drag_from_any_direction() {
        assert_eq!(
            selection_rect_from_drag((100.0, 80.0), (20.0, 30.0)),
            (20, 30, 80, 50)
        );
        assert_eq!(
            selection_rect_from_drag((20.0, 30.0), (100.0, 80.0)),
            (20, 30, 80, 50)
        );
    }

    #[test]
    fn full_screen_command_carries_output_identity() {
        let preferences = GraphicalPreferences {
            mode: CaptureMode::FullScreen,
            ..GraphicalPreferences::default()
        };

        assert_eq!(
            capture_full_screen_command(Some("eDP-1".to_string()), preferences),
            SessionCommand::CaptureFullScreen(
                FullScreenSelection {
                    output_name: Some("eDP-1".to_string()),
                },
                preferences
            )
        );
    }

    #[test]
    fn window_command_carries_target_point() {
        let preferences = GraphicalPreferences {
            mode: CaptureMode::Window,
            ..GraphicalPreferences::default()
        };

        assert_eq!(
            capture_window_command(
                (120, 80),
                (800, 600),
                Some("eDP-1".to_string()),
                preferences
            ),
            SessionCommand::CaptureWindow(
                WindowSelection {
                    point: (120, 80),
                    surface_size: (800, 600),
                    output_name: Some("eDP-1".to_string()),
                },
                preferences
            )
        );
    }

    #[test]
    fn full_screen_mode_selection_captures_immediately() {
        let preferences = GraphicalPreferences {
            output: OutputDestination::Save,
            format: crate::session::GraphicalFormat::Jpg,
            location: SaveLocationChoice::CurrentDirectory,
            mode: CaptureMode::Area,
        };
        let expected_preferences = GraphicalPreferences {
            mode: CaptureMode::FullScreen,
            ..preferences
        };

        assert_eq!(
            mode_selection_command(
                CaptureMode::FullScreen,
                Some("eDP-1".to_string()),
                preferences,
            ),
            SessionCommand::CaptureFullScreen(
                FullScreenSelection {
                    output_name: Some("eDP-1".to_string()),
                },
                expected_preferences,
            )
        );
    }

    #[test]
    fn area_and_window_mode_selection_still_changes_mode() {
        let preferences = GraphicalPreferences::default();

        assert_eq!(
            mode_selection_command(CaptureMode::Area, Some("eDP-1".to_string()), preferences),
            SessionCommand::SetMode(CaptureMode::Area)
        );
        assert_eq!(
            mode_selection_command(CaptureMode::Window, Some("eDP-1".to_string()), preferences),
            SessionCommand::SetMode(CaptureMode::Window)
        );
    }

    #[test]
    fn keyboard_shortcuts_share_toolbar_commands() {
        let preferences = GraphicalPreferences {
            output: OutputDestination::Save,
            format: crate::session::GraphicalFormat::Jpg,
            location: SaveLocationChoice::CurrentDirectory,
            mode: CaptureMode::Area,
        };

        assert_eq!(
            shortcut_session_command(Keysym::a, Some("eDP-1".to_string()), preferences),
            Some(SessionCommand::SetMode(CaptureMode::Area))
        );
        assert_eq!(
            shortcut_session_command(Keysym::w, Some("eDP-1".to_string()), preferences),
            Some(SessionCommand::SetMode(CaptureMode::Window))
        );
        assert_eq!(
            shortcut_session_command(Keysym::f, Some("eDP-1".to_string()), preferences),
            Some(SessionCommand::CaptureFullScreen(
                FullScreenSelection {
                    output_name: Some("eDP-1".to_string()),
                },
                GraphicalPreferences {
                    mode: CaptureMode::FullScreen,
                    ..preferences
                },
            ))
        );
        assert_eq!(
            shortcut_session_command(Keysym::o, None, preferences),
            Some(SessionCommand::SetOutput(OutputDestination::CopyAndSave))
        );
    }

    #[test]
    fn confirm_command_is_consistent_across_modes() {
        let preferences = GraphicalPreferences::default();

        assert_eq!(
            confirmed_capture_command(
                preferences,
                Some((10, 20, 30, 40)),
                None,
                (800, 600),
                Some("eDP-1".to_string())
            ),
            SessionCommand::CaptureArea(
                AreaSelection {
                    rect: (10, 20, 30, 40),
                    surface_size: (800, 600),
                    output_name: Some("eDP-1".to_string()),
                },
                preferences,
            )
        );

        let preferences = GraphicalPreferences {
            mode: CaptureMode::Window,
            ..preferences
        };
        assert_eq!(
            confirmed_capture_command(
                preferences,
                None,
                Some((120, 80)),
                (800, 600),
                Some("eDP-1".to_string())
            ),
            SessionCommand::CaptureWindow(
                WindowSelection {
                    point: (120, 80),
                    surface_size: (800, 600),
                    output_name: Some("eDP-1".to_string()),
                },
                preferences,
            )
        );

        let preferences = GraphicalPreferences {
            mode: CaptureMode::FullScreen,
            ..preferences
        };
        assert_eq!(
            confirmed_capture_command(preferences, None, None, (800, 600), None),
            SessionCommand::CaptureFullScreen(
                FullScreenSelection { output_name: None },
                preferences
            )
        );
    }

    #[test]
    fn draws_window_target_marker() {
        let mut pixels = vec![CLEAR_PIXEL; 20 * 20];

        draw_window_target(&mut pixels, 20, 20, (10, 10), 1);

        assert_eq!(pixels[10 * 20 + 10], BORDER_PIXEL);
        assert_eq!(pixels[10 * 20], BORDER_PIXEL);
        assert_eq!(pixels[10], BORDER_PIXEL);
    }

    #[test]
    fn scales_geometry_into_physical_pixels() {
        // A logical point/rect maps to physical coordinates at the buffer scale.
        assert_eq!(scaled_point(Some((10.0, 20.0)), 2), Some((20.0, 40.0)));
        assert_eq!(scaled_rect(Some((5, 6, 7, 8)), 2), Some((10, 12, 14, 16)));
        assert_eq!(scaled_point(None, 2), None);
        assert_eq!(scaled_rect(None, 2), None);
    }

    #[test]
    fn draws_scaled_window_target_marker() {
        // At scale 2 the marker is drawn in a physical-resolution buffer.
        let mut pixels = vec![CLEAR_PIXEL; 40 * 40];

        draw_window_target(&mut pixels, 40, 40, (20, 20), 2);

        // Centre crosshair lands at the scaled point.
        assert_eq!(pixels[20 * 40 + 20], BORDER_PIXEL);
        // The crosshair arms reach 18*scale=36 pixels out, clamped to the
        // buffer edges without overflowing.
        assert_eq!(pixels[20 * 40], BORDER_PIXEL);
        assert_eq!(pixels[20 * 40 + 39], BORDER_PIXEL);
    }

    #[test]
    fn highlights_the_first_window_containing_the_point() {
        // Two overlapping windows; the first in order wins, mirroring capture.
        let rects = [(0, 0, 200, 150), (50, 50, 200, 150)];
        assert_eq!(
            window_rect_at_point(&rects, (0, 0), (60.0, 60.0)),
            Some((0, 0, 200, 150)),
        );
        // A point only inside the second window selects it.
        assert_eq!(
            window_rect_at_point(&rects, (0, 0), (220.0, 160.0)),
            Some((50, 50, 200, 150)),
        );
        // Empty space highlights nothing.
        assert_eq!(window_rect_at_point(&rects, (0, 0), (400.0, 400.0)), None);
    }

    #[test]
    fn highlight_hit_test_maps_through_output_origin() {
        // The overlay's output sits at logical (1920, 0); a window at global
        // (2000, 100) is hit by the surface-local point (80, 100) and returned
        // in surface-local coordinates.
        let rects = [(2000, 100, 300, 200)];
        assert_eq!(
            window_rect_at_point(&rects, (1920, 0), (100.0, 150.0)),
            Some((80, 100, 300, 200)),
        );
        // The same surface point on a single-output desktop (origin 0,0) misses.
        assert_eq!(window_rect_at_point(&rects, (0, 0), (100.0, 150.0)), None);
    }

    #[test]
    fn draws_window_highlight_fill_and_border() {
        let mut pixels = vec![CLEAR_PIXEL; 40 * 40];

        draw_window_highlight(&mut pixels, 40, 40, (10, 10, 20, 20), 1);

        // Interior is the translucent fill.
        assert_eq!(pixels[20 * 40 + 20], HIGHLIGHT_FILL_PIXEL);
        // The top-left corner of the rect is on the solid border.
        assert_eq!(pixels[10 * 40 + 10], HIGHLIGHT_BORDER_PIXEL);
        // Outside the rect stays untouched.
        assert_eq!(pixels[5 * 40 + 5], CLEAR_PIXEL);
    }

    #[test]
    fn window_highlight_clips_to_buffer_bounds() {
        // A window extending past the top/left of the output (negative origin)
        // and beyond the right/bottom is clipped without panicking.
        let mut pixels = vec![CLEAR_PIXEL; 20 * 20];

        draw_window_highlight(&mut pixels, 20, 20, (-10, -10, 40, 40), 1);

        // The visible portion is filled (border within the first rows/cols).
        assert_eq!(pixels[0], HIGHLIGHT_BORDER_PIXEL);
        assert_eq!(pixels[10 * 20 + 10], HIGHLIGHT_FILL_PIXEL);
    }

    #[test]
    fn rounds_selection_drag_to_logical_pixels() {
        assert_eq!(
            selection_rect_from_drag((10.4, 20.5), (25.6, 30.4)),
            (10, 21, 16, 9)
        );
    }

    #[test]
    fn moves_selection_within_surface_bounds() {
        assert_eq!(
            move_selection_rect((20, 30, 80, 50), (15, -10), (200, 120)),
            (35, 20, 80, 50)
        );
        assert_eq!(
            move_selection_rect((20, 30, 80, 50), (-50, 100), (200, 120)),
            (0, 70, 80, 50)
        );
    }

    #[test]
    fn resizes_selection_from_edges_and_corners() {
        assert_eq!(
            resize_selection_rect(
                (20, 30, 80, 50),
                ResizeHandle::SouthEast,
                (140, 100),
                (200, 120)
            ),
            (20, 30, 120, 70)
        );
        assert_eq!(
            resize_selection_rect((20, 30, 80, 50), ResizeHandle::West, (10, 99), (200, 120)),
            (10, 30, 90, 50)
        );
    }

    #[test]
    fn resize_enforces_minimum_size_and_surface_bounds() {
        assert_eq!(
            resize_selection_rect(
                (20, 30, 80, 50),
                ResizeHandle::NorthWest,
                (99, 79),
                (200, 120)
            ),
            (92, 72, 8, 8)
        );
        assert_eq!(
            resize_selection_rect(
                (20, 30, 80, 50),
                ResizeHandle::SouthEast,
                (300, 200),
                (120, 90)
            ),
            (20, 30, 100, 60)
        );
    }

    #[test]
    fn hit_testing_prioritizes_handles_over_move_region() {
        let rect = (20, 30, 80, 50);

        assert_eq!(
            hit_resize_handle(rect, (20, 30)),
            Some(ResizeHandle::NorthWest)
        );
        assert_eq!(
            hit_resize_handle(rect, (100, 80)),
            Some(ResizeHandle::SouthEast)
        );
        assert_eq!(hit_resize_handle(rect, (60, 30)), Some(ResizeHandle::North));
        assert_eq!(hit_resize_handle(rect, (60, 55)), None);
        assert!(point_in_rect(rect, (60, 55)));
    }

    #[test]
    fn capture_command_uses_final_edited_rectangle() {
        let preferences = GraphicalPreferences::default();

        assert_eq!(
            capture_area_command(
                (35, 20, 80, 50),
                (200, 120),
                Some("eDP-1".to_string()),
                preferences,
            ),
            SessionCommand::CaptureArea(
                AreaSelection {
                    rect: (35, 20, 80, 50),
                    surface_size: (200, 120),
                    output_name: Some("eDP-1".to_string()),
                },
                preferences,
            )
        );
    }
}
