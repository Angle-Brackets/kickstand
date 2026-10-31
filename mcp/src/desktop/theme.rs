// SPDX-License-Identifier: GPL-3.0-only
//! The desktop app's look: a printed spec sheet. Paper and ink, rules instead of cards, Archivo for words,
//! JetBrains Mono for labels and values, and red only where something is live or to be pressed.

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontData, FontDefinitions, FontFamily, FontId, Pos2, Rect, Response, Sense, Shape,
    Stroke, StrokeKind, Ui, Vec2, text::LayoutJob,
};

pub const PAPER: Color32 = Color32::from_rgb(0xF2, 0xF0, 0xEA);
pub const INK: Color32 = Color32::from_rgb(0x15, 0x17, 0x1C);
pub const RED: Color32 = Color32::from_rgb(0xFF, 0x45, 0x54);
pub const BLUE: Color32 = Color32::from_rgb(0x0A, 0xB9, 0xE6);
/// Secondary text: labels, metadata.
pub const MUTED: Color32 = Color32::from_rgb(0x5A, 0x59, 0x54);
/// Body copy.
pub const BODY: Color32 = Color32::from_rgb(0x3A, 0x3A, 0x37);
pub const RULE: Color32 = Color32::from_rgb(0xC9, 0xC5, 0xBA);
pub const RULE_LIGHT: Color32 = Color32::from_rgb(0xDD, 0xD9, 0xCF);
pub const FIELD: Color32 = Color32::from_rgb(0xFB, 0xFA, 0xF6);
/// Hovered outline buttons.
pub const PRESSED_PAPER: Color32 = Color32::from_rgb(0xE6, 0xE3, 0xDA);
/// The Switch's screen, and the text on it.
pub const SCREEN: Color32 = Color32::from_rgb(0x23, 0x26, 0x2E);
pub const SCREEN_TEXT: Color32 = Color32::from_rgb(0xA9, 0xA7, 0xAE);

/// Page margins, and the widths the layout switches at.
pub const GUTTER: f32 = 40.0;

#[derive(Clone, Copy)]
pub enum Face {
    Regular,
    Semi,
    Bold,
    Heavy,
    Mono,
    MonoSemi,
}

impl Face {
    fn family(self) -> &'static str {
        match self {
            Face::Regular => "archivo-400",
            Face::Semi => "archivo-600",
            Face::Bold => "archivo-700",
            Face::Heavy => "archivo-800",
            Face::Mono => "mono-400",
            Face::MonoSemi => "mono-600",
        }
    }

    pub fn size(self, size: f32) -> FontId {
        FontId::new(size, FontFamily::Name(self.family().into()))
    }
}

const FONTS: [(&str, &[u8]); 6] = [
    ("archivo-400", include_bytes!("../../../assets/fonts/Archivo-Regular.ttf")),
    ("archivo-600", include_bytes!("../../../assets/fonts/Archivo-SemiBold.ttf")),
    ("archivo-700", include_bytes!("../../../assets/fonts/Archivo-Bold.ttf")),
    ("archivo-800", include_bytes!("../../../assets/fonts/Archivo-ExtraBold.ttf")),
    ("mono-400", include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf")),
    ("mono-600", include_bytes!("../../../assets/fonts/JetBrainsMono-SemiBold.ttf")),
];

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    // egui's own fonts stay behind each family, for symbols ours lack.
    let fallback: Vec<String> = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    for (name, data) in FONTS {
        fonts.font_data.insert(name.into(), FontData::from_static(data).into());
        let mut family = vec![name.to_string()];
        family.extend(fallback.iter().cloned());
        fonts.families.insert(FontFamily::Name(name.into()), family);
    }
    fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "archivo-400".into());
    fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "mono-400".into());
    ctx.set_fonts(fonts);

    ctx.set_theme(egui::Theme::Light);
    ctx.all_styles_mut(|style| {
        // Folding groups and egui's own animations at the pace of the rest.
        style.animation_time = super::OPEN_S;
        let v = &mut style.visuals;
        *v = egui::Visuals::light();
        v.panel_fill = PAPER;
        v.window_fill = PAPER;
        v.extreme_bg_color = FIELD;
        v.faint_bg_color = PRESSED_PAPER;
        v.override_text_color = Some(INK);
        v.hyperlink_color = INK;
        v.selection.bg_fill = RED.gamma_multiply(0.35);
        v.selection.stroke = Stroke::new(1.5, INK);
        v.window_stroke = Stroke::new(1.5, INK);
        v.window_corner_radius = 0.into();
        v.window_shadow = egui::Shadow::NONE;
        v.popup_shadow = egui::Shadow::NONE;
        v.text_cursor.stroke = Stroke::new(2.0, INK);
        for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.corner_radius = 0.into();
            w.fg_stroke.color = INK;
        }
        v.widgets.inactive.bg_stroke = Stroke::new(1.5, INK);
        v.widgets.hovered.bg_stroke = Stroke::new(1.5, INK);
        v.widgets.active.bg_stroke = Stroke::new(2.0, INK);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, RULE);
        style.spacing.item_spacing = Vec2::new(10.0, 10.0);
        style.interaction.selectable_labels = false;
    });
}

/// `text` as one run, with `spacing` extra between letters in em (−0.02 for headings, 0.04 for labels).
pub fn job(text: &str, face: Face, size: f32, color: Color32, spacing: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let format = egui::TextFormat { font_id: face.size(size), color, extra_letter_spacing: spacing * size, ..Default::default() };
    job.append(text, 0.0, format);
    job
}

/// A run of text that wraps at the available width, with room between its lines.
pub fn text(ui: &mut Ui, text: &str, face: Face, size: f32, color: Color32) -> Response {
    let mut job = job(text, face, size, color, 0.0);
    job.sections[0].format.line_height = Some(size * 1.45);
    ui.add(egui::Label::new(job).wrap())
}

/// A heading in heavy Archivo, tightly spaced.
pub fn heading(ui: &mut Ui, text: &str, size: f32) -> Response {
    let mut job = job(text, Face::Heavy, size, INK, -0.02);
    job.sections[0].format.line_height = Some(size * 1.04);
    ui.add(egui::Label::new(job).wrap())
}

/// An uppercase mono label (section titles, metadata).
pub fn label(ui: &mut Ui, text: &str, semi: bool, size: f32, color: Color32) -> Response {
    let face = if semi { Face::MonoSemi } else { Face::Mono };
    ui.add(egui::Label::new(job(text, face, size, color, 0.04)).wrap())
}

/// A horizontal rule across the available width.
pub fn rule(ui: &mut Ui, width: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), width), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, color);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Ink, with a red edge under it: the thing to do next.
    Primary,
    Outline,
    /// Something that interrupts the console (restarting it).
    Dashed,
}

/// One of the app's buttons. `tall` is the 48 px size of the main actions; others are 44 px.
pub fn button(ui: &mut Ui, label: &str, kind: Kind, tall: bool, enabled: bool) -> Response {
    let size = if tall { 16.0 } else { 14.0 };
    let face = if kind == Kind::Primary { Face::Bold } else { Face::Semi };
    let galley = ui.painter().layout_job(job(label, face, size, INK, 0.0));
    let pad = if tall { 26.0 } else { 16.0 };
    let height = if tall { 48.0 } else { 44.0 };
    let edge = if kind == Kind::Primary { if tall { 4.0 } else { 3.0 } } else { 0.0 };
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(galley.size().x + 2.0 * pad, height + edge), sense);
    let response = if enabled { response.on_hover_cursor(CursorIcon::PointingHand) } else { response };
    let pressed = response.is_pointer_button_down_on();
    // Hover colours ease in and out.
    let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered() && !pressed, 0.12);
    let alpha = if enabled { 1.0 } else { 0.45 };
    let painter = ui.painter();
    // Pressing a primary button pushes it down onto its red edge.
    let sink = if pressed && kind == Kind::Primary { edge - 1.0 } else { 0.0 };
    let face_rect = Rect::from_min_size(rect.min + Vec2::new(0.0, sink), Vec2::new(rect.width(), height));
    let fg = match kind {
        Kind::Primary => {
            painter.rect_filled(Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - edge), rect.max), 0.0, RED.gamma_multiply(alpha));
            let fill = INK.lerp_to_gamma(Color32::from_rgb(0x2A, 0x2D, 0x35), hover);
            painter.rect_filled(face_rect, 0.0, fill.gamma_multiply(alpha));
            PAPER
        }
        Kind::Outline | Kind::Dashed => {
            painter.rect_filled(face_rect, 0.0, if pressed { RULE_LIGHT } else { PRESSED_PAPER.gamma_multiply(hover) });
            let stroke = Stroke::new(1.5, INK.gamma_multiply(alpha));
            if kind == Kind::Outline {
                painter.rect_stroke(face_rect, 0.0, stroke, StrokeKind::Inside);
            } else {
                dashed_rect(painter, face_rect.shrink(0.75), stroke);
            }
            INK
        }
    };
    let galley = ui.painter().layout_job(job(label, face, size, fg.gamma_multiply(alpha), 0.0));
    let pos = Align2::CENTER_CENTER.anchor_size(face_rect.center(), galley.size()).min;
    ui.painter().galley(pos, galley, fg);
    response
}

/// Underlined text that acts as a link ("Back", "Enter its address").
pub fn link(ui: &mut Ui, label: &str, size: f32, face: Face) -> Response {
    let galley = ui.painter().layout_job(job(label, face, size, INK, 0.0));
    let (rect, response) = ui.allocate_exact_size(galley.size() + Vec2::new(0.0, 4.0), Sense::click());
    let response = response.on_hover_cursor(CursorIcon::PointingHand);
    let color = if response.hovered() { MUTED } else { INK };
    let painter = ui.painter();
    painter.galley(rect.min, galley, color);
    let y = rect.min.y + size * 1.12;
    painter.line_segment([Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)], Stroke::new(1.5, color));
    response
}

pub fn dashed_rect(painter: &egui::Painter, rect: Rect, stroke: Stroke) {
    let corners = [rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom(), rect.left_top()];
    painter.extend(Shape::dashed_line(&corners, stroke, 6.0, 4.0));
}

/// A small solid tag ("SHOWING A CODE") or, with `outline`, an outlined one ("PAIRED").
pub fn tag(ui: &mut Ui, text: &str, outline: bool) -> Response {
    let galley = ui.painter().layout_job(job(text, Face::MonoSemi, 11.0, INK, 0.06));
    let (rect, response) = ui.allocate_exact_size(galley.size() + Vec2::new(14.0, 6.0), Sense::hover());
    if outline {
        ui.painter().rect_stroke(rect, 0.0, Stroke::new(1.5, INK), StrokeKind::Inside);
    } else {
        ui.painter().rect_filled(rect, 0.0, RED);
    }
    ui.painter().galley(rect.min + Vec2::new(7.0, 3.0), galley, INK);
    response
}

/// The small square before a status ("AGENT CONNECTED").
pub fn status(ui: &mut Ui, text: &str, color: Color32) -> Response {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
        ui.painter().rect_filled(rect, 0.0, color);
        label(ui, text, true, 13.0, INK);
    })
    .response
}
