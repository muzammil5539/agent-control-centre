//! Visual design system (from ui-ux-pro-max: dark slate + green accent, Fira Sans / Fira Code).
use eframe::egui::{
    self, Color32, FontData, FontDefinitions, FontFamily, FontId, TextStyle, Visuals,
};
use std::sync::Arc;

pub const BG: Color32 = Color32::from_rgb(0x0F, 0x17, 0x2A);
pub const WELL: Color32 = Color32::from_rgb(0x0B, 0x11, 0x20);
pub const CARD: Color32 = Color32::from_rgb(0x1B, 0x23, 0x36);
pub const MUTED: Color32 = Color32::from_rgb(0x27, 0x2F, 0x42);
pub const BORDER: Color32 = Color32::from_rgb(0x2E, 0x3A, 0x55);
pub const FG: Color32 = Color32::from_rgb(0xF8, 0xFA, 0xFC);
pub const DIM: Color32 = Color32::from_rgb(0x94, 0xA3, 0xB8);
pub const ACCENT: Color32 = Color32::from_rgb(0x22, 0xC5, 0x5E);
pub const DANGER: Color32 = Color32::from_rgb(0xEF, 0x44, 0x44);

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

pub fn apply(ctx: &egui::Context) {
    let mut f = FontDefinitions::default();
    for (k, bytes) in [
        (
            "fira",
            &include_bytes!("../assets/fonts/FiraSans-Regular.ttf")[..],
        ),
        (
            "firasb",
            &include_bytes!("../assets/fonts/FiraSans-SemiBold.ttf")[..],
        ),
        (
            "firacode",
            &include_bytes!("../assets/fonts/FiraCode.ttf")[..],
        ),
    ] {
        f.font_data
            .insert(k.into(), Arc::new(FontData::from_static(bytes)));
    }
    // Defaults stay as fallbacks (emoji/symbols); ours go first.
    f.families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "fira".into());
    f.families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "firacode".into());
    f.families.insert(
        FontFamily::Name("semibold".into()),
        vec!["firasb".into(), "fira".into()],
    );
    ctx.set_fonts(f);

    ctx.global_style_mut(|s| {
        let mut v = Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = CARD;
        v.override_text_color = Some(FG);
        v.extreme_bg_color = WELL; // text inputs
        v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
        for w in [
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
        ] {
            w.corner_radius = 8.into();
        }
        v.widgets.inactive.bg_fill = MUTED;
        v.widgets.inactive.weak_bg_fill = MUTED;
        v.widgets.hovered.weak_bg_fill = BORDER;
        v.widgets.hovered.bg_fill = BORDER;
        v.widgets.noninteractive.bg_stroke.color = BORDER;
        s.visuals = v;
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.interaction.selectable_labels = false;
        s.text_styles = [
            (TextStyle::Heading, semibold(18.0)),
            (TextStyle::Body, FontId::proportional(13.5)),
            (TextStyle::Button, FontId::proportional(13.0)),
            (TextStyle::Small, FontId::proportional(11.0)),
            (TextStyle::Monospace, FontId::monospace(12.0)),
        ]
        .into();
    });
}
