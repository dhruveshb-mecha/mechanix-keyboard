use assets::SpriteRegion;
use serde::Deserialize;
use std::collections::HashMap;
use std::{env, fs};
use tracing::{info, warn};
use utils::Rect;
use xkbcommon::xkb::{self, Keysym};

use crate::{MechanixKeyboardState, icons};

#[derive(Debug, Deserialize)]
struct Layout {
    outlines: HashMap<String, Outline>,
    views: HashMap<String, Vec<String>>,
    #[serde(default)]
    buttons: HashMap<String, Button>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct Outline {
    width: f32,
    height: f32,
}

/// A squeekboard button's `action:` value — either a bare name (`erase`,
/// `show_prefs`) or a structured single-key map (`set_view`, `locking`). Tried
/// as untagged variants in order; `Other` is the `IgnoredAny` catch-all, so an
/// unrecognised structured action still parses (and later resolves to
/// `Unhandled`) instead of failing the whole layout.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ActionSpec {
    Named(String),
    SetView { set_view: String },
    Locking { locking: LockingSpec },
    Other(serde::de::IgnoredAny),
}

/// The body of a squeekboard `locking` action: the view tapped *to* (`lock_view`),
/// and the view tapped *back to* (`unlock_view`) once the lock view is current.
#[derive(Debug, Deserialize)]
struct LockingSpec {
    lock_view: String,
    unlock_view: String,
}

#[derive(Debug, Default, Deserialize)]
struct Button {
    outline: Option<String>,
    label: Option<String>,
    icon: Option<String>,
    /// Emit fields the Key action resolves from; see `resolve_action`.
    keysym: Option<String>,
    text: Option<String>,
    #[serde(default)]
    action: Option<ActionSpec>,
    modifier: Option<String>,
}

/// Size used when a button names an outline that isn't defined (and no
/// `default` outline exists to fall back to). Keeps conversion total.
const FALLBACK_OUTLINE: Outline = Outline {
    width: 50.0,
    height: 50.0,
};

#[derive(Debug)]
pub struct Keymap {
    pub views: Vec<View>,
}

/// One selectable arrangement of keys (e.g. `base`, `upper`).
#[derive(Debug)]
pub struct View {
    pub name: String,
    pub rows: Vec<Row>,
}

/// One horizontal line of keys within a view.
#[derive(Debug)]
pub struct Row {
    pub keys: Vec<Key>,
}

/// What a key draws in its cell: a text label *or* a symbolic icon, never both.
/// Resolved at IR-build time — the icon is already its baked atlas sprite region,
/// so the renderer does no per-frame lookup.
#[derive(Debug, Clone)]
pub enum KeyFace {
    Text(String),
    Icon(SpriteRegion),
}

/// What a Key *does* when activated — the behavioural counterpart to `KeyFace`.
/// Resolved at IR-build time from a squeekboard button's `keysym`/`text`/`action`
/// fields.
#[derive(Debug, Clone)]
pub enum KeyAction {
    /// Emit a single resolved xkb keysym (letters, digits, Return, BackSpace…).
    EmitKeysym(Keysym),
    /// Emit literal text, one keysym per char (e.g. the space key's `" "`).
    EmitText(String),
    /// Switch to a named view and stay there (squeekboard `set_view`). Sticky —
    /// no auto-return.
    SetView(String),
    /// Flip between two named views by current view (squeekboard `locking`): go
    /// to `lock` normally, or back to `unlock` when `lock` is already current.
    ToggleView { lock: String, unlock: String },
    /// Arm a real modifier (Control) for exactly the *next* keystroke, then it
    /// auto-clears — the OSK one-shot latch. The modifier's mask is OR'd into the
    /// next emitted Keystroke; tapping again disarms. Carries the squeekboard
    /// modifier name (e.g. `Control`).
    LatchModifier(String),
    /// A squeekboard action not wired this pass (prefs, and modifiers other than
    /// Control). The key still draws and hit-tests; tapping it logs but types
    /// nothing. Carries a name for that log.
    Unhandled(String),
}

/// One drawable key: what to draw, what it does, and where (logical units).
#[derive(Debug)]
pub struct Key {
    pub face: KeyFace,
    pub action: KeyAction,
    pub rect: Rect,
    pub touch_area: Rect,
}

impl Key {
    /// A human-readable label for bring-up tracing (hover/tap). Icon keys have no
    /// text — and we dropped their name when resolving to a region — so they log
    /// as `[icon]`.
    pub fn display_label(&self) -> &str {
        match &self.face {
            KeyFace::Text(s) => s,
            KeyFace::Icon(_) => "[icon]",
        }
    }
}

impl Row {
    /// The row's keys.
    pub fn keys(&self) -> &[Key] {
        &self.keys
    }
}

impl View {
    /// Every key in the view, flattened across its rows.
    pub fn keys(&self) -> impl Iterator<Item = &Key> {
        self.rows.iter().flat_map(|row| row.keys.iter())
    }

    /// The view's intrinsic logical width: the right edge of its widest row.
    pub fn width(&self) -> f32 {
        self.keys().map(|k| k.rect.right()).fold(0.0_f32, f32::max)
    }

    /// The view's intrinsic logical height: the bottom edge of its last row.
    pub fn height(&self) -> f32 {
        self.keys().map(|k| k.rect.bottom()).fold(0.0_f32, f32::max)
    }
}

impl Keymap {
    /// Every key in the keymap, flattened across every view.
    pub fn keys(&self) -> impl Iterator<Item = &Key> {
        self.views.iter().flat_map(|view| view.keys())
    }

    /// Look a view up by name (e.g. the initial `base` view to render).
    pub fn view(&self, name: &str) -> Option<&View> {
        self.views.iter().find(|v| v.name == name)
    }

    /// The index of the view with this name, for storing as the current view.
    /// Resolved once per switch so per-frame reads stay O(1) array indexing.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.views.iter().position(|v| v.name == name)
    }

    /// Convert a parsed squeekboard layout into the IR, resolving each key's
    /// label and geometry.
    fn from_layout(layout: &Layout) -> Self {
        let views = layout
            .views
            .iter()
            .map(|(name, rows)| View::resolve(layout, name, rows))
            .collect();
        Keymap { views }
    }
}

impl View {
    /// Resolve one view's rows into laid-out keys.
    ///
    /// Two passes: first resolve every key's face + outline, then flow them.
    /// Keys butt together left-to-right with no gap; each row's height is the
    /// max key height in it; rows stack top-down; and each row is centred
    /// within the view's width (the widest row), matching squeekboard's look.
    fn resolve(layout: &Layout, name: &str, rows: &[String]) -> View {
        // Pass 1: resolve face + action + size for every key, grouped by row.
        let sized: Vec<Vec<(KeyFace, KeyAction, Outline)>> = rows
            .iter()
            .map(|row| {
                row.split_whitespace()
                    .map(|token| {
                        let button = layout.buttons.get(token);
                        let outline = resolve_outline(layout, button);
                        let face = resolve_face(token, button);
                        let action = resolve_action(token, button);
                        (face, action, outline)
                    })
                    .collect()
            })
            .collect();

        // The view is as wide as its widest row.
        let view_width = sized
            .iter()
            .map(|row| row.iter().map(|(_, _, o)| o.width).sum::<f32>())
            .fold(0.0_f32, f32::max);

        // Pass 2: flow each row, centred, stacking downward.
        let mut y = 0.0_f32;
        let mut out_rows = Vec::with_capacity(sized.len());
        for row in &sized {
            let row_width: f32 = row.iter().map(|(_, _, o)| o.width).sum();
            let row_height = row.iter().map(|(_, _, o)| o.height).fold(0.0_f32, f32::max);
            let mut x = (view_width - row_width) / 2.0;
            let keys = row
                .iter()
                .map(|(face, action, o)| {
                    let rect = Rect::new(x, y, o.width, o.height);
                    let key = Key {
                        face: face.clone(),
                        action: action.clone(),
                        rect,
                        touch_area: rect,
                    };
                    x += o.width;
                    key
                })
                .collect();
            out_rows.push(Row { keys });
            y += row_height;
        }

        View {
            name: name.to_string(),
            rows: out_rows,
        }
    }
}

/// Resolve a button token's face. Icon wins over label when both are set
/// (matching squeekboard) — and warns. An icon name absent from the dictionary
/// warns and falls back to an empty text face, so the key draws its box but no
/// glyph. With neither icon nor label, the button's own name is the label.
fn resolve_face(token: &str, button: Option<&Button>) -> KeyFace {
    if let Some(icon) = button.and_then(|b| b.icon.as_deref()) {
        if button.and_then(|b| b.label.as_deref()).is_some() {
            warn!("button {token:?} sets both `icon` and `label`; using icon {icon:?}");
        }
        return match icons::icon_region(icon) {
            Some(region) => KeyFace::Icon(region),
            None => {
                warn!("icon {icon:?} not in the icon dictionary; drawing empty key");
                KeyFace::Text(String::new())
            }
        };
    }
    let label = button
        .and_then(|b| b.label.clone())
        .unwrap_or_else(|| token.to_string());
    KeyFace::Text(label)
}

/// Resolve a button token's Key action — what it emits when tapped. Priority:
/// explicit `keysym:` → `action: erase` (→ BackSpace) → `text:` → a single-char
/// token/label (→ its keysym). A `modifier`, a structured/other `action`, or a
/// multi-char label with no keysym all resolve to `Unhandled` this pass.
fn resolve_action(token: &str, button: Option<&Button>) -> KeyAction {
    if let Some(b) = button {
        if let Some(name) = b.keysym.as_deref() {
            let ks = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
            if ks.raw() != 0 {
                return KeyAction::EmitKeysym(ks);
            }
            warn!("keysym {name:?} on button {token:?} is unknown; key won't type");
            return KeyAction::Unhandled(token.to_string());
        }
        match &b.action {
            Some(ActionSpec::Named(a)) if a == "erase" => {
                return KeyAction::EmitKeysym(xkb::keysym_from_name(
                    "BackSpace",
                    xkb::KEYSYM_NO_FLAGS,
                ));
            }
            Some(ActionSpec::Named(a)) => return KeyAction::Unhandled(a.clone()),
            Some(ActionSpec::SetView { set_view }) => {
                return KeyAction::SetView(set_view.clone());
            }
            Some(ActionSpec::Locking { locking }) => {
                return KeyAction::ToggleView {
                    lock: locking.lock_view.clone(),
                    unlock: locking.unlock_view.clone(),
                };
            }
            Some(ActionSpec::Other(_)) => return KeyAction::Unhandled(token.to_string()),
            None => {}
        }
        // A modifier button latches for the next keystroke. Only Control is wired
        // this pass (see `virtual_keyboard::toggle_latch` and its `mod_masks`);
        // every other modifier still defers to `Unhandled`.
        if let Some(m) = b.modifier.as_deref() {
            if m == "Control" {
                return KeyAction::LatchModifier(m.to_string());
            }
            return KeyAction::Unhandled(token.to_string());
        }
        if let Some(text) = &b.text {
            return KeyAction::EmitText(text.clone());
        }
        if let Some(ks) = b.label.as_deref().and_then(single_char_keysym) {
            return KeyAction::EmitKeysym(ks);
        }
    }
    // No button entry (or nothing above matched): a single-char token is itself
    // the keysym — this covers every plain letter/digit/punctuation key.
    if let Some(ks) = single_char_keysym(token) {
        return KeyAction::EmitKeysym(ks);
    }
    KeyAction::Unhandled(token.to_string())
}

/// The keysym for a single-character string, or `None` if `s` isn't exactly one
/// char or that char has no keysym.
fn single_char_keysym(s: &str) -> Option<Keysym> {
    let mut chars = s.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    let ks = Keysym::from_char(c);
    (ks.raw() != 0).then_some(ks)
}

/// Resolve a button's outline size: the button's named outline, else the
/// `default` outline, else a hard fallback. Warns on a dangling outline name.
fn resolve_outline(layout: &Layout, button: Option<&Button>) -> Outline {
    let name = button
        .and_then(|b| b.outline.as_deref())
        .unwrap_or("default");
    if let Some(outline) = layout.outlines.get(name) {
        return *outline;
    }
    warn!("outline {name:?} not defined; falling back to `default`");
    layout
        .outlines
        .get("default")
        .copied()
        .unwrap_or(FALLBACK_OUTLINE)
}

static FALLBACK_LAYOUT: &str = include_str!("../resources/layout.yaml");

/// Load and convert the layout on startup, storing the IR on state.
pub fn module<S>() -> impl app::RegisteredModule<MechanixKeyboardState, S> {
    app::Module::new().on(|s: &mut MechanixKeyboardState, _: &app::Start| {
        let contents = match env::var("MECHA_KBD_LAYOUT") {
            Ok(path) => fs::read_to_string(path).expect("Error: Failed to read layout file"),
            Err(_) => {
                let local = std::path::Path::new("layout.yaml");
                if local.is_file() {
                    info!("Using layout.yaml from working directory");
                    fs::read_to_string(local).expect("Error: Failed to read layout file")
                } else {
                    warn!("Warning: No config path provided! Using fallback...");
                    FALLBACK_LAYOUT.into()
                }
            }
        };

        let layout: Layout = yaml_serde::from_str(&contents).expect("Error: Failed to parse yaml");
        let keymap = Keymap::from_layout(&layout);

        info!("loaded keymap: {} view(s)", keymap.views.len());
        for view in &keymap.views {
            info!("  view {}: {} key(s)", view.name, view.keys().count());
        }

        // Start on the initial view; fall back to the first view if it's absent.
        s.current_view = keymap.index_of(crate::INITIAL_VIEW).unwrap_or(0);
        s.keymap = Some(keymap);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Button` with only the fields a test cares about set.
    fn button(f: impl FnOnce(&mut Button)) -> Button {
        let mut b = Button::default();
        f(&mut b);
        b
    }

    #[test]
    fn plain_char_token_resolves_to_its_keysym() {
        for tok in ["q", "1", ",", "."] {
            let c = tok.chars().next().unwrap();
            assert!(
                matches!(resolve_action(tok, None), KeyAction::EmitKeysym(ks) if ks == Keysym::from_char(c)),
                "token {tok:?} should emit its own keysym",
            );
        }
    }

    #[test]
    fn explicit_keysym_field_wins() {
        let b = button(|b| b.keysym = Some("Return".into()));
        let want = xkb::keysym_from_name("Return", xkb::KEYSYM_NO_FLAGS);
        assert!(
            matches!(resolve_action("Return", Some(&b)), KeyAction::EmitKeysym(ks) if ks == want)
        );
    }

    #[test]
    fn erase_action_maps_to_backspace() {
        let b = button(|b| b.action = Some(ActionSpec::Named("erase".into())));
        let want = xkb::keysym_from_name("BackSpace", xkb::KEYSYM_NO_FLAGS);
        assert!(
            matches!(resolve_action("BackSpace", Some(&b)), KeyAction::EmitKeysym(ks) if ks == want)
        );
    }

    #[test]
    fn text_field_resolves_to_emit_text() {
        let b = button(|b| b.text = Some(" ".into()));
        assert!(matches!(resolve_action("space", Some(&b)), KeyAction::EmitText(t) if t == " "));
    }

    #[test]
    fn set_view_action_resolves_to_setview() {
        let b = button(|b| {
            b.action = Some(ActionSpec::SetView {
                set_view: "symbols".into(),
            })
        });
        assert!(matches!(
            resolve_action("show_symbols", Some(&b)),
            KeyAction::SetView(v) if v == "symbols"
        ));
    }

    #[test]
    fn locking_action_resolves_to_toggleview() {
        let b = button(|b| {
            b.action = Some(ActionSpec::Locking {
                locking: LockingSpec {
                    lock_view: "upper".into(),
                    unlock_view: "base".into(),
                },
            })
        });
        assert!(matches!(
            resolve_action("Shift_L", Some(&b)),
            KeyAction::ToggleView { lock, unlock } if lock == "upper" && unlock == "base"
        ));
    }

    #[test]
    fn deferred_actions_are_unhandled() {
        // An unrecognised structured action, a bare non-erase action, and an
        // unwired modifier all defer this pass — none should type or switch.
        let unknown = button(|b| b.action = Some(ActionSpec::Other(serde::de::IgnoredAny)));
        assert!(matches!(
            resolve_action("mystery", Some(&unknown)),
            KeyAction::Unhandled(_)
        ));

        let prefs = button(|b| b.action = Some(ActionSpec::Named("show_prefs".into())));
        assert!(matches!(
            resolve_action("preferences", Some(&prefs)),
            KeyAction::Unhandled(_)
        ));

        // Alt is a modifier, but not wired to latch this pass — still deferred.
        let alt = button(|b| b.modifier = Some("Alt".into()));
        assert!(matches!(
            resolve_action("Alt", Some(&alt)),
            KeyAction::Unhandled(_)
        ));
    }

    #[test]
    fn control_modifier_latches() {
        let ctrl = button(|b| b.modifier = Some("Control".into()));
        assert!(matches!(
            resolve_action("Ctrl", Some(&ctrl)),
            KeyAction::LatchModifier(m) if m == "Control"
        ));
    }

    #[test]
    fn fallback_structured_actions_resolve() {
        // End-to-end: the untagged `ActionSpec` must parse real layout YAML, and
        // the structured actions must reach their view-switch variants.
        let layout: Layout = yaml_serde::from_str(FALLBACK_LAYOUT).expect("fallback parses");
        let keymap = Keymap::from_layout(&layout);

        let has_set_view = keymap
            .keys()
            .any(|k| matches!(&k.action, KeyAction::SetView(v) if v == "symbols"));
        assert!(has_set_view, "a `set_view: symbols` key should be SetView");

        let has_toggle = keymap.keys().any(|k| {
            matches!(&k.action, KeyAction::ToggleView { lock, unlock } if lock == "upper" && unlock == "base")
        });
        assert!(has_toggle, "Shift_L should be ToggleView upper/base");
    }

    #[test]
    fn fallback_layout_base_view_has_expected_actions() {
        let layout: Layout = yaml_serde::from_str(FALLBACK_LAYOUT).expect("fallback parses");
        let keymap = Keymap::from_layout(&layout);
        let base = keymap.view("base").expect("base view exists");

        // Every base-view key resolves to *some* action, and the ones that type
        // are keysym/text — no base key is left ambiguous.
        let typeable = base
            .keys()
            .filter(|k| matches!(k.action, KeyAction::EmitKeysym(_) | KeyAction::EmitText(_)))
            .count();
        assert!(
            typeable >= 40,
            "base view should have many typeable keys, got {typeable}"
        );
    }
}
