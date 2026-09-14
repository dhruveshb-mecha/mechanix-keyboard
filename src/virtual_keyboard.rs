use interactivity::pointer::MouseButton;
use rustix::fs::{MemfdFlags, SealFlags};
use rustix::mm::{MapFlags, ProtFlags};
use std::collections::{HashMap, HashSet};
use std::os::fd::{AsFd, OwnedFd};
use std::time::Instant;
use utils::Point;
use wayland::{
    Handle, Interface, WlKeyboard, WlKeyboardEvent, WlKeyboardKeyState, WlKeyboardKeymapFormat,
    WlPointer, WlPointerEvent, WlRegistryEvent, WlSeat, WlSeatCapability, WlSeatEvent, WlTouch,
    WlTouchEvent, ZwpVirtualKeyboardManagerV1, ZwpVirtualKeyboardV1,
};
use xkbcommon::xkb::ffi::XKB_KEYMAP_FORMAT_TEXT_V1;
use xkbcommon::xkb::{self, Context, Keycode, Keymap, Keysym, MOD_NAME_CTRL, MOD_NAME_SHIFT};

use crate::layout::{KeyAction, View};
use crate::window::{handle_rect, scale_rect, toggle_visibility, view_and_factor};
use crate::{MechanixKeyboardState, render};

pub struct KeymapWithFd {
    fd: OwnedFd,
    size: u32,
}

impl KeymapWithFd {
    pub fn new(text: &[u8]) -> rustix::io::Result<Self> {
        let (fd, size) = make_keymap_fd(text)?;
        Ok(Self { fd, size })
    }
}

/// One resolved keystroke: the evdev keycode to press plus the modifier mask to
/// hold while pressing it. A level-0 keysym gets `mods == 0`; a level-1 keysym
/// (e.g. `Q`, `exclam`) gets its physical key's keycode plus the Shift mask, so
/// emission reproduces the shifted keysym without any persistent modifier.
#[derive(Debug, Clone, Copy)]
pub struct Keystroke {
    pub code: u32,
    pub mods: u32,
}

pub struct VirtualKeyboardState {
    /// Wayland handles
    pub seat: Option<Handle<WlSeat>>,
    pub pointer: Option<Handle<WlPointer>>,
    pub keyboard: Option<Handle<WlKeyboard>>,
    pub touch: Option<Handle<WlTouch>>,
    pub virtual_keyboard_manager: Option<Handle<ZwpVirtualKeyboardManagerV1>>,
    pub virtual_keyboard: Option<Handle<ZwpVirtualKeyboardV1>>,

    pub start_time: Instant,
    pub keymap: Option<KeymapWithFd>,
    /// keysym → keystroke, scanned from the uploaded keymap's base and shifted
    /// levels. Empty until the keymap is sent; emission is a no-op until then.
    pub keycodes: HashMap<Keysym, Keystroke>,
    /// squeekboard modifier name → its serialized mask, derived from the keymap
    /// (e.g. `Control` → the Control mask). Only latchable modifiers are indexed.
    pub mod_masks: HashMap<String, u32>,
    /// The modifier names currently latched — armed for the next keystroke, which
    /// consumes (clears) them. OSK one-shot semantics; see `toggle_latch`.
    pub latched: HashSet<String>,
}

impl VirtualKeyboardState {
    pub fn new() -> Self {
        Self {
            seat: None,
            pointer: None,
            keyboard: None,
            touch: None,
            virtual_keyboard_manager: None,
            virtual_keyboard: None,
            start_time: Instant::now(),
            keymap: None,
            keycodes: HashMap::new(),
            mod_masks: HashMap::new(),
            latched: HashSet::new(),
        }
    }
}

pub fn module<S>() -> impl app::RegisteredModule<MechanixKeyboardState, S> {
    app::Module::new()
        .on(on_registry)
        .on(on_seat)
        .on(on_pointer)
        .on(on_keyboard)
        .on(on_touch)
}

/// Bind to the globals when the registry advertises them.
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
        WlSeat::NAME => s.virtual_keyboard_state.seat = Some(sender.bind(*name, *version)),
        ZwpVirtualKeyboardManagerV1::NAME => {
            s.virtual_keyboard_state.virtual_keyboard_manager = Some(sender.bind(*name, *version))
        }
        _ => {}
    }
    if s.virtual_keyboard_state.seat.is_some()
        && s.virtual_keyboard_state.virtual_keyboard_manager.is_some()
    {
        init(s);
    }
}

/// Create the virtual keyboard after the globals are available.
fn init(s: &mut MechanixKeyboardState) {
    let (Some(seat), Some(manager)) = (
        s.virtual_keyboard_state.seat.clone(),
        s.virtual_keyboard_state.virtual_keyboard_manager.clone(),
    ) else {
        tracing::error!("could not find globals!");
        return;
    };

    let vkbd = manager.create_virtual_keyboard(&seat);

    // Compile a standard keymap from the default rules and serialise it.
    let ctx = Context::new(0);
    let keymap = Keymap::new_from_names(&ctx, "", "", "us", "", None, 0)
        .expect("failed to compile default keymap");

    // Index the keymap's base level (layout 0, level 0) so a tapped keysym maps
    // to the evdev keycode to send. `evdev = xkb_keycode - 8`; first key wins for
    // a keysym that appears on more than one physical key.
    let keycodes = scan_keycodes(&keymap);

    let text = keymap.get_as_string(XKB_KEYMAP_FORMAT_TEXT_V1);
    let keymap_fd = match KeymapWithFd::new(text.as_bytes()) {
        Ok(keymap) => keymap,
        Err(err) => {
            tracing::warn!(%err, "failed to create keymap memfd");
            return;
        }
    };

    vkbd.keymap(
        WlKeyboardKeymapFormat::XkbV1,
        keymap_fd.fd.as_fd(),
        keymap_fd.size,
    );

    // Index the modifiers a latched key can arm, mapping the squeekboard name
    // used in the layout to the serialized mask sent over the wire. Control only
    // this pass — mirrors `layout::resolve_action`'s modifier gate.
    let mut mod_masks = HashMap::new();
    mod_masks.insert(
        "Control".to_string(),
        1u32 << keymap.mod_get_index(MOD_NAME_CTRL),
    );

    tracing::info!(mapped = keycodes.len(), "virtual keyboard ready");
    s.virtual_keyboard_state.virtual_keyboard = Some(vkbd);
    s.virtual_keyboard_state.keymap = Some(keymap_fd);
    s.virtual_keyboard_state.keycodes = keycodes;
    s.virtual_keyboard_state.mod_masks = mod_masks;
}

/// Bind keyboard/pointer/touch as the seat reports having them.
fn on_seat(s: &mut MechanixKeyboardState, event: &WlSeatEvent) {
    let WlSeatEvent::Capabilities { capabilities, .. } = event else {
        return;
    };
    let Some(seat) = s.virtual_keyboard_state.seat.clone() else {
        return;
    };
    if capabilities.contains(WlSeatCapability::Keyboard)
        && s.virtual_keyboard_state.keyboard.is_none()
    {
        s.virtual_keyboard_state.keyboard = Some(seat.get_keyboard());
    }
    if capabilities.contains(WlSeatCapability::Pointer)
        && s.virtual_keyboard_state.pointer.is_none()
    {
        s.virtual_keyboard_state.pointer = Some(seat.get_pointer());
    }
    if capabilities.contains(WlSeatCapability::Touch) && s.virtual_keyboard_state.touch.is_none() {
        s.virtual_keyboard_state.touch = Some(seat.get_touch());
    }
}

// ── input → interactivity ──────────────────────────────────────────────────

fn on_keyboard(s: &mut MechanixKeyboardState, event: &WlKeyboardEvent) {
    s.interactivity.call_before_frame();
    s.interactivity.process_keyboard(event);
    tracing::debug!(
        just_pressed = ?s.interactivity.keyboard.just_pressed_keys(),
        just_released = ?s.interactivity.keyboard.just_released_keys(),
        modifiers = ?s.interactivity.keyboard.modifiers(),
        "keyboard input",
    );
}

fn on_pointer(s: &mut MechanixKeyboardState, event: &WlPointerEvent) {
    s.interactivity.call_before_frame();
    s.interactivity.process_pointer(event);

    // Copy the surface-local points out before the keymap borrow, so the
    // interactivity borrow is released for the hit-test below.
    let position = s.interactivity.pointer.position();
    let pressed = s
        .interactivity
        .pointer
        .just_pressed_position(MouseButton::Left)
        .copied();

    // A click on the Handle toggles Bar visibility, in either state. The Handle
    // sits below the keys, so it never overlaps a key's touch area.
    if let Some(hr) = handle_rect(s) {
        if pressed.is_some_and(|p| hr.contains_point(p)) {
            toggle_visibility(s);
            return;
        }
    }

    // Keys are only live while shown; when hidden, clear any stale hover.
    if !s.window.as_ref().is_some_and(|w| w.visible) {
        if s.last_hover.take().is_some() {
            tracing::info!("hover: none");
        }
        return;
    }

    // Resolve the hover label and the clicked key's action while the keymap is
    // borrowed, then act after the borrow ends (emitting needs `&mut s`).
    let (hover, clicked) = {
        let Some((view, f)) = view_and_factor(s) else {
            return;
        };
        let hover = key_at(view, f, position);
        let clicked = pressed.and_then(|p| action_at(view, f, p));
        (hover, clicked)
    };

    // Click: type the key the left button went down on this frame.
    if let Some(action) = clicked {
        dispatch_action(s, &action);
    }

    // Hover: print only when the key under the pointer changes.
    if hover != s.last_hover {
        match &hover {
            Some(label) => tracing::info!(key = %label, "hover"),
            None => tracing::info!("hover: none"),
        }
        s.last_hover = hover;
    }
}

fn on_touch(s: &mut MechanixKeyboardState, event: &WlTouchEvent) {
    s.interactivity.call_before_frame();
    s.interactivity.process_touch(event);

    // A tap on the Handle toggles Bar visibility, in either state. Check it
    // first; the Handle sits below the keys, so it never overlaps a key.
    if let Some(hr) = handle_rect(s) {
        if s.interactivity.touch.tapped(hr) {
            toggle_visibility(s);
            return;
        }
    }

    // Keys are only live while shown.
    if !s.window.as_ref().is_some_and(|w| w.visible) {
        return;
    }

    // Probe each key's touch area for a tap that landed and completed this frame,
    // cloning the tapped key's action out so the keymap borrow ends before we
    // emit (which needs `&mut s`).
    let tapped = {
        let Some((view, f)) = view_and_factor(s) else {
            return;
        };
        view.keys()
            .find(|key| s.interactivity.touch.tapped(scale_rect(key.touch_area, f)))
            .map(|key| key.action.clone())
    };

    if let Some(action) = tapped {
        dispatch_action(s, &action);
    }
}

/// Route a tapped key's action. A view switch mutates the Current view; a
/// modifier latch arms/disarms; both repaint here. Every other action is a
/// keystroke the virtual keyboard emits (which also repaints if it consumes a
/// latch, so the armed highlight clears).
fn dispatch_action(s: &mut MechanixKeyboardState, action: &KeyAction) {
    let target = match action {
        KeyAction::SetView(name) => name.as_str(),
        KeyAction::ToggleView { lock, unlock } => {
            // Toggle by current view: if the lock view is already showing, go
            // back to `unlock`; otherwise switch to `lock`.
            let current = s.current_view().map(|v| v.name.as_str());
            if current == Some(lock.as_str()) {
                unlock.as_str()
            } else {
                lock.as_str()
            }
        }
        KeyAction::LatchModifier(name) => {
            // Arm/disarm the modifier and repaint so its key shows the change.
            toggle_latch(s, name);
            render::render(s);
            return;
        }
        _ => {
            // Emit; if a latch was armed, it's now consumed, so repaint to drop
            // the highlight. Compare the armed count across the emit.
            let armed_before = s.virtual_keyboard_state.latched.len();
            emit_action(s, action);
            if s.virtual_keyboard_state.latched.len() != armed_before {
                render::render(s);
            }
            return;
        }
    };
    switch_view(s, target);
}

/// Switch the Current view to the named one and repaint. A no-op (no repaint) if
/// the name is unknown or already current.
fn switch_view(s: &mut MechanixKeyboardState, name: &str) {
    let Some(idx) = s.keymap.as_ref().and_then(|km| km.index_of(name)) else {
        tracing::warn!(view = %name, "view switch to unknown view; ignored");
        return;
    };
    if idx == s.current_view {
        return;
    }
    s.current_view = idx;
    tracing::info!(view = %name, "switched view");
    render::render(s);
}

/// Label of the first key whose (scaled) touch area contains `p`, else `None`.
fn key_at(view: &View, f: f32, p: Point) -> Option<String> {
    view.keys()
        .find(|k| scale_rect(k.touch_area, f).contains_point(p))
        .map(|k| k.display_label().to_string())
}

/// Action of the first key whose (scaled) touch area contains `p`, cloned.
fn action_at(view: &View, f: f32, p: Point) -> Option<KeyAction> {
    view.keys()
        .find(|k| scale_rect(k.touch_area, f).contains_point(p))
        .map(|k| k.action.clone())
}

/// Build the `keysym → Keystroke` map from a compiled keymap's base (level 0) and
/// shifted (level 1) levels. A level-0 keysym stores an empty modifier mask; a
/// level-1 keysym stores the Shift mask, so tapping it emits the shifted keysym.
/// Levels are scanned low-to-high with `or_insert`, so the fewest-modifiers form
/// wins for a keysym present at both (e.g. a keysym that is its own shift).
fn scan_keycodes(keymap: &Keymap) -> HashMap<Keysym, Keystroke> {
    let shift = 1u32 << keymap.mod_get_index(MOD_NAME_SHIFT);
    let mut map = HashMap::new();
    for (level, mods) in [(0u32, 0u32), (1u32, shift)] {
        for kc in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
            if kc < 8 {
                continue;
            }
            let syms = keymap.key_get_syms_by_level(Keycode::new(kc), 0, level);
            if let Some(ks) = syms.first().copied()
                && ks.raw() != 0
            {
                map.entry(ks).or_insert(Keystroke { code: kc - 8, mods });
            }
        }
    }
    map
}

/// Emit a Key action over the virtual keyboard: a keysym (or each char of a text
/// run) becomes a keycode down+up; unwired actions just log. A keystroke emission
/// consumes any latched modifiers (one-shot); an `Unhandled` tap does not, so a
/// latch stays armed until a real key fires.
pub fn emit_action(s: &mut MechanixKeyboardState, action: &KeyAction) {
    match action {
        KeyAction::EmitKeysym(ks) => {
            emit_keysym(s, *ks);
            consume_latch(s);
        }
        KeyAction::EmitText(text) => {
            for ch in text.chars() {
                emit_keysym(s, Keysym::from_char(ch));
            }
            consume_latch(s);
        }
        KeyAction::Unhandled(name) => {
            tracing::info!(action = %name, "tapped key with no wired action");
        }
        // View switches and latches are peeled off by `window::dispatch_action`
        // before this; reaching here means a dispatch bug, not a keystroke.
        KeyAction::SetView(_) | KeyAction::ToggleView { .. } | KeyAction::LatchModifier(_) => {
            tracing::error!("non-emitting action reached the virtual keyboard; dispatch bug");
        }
    }
}

/// Toggle a modifier's latch: arm it if idle, disarm it if already armed (a
/// second tap cancels). No-op with a warning for a modifier we have no mask for.
/// The armed state is one-shot — the next keystroke emission clears it.
pub fn toggle_latch(s: &mut MechanixKeyboardState, name: &str) {
    if !s.virtual_keyboard_state.mod_masks.contains_key(name) {
        tracing::warn!(modifier = %name, "modifier has no mask; not latched");
        return;
    }
    if s.virtual_keyboard_state.latched.remove(name) {
        tracing::info!(modifier = %name, "modifier latch cleared");
    } else {
        s.virtual_keyboard_state.latched.insert(name.to_string());
        tracing::info!(modifier = %name, "modifier latched; armed for next key");
    }
}

/// The combined mask of every currently-latched modifier, to OR into a keystroke.
fn latched_mask(s: &MechanixKeyboardState) -> u32 {
    s.virtual_keyboard_state
        .latched
        .iter()
        .filter_map(|n| s.virtual_keyboard_state.mod_masks.get(n))
        .fold(0, |acc, m| acc | m)
}

/// Clear all latched modifiers after a keystroke fires (one-shot). No-op when
/// nothing is armed, so callers can invoke it unconditionally.
fn consume_latch(s: &mut MechanixKeyboardState) {
    if !s.virtual_keyboard_state.latched.is_empty() {
        s.virtual_keyboard_state.latched.clear();
        tracing::debug!("latched modifiers consumed");
    }
}

/// Send one keysym as a keycode down+up, holding the keystroke's modifiers around
/// it (e.g. Shift for an uppercase or shifted keysym) and clearing them after.
/// No-ops (with a log) if the keysym isn't in the uploaded keymap or the keyboard
/// isn't ready yet.
fn emit_keysym(s: &mut MechanixKeyboardState, ks: Keysym) {
    let name = xkb::keysym_get_name(ks);
    let Some(&stroke) = s.virtual_keyboard_state.keycodes.get(&ks) else {
        tracing::warn!(keysym = %name, "keysym absent from keymap; not typed");
        return;
    };
    let Some(vkbd) = s.virtual_keyboard_state.virtual_keyboard.clone() else {
        tracing::warn!(keysym = %name, "virtual keyboard not ready; key dropped");
        return;
    };
    // Combine the keystroke's own modifiers (e.g. Shift for a shifted keysym)
    // with any latched modifiers (e.g. an armed Ctrl) — a latched Ctrl over an
    // upper-view key yields Ctrl+Shift+key.
    let mods = stroke.mods | latched_mask(s);
    let time = (Instant::now() - s.virtual_keyboard_state.start_time).as_millis() as u32;
    // Depress the combined modifiers before the key so the receiving app maps the
    // keycode to the right level; clear them after so nothing lingers. A bare
    // level-0 key with no latch (mask 0) skips this — wire traffic is unchanged.
    if mods != 0 {
        vkbd.modifiers(mods, 0, 0, 0);
    }
    vkbd.key(time, stroke.code, WlKeyboardKeyState::Pressed.into());
    vkbd.key(time, stroke.code, WlKeyboardKeyState::Released.into());
    if mods != 0 {
        vkbd.modifiers(0, 0, 0, 0);
    }
    tracing::info!(keysym = %name, code = stroke.code, mods, "typed");
}

/// Builds a sealed, shared memfd holding `text` as a NUL-terminated
/// buffer, ready to send as `set_keymap`'s fd + size.
pub fn make_keymap_fd(text: &[u8]) -> rustix::io::Result<(OwnedFd, u32)> {
    let size = text.len() + 1; // +1 for the trailing NUL the protocol expects

    let fd: OwnedFd = rustix::fs::memfd_create(
        c"mechanix-keyboard-keymap",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?;

    rustix::fs::ftruncate(&fd, size as u64)?;

    // SAFETY: `fd` is a valid memfd truncated to `size` bytes; the mapping
    // is unmapped (via the guard below) before this function returns, and
    // nothing else touches `fd` concurrently.
    let map = unsafe {
        rustix::mm::mmap(
            std::ptr::null_mut(),
            size,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            &fd,
            0,
        )?
    };

    struct MmapGuard(*mut core::ffi::c_void, usize);
    impl Drop for MmapGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = rustix::mm::munmap(self.0, self.1);
            }
        }
    }
    let guard = MmapGuard(map, size);

    // SAFETY: `guard.0` points to `size` writable, exclusively-mapped bytes.
    // We write `text.len()` bytes then the NUL, totaling exactly `size`
    // bytes, so this cannot read or write out of bounds. The mapping was
    // freshly ftruncate'd, so the trailing byte is already zero, but we
    // set it explicitly for clarity/robustness.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), guard.0.cast(), text.len());
        *(guard.0 as *mut u8).add(text.len()) = 0;
    }
    drop(guard);

    rustix::fs::fcntl_add_seals(
        &fd,
        SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE | SealFlags::SEAL,
    )?;

    Ok((fd, size as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same default US keymap the running app uploads.
    fn us_keymap() -> Keymap {
        let ctx = Context::new(0);
        Keymap::new_from_names(&ctx, "", "", "us", "", None, 0).expect("compile us keymap")
    }

    #[test]
    fn shifted_keysym_carries_shift_on_the_same_key() {
        let keymap = us_keymap();
        let map = scan_keycodes(&keymap);

        // A plain lowercase letter is level 0: a bare keycode, no modifiers.
        let q = map[&Keysym::from_char('q')];
        assert_eq!(q.mods, 0, "lowercase q should need no modifiers");

        // Its uppercase reaches the *same physical key*, plus a non-empty mask —
        // this is exactly what was missing before (uppercase typed nothing).
        let cap_q = map[&Keysym::from_char('Q')];
        assert_eq!(cap_q.code, q.code, "Q must be the q key, shifted");
        assert_ne!(cap_q.mods, 0, "Q must carry the Shift mask");

        // The upper view's symbols ride the same mechanism: `!` is Shift+1.
        let one = map[&Keysym::from_char('1')];
        let bang = map[&Keysym::from_char('!')];
        assert_eq!(one.mods, 0, "digit 1 should need no modifiers");
        assert_eq!(bang.code, one.code, "! must be the 1 key, shifted");
        assert_ne!(bang.mods, 0, "! must carry the Shift mask");
    }
}
