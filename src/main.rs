#![recursion_limit = "1024"]
use std::collections::HashSet;

use app::prelude::*;
use interactivity::InteractivityState;
use io_ring::Ring;
use renderer::{Renderer, TextureId};
use wayland::*;

mod layout;
mod render;
mod virtual_keyboard;
mod window;

/// The Keymap view shown when the keyboard first appears. The current view then
/// changes at runtime as view-switch keys are tapped; see `current_view`.
pub const INITIAL_VIEW: &str = "base";

/// Baked glyph atlas + font consts generated at build time by `assets::builder`.
pub mod atlas {
    include!(concat!(env!("OUT_DIR"), "/keyboard_gen.rs"));
}

/// The icon dictionary — `icon name -> SpriteRegion` — generated at build time
/// from `config.toml`. References the sprite consts in `atlas`.
pub mod icons {
    include!(concat!(env!("OUT_DIR"), "/icons_gen.rs"));
}

use window::{WaylandGlobals, WindowState};

use crate::virtual_keyboard::VirtualKeyboardState;

#[derive(State)]
pub struct MechanixKeyboardState {
    ring: Ring,
    wayland: Wayland,
    #[lens(skip)]
    globals: WaylandGlobals,
    #[lens(skip)]
    renderer: Renderer,
    #[lens(skip)]
    interactivity: InteractivityState,
    #[lens(skip)]
    window: Option<WindowState>,
    #[lens(skip)]
    frame_callbacks: HashSet<ObjectId>,
    #[lens(skip)]
    keymap: Option<layout::Keymap>,
    /// Index into `keymap.views` of the Current view — the one we render and
    /// hit-test. Resolved from `INITIAL_VIEW` when the keymap loads, then updated
    /// by view-switch keys. Reads are O(1) indexing (no per-frame name scan).
    #[lens(skip)]
    current_view: usize,
    /// The uploaded glyph atlas, resolved once the renderer has a GL context.
    #[lens(skip)]
    atlas_texture: Option<TextureId>,
    /// Output buffer-scale factor (HiDPI); read from `wl_output.scale`.
    #[lens(skip)]
    scale: i32,
    #[lens(skip)]
    last_hover: Option<String>,
    #[lens(skip)]
    virtual_keyboard_state: VirtualKeyboardState,
}

impl MechanixKeyboardState {
    fn new() -> Self {
        let ring = Ring::default();
        let wayland = Wayland::new(ring.proxy());
        let renderer = Renderer::new().expect("renderer init failed");
        Self {
            ring,
            wayland,
            globals: WaylandGlobals::default(),
            renderer,
            interactivity: InteractivityState::new(),
            window: None,
            frame_callbacks: HashSet::new(),
            keymap: None,
            current_view: 0,
            atlas_texture: None,
            scale: 1,
            last_hover: None,
            virtual_keyboard_state: VirtualKeyboardState::new(),
        }
    }

    /// The Current view — what to render and hit-test — or `None` until the
    /// keymap is loaded.
    fn current_view(&self) -> Option<&layout::View> {
        self.keymap.as_ref()?.views.get(self.current_view)
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .init();

    let state = MechanixKeyboardState::new();
    let mut app = App::new(state)
        .mount(io_ring::module())
        .mount(wayland::module())
        .mount(render::module())
        .mount(window::module())
        .mount(layout::module())
        .mount(virtual_keyboard::module());

    app.dispatch(&app::Start);
    loop {
        app.dispatch(&app::PrePoll);
        app.dispatch(&app::Poll);
    }
}
