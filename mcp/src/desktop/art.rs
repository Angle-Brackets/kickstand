// SPDX-License-Identifier: GPL-3.0-only
//! The drawings: the Switch from the side (up on its kickstand while it's reachable, lying flat when
//! not) and from the front, showing the pairing code.

use eframe::egui::{Align2, Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2};

use super::theme::{BLUE, Face, INK, PAPER, RED, SCREEN, SCREEN_TEXT};

/// The side view, drawn in a 260×170 box scaled into `rect`. `standing`: on its kickstand.
pub fn side(painter: &Painter, rect: Rect, standing: bool) {
    let s = rect.width() / 260.0;
    let p = |x: f32, y: f32| rect.min + Vec2::new(x, y) * s;
    painter.line_segment([p(8.0, 158.0), p(252.0, 158.0)], Stroke::new(2.0 * s, INK));
    if standing {
        let slab = rounded_rect(Rect::from_min_size(p(110.0, 24.0), Vec2::new(16.0, 134.0) * s), 5.0 * s);
        let pivot = p(110.0, 158.0);
        let slab = slab.into_iter().map(|q| rotate(q, pivot, 14f32.to_radians())).collect();
        painter.add(Shape::convex_polygon(slab, INK, Stroke::NONE));
        painter.line_segment([p(141.0, 88.0), p(186.0, 157.0)], Stroke::new(7.0 * s, RED));
        painter.circle_filled(p(186.0, 157.0), 3.5 * s, RED);
        painter.circle(p(141.0, 88.0), 5.0 * s, PAPER, Stroke::new(2.0 * s, INK));
    } else {
        let slab = rounded_rect(Rect::from_min_size(p(52.0, 141.0), Vec2::new(134.0, 16.0) * s), 5.0 * s);
        painter.add(Shape::convex_polygon(slab, INK, Stroke::NONE));
    }
}

/// The front view with `code` on its screen, drawn in a 440×230 box scaled into `rect`.
pub fn front(painter: &Painter, rect: Rect, code: &str) {
    let s = rect.width() / 440.0;
    let p = |x: f32, y: f32| rect.min + Vec2::new(x, y) * s;
    let r = |x: f32, y: f32, w: f32, h: f32| Rect::from_min_size(p(x, y), Vec2::new(w, h) * s);
    let outline = Stroke::new(2.5 * s, INK);
    let round = (32.0 * s).round().min(255.0) as u8;
    let left = CornerRadius { nw: round, sw: round, ne: 0, se: 0 };
    let right = CornerRadius { nw: 0, sw: 0, ne: round, se: round };
    painter.rect(r(4.0, 10.0, 66.0, 210.0), left, BLUE, outline, StrokeKind::Middle);
    painter.rect(r(370.0, 10.0, 66.0, 210.0), right, RED, outline, StrokeKind::Middle);
    painter.rect_filled(r(72.0, 10.0, 296.0, 210.0), 3.0 * s, INK);
    painter.rect_filled(r(86.0, 24.0, 268.0, 182.0), 2.0 * s, SCREEN);

    let thick = Stroke::new(2.5 * s, INK);
    let thin = Stroke::new(2.0 * s, INK);
    // Left Joy-Con: minus, stick, D-pad.
    painter.line_segment([p(50.0, 28.0), p(60.0, 28.0)], thick);
    painter.circle_stroke(p(38.0, 66.0), 14.0 * s, thick);
    painter.circle_filled(p(38.0, 66.0), 6.0 * s, INK);
    for (x, y) in [(38.0, 116.0), (38.0, 146.0), (23.0, 131.0), (53.0, 131.0)] {
        painter.circle_stroke(p(x, y), 6.0 * s, thin);
    }
    // Right Joy-Con: plus, face buttons, stick.
    painter.line_segment([p(380.0, 28.0), p(390.0, 28.0)], thick);
    painter.line_segment([p(385.0, 23.0), p(385.0, 33.0)], thick);
    for (x, y) in [(402.0, 62.0), (402.0, 92.0), (387.0, 77.0), (417.0, 77.0)] {
        painter.circle_stroke(p(x, y), 6.0 * s, thin);
    }
    painter.circle_stroke(p(402.0, 142.0), 14.0 * s, thick);
    painter.circle_filled(p(402.0, 142.0), 6.0 * s, INK);

    painter.text(p(106.0, 62.0), Align2::LEFT_BOTTOM, "Pair a PC to start", Face::Regular.size(13.0 * s), SCREEN_TEXT);
    let code_rect = painter.text(p(106.0, 124.0), Align2::LEFT_BOTTOM, code, Face::MonoSemi.size(40.0 * s), PAPER);
    painter.rect_filled(Rect::from_min_size(p(106.0, 136.0), Vec2::new(code_rect.width().min(96.0 * s), 4.0 * s)), 0.0, RED);
    painter.text(p(106.0, 184.0), Align2::LEFT_BOTTOM, "Same Wi-Fi as your PC", Face::Regular.size(12.0 * s), SCREEN_TEXT);
}

/// The little signal bars beside "LOOKING", the red one stepping along while `t` (seconds) runs.
pub fn looking(painter: &Painter, at: Pos2, t: f64) {
    let heights = [8.0, 10.0, 12.0, 6.0];
    let lit = (t * 2.5) as usize % heights.len();
    for (i, h) in heights.iter().enumerate() {
        let x = at.x + i as f32 * 5.0;
        let color = if i == lit { RED } else { INK };
        painter.rect_filled(Rect::from_min_max(Pos2::new(x, at.y + 12.0 - h), Pos2::new(x + 3.0, at.y + 12.0)), 0.0, color);
    }
}

/// A check mark in a 14×11 box at `at`.
pub fn check(painter: &Painter, at: Pos2, color: Color32) {
    let p = |x: f32, y: f32| at + Vec2::new(x, y);
    painter.add(Shape::line(vec![p(1.0, 5.5), p(5.0, 9.5), p(13.0, 1.5)], Stroke::new(2.2, color)));
}

/// A down chevron in a 12×8 box at `at`.
pub fn chevron(painter: &Painter, at: Pos2) {
    let p = |x: f32, y: f32| at + Vec2::new(x, y);
    painter.add(Shape::line(vec![p(1.0, 1.5), p(6.0, 6.5), p(11.0, 1.5)], Stroke::new(2.0, INK)));
}

/// A section's fold mark in a 12×12 box at `at`: pointing down when `open` is 1, right when 0, turning
/// in between.
pub fn fold(painter: &Painter, at: Pos2, open: f32, color: Color32) {
    let center = at + Vec2::splat(6.0);
    let turn = eframe::egui::emath::Rot2::from_angle((1.0 - open) * -std::f32::consts::FRAC_PI_2);
    let points = [Vec2::new(-4.5, -2.0), Vec2::new(0.0, 2.5), Vec2::new(4.5, -2.0)].map(|v| center + turn * v);
    painter.add(Shape::line(points.to_vec(), Stroke::new(2.0, color)));
}

/// Kickstand's mark: the K whose lower leg is a kickstand, on an ink tile filling `rect` (drawn in the
/// 256×256 box of `assets/logo/kickstand-icon.svg`).
pub fn mark(painter: &Painter, rect: Rect) {
    painter.rect_filled(rect, 0.0, INK);
    let s = rect.width() / 256.0;
    let p = |x: f32, y: f32| rect.min + Vec2::new(x, y) * s;
    let fill = |points: Vec<Pos2>, color: Color32| {
        painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
    };
    fill(rounded_rect(Rect::from_min_size(p(78.0, 56.0), Vec2::new(30.0, 144.0) * s), 7.0 * s), PAPER);
    let origin = p(108.0, 132.0);
    let part = |x: f32, y: f32, w: f32, h: f32, radius: f32, degrees: f32| -> Vec<Pos2> {
        let local = Rect::from_min_size(origin + Vec2::new(x, y) * s, Vec2::new(w, h) * s);
        rounded_rect(local, radius * s).into_iter().map(|q| rotate(q, origin, degrees.to_radians())).collect()
    };
    fill(part(-15.0, -100.0, 30.0, 100.0, 7.0, 42.0), PAPER);
    fill(part(-15.0, 0.0, 30.0, 96.0, 7.0, -42.0), RED);
    fill(part(-15.0, 84.0, 30.0, 12.0, 5.0, -42.0), Color32::from_black_alpha(71));
    painter.circle_filled(rotate(origin + Vec2::new(0.0, 11.0) * s, origin, (-42f32).to_radians()), 4.5 * s, INK);
}

fn rotate(q: Pos2, about: Pos2, angle: f32) -> Pos2 {
    let (sin, cos) = angle.sin_cos();
    let d = q - about;
    about + Vec2::new(d.x * cos - d.y * sin, d.x * sin + d.y * cos)
}

/// The outline of `rect` with corners of `radius`, clockwise.
fn rounded_rect(rect: Rect, radius: f32) -> Vec<Pos2> {
    let corners = [
        (rect.right_top() + Vec2::new(-radius, radius), -90f32),
        (rect.right_bottom() + Vec2::new(-radius, -radius), 0.0),
        (rect.left_bottom() + Vec2::new(radius, -radius), 90.0),
        (rect.left_top() + Vec2::new(radius, radius), 180.0),
    ];
    let mut points = Vec::new();
    for (center, start) in corners {
        for step in 0..=6 {
            let a = (start + step as f32 * 15.0).to_radians();
            points.push(center + Vec2::new(a.cos(), a.sin()) * radius);
        }
    }
    points
}
