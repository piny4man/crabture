//! Wayland layer-shell overlay for interactive area selection.
//!
//! Uses smithay-client-toolkit to create a fullscreen overlay surface on the
//! `Overlay` layer.  A semi-transparent dark tint covers the screen; the
//! selected rectangle is shown as a clear (transparent) area with a white
//! border so the user can see the content underneath.
//! Pointer click-drag selects the area; Escape or right-click cancels.

use crate::session::{
    AreaSelection, CaptureMode, FullScreenSelection, GraphicalPreferences, OutputDestination,
    SaveLocationChoice, SessionCommand, WindowSelection,
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
const HUD_PIXEL: u32 = 0xDD22_2222;
const HUD_ACTIVE_PIXEL: u32 = 0xDD44_6688;
const HUD_TEXT_PIXEL: u32 = 0xFFFF_FFFF;

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

    let surface = compositor.create_surface(&qh);

    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some("crabture-select"),
        None,
    );
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.set_size(0, 0);
    layer.commit();

    let pool = SlotPool::new(1024, &shm).context("failed to create shm pool")?;

    // Optional: cursor shape protocol (not all compositors support it).
    let cursor_shape_manager = CursorShapeManager::bind(&globals, &qh).ok();

    let mut state = OverlayState {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        keyboard: None,
        pointer: None,
        cursor_shape_manager,
        cursor_shape_device: None,

        surface_w: 0,
        surface_h: 0,

        selecting: false,
        start: None,
        current: None,
        selection: None,
        area_drag: None,
        window_target: None,

        output_name: None,

        dirty: false,
        frame_pending: false,

        first_configure: true,
        exit: false,
        cancelled: false,
        hud_active: false,
        preferences: GraphicalPreferences::default(),
        hud_result: None,
    };

    loop {
        event_queue
            .blocking_dispatch(&mut state)
            .context("Wayland dispatch error")?;
        if state.exit {
            break;
        }
    }

    let surf = (state.surface_w, state.surface_h);
    let output_name = state.output_name.clone();
    teardown_layer_surface(&conn, &state.layer);
    event_queue.roundtrip(&mut state).ok();

    if state.cancelled {
        return Ok((None, surf, output_name));
    }

    Ok((state.selection, surf, output_name))
}

pub fn run_screenshot_hud(default_preferences: GraphicalPreferences) -> Result<SessionCommand> {
    let conn = Connection::connect_to_env().context("failed to connect to Wayland")?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).context("failed to initialise Wayland registry")?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("wlr-layer-shell not available")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;

    let surface = compositor.create_surface(&qh);

    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("crabture-hud"), None);
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.set_size(0, 0);
    layer.commit();

    let pool = SlotPool::new(1024, &shm).context("failed to create shm pool")?;
    let cursor_shape_manager = CursorShapeManager::bind(&globals, &qh).ok();

    let mut state = OverlayState {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        keyboard: None,
        pointer: None,
        cursor_shape_manager,
        cursor_shape_device: None,

        surface_w: 0,
        surface_h: 0,

        selecting: false,
        start: None,
        current: None,
        selection: None,
        area_drag: None,
        window_target: None,

        output_name: None,

        dirty: false,
        frame_pending: false,

        first_configure: true,
        exit: false,
        cancelled: false,
        hud_active: true,
        preferences: default_preferences,
        hud_result: None,
    };

    loop {
        event_queue
            .blocking_dispatch(&mut state)
            .context("Wayland dispatch error")?;
        if state.exit {
            break;
        }
    }

    if state.cancelled {
        teardown_layer_surface(&conn, &state.layer);
        event_queue.roundtrip(&mut state).ok();
        return Ok(SessionCommand::Cancel);
    }

    let result = state.hud_result.clone().unwrap_or(SessionCommand::Cancel);
    teardown_layer_surface(&conn, &state.layer);
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

struct OverlayState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_shape_manager: Option<CursorShapeManager>,
    cursor_shape_device: Option<WpCursorShapeDeviceV1>,

    surface_w: u32,
    surface_h: u32,

    selecting: bool,
    start: Option<(f64, f64)>,
    current: Option<(f64, f64)>,
    selection: Option<(u32, u32, u32, u32)>,
    area_drag: Option<AreaDrag>,
    window_target: Option<(u32, u32)>,

    /// Name of the output the overlay surface is on (e.g. "eDP-1", "HDMI-A-1").
    output_name: Option<String>,

    /// True when the selection changed and we need a redraw.
    dirty: bool,
    /// True when we've requested a frame callback and are waiting for it.
    frame_pending: bool,

    first_configure: bool,
    exit: bool,
    cancelled: bool,
    hud_active: bool,
    preferences: GraphicalPreferences,
    hud_result: Option<SessionCommand>,
}

impl OverlayState {
    fn activate_mode(&mut self, mode: CaptureMode, qh: &QueueHandle<Self>) {
        match mode_selection_command(mode, self.output_name.clone(), self.preferences) {
            SessionCommand::SetMode(mode) => {
                self.preferences.mode = mode;
                self.draw(qh);
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
                self.draw(qh);
            }
            SessionCommand::SetFormat(format) => {
                self.preferences.format = format;
                self.draw(qh);
            }
            SessionCommand::SetLocation(location) => {
                self.preferences.location = location;
                self.draw(qh);
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

    fn capture_or_error_command(&self) -> SessionCommand {
        confirmed_capture_command(
            self.preferences,
            self.selection,
            self.window_target,
            (self.surface_w, self.surface_h),
            self.output_name.clone(),
        )
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let w = self.surface_w as usize;
        let h = self.surface_h as usize;
        if w == 0 || h == 0 {
            return;
        }

        let stride = w as i32 * 4;
        let (buffer, canvas) =
            match self
                .pool
                .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
            {
                Ok(pair) => pair,
                Err(_) => return,
            };

        // Interpret canvas as u32 slice for fast fills.
        // Safety: canvas is aligned and sized as wl_shm buffer (4-byte pixels).
        let pixels: &mut [u32] =
            unsafe { std::slice::from_raw_parts_mut(canvas.as_mut_ptr() as *mut u32, w * h) };

        if self.hud_active {
            self.draw_hud(pixels, w, h);
        } else {
            pixels.fill(OVERLAY_PIXEL);
            draw_selection(pixels, w, h, self.start, self.current, self.selection);
        }

        self.layer.wl_surface().set_buffer_scale(1);

        self.layer
            .wl_surface()
            .damage_buffer(0, 0, w as i32, h as i32);

        buffer
            .attach_to(self.layer.wl_surface())
            .expect("buffer attach");

        // Request frame callback BEFORE commit so we get notified for next frame.
        if self.selecting {
            self.layer
                .wl_surface()
                .frame(qh, self.layer.wl_surface().clone());
            self.frame_pending = true;
        }

        self.layer.commit();
    }

    fn draw_hud(&self, pixels: &mut [u32], w: usize, h: usize) {
        if self.preferences.mode == CaptureMode::Area {
            pixels.fill(OVERLAY_PIXEL);
            draw_selection(pixels, w, h, self.start, self.current, self.selection);
        } else if self.preferences.mode == CaptureMode::Window {
            pixels.fill(CLEAR_PIXEL);
            if let Some(point) = self.window_target {
                draw_window_target(pixels, w, h, point);
            }
        } else {
            pixels.fill(CLEAR_PIXEL);
        }
        for button in hud_buttons(w, h, self.preferences) {
            let fill = if button.mode == Some(self.preferences.mode) {
                HUD_ACTIVE_PIXEL
            } else {
                HUD_PIXEL
            };
            fill_rect(
                pixels,
                w,
                button.x,
                button.y,
                button.width,
                button.height,
                fill,
            );
            draw_text(
                pixels,
                w,
                button.x + 14,
                button.y + 17,
                &button.label,
                HUD_TEXT_PIXEL,
            );
        }
    }
}

fn draw_window_target(pixels: &mut [u32], w: usize, h: usize, point: (u32, u32)) {
    let cx = point.0 as usize;
    let cy = point.1 as usize;
    let radius = 18usize;

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

struct HudButton {
    label: String,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    command: SessionCommand,
    mode: Option<CaptureMode>,
}

fn hud_buttons(w: usize, h: usize, preferences: GraphicalPreferences) -> Vec<HudButton> {
    let widths = [72, 96, 72, 96, 118, 80, 112, 96];
    let gap = 8;
    let total: usize = widths.iter().sum::<usize>() + gap * (widths.len() - 1);
    let x = w.saturating_sub(total) / 2;
    let y = h.saturating_sub(76);
    let mut next_x = x;
    let mut button =
        |label: String, width: usize, command: SessionCommand, mode: Option<CaptureMode>| {
            let current_x = next_x;
            next_x += width + gap;
            HudButton {
                label,
                x: current_x,
                y,
                width,
                height: 44,
                command,
                mode,
            }
        };

    vec![
        button(
            "AREA".to_string(),
            widths[0],
            SessionCommand::SetMode(CaptureMode::Area),
            Some(CaptureMode::Area),
        ),
        button(
            "WINDOW".to_string(),
            widths[1],
            SessionCommand::SetMode(CaptureMode::Window),
            Some(CaptureMode::Window),
        ),
        button(
            "FULL".to_string(),
            widths[2],
            SessionCommand::SetMode(CaptureMode::FullScreen),
            Some(CaptureMode::FullScreen),
        ),
        button(
            output_label(preferences.output),
            widths[3],
            SessionCommand::SetOutput(preferences.output.next()),
            None,
        ),
        button(
            location_label(preferences.location),
            widths[4],
            SessionCommand::SetLocation(preferences.location.next()),
            None,
        ),
        button(
            preferences.format.as_str().to_ascii_uppercase(),
            widths[5],
            SessionCommand::SetFormat(preferences.format.next()),
            None,
        ),
        button(
            "CAPTURE".to_string(),
            widths[6],
            SessionCommand::Capture,
            None,
        ),
        button(
            "CANCEL".to_string(),
            widths[7],
            SessionCommand::Cancel,
            None,
        ),
    ]
}

fn output_label(output: OutputDestination) -> String {
    match output {
        OutputDestination::Clipboard => "COPY".to_string(),
        OutputDestination::Save => "SAVE".to_string(),
        OutputDestination::CopyAndSave => "COPY+SAVE".to_string(),
    }
}

fn location_label(location: SaveLocationChoice) -> String {
    match location {
        SaveLocationChoice::Screenshots => "SCREENSHOTS".to_string(),
        SaveLocationChoice::CurrentDirectory => "CURRENT DIR".to_string(),
    }
}

fn fill_rect(
    pixels: &mut [u32],
    surface_w: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    pixel: u32,
) {
    for row in y..(y + height) {
        let start = row * surface_w + x;
        let end = start + width;
        pixels[start..end].fill(pixel);
    }
}

fn draw_text(pixels: &mut [u32], surface_w: usize, x: usize, y: usize, text: &str, pixel: u32) {
    let mut cursor = x;
    for ch in text.chars() {
        draw_char(pixels, surface_w, cursor, y, ch, pixel);
        cursor += 7;
    }
}

fn draw_char(pixels: &mut [u32], surface_w: usize, x: usize, y: usize, ch: char, pixel: u32) {
    let glyph = match ch {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'C' => [0x0F, 0x10, 0x10, 0x10, 0x10, 0x10, 0x0F],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'I' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x1F],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11],
        _ => [0, 0, 0, 0, 0, 0, 0],
    };

    for (row_idx, row) in glyph.iter().enumerate() {
        for col in 0..5 {
            if row & (1 << (4 - col)) != 0 {
                pixels[(y + row_idx) * surface_w + x + col] = pixel;
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
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
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
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.frame_pending = false;
        if self.dirty {
            self.dirty = false;
            self.draw(qh);
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        // Record which output the overlay landed on so we can capture the
        // correct monitor later.
        if let Some(info) = self.output_state.info(output) {
            self.output_name = info.name;
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
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        if configure.new_size.0 > 0 {
            self.surface_w = configure.new_size.0;
        }
        if configure.new_size.1 > 0 {
            self.surface_h = configure.new_size.1;
        }

        // Allocate pool now that we know the surface size.
        let needed = self.surface_w as usize * self.surface_h as usize * 4 * 2;
        if self.pool.len() < needed
            && let Ok(p) = SlotPool::new(needed, &self.shm)
        {
            self.pool = p;
        }

        if self.first_configure {
            self.first_configure = false;
            self.draw(qh);
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
                self.selection = None;
                self.start = None;
                self.current = None;
                self.area_drag = None;
                self.draw(qh);
            }
            _ => {
                if let Some(command) = shortcut_session_command(
                    event.keysym,
                    self.output_name.clone(),
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
        let mut needs_redraw = false;

        for event in events {
            if &event.surface != self.layer.wl_surface() {
                continue;
            }

            let lx = event.position.0;
            let ly = event.position.1;

            match event.kind {
                PointerEventKind::Enter { serial } => {
                    // Set crosshair cursor when entering the overlay surface.
                    if let Some(ref device) = self.cursor_shape_device {
                        device.set_shape(serial, Shape::Crosshair);
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    if self.hud_active {
                        let mode = self.preferences.mode;
                        if button == 0x110 {
                            let x = lx.round() as usize;
                            let y = ly.round() as usize;
                            for hud_button in hud_buttons(
                                self.surface_w as usize,
                                self.surface_h as usize,
                                self.preferences,
                            ) {
                                let inside_x =
                                    x >= hud_button.x && x < hud_button.x + hud_button.width;
                                let inside_y =
                                    y >= hud_button.y && y < hud_button.y + hud_button.height;
                                if inside_x && inside_y {
                                    self.apply_hud_command(hud_button.command, qh);
                                    return;
                                }
                            }
                            if mode == CaptureMode::Area {
                                let point = (lx.round() as i32, ly.round() as i32);
                                self.area_drag = if let Some(rect) = self.selection {
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
                                        self.selection = None;
                                        self.start = Some((lx, ly));
                                        self.current = Some((lx, ly));
                                        Some(AreaDrag::Drawing)
                                    }
                                } else {
                                    self.start = Some((lx, ly));
                                    self.current = Some((lx, ly));
                                    Some(AreaDrag::Drawing)
                                };
                                self.selecting = true;
                                self.draw(qh);
                            } else if mode == CaptureMode::Window {
                                let point = (lx.round() as u32, ly.round() as u32);
                                self.window_target = Some(point);
                                self.hud_result = Some(capture_window_command(
                                    point,
                                    (self.surface_w, self.surface_h),
                                    self.output_name.clone(),
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
                        self.start = Some((lx, ly));
                        self.current = Some((lx, ly));
                        self.area_drag = Some(AreaDrag::Drawing);
                        self.selecting = true;
                        needs_redraw = true;
                    }
                    if button == 0x111 {
                        self.cancelled = true;
                        self.exit = true;
                        return;
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    if button == 0x110 && self.selecting {
                        if matches!(self.area_drag, Some(AreaDrag::Drawing))
                            && let (Some((sx, sy)), Some((cx, cy))) = (self.start, self.current)
                        {
                            let rect = clamp_selection_rect(
                                selection_rect_from_drag((sx, sy), (cx, cy)),
                                (self.surface_w, self.surface_h),
                            );
                            self.selection = Some(rect);
                            self.start = None;
                            self.current = None;
                            if !self.hud_active {
                                self.hud_result = Some(capture_area_command(
                                    rect,
                                    (self.surface_w, self.surface_h),
                                    self.output_name.clone(),
                                    self.preferences,
                                ));
                            }
                        }
                        self.selecting = false;
                        self.area_drag = None;
                        if !self.hud_active {
                            self.exit = true;
                        } else {
                            self.draw(qh);
                        }
                        return;
                    }
                }
                PointerEventKind::Motion { .. } if self.selecting => {
                    match self.area_drag {
                        Some(AreaDrag::Drawing) => self.current = Some((lx, ly)),
                        Some(AreaDrag::Moving {
                            original,
                            pointer_start,
                        }) => {
                            self.selection = Some(move_selection_rect(
                                original,
                                (
                                    lx.round() as i32 - pointer_start.0,
                                    ly.round() as i32 - pointer_start.1,
                                ),
                                (self.surface_w, self.surface_h),
                            ));
                        }
                        Some(AreaDrag::Resizing { original, handle }) => {
                            self.selection = Some(resize_selection_rect(
                                original,
                                handle,
                                (lx.round() as i32, ly.round() as i32),
                                (self.surface_w, self.surface_h),
                            ));
                        }
                        None => {}
                    }
                    needs_redraw = true;
                }
                PointerEventKind::Motion { .. }
                    if self.hud_active && self.preferences.mode == CaptureMode::Window =>
                {
                    self.window_target = Some((lx.round() as u32, ly.round() as u32));
                    needs_redraw = true;
                }
                _ => {}
            }
        }

        if needs_redraw {
            self.dirty = true;
            if !self.frame_pending {
                self.dirty = false;
                self.draw(qh);
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

        draw_window_target(&mut pixels, 20, 20, (10, 10));

        assert_eq!(pixels[10 * 20 + 10], BORDER_PIXEL);
        assert_eq!(pixels[10 * 20], BORDER_PIXEL);
        assert_eq!(pixels[10], BORDER_PIXEL);
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
