//! Quiet, low-contrast themes with a single accent color. Dark and light
//! palettes; the active one follows egui's current theme (which can follow
//! the macOS appearance).

#![allow(non_snake_case)]

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle,
    Theme, ThemePreference, Visuals, vec2,
};
use rgrip_core::config::ThemeChoice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub struct Palette {
    pub name: &'static str,
    pub bg: Color32,
    surface: Color32,
    border: Color32,
    hover: Color32,
    popup: Color32,
    pub text: Color32,
    muted: Color32,
    faint: Color32,
    pub accent: Color32,
    ok: Color32,
    warn: Color32,
    error: Color32,
    string: Color32,
    geo: Color32,
    view: Color32,
    /// Text inputs.
    input: Color32,
    /// Zebra stripes.
    stripe: Color32,
    /// Button fills: inactive, hovered, pressed, open.
    widget: [Color32; 4],
    /// Text on hovered/pressed widgets.
    strong: Color32,
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// Dark themes; the first is the default. Colors follow each theme's
/// published palette (Dracula, Nord, One Dark Pro, …).
pub const DARK_THEMES: &[Palette] = &[
    Palette {
        name: "rgrip Dark",
        bg: hex(0x161618),
        surface: hex(0x1B1B1E),
        border: hex(0x242428),
        hover: hex(0x222226),
        popup: hex(0x1E1E22),
        text: hex(0xD4D4D8),
        muted: hex(0x7A7A82),
        faint: hex(0x505058),
        accent: hex(0x7AA2F7),
        ok: hex(0x9ECE6A),
        warn: hex(0xE0AF68),
        error: hex(0xF7768E),
        string: hex(0xA9C77E),
        geo: hex(0x73DACA),
        view: hex(0xBB9AF7),
        input: hex(0x212125),
        stripe: hex(0x19191C),
        widget: [hex(0x222226), hex(0x2C2C32), hex(0x34343A), hex(0x28282D)],
        strong: hex(0xFFFFFF),
    },
    Palette {
        name: "Dracula",
        bg: hex(0x21222C),
        surface: hex(0x282A36),
        border: hex(0x343746),
        hover: hex(0x343746),
        popup: hex(0x2E303E),
        text: hex(0xF8F8F2),
        muted: hex(0xA9ACC6),
        faint: hex(0x6272A4),
        accent: hex(0xBD93F9),
        ok: hex(0x50FA7B),
        warn: hex(0xFFB86C),
        error: hex(0xFF5555),
        string: hex(0xF1FA8C),
        geo: hex(0x8BE9FD),
        view: hex(0xFF79C6),
        input: hex(0x343746),
        stripe: hex(0x2B2D3A),
        widget: [hex(0x343746), hex(0x414558), hex(0x4D5166), hex(0x3C3F52)],
        strong: hex(0xFFFFFF),
    },
    Palette {
        name: "Nord",
        bg: hex(0x272C36),
        surface: hex(0x2E3440),
        border: hex(0x3B4252),
        hover: hex(0x3B4252),
        popup: hex(0x3B4252),
        text: hex(0xD8DEE9),
        muted: hex(0xA1AAB8),
        faint: hex(0x616E88),
        accent: hex(0x88C0D0),
        ok: hex(0xA3BE8C),
        warn: hex(0xEBCB8B),
        error: hex(0xBF616A),
        string: hex(0xA3BE8C),
        geo: hex(0x8FBCBB),
        view: hex(0xB48EAD),
        input: hex(0x3B4252),
        stripe: hex(0x313744),
        widget: [hex(0x3B4252), hex(0x434C5E), hex(0x4C566A), hex(0x434C5E)],
        strong: hex(0xECEFF4),
    },
    Palette {
        name: "One Dark Pro",
        bg: hex(0x21252B),
        surface: hex(0x282C34),
        border: hex(0x2F343D),
        hover: hex(0x2C313A),
        popup: hex(0x21252B),
        text: hex(0xABB2BF),
        muted: hex(0x7F848E),
        faint: hex(0x5C6370),
        accent: hex(0x61AFEF),
        ok: hex(0x98C379),
        warn: hex(0xE5C07B),
        error: hex(0xE06C75),
        string: hex(0x98C379),
        geo: hex(0x56B6C2),
        view: hex(0xC678DD),
        input: hex(0x1D1F23),
        stripe: hex(0x2A2E36),
        widget: [hex(0x2C313A), hex(0x3A3F4B), hex(0x454B57), hex(0x353B45)],
        strong: hex(0xFFFFFF),
    },
    Palette {
        name: "Tokyo Night",
        bg: hex(0x16161E),
        surface: hex(0x1A1B26),
        border: hex(0x232433),
        hover: hex(0x1F2335),
        popup: hex(0x16161E),
        text: hex(0xA9B1D6),
        muted: hex(0x787C99),
        faint: hex(0x565F89),
        accent: hex(0x7AA2F7),
        ok: hex(0x9ECE6A),
        warn: hex(0xE0AF68),
        error: hex(0xF7768E),
        string: hex(0x9ECE6A),
        geo: hex(0x7DCFFF),
        view: hex(0xBB9AF7),
        input: hex(0x14141B),
        stripe: hex(0x1C1D29),
        widget: [hex(0x1F2335), hex(0x292E42), hex(0x33394F), hex(0x24283B)],
        strong: hex(0xC0CAF5),
    },
    Palette {
        name: "Monokai",
        bg: hex(0x1E1F1C),
        surface: hex(0x272822),
        border: hex(0x3E3D32),
        hover: hex(0x3E3D32),
        popup: hex(0x1E1F1C),
        text: hex(0xF8F8F2),
        muted: hex(0xA59F85),
        faint: hex(0x75715E),
        accent: hex(0x66D9EF),
        ok: hex(0xA6E22E),
        warn: hex(0xFD971F),
        error: hex(0xF92672),
        string: hex(0xE6DB74),
        geo: hex(0x66D9EF),
        view: hex(0xAE81FF),
        input: hex(0x1E1F1C),
        stripe: hex(0x2C2D27),
        widget: [hex(0x3E3D32), hex(0x49483E), hex(0x57564A), hex(0x414137)],
        strong: hex(0xFFFFFF),
    },
    Palette {
        name: "Catppuccin Mocha",
        bg: hex(0x181825),
        surface: hex(0x1E1E2E),
        border: hex(0x313244),
        hover: hex(0x313244),
        popup: hex(0x181825),
        text: hex(0xCDD6F4),
        muted: hex(0xA6ADC8),
        faint: hex(0x6C7086),
        accent: hex(0x89B4FA),
        ok: hex(0xA6E3A1),
        warn: hex(0xFAB387),
        error: hex(0xF38BA8),
        string: hex(0xA6E3A1),
        geo: hex(0x94E2D5),
        view: hex(0xCBA6F7),
        input: hex(0x11111B),
        stripe: hex(0x232334),
        widget: [hex(0x313244), hex(0x45475A), hex(0x585B70), hex(0x3B3D50)],
        strong: hex(0xFFFFFF),
    },
    Palette {
        name: "Gruvbox Dark",
        bg: hex(0x1D2021),
        surface: hex(0x282828),
        border: hex(0x3C3836),
        hover: hex(0x3C3836),
        popup: hex(0x1D2021),
        text: hex(0xEBDBB2),
        muted: hex(0xA89984),
        faint: hex(0x7C6F64),
        accent: hex(0x83A598),
        ok: hex(0xB8BB26),
        warn: hex(0xFABD2F),
        error: hex(0xFB4934),
        string: hex(0xB8BB26),
        geo: hex(0x8EC07C),
        view: hex(0xD3869B),
        input: hex(0x1D2021),
        stripe: hex(0x2E2B2A),
        widget: [hex(0x3C3836), hex(0x504945), hex(0x665C54), hex(0x45403D)],
        strong: hex(0xFBF1C7),
    },
    Palette {
        name: "GitHub Dark",
        bg: hex(0x010409),
        surface: hex(0x0D1117),
        border: hex(0x21262D),
        hover: hex(0x161B22),
        popup: hex(0x161B22),
        text: hex(0xE6EDF3),
        muted: hex(0x8D96A0),
        faint: hex(0x6E7681),
        accent: hex(0x4493F8),
        ok: hex(0x3FB950),
        warn: hex(0xD29922),
        error: hex(0xF85149),
        string: hex(0xA5D6FF),
        geo: hex(0x39C5CF),
        view: hex(0xBC8CFF),
        input: hex(0x010409),
        stripe: hex(0x11161D),
        widget: [hex(0x21262D), hex(0x30363D), hex(0x3D444D), hex(0x262C36)],
        strong: hex(0xFFFFFF),
    },
    Palette {
        name: "Solarized Dark",
        bg: hex(0x00252F),
        surface: hex(0x002B36),
        border: hex(0x073642),
        hover: hex(0x073642),
        popup: hex(0x00303C),
        text: hex(0x93A1A1),
        muted: hex(0x839496),
        faint: hex(0x586E75),
        accent: hex(0x268BD2),
        ok: hex(0x859900),
        warn: hex(0xB58900),
        error: hex(0xDC322F),
        string: hex(0x2AA198),
        geo: hex(0x2AA198),
        view: hex(0x6C71C4),
        input: hex(0x00212B),
        stripe: hex(0x01313D),
        widget: [hex(0x073642), hex(0x0B4250), hex(0x104B5A), hex(0x083B48)],
        strong: hex(0xEEE8D5),
    },
];

/// Light themes; the first is the default.
pub const LIGHT_THEMES: &[Palette] = &[
    Palette {
        name: "rgrip Light",
        bg: hex(0xF4F4F6),
        surface: hex(0xFFFFFF),
        border: hex(0xE2E2E7),
        hover: hex(0xEAEAEE),
        popup: hex(0xFFFFFF),
        text: hex(0x202026),
        muted: hex(0x686874),
        faint: hex(0xA0A0AA),
        accent: hex(0x3465E0),
        ok: hex(0x26823C),
        warn: hex(0xB07014),
        error: hex(0xCE3250),
        string: hex(0x387828),
        geo: hex(0x0E8C80),
        view: hex(0x8250DF),
        input: hex(0xF0F0F3),
        stripe: hex(0xF9F9FB),
        widget: [hex(0xECECF0), hex(0xE2E2E8), hex(0xD6D6DE), hex(0xE6E6EC)],
        strong: hex(0x000000),
    },
    Palette {
        name: "GitHub Light",
        bg: hex(0xF6F8FA),
        surface: hex(0xFFFFFF),
        border: hex(0xD0D7DE),
        hover: hex(0xEAEEF2),
        popup: hex(0xFFFFFF),
        text: hex(0x1F2328),
        muted: hex(0x59636E),
        faint: hex(0x8C959F),
        accent: hex(0x0969DA),
        ok: hex(0x1A7F37),
        warn: hex(0x9A6700),
        error: hex(0xCF222E),
        string: hex(0x0A3069),
        geo: hex(0x1B7C83),
        view: hex(0x8250DF),
        input: hex(0xF6F8FA),
        stripe: hex(0xF6F8FA),
        widget: [hex(0xEAEEF2), hex(0xDDE3E9), hex(0xD0D7DE), hex(0xE1E6EB)],
        strong: hex(0x000000),
    },
    Palette {
        name: "Solarized Light",
        bg: hex(0xEEE8D5),
        surface: hex(0xFDF6E3),
        border: hex(0xE4DDC8),
        hover: hex(0xE9E3CF),
        popup: hex(0xFDF6E3),
        text: hex(0x586E75),
        muted: hex(0x657B83),
        faint: hex(0x93A1A1),
        accent: hex(0x268BD2),
        ok: hex(0x859900),
        warn: hex(0xB58900),
        error: hex(0xDC322F),
        string: hex(0x2AA198),
        geo: hex(0x2AA198),
        view: hex(0x6C71C4),
        input: hex(0xF5EFDC),
        stripe: hex(0xF8F1DE),
        widget: [hex(0xE9E3CF), hex(0xE0D9C3), hex(0xD6CFB8), hex(0xE4DDC8)],
        strong: hex(0x002B36),
    },
    Palette {
        name: "Catppuccin Latte",
        bg: hex(0xE6E9EF),
        surface: hex(0xEFF1F5),
        border: hex(0xCCD0DA),
        hover: hex(0xDCE0E8),
        popup: hex(0xEFF1F5),
        text: hex(0x4C4F69),
        muted: hex(0x6C6F85),
        faint: hex(0x9CA0B0),
        accent: hex(0x1E66F5),
        ok: hex(0x40A02B),
        warn: hex(0xDF8E1D),
        error: hex(0xD20F39),
        string: hex(0x40A02B),
        geo: hex(0x179299),
        view: hex(0x8839EF),
        input: hex(0xE6E9EF),
        stripe: hex(0xE9ECF1),
        widget: [hex(0xDCE0E8), hex(0xCCD0DA), hex(0xBCC0CC), hex(0xD5D9E2)],
        strong: hex(0x11111B),
    },
    Palette {
        name: "One Light",
        bg: hex(0xEAEAEB),
        surface: hex(0xFAFAFA),
        border: hex(0xDBDBDC),
        hover: hex(0xE5E5E6),
        popup: hex(0xFFFFFF),
        text: hex(0x383A42),
        muted: hex(0x696C77),
        faint: hex(0xA0A1A7),
        accent: hex(0x4078F2),
        ok: hex(0x50A14F),
        warn: hex(0xC18401),
        error: hex(0xE45649),
        string: hex(0x50A14F),
        geo: hex(0x0184BC),
        view: hex(0xA626A4),
        input: hex(0xF0F0F1),
        stripe: hex(0xF4F4F5),
        widget: [hex(0xE5E5E6), hex(0xDBDBDC), hex(0xCFCFD0), hex(0xE0E0E1)],
        strong: hex(0x000000),
    },
];

static IS_DARK: AtomicBool = AtomicBool::new(true);
static DARK_IDX: AtomicUsize = AtomicUsize::new(0);
static LIGHT_IDX: AtomicUsize = AtomicUsize::new(0);

fn p() -> &'static Palette {
    if IS_DARK.load(Ordering::Relaxed) {
        &DARK_THEMES[DARK_IDX.load(Ordering::Relaxed)]
    } else {
        &LIGHT_THEMES[LIGHT_IDX.load(Ordering::Relaxed)]
    }
}

fn index_of(themes: &[Palette], name: &str) -> usize {
    themes.iter().position(|t| t.name == name).unwrap_or(0)
}

pub fn is_dark() -> bool {
    IS_DARK.load(Ordering::Relaxed)
}

/// Swatch color for a connection color; `None` for no color.
pub fn conn_color(c: rgrip_core::ConnColor) -> Option<Color32> {
    use rgrip_core::ConnColor::*;
    Some(match (c, is_dark()) {
        (None, _) => return Option::None,
        (Red, true) => hex(0xF7768E),
        (Red, false) => hex(0xD6334F),
        (Orange, true) => hex(0xFF9E64),
        (Orange, false) => hex(0xD9701F),
        (Yellow, true) => hex(0xE0AF68),
        (Yellow, false) => hex(0xB8860B),
        (Green, true) => hex(0x9ECE6A),
        (Green, false) => hex(0x3E8E41),
        (Blue, true) => hex(0x7AA2F7),
        (Blue, false) => hex(0x2F6FDE),
        (Purple, true) => hex(0xBB9AF7),
        (Purple, false) => hex(0x8250DF),
        (Gray, true) => hex(0x9AA0A6),
        (Gray, false) => hex(0x6B7078),
    })
}

/// `color` laid over `base` at strength `t`; the background only moves a
/// little, so tab text stays readable in both themes.
pub fn tint(base: Color32, color: Color32, t: f32) -> Color32 {
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color32::from_rgb(mix(base.r(), color.r()), mix(base.g(), color.g()), mix(base.b(), color.b()))
}

pub fn BG() -> Color32 { p().bg }
pub fn SURFACE() -> Color32 { p().surface }
pub fn BORDER() -> Color32 { p().border }
pub fn HOVER() -> Color32 { p().hover }
pub fn POPUP() -> Color32 { p().popup }
pub fn TEXT() -> Color32 { p().text }
pub fn MUTED() -> Color32 { p().muted }
pub fn FAINT() -> Color32 { p().faint }
pub fn ACCENT() -> Color32 { p().accent }
pub fn OK() -> Color32 { p().ok }
pub fn WARN() -> Color32 { p().warn }
pub fn ERROR() -> Color32 { p().error }
pub fn STRING() -> Color32 { p().string }
/// Geometry values and map features.
pub fn GEO() -> Color32 { p().geo }
pub fn VIEW() -> Color32 { p().view }
/// Selected segment / pressed fill.
pub fn SELECTED() -> Color32 { p().widget[2] }

/// Call once per frame: picks the palette for the theme egui is showing
/// (relevant when following the system appearance).
pub fn sync(ctx: &egui::Context) {
    IS_DARK.store(ctx.theme() == Theme::Dark, Ordering::Relaxed);
}

/// Switch theme preference at runtime.
pub fn set_choice(ctx: &egui::Context, choice: ThemeChoice) {
    ctx.set_theme(match choice {
        ThemeChoice::System => ThemePreference::System,
        ThemeChoice::Light => ThemePreference::Light,
        ThemeChoice::Dark => ThemePreference::Dark,
    });
    sync(ctx);
}

/// Prefer native macOS fonts (and JetBrains Mono if installed), keeping
/// egui's bundled fonts as fallbacks for symbols.
fn install_fonts(ctx: &egui::Context) {
    let home = std::env::var("HOME").unwrap_or_default();
    let ui_fonts = ["/System/Library/Fonts/SFNS.ttf".to_string()];
    let mono_fonts = [
        format!("{home}/Library/Fonts/JetBrainsMonoNLNerdFont-Regular.ttf"),
        format!("{home}/Library/Fonts/JetBrainsMono-Regular.ttf"),
        "/System/Library/Fonts/SFNSMono.ttf".to_string(),
    ];

    let mut fonts = FontDefinitions::default();
    let mut add = |family: FontFamily, paths: &[String], name: &str| {
        if let Some(bytes) = paths.iter().find_map(|p| std::fs::read(p).ok()) {
            fonts.font_data.insert(name.into(), Arc::new(FontData::from_owned(bytes)));
            fonts.families.entry(family).or_default().insert(0, name.into());
        }
    };
    add(FontFamily::Proportional, &ui_fonts, "ui");
    add(FontFamily::Monospace, &mono_fonts, "mono");

    // Khmer has no glyphs in SF / JetBrains Mono: fall back to a Khmer font
    // right after the primary one, for both families.
    let khmer_fonts = [
        format!("{home}/Library/Fonts/KantumruyPro-VariableFont_wght.ttf"),
        "/Library/Fonts/KantumruyPro-VariableFont_wght.ttf".to_string(),
        "/System/Library/Fonts/Supplemental/Khmer Sangam MN.ttf".to_string(),
    ];
    if let Some(bytes) = khmer_fonts.iter().find_map(|p| std::fs::read(p).ok()) {
        crate::shaping::init(bytes.clone());
        fonts.font_data.insert("khmer".into(), Arc::new(FontData::from_owned(bytes)));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let list = fonts.families.entry(family).or_default();
            list.insert(1.min(list.len()), "khmer".into());
        }
    }
    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context, choice: ThemeChoice, dark: &str, light: &str) {
    install_fonts(ctx);
    set_themes(ctx, dark, light);
    set_choice(ctx, choice);
}

/// Pick the palettes used for dark and light appearance.
pub fn set_themes(ctx: &egui::Context, dark: &str, light: &str) {
    let (d, l) = (index_of(DARK_THEMES, dark), index_of(LIGHT_THEMES, light));
    DARK_IDX.store(d, Ordering::Relaxed);
    LIGHT_IDX.store(l, Ordering::Relaxed);
    ctx.style_mut_of(Theme::Dark, |style| style_for(style, &DARK_THEMES[d], Visuals::dark()));
    ctx.style_mut_of(Theme::Light, |style| style_for(style, &LIGHT_THEMES[l], Visuals::light()));
}

fn style_for(style: &mut egui::Style, c: &Palette, base: Visuals) {
    let mut v = base;
    v.override_text_color = Some(c.text);
    v.weak_text_color = Some(c.faint);
    v.panel_fill = c.bg;
    v.window_fill = c.surface;
    v.extreme_bg_color = c.input;
    v.faint_bg_color = c.stripe;
    v.code_bg_color = c.surface;
    v.window_stroke = Stroke::new(1.0_f32, c.border);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.hyperlink_color = c.accent;
    v.error_fg_color = c.error;
    v.warn_fg_color = c.warn;
    v.selection.bg_fill = c.accent.gamma_multiply(0.28);
    v.selection.stroke = Stroke::new(1.0_f32, c.accent);
    v.text_cursor.stroke = Stroke::new(1.5_f32, c.accent);

    let r = CornerRadius::same(5);
    let w = &mut v.widgets;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, c.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, c.text);
    w.noninteractive.corner_radius = r;
    for (state, fill) in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open].into_iter().zip(c.widget) {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::NONE;
        state.corner_radius = r;
        state.expansion = 0.0;
    }
    w.hovered.bg_stroke = Stroke::new(1.0_f32, c.border);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, c.text);
    w.hovered.fg_stroke = Stroke::new(1.0_f32, c.strong);
    w.active.fg_stroke = Stroke::new(1.0_f32, c.strong);
    style.visuals = v;

    style.spacing.item_spacing = vec2(8.0, 5.0);
    style.spacing.button_padding = vec2(9.0, 4.0);
    style.spacing.interact_size.y = 24.0;
    style.spacing.indent = 14.0;
    style.spacing.scroll.floating = true;
    style.spacing.scroll.bar_width = 8.0;

    style.text_styles = [
        (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(13.0, FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(18.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
    ]
    .into();
}
