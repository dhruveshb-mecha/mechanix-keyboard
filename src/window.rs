use utils::Rect;
use wayland::*;

use crate::MechanixKeyboardState;
use crate::layout::View;
use crate::render;

/// Height in logical px of the Handle — the full-width band at the bottom edge,
/// always drawn, whose tap toggles Bar visibility. It is the bar's entire height
/// when hidden, and is added below the aspect-locked keyboard when shown.
pub const HANDLE_HEIGHT: u32 = 40;

#[derive(Default)]
pub struct WaylandGlobals {
    pub compositor: Option<Handle<WlCompositor>>,
    pub output: Option<Handle<WlOutput>>,
    pub layer_shell: Option<Handle<ZwlrLayerShellV1>>,
    pub dmabuf: Option<Handle<ZwpLinuxDmabufV1>>,
}

pub struct WindowState {
    pub surface: Handle<WlSurface>,
    pub layer_surface: Handle<ZwlrLayerSurfaceV1>,
    pub slots: Option<[render::Slot; 2]>,
    pub back: usize,
    pub physical_width: u32,
    pub physical_height: u32,

    pub logical_width: u32,
    pub logical_height: u32,
    /// The logical height we last asked the compositor for, so we only re-request
    /// (and wait for another `Configure`) when the aspect-derived height changes.
    pub requested_height: u32,
    /// A frame callback fired while the back buffer was still in flight; draw as
    /// soon as its `wl_buffer.release` lands.
    pub pending_frame: bool,
    /// Bar visibility: `true` shows the keyboard above the Handle, `false` shows
    /// only the Handle. Flipped by a Handle tap; drives the surface height. Starts
    /// hidden.
    pub visible: bool,
}

/// Kick off the registry roundtrip that discovers the globals.
fn on_start(s: &mut MechanixKeyboardState, _: &app::Start) {
    s.wayland.display().get_registry();
    s.wayland.display().sync();
}

/// Push queued requests to the compositor each poll.
fn on_pre_poll(s: &mut MechanixKeyboardState, _: &app::PrePoll) {
    s.wayland.proxy().flush();
}

/// Bind the globals the bar needs as the registry advertises them.
fn on_registry(s: &mut MechanixKeyboardState, event: &WlRegistryEvent) {
    let WlRegistryEvent::Global {
        sender,
        name,
        interface,
        version,
    } = event
    else {
        return;
    };
    match interface.as_str() {
        WlCompositor::NAME => s.globals.compositor = Some(sender.bind(*name, *version)),
        ZwlrLayerShellV1::NAME => s.globals.layer_shell = Some(sender.bind(*name, *version)),
        WlOutput::NAME => s.globals.output = Some(sender.bind(*name, *version)),
        ZwpLinuxDmabufV1::NAME => s.globals.dmabuf = Some(sender.bind(*name, *version)),
        _ => {}
    }
}

/// Track the output's buffer-scale factor (HiDPI). Drives the physical buffer
/// size and `wl_surface.set_buffer_scale` so text stays crisp on a 2× display.
fn on_output(s: &mut MechanixKeyboardState, event: &WlOutputEvent) {
    let WlOutputEvent::Scale { factor, .. } = event else {
        return;
    };
    if *factor > 0 {
        s.scale = *factor;
        tracing::info!("output buffer-scale: {factor}");
    }
}

/// Registry roundtrip done: globals are in, so create the layer surface and
/// commit it (no buffer yet — that waits for the first `configure`).
fn create_window(s: &mut MechanixKeyboardState) {
    if s.window.is_some() {
        return;
    }
    let (Some(compositor), Some(layer_shell)) = (&s.globals.compositor, &s.globals.layer_shell)
    else {
        return;
    };

    let surface = compositor.create_surface();
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        None,
        ZwlrLayerShellV1Layer::Top,
        "mechanix-keyboard",
    );
    // Start hidden: the bar maps as just the Handle. Its height is fixed (no
    // aspect dance needed), so the first Configure matches this request directly.
    layer_surface.set_size(0, HANDLE_HEIGHT);
    layer_surface.set_anchor(
        ZwlrLayerSurfaceV1Anchor::Bottom
            | ZwlrLayerSurfaceV1Anchor::Left
            | ZwlrLayerSurfaceV1Anchor::Right,
    );
    // Reserve a constant Handle-height zone in both states, so toggling never
    // reflows other clients; a shown keyboard overlaps the app's bottom content.
    layer_surface.set_exclusive_zone(HANDLE_HEIGHT as i32);
    layer_surface.set_keyboard_interactivity(ZwlrLayerSurfaceV1KeyboardInteractivity::None);
    surface.commit();

    s.window = Some(WindowState {
        surface,
        layer_surface,
        slots: None,
        back: 0,
        physical_width: 0,
        physical_height: 0,
        logical_width: 0,
        logical_height: 0,
        requested_height: HANDLE_HEIGHT,
        pending_frame: false,
        visible: false,
    });
}

/// One `wl_callback.done`: either a frame callback we requested (repaint) or the
/// initial registry roundtrip (create the surface).
fn on_callback(s: &mut MechanixKeyboardState, event: &WlCallbackEvent) {
    let WlCallbackEvent::Done { sender, .. } = event;
    let Some(id) = sender.object_id() else {
        return;
    };
    if s.frame_callbacks.remove(&id) {
        // Frame callback: repaint (static colour today, ready for a live UI
        // once there are keys to draw).
        render::render(s);
    } else {
        create_window(s);
    }
}

/// Compositor sized the surface. The layer-shell grants the width (we anchor
/// left+right); we infer the height from it, aspect-locked to the rendered
/// view. On the first `Configure` the granted height won't match that inference,
/// so we re-request the derived height and wait for the next `Configure`; once
/// it matches, we allocate physical-resolution slots and present the first frame.
fn on_configure(s: &mut MechanixKeyboardState, event: &ZwlrLayerSurfaceV1Event) {
    let ZwlrLayerSurfaceV1Event::Configure {
        serial,
        width,
        height: _,
        ..
    } = event
    else {
        return;
    };
    let Some(dmabuf) = s.globals.dmabuf.clone() else {
        return;
    };
    let scale = s.scale.max(1);

    // The current view's intrinsic logical size drives the keyboard's aspect
    // ratio. All views share it, so the keyboard height is sized from one view
    // and never resized on a view switch. Bar visibility is a separate axis that
    // *does* resize: the Handle height is added when shown and is the whole bar
    // when hidden.
    let (view_w, view_h) = {
        let Some(view) = s.current_view() else {
            return;
        };
        (view.width(), view.height())
    };
    if view_w <= 0.0 {
        return;
    }

    // Resolve width, ack, and decide whether we still need to re-request height.
    let ready = {
        let Some(window) = s.window.as_mut() else {
            return;
        };
        window.layer_surface.ack_configure(*serial);

        let logical_w = if *width == 0 {
            window.logical_width.max(1)
        } else {
            *width
        };
        window.logical_width = logical_w;

        // Shown: aspect-locked keyboard height plus the Handle. Hidden: Handle
        // only, a fixed height that never depends on the granted width.
        let desired_h = if window.visible {
            let kb_h = (view_h * logical_w as f32 / view_w).round() as u32;
            if kb_h == 0 {
                return;
            }
            kb_h + HANDLE_HEIGHT
        } else {
            HANDLE_HEIGHT
        };

        if window.requested_height != desired_h {
            // Ask for the height this visibility state wants and wait for the
            // next Configure.
            window.requested_height = desired_h;
            window.layer_surface.set_size(0, desired_h);
            window.surface.commit();
            None
        } else {
            Some((logical_w, desired_h))
        }
    };
    let Some((logical_w, logical_h)) = ready else {
        return;
    };

    // (Re)allocate physical-resolution slots whenever the buffer size changes —
    // at first map, and on every visibility toggle that resizes the bar.
    let (buf_w, buf_h) = (logical_w * scale as u32, logical_h * scale as u32);
    let needs_alloc = s.window.as_ref().is_some_and(|w| {
        w.slots.is_none() || w.physical_width != buf_w || w.physical_height != buf_h
    });
    if needs_alloc {
        {
            let window = s.window.as_mut().expect("window exists");
            window.logical_height = logical_h;
            window.physical_width = buf_w;
            window.physical_height = buf_h;
            window.surface.set_buffer_scale(scale);
        }
        let slots = render::alloc_slots(&mut s.renderer, &dmabuf, buf_w, buf_h);
        s.window.as_mut().expect("window exists").slots = Some(slots);
    }

    render::render(s);
}

/// The compositor handed a buffer back; mark it drawable and service any frame
/// that was waiting on it.
fn on_buffer_release(s: &mut MechanixKeyboardState, event: &WlBufferEvent) {
    let WlBufferEvent::Release { sender } = event;
    let Some(id) = sender.object_id() else {
        return;
    };
    if let Some(slots) = s.window.as_mut().and_then(|w| w.slots.as_mut()) {
        for slot in slots.iter_mut() {
            if slot.buffer_id == id {
                slot.released = true;
            }
        }
    }
    if s.window.as_ref().map_or(false, |w| w.pending_frame) {
        render::render(s);
    }
}

// ── window interactivity helpers ──────────────────────────────────────────────────

/// The Handle's rect in surface-local coordinates: the bottom `HANDLE_HEIGHT`
/// band, full width. `None` until the surface size is known. When hidden the
/// bar is only this tall, so the Handle is the whole surface.
pub(crate) fn handle_rect(s: &MechanixKeyboardState) -> Option<Rect> {
    let window = s.window.as_ref()?;
    if window.logical_width == 0 || window.logical_height == 0 {
        return None;
    }
    let h = HANDLE_HEIGHT as f32;
    Some(Rect::new(
        0.0,
        window.logical_height as f32 - h,
        window.logical_width as f32,
        h,
    ))
}

/// Flip keyboard visibility and re-request the matching bar height. The resulting
/// `Configure` reallocates slots at the new size and repaints.
pub(crate) fn toggle_visibility(s: &mut MechanixKeyboardState) {
    let visible = s.window.as_mut().expect("window exists").visible;
    set_visibility(s, !visible);
    tracing::info!("toggled keyboard visibility");
}

/// Sets keyboard window visibility
pub(crate) fn set_visibility(s: &mut MechanixKeyboardState, visible: bool) {
    if let Some(window) = &s.window
        && window.visible == visible
    {
        return;
    }
    let logical_w = match s.window.as_ref() {
        Some(w) if w.logical_width > 0 => w.logical_width,
        _ => return,
    };
    let (view_w, view_h) = match s.current_view() {
        Some(v) if v.width() > 0.0 => (v.width(), v.height()),
        _ => return,
    };
    let window = s.window.as_mut().expect("window exists");
    window.visible = visible;
    let target = if window.visible {
        (view_h * logical_w as f32 / view_w).round() as u32 + HANDLE_HEIGHT
    } else {
        HANDLE_HEIGHT
    };
    window.requested_height = target;
    window.layer_surface.set_size(0, target);
    window.surface.commit();
    tracing::info!(
        visible = window.visible,
        target,
        "toggled keyboard visibility"
    );
}

/// The rendered view and the factor mapping its logical layout units onto
/// surface-local (input) coordinates: `f = logical_width / view_width`. Returns
/// `None` until the keymap and surface width are known.
pub(crate) fn view_and_factor(s: &MechanixKeyboardState) -> Option<(&View, f32)> {
    let view = s.current_view()?;
    let view_w = view.width();
    let logical_w = s.window.as_ref()?.logical_width;
    if view_w <= 0.0 || logical_w == 0 {
        return None;
    }
    Some((view, logical_w as f32 / view_w))
}

/// Scale a layout-unit rect into surface-local coordinates.
pub(crate) fn scale_rect(r: Rect, f: f32) -> Rect {
    Rect::new(r.x() * f, r.y() * f, r.width() * f, r.height() * f)
}

pub fn module<S>() -> impl app::RegisteredModule<MechanixKeyboardState, S> {
    app::Module::new()
        .on(on_start)
        .on(on_pre_poll)
        .on(on_registry)
        .on(on_output)
        .on(on_callback)
        .on(on_configure)
        .on(on_buffer_release)
}
