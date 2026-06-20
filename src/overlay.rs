//! Wayland layer-shell overlay for interactive area selection.
//!
//! Uses smithay-client-toolkit to create a fullscreen overlay surface on the
//! `Overlay` layer.  A semi-transparent dark tint covers the screen; the
//! selected rectangle is shown as a clear (transparent) area with a white
//! border so the user can see the content underneath.
//! Pointer click-drag selects the area; Escape or right-click cancels.

use crate::session::{CaptureMode, SessionCommand};
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
            LayerSurfaceConfigure,
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

        output_name: None,

        dirty: false,
        frame_pending: false,

        first_configure: true,
        exit: false,
        cancelled: false,
        hud_mode: None,
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
    let output_name = state.output_name;

    if state.cancelled {
        return Ok((None, surf, output_name));
    }

    Ok((state.selection, surf, output_name))
}

pub fn run_screenshot_hud(default_mode: CaptureMode) -> Result<SessionCommand> {
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

        output_name: None,

        dirty: false,
        frame_pending: false,

        first_configure: true,
        exit: false,
        cancelled: false,
        hud_mode: Some(default_mode),
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
        return Ok(SessionCommand::Cancel);
    }

    Ok(state.hud_result.unwrap_or(SessionCommand::Cancel))
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

    /// Name of the output the overlay surface is on (e.g. "eDP-1", "HDMI-A-1").
    output_name: Option<String>,

    /// True when the selection changed and we need a redraw.
    dirty: bool,
    /// True when we've requested a frame callback and are waiting for it.
    frame_pending: bool,

    first_configure: bool,
    exit: bool,
    cancelled: bool,
    hud_mode: Option<CaptureMode>,
    hud_result: Option<SessionCommand>,
}

impl OverlayState {
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

        if let Some(mode) = self.hud_mode {
            self.draw_hud(pixels, w, h, mode);
        } else {
            // Fill entire surface with dark overlay.
            pixels.fill(OVERLAY_PIXEL);

            // Draw selection rectangle at pointer coordinates.
            if let (Some(start), Some(cur)) = (self.start, self.current) {
                let x1 = (start.0.min(cur.0).round() as usize).min(w.saturating_sub(1));
                let y1 = (start.1.min(cur.1).round() as usize).min(h.saturating_sub(1));
                let x2 = (start.0.max(cur.0).round() as usize).min(w);
                let y2 = (start.1.max(cur.1).round() as usize).min(h);

                if x2 > x1 && y2 > y1 {
                    // Clear the selection interior (transparent).
                    for row in y1..y2 {
                        let start = row * w + x1;
                        let end = row * w + x2;
                        pixels[start..end].fill(CLEAR_PIXEL);
                    }

                    // White border (1px).
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

    fn draw_hud(&self, pixels: &mut [u32], w: usize, h: usize, mode: CaptureMode) {
        pixels.fill(CLEAR_PIXEL);
        for button in hud_buttons(w, h) {
            let fill = if button.mode == Some(mode) {
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
                button.label,
                HUD_TEXT_PIXEL,
            );
        }
    }
}

struct HudButton {
    label: &'static str,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    command: SessionCommand,
    mode: Option<CaptureMode>,
}

fn hud_buttons(w: usize, h: usize) -> [HudButton; 5] {
    let widths = [72, 96, 72, 112, 96];
    let gap = 8;
    let total: usize = widths.iter().sum::<usize>() + gap * 4;
    let x = w.saturating_sub(total) / 2;
    let y = h.saturating_sub(76);

    [
        HudButton {
            label: "AREA",
            x,
            y,
            width: widths[0],
            height: 44,
            command: SessionCommand::SetMode(CaptureMode::Area),
            mode: Some(CaptureMode::Area),
        },
        HudButton {
            label: "WINDOW",
            x: x + widths[0] + gap,
            y,
            width: widths[1],
            height: 44,
            command: SessionCommand::SetMode(CaptureMode::Window),
            mode: Some(CaptureMode::Window),
        },
        HudButton {
            label: "FULL",
            x: x + widths[0] + widths[1] + gap * 2,
            y,
            width: widths[2],
            height: 44,
            command: SessionCommand::SetMode(CaptureMode::FullScreen),
            mode: Some(CaptureMode::FullScreen),
        },
        HudButton {
            label: "CAPTURE",
            x: x + widths[0] + widths[1] + widths[2] + gap * 3,
            y,
            width: widths[3],
            height: 44,
            command: SessionCommand::Capture,
            mode: None,
        },
        HudButton {
            label: "CANCEL",
            x: x + widths[0] + widths[1] + widths[2] + widths[3] + gap * 4,
            y,
            width: widths[4],
            height: 44,
            command: SessionCommand::Cancel,
            mode: None,
        },
    ]
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
        if self.hud_mode.is_some() {
            match event.keysym {
                Keysym::Escape => {
                    self.cancelled = true;
                    self.exit = true;
                }
                Keysym::Return => {
                    self.hud_result = Some(SessionCommand::Capture);
                    self.exit = true;
                }
                Keysym::a | Keysym::A => {
                    self.hud_mode = Some(CaptureMode::Area);
                    self.draw(qh);
                }
                Keysym::w | Keysym::W => {
                    self.hud_mode = Some(CaptureMode::Window);
                    self.draw(qh);
                }
                Keysym::f | Keysym::F => {
                    self.hud_mode = Some(CaptureMode::FullScreen);
                    self.draw(qh);
                }
                _ => {}
            }
            return;
        }

        if event.keysym == Keysym::Escape {
            self.cancelled = true;
            self.exit = true;
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
                    if self.hud_mode.is_some() {
                        if button == 0x110 {
                            let x = lx.round() as usize;
                            let y = ly.round() as usize;
                            for hud_button in
                                hud_buttons(self.surface_w as usize, self.surface_h as usize)
                            {
                                let inside_x =
                                    x >= hud_button.x && x < hud_button.x + hud_button.width;
                                let inside_y =
                                    y >= hud_button.y && y < hud_button.y + hud_button.height;
                                if inside_x && inside_y {
                                    match hud_button.command {
                                        SessionCommand::SetMode(mode) => {
                                            self.hud_mode = Some(mode);
                                            self.draw(qh);
                                        }
                                        SessionCommand::Capture => {
                                            self.hud_result = Some(SessionCommand::Capture);
                                            self.exit = true;
                                        }
                                        SessionCommand::Cancel => {
                                            self.cancelled = true;
                                            self.exit = true;
                                        }
                                    }
                                    return;
                                }
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
                        if let (Some((sx, sy)), Some((cx, cy))) = (self.start, self.current) {
                            self.selection = Some(selection_rect_from_drag((sx, sy), (cx, cy)));
                        }
                        self.selecting = false;
                        self.exit = true;
                        return;
                    }
                }
                PointerEventKind::Motion { .. } if self.selecting => {
                    self.current = Some((lx, ly));
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
    fn rounds_selection_drag_to_logical_pixels() {
        assert_eq!(
            selection_rect_from_drag((10.4, 20.5), (25.6, 30.4)),
            (10, 21, 16, 9)
        );
    }
}
