//! `zwp_input_method_v2` — the input-method-v2 protocol client.
//!
//! The compositor advertises `zwp_input_method_manager_v2`; we bind it and obtain
//! a per-seat `zwp_input_method_v2` object. That object is a *double-buffered*
//! state machine in both directions:
//!
//! - **Inbound (compositor → us):** the `activate`/`deactivate`/
//!   `surrounding_text`/`text_change_cause`/`content_type` events populate the
//!   *pending* context; `done` atomically applies it to the *current* context
//!   and bumps the serial we echo back. This mirrors text-input-v3's semantics.
//! - **Outbound (us → compositor):** `commit_string`/`set_preedit_string`/
//!   `delete_surrounding_text` populate a *pending edit*; `commit(serial)`
//!   flushes it. Each field is set (not accumulated), exactly as the protocol's
//!   "initial value" reset-on-commit rule demands.
//!
//! The keyboard uses this as the idiomatic text-commit path: when a text input is
//! focused the compositor sends `activate`, taps commit text through here, and
//! the virtual-keyboard-v1 transport stays as the keysym fallback.

use wayland::{
    Handle, Interface, WlRegistryEvent, WlSeat, ZwpInputMethodManagerV2, ZwpInputMethodV2,
    ZwpInputMethodV2Event, ZwpTextInputV3ChangeCause, ZwpTextInputV3ContentHint,
    ZwpTextInputV3ContentPurpose,
};

use crate::{MechanixKeyboardState, window::set_visibility};

/// The text-input context the compositor reports for the focused field.
///
/// Inbound state is double-buffered: events fill `pending`, `Done` copies it
/// into `current`. Fields mirror the protocol's "initial values" so a context
/// that never reported surrounding text reads as "unsupported" (empty), exactly
/// as the spec requires.
#[derive(Debug, Clone)]
pub struct InputMethodContext {
    /// `true` once `activate` has applied (on a `done`); `false` after
    /// `deactivate` applies. Drives whether taps route through IM2.
    pub active: bool,
    /// The last reported surrounding text, cursor, and selection anchor. Empty
    /// text means the text input does not support surrounding text.
    pub surrounding: SurroundingText,
    /// Why the surrounding text last changed. Initial value is `InputMethod`.
    pub change_cause: ZwpTextInputV3ChangeCause,
    /// The focused field's content hint + purpose. Initial `none`/`normal`.
    pub content_type: ContentType,
}

impl Default for InputMethodContext {
    fn default() -> Self {
        // The protocol documents these as the initial values applied on a
        // `done`: cause `input_method`, hint `none`, purpose `normal`, no
        // surrounding text, inactive.
        Self {
            active: false,
            surrounding: SurroundingText::default(),
            change_cause: ZwpTextInputV3ChangeCause::InputMethod,
            content_type: ContentType::default(),
        }
    }
}

/// Surrounding-text slice reported by the text input (excluding any preedit).
#[derive(Debug, Clone, Default)]
pub struct SurroundingText {
    pub text: String,
    pub cursor: u32,
    pub anchor: u32,
    /// `true` once a `surrounding_text` event has been seen in the pending
    /// batch. Distinguishes "reported empty" from "never reported" so we honour
    /// the spec's "ignore following surrounding_text events" gate.
    pub reported: bool,
}

/// Content type hint + purpose for the focused field.
#[derive(Debug, Clone, Copy)]
pub struct ContentType {
    pub hint: ZwpTextInputV3ContentHint,
    pub purpose: ZwpTextInputV3ContentPurpose,
}

impl Default for ContentType {
    fn default() -> Self {
        // Initial values per protocol: hint `none`, purpose `normal`.
        Self {
            hint: ZwpTextInputV3ContentHint::None,
            purpose: ZwpTextInputV3ContentPurpose::Normal,
        }
    }
}

/// One staged preedit string. `None` means "no preedit change this commit";
/// the protocol's initial value is an empty string with `cursor_begin == 0`.
#[derive(Debug, Clone)]
pub struct Preedit {
    pub text: String,
    pub cursor_begin: i32,
    pub cursor_end: i32,
}

/// One staged surrounding-text deletion. `None` means "no deletion this commit";
/// the protocol's initial values are `before == 0, after == 0` (a no-op).
#[derive(Debug, Clone, Copy)]
pub struct DeleteSurrounding {
    pub before: u32,
    pub after: u32,
}

/// Outbound edits being staged for the next `commit`. Each field is `Option`,
/// distinguishing "explicitly set this commit" from "leave at initial value",
/// which is exactly the double-buffered set-vs-accumulate distinction the
/// protocol makes.
#[derive(Debug, Default)]
struct PendingEdit {
    commit_string: Option<String>,
    preedit: Option<Preedit>,
    delete: Option<DeleteSurrounding>,
}

/// All state the input-method-v2 client owns. Lives on `MechanixKeyboardState`
/// alongside the virtual-keyboard-v1 state.
pub struct InputMethodState {
    /// Bound `zwp_input_method_manager_v2` global.
    pub manager: Option<Handle<ZwpInputMethodManagerV2>>,
    /// The per-seat `zwp_input_method_v2` object; `None` until both the manager
    /// and a seat are available.
    pub input_method: Option<Handle<ZwpInputMethodV2>>,

    /// The applied (post-`done`) inbound context — what the keyboard reads.
    pub current: InputMethodContext,
    /// The pending inbound context — what events are filling this batch.
    pending: InputMethodContext,

    /// Number of `done` events received. This is the serial we echo back in
    /// `commit(serial)`; the compositor ignores commits whose serial doesn't
    /// match, per the protocol.
    serial: u32,

    /// Outbound edits staged since the last `commit`. Flushed by `flush`.
    pending_edit: PendingEdit,

    /// `true` after `unavailable` — the object is inert; all further requests
    /// (except `destroy`) must be ignored.
    inert: bool,
}

impl Default for InputMethodState {
    fn default() -> Self {
        Self {
            manager: None,
            input_method: None,
            // `change_cause`'s documented initial value is `InputMethod`.
            current: InputMethodContext {
                change_cause: ZwpTextInputV3ChangeCause::InputMethod,
                ..Default::default()
            },
            pending: InputMethodContext {
                change_cause: ZwpTextInputV3ChangeCause::InputMethod,
                ..Default::default()
            },
            serial: 0,
            pending_edit: PendingEdit::default(),
            inert: false,
        }
    }
}

impl InputMethodState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether text taps should commit through IM2 rather than the
    /// virtual-keyboard-v1 transport. True only when active and not inert.
    pub fn should_commit(&self) -> bool {
        !self.inert && self.input_method.is_some() && self.current.active
    }
}

pub fn module<S>() -> impl app::RegisteredModule<MechanixKeyboardState, S> {
    app::Module::new().on(on_registry).on(on_input_method_event)
}

/// Bind the manager global as the registry advertises it, then create the IM
/// object once a seat is also available. The seat is shared from the
/// virtual-keyboard module — a second binding would be protocol-legal but
/// redundant, so this stays the single source of truth. `try_create` runs on
/// every global so the IM binds regardless of advertisement order.
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
    if interface.as_str() == ZwpInputMethodManagerV2::NAME {
        s.input_method_state.manager = Some(sender.bind(*name, *version));
    }
    try_create(s);
}

/// Create the `zwp_input_method_v2` object once the manager and a seat are both
/// bound. Idempotent — a no-op if the IM already exists or a precondition is
/// missing.
fn try_create(s: &mut MechanixKeyboardState) {
    if s.input_method_state.input_method.is_some() || s.input_method_state.inert {
        return;
    }
    let (Some(manager), Some(seat)) = (
        s.input_method_state.manager.clone(),
        s.virtual_keyboard_state.seat.clone(),
    ) else {
        return;
    };
    s.input_method_state.input_method = Some(manager.get_input_method(&seat));
    tracing::info!("input-method-v2 bound");
}

/// Handle the inbound double-buffered state machine: events fill `pending`,
/// `done` applies it to `current` and bumps the serial, `unavailable` marks the
/// object inert. Requests/keys after `unavailable` are ignored (per protocol,
/// only `destroy` remains valid).
fn on_input_method_event(s: &mut MechanixKeyboardState, event: &ZwpInputMethodV2Event) {
    if s.input_method_state.inert {
        return;
    }
    match event {
        ZwpInputMethodV2Event::Activate { .. } => {
            // Activate resets all prior inbound state (per protocol) then arms
            // active. We reset pending to defaults so the following events
            // rebuild a clean context applied at the next `done`.
            s.input_method_state.pending = InputMethodContext {
                active: true,
                change_cause: ZwpTextInputV3ChangeCause::InputMethod,
                ..Default::default()
            };
            tracing::info!("input-method: activate (pending)");
        }
        ZwpInputMethodV2Event::Deactivate { .. } => {
            s.input_method_state.pending.active = false;
            tracing::info!("input-method: deactivate (pending)");
        }
        ZwpInputMethodV2Event::SurroundingText {
            text,
            cursor,
            anchor,
            ..
        } => {
            s.input_method_state.pending.surrounding = SurroundingText {
                text: text.clone(),
                cursor: *cursor,
                anchor: *anchor,
                reported: true,
            };
        }
        ZwpInputMethodV2Event::TextChangeCause { cause, .. } => {
            s.input_method_state.pending.change_cause = *cause;
        }
        ZwpInputMethodV2Event::ContentType { hint, purpose, .. } => {
            s.input_method_state.pending.content_type = ContentType {
                hint: *hint,
                purpose: *purpose,
            };
        }
        ZwpInputMethodV2Event::Done { .. } => {
            let st = &mut s.input_method_state;
            st.current = st.pending.clone();
            st.serial = st.serial.wrapping_add(1);
            let active = st.current.active;
            let serial = st.serial;
            set_visibility(s, active);
            tracing::info!(
                active = active,
                serial = serial,
                "input-method: state applied"
            );
        }
        ZwpInputMethodV2Event::Unavailable { .. } => {
            s.input_method_state.inert = true;
            tracing::warn!("input-method: unavailable; object now inert");
        }
    }
}

// ── outbound (staged, double-buffered) edits ──────────────────────────────

/// Stage a `commit_string` for the next flush. Replaces any previously staged
/// commit string (the protocol field is set, not accumulated). No-op when the
/// IM object isn't bound or is inert.
pub fn stage_commit_string(s: &mut MechanixKeyboardState, text: impl Into<String>) {
    let st = &mut s.input_method_state;
    if st.inert || st.input_method.is_none() {
        return;
    }
    st.pending_edit.commit_string = Some(text.into());
}

/// Stage a `set_preedit_string` for the next flush. Replaces any previously
/// staged preedit. No-op when the IM object isn't bound or is inert.
pub fn stage_preedit(
    s: &mut MechanixKeyboardState,
    text: impl Into<String>,
    cursor_begin: i32,
    cursor_end: i32,
) {
    let st = &mut s.input_method_state;
    if st.inert || st.input_method.is_none() {
        return;
    }
    st.pending_edit.preedit = Some(Preedit {
        text: text.into(),
        cursor_begin,
        cursor_end,
    });
}

/// Stage a `delete_surrounding_text` for the next flush. Replaces any previously
/// staged deletion. No-op when the IM object isn't bound or is inert.
pub fn stage_delete_surrounding(s: &mut MechanixKeyboardState, before: u32, after: u32) {
    let st = &mut s.input_method_state;
    if st.inert || st.input_method.is_none() {
        return;
    }
    st.pending_edit.delete = Some(DeleteSurrounding { before, after });
}

/// Flush all staged edits with a `commit(serial)` request. The serial is the
/// number of `done` events received, exactly as the protocol requires. After
/// flushing, the pending edit is reset to its initial (all-`None`) state so the
/// next commit starts clean. No-op with nothing staged, when inert, or when the
/// IM object isn't bound.
pub fn flush(s: &mut MechanixKeyboardState) {
    let st = &mut s.input_method_state;
    if st.inert {
        return;
    }
    let Some(im) = st.input_method.clone() else {
        return;
    };
    let edit = std::mem::take(&mut st.pending_edit);
    // Capture which fields were staged before any of them move into the send
    // calls, for the trace below.
    let (has_commit, has_preedit, has_delete) = (
        edit.commit_string.is_some(),
        edit.preedit.is_some(),
        edit.delete.is_some(),
    );
    // Only send a field when it was explicitly staged this batch — leaving an
    // unset field at its protocol initial value is correct and avoids sending
    // redundant no-op requests.
    if let Some(text) = edit.commit_string {
        im.commit_string(&text);
    }
    if let Some(preedit) = edit.preedit {
        im.set_preedit_string(&preedit.text, preedit.cursor_begin, preedit.cursor_end);
    }
    if let Some(del) = edit.delete {
        im.delete_surrounding_text(del.before, del.after);
    }
    im.commit(st.serial);
    tracing::info!(
        serial = st.serial,
        committed = has_commit,
        preedit = has_preedit,
        deleted = has_delete,
        "input-method: commit flushed"
    );
}

/// Convenience: stage a commit string and flush immediately. This is the
/// idiomatic one-shot text commit an OSK uses per tap when a text input is
/// focused. Returns `true` if the edit was sent (IM bound + active).
pub fn commit_text(s: &mut MechanixKeyboardState, text: &str) -> bool {
    if !s.input_method_state.should_commit() {
        return false;
    }
    stage_commit_string(s, text);
    flush(s);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh state reports neither active nor inert, and the serial is zero
    /// until the first `done`.
    #[test]
    fn fresh_state_is_inactive_and_uncommitted() {
        let st = InputMethodState::new();
        assert!(!st.should_commit());
        assert!(!st.inert);
        assert_eq!(st.serial, 0);
        assert!(!st.current.active);
    }

    /// `change_cause`'s documented initial value is `InputMethod`; both buffers
    /// start there so a `done` before any `text_change_cause` event applies the
    /// spec-correct default.
    #[test]
    fn initial_change_cause_is_input_method() {
        let st = InputMethodState::new();
        assert_eq!(
            st.current.change_cause,
            ZwpTextInputV3ChangeCause::InputMethod
        );
        assert_eq!(
            st.pending.change_cause,
            ZwpTextInputV3ChangeCause::InputMethod
        );
    }
}
