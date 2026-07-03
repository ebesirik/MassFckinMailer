//! The redesign's semantic color tokens (see `docs/CLAUDE-CODE-INSTRUCTIONS.md`)
//! and the glue that drives gpui-component's [`Theme`] from them.
//!
//! Two layers:
//! 1. [`apply_palette`] writes the palette onto gpui-component's `ThemeColor` so
//!    every built-in widget (Button, Switch, Input, TitleBar, …) renders
//!    on-palette without per-call color overrides.
//! 2. [`Tokens`] is a tiny global holding the four semantic colors gpui-component
//!    has no field for (`surface_2`, `border_strong`, `text_faint`,
//!    `accent_soft`); hand-built widgets read it via `cx.tokens()`.
//!
//! Rule for render code: never hardcode a color — pull from `cx.theme().*` or
//! `cx.tokens().*`.

use gpui::{App, BoxShadow, Global, Hsla, hsla, point, px, rgb};
use gpui_component::{Theme, ThemeMode};

/// UI font family (bundled, see `assets/fonts`).
pub const UI_FONT: &str = "Geist";
/// Monospace family for technical strings (paths, `{{fields}}`, code, emails).
pub const MONO_FONT: &str = "Geist Mono";

/// Corner radii (brief: inputs/buttons 8–10, cards 12–14).
pub const RADIUS: f32 = 10.;
pub const RADIUS_LG: f32 = 14.;

/// The full set of design tokens for one theme mode.
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Hsla,
    pub surface: Hsla,
    pub surface_2: Hsla,
    pub border: Hsla,
    pub border_strong: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_faint: Hsla,
    pub accent: Hsla,
    pub accent_hover: Hsla,
    pub accent_soft: Hsla,
    pub accent_fg: Hsla,
    pub success: Hsla,
    pub danger: Hsla,
    /// Not in the brief table, but existing UI needs an amber for
    /// duplicates/skipped/attention states.
    pub warning: Hsla,
}

fn light() -> Palette {
    Palette {
        bg: rgb(0xF4F5F7).into(),
        surface: rgb(0xFFFFFF).into(),
        surface_2: rgb(0xEEF0F3).into(),
        border: rgb(0xE6E8EC).into(),
        border_strong: rgb(0xD8DBE0).into(),
        text: rgb(0x191B1F).into(),
        text_muted: rgb(0x6A7280).into(),
        text_faint: rgb(0x9BA1AB).into(),
        accent: rgb(0x0369A1).into(),
        accent_hover: rgb(0x025887).into(),
        accent_soft: rgb(0xE3F0F8).into(),
        accent_fg: rgb(0xFFFFFF).into(),
        success: rgb(0x15A34A).into(),
        danger: rgb(0xDC2626).into(),
        warning: rgb(0xD97706).into(),
    }
}

fn dark() -> Palette {
    Palette {
        bg: rgb(0x111318).into(),
        surface: rgb(0x191C22).into(),
        surface_2: rgb(0x21252C).into(),
        border: rgb(0x282C34).into(),
        border_strong: rgb(0x363B45).into(),
        text: rgb(0xE9EBEF).into(),
        text_muted: rgb(0x9AA1AC).into(),
        text_faint: rgb(0x69707B).into(),
        accent: rgb(0x3DA9E0).into(),
        accent_hover: rgb(0x338EBC).into(),
        accent_soft: rgb(0x0F2A3C).into(),
        accent_fg: rgb(0x07141E).into(),
        success: rgb(0x34D399).into(),
        danger: rgb(0xF87171).into(),
        warning: rgb(0xFBBF24).into(),
    }
}

pub fn palette(mode: ThemeMode) -> Palette {
    if mode.is_dark() { dark() } else { light() }
}

/// The semantic colors gpui-component's `ThemeColor` has no field for. Stored as
/// a global and refreshed by [`apply_palette`]; read via `cx.tokens()`. Status
/// colors (success/danger/warning) and `accent` live on `cx.theme()`.
#[derive(Clone, Copy)]
pub struct Tokens {
    pub surface: Hsla,
    pub surface_2: Hsla,
    pub border_strong: Hsla,
    pub text_faint: Hsla,
    pub accent_soft: Hsla,
    dark: bool,
}

impl Global for Tokens {}

impl Tokens {
    /// The soft card shadow (brief: `0 1px 2px` + `0 4px 16px`).
    pub fn card_shadow(&self) -> Vec<BoxShadow> {
        let (near, far) = if self.dark {
            (hsla(0., 0., 0., 0.40), hsla(0., 0., 0., 0.35))
        } else {
            (
                hsla(220. / 360., 0.16, 0.07, 0.06),
                hsla(220. / 360., 0.16, 0.07, 0.05),
            )
        };
        vec![
            BoxShadow {
                color: near,
                offset: point(px(0.), px(1.)),
                blur_radius: px(2.),
                spread_radius: px(0.),
            },
            BoxShadow {
                color: far,
                offset: point(px(0.), px(4.)),
                blur_radius: px(16.),
                spread_radius: px(0.),
            },
        ]
    }
}

/// Read the extra semantic tokens. Safe after [`apply_palette`] has run once
/// (which happens in `MainWindow::new`, before the first paint).
pub trait ActiveTokens {
    fn tokens(&self) -> Tokens;
}

impl ActiveTokens for App {
    fn tokens(&self) -> Tokens {
        *self.global::<Tokens>()
    }
}

/// Write the palette for `mode` onto the global gpui-component `Theme` and the
/// [`Tokens`] extras. Call **after** every `Theme::change`, which re-applies its
/// own config and would otherwise clobber these.
pub fn apply_palette(mode: ThemeMode, cx: &mut App) {
    let p = palette(mode);

    cx.set_global(Tokens {
        surface: p.surface,
        surface_2: p.surface_2,
        border_strong: p.border_strong,
        text_faint: p.text_faint,
        accent_soft: p.accent_soft,
        dark: mode.is_dark(),
    });

    let theme = Theme::global_mut(cx);

    // Typography + shape.
    theme.font_family = UI_FONT.into();
    theme.mono_font_family = MONO_FONT.into();
    theme.radius = px(RADIUS);
    theme.radius_lg = px(RADIUS_LG);

    // Surfaces / text / borders.
    theme.background = p.bg;
    theme.foreground = p.text;
    theme.border = p.border;
    theme.muted = p.surface_2;
    theme.muted_foreground = p.text_muted;
    theme.popover = p.surface;
    theme.popover_foreground = p.text;
    theme.secondary = p.surface_2;
    theme.secondary_foreground = p.text;
    theme.secondary_hover = p.border;
    theme.secondary_active = p.border_strong;
    theme.selection = p.accent_soft;
    // Inputs render recessed (window bg) with a border, matching the mock's
    // in-card fields.
    theme.input = p.bg;

    // Brand (gpui-component's `primary`) vs. subtle highlight (`accent`).
    theme.primary = p.accent;
    theme.primary_hover = p.accent_hover;
    theme.primary_active = p.accent_hover;
    theme.primary_foreground = p.accent_fg;
    theme.accent = p.accent_soft;
    theme.accent_foreground = p.accent;
    theme.link = p.accent;
    theme.link_hover = p.accent_hover;
    theme.link_active = p.accent_hover;
    theme.ring = p.accent;
    theme.switch = p.border_strong;
    theme.caret = p.accent;

    // Title bar + sidebar.
    theme.title_bar = p.surface;
    theme.title_bar_border = p.border;
    theme.sidebar = p.surface;
    theme.sidebar_border = p.border;
    theme.sidebar_foreground = p.text;
    theme.sidebar_accent = p.accent_soft;
    theme.sidebar_accent_foreground = p.accent;
    theme.sidebar_primary = p.accent;
    theme.sidebar_primary_foreground = p.accent_fg;

    // Tables / lists.
    theme.list = p.surface;
    theme.list_hover = p.surface_2;
    theme.table = p.surface;
    theme.table_head = p.surface_2;
    theme.table_head_foreground = p.text_muted;
    theme.table_row_border = p.border;
    theme.table_hover = p.surface_2;

    // Status.
    theme.success = p.success;
    theme.danger = p.danger;
    theme.danger_foreground = hsla(0., 0., 0.98, 1.);
    theme.warning = p.warning;
}
