// SPDX-License-Identifier: GPL-3.0-only
//! The window's own title bar: a strip along the top that moves the window, with minimize, maximize and
//! close in its right corner, drawn in the app's style. Off macOS the window has no system frame, so its
//! edges resize it here. macOS keeps its frame and its own buttons on the left, over the strip, with the
//! system title bar hidden.

use eframe::egui::{self, Color32, Painter, Pos2, Rect, Sense, Stroke, Vec2, ViewportCommand};
use egui::viewport::ResizeDirection;

use super::theme::{INK, PAPER, PRESSED_PAPER, RED};

pub const MAC: bool = cfg!(target_os = "macos");
/// How far in from the window's edge a press resizes it.
const EDGE: f32 = 6.0;
/// The strip's height: on macOS, its title bar's, which its buttons are centred in.
const BAR: f32 = if MAC { 28.0 } else { 32.0 };
const BUTTON_WIDTH: f32 = 46.0;

pub fn viewport(viewport: egui::ViewportBuilder) -> egui::ViewportBuilder {
    if MAC {
        viewport.with_fullsize_content_view(true).with_titlebar_shown(false).with_title_shown(false)
    } else {
        viewport.with_decorations(false)
    }
}

fn maximized(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.viewport().maximized.unwrap_or(false) || i.viewport().fullscreen.unwrap_or(false))
}

/// The edge or corner under the pointer that resizes the window, if any.
fn edge(ctx: &egui::Context) -> Option<ResizeDirection> {
    if MAC || maximized(ctx) {
        return None;
    }
    let rect = ctx.content_rect();
    let at = ctx.input(|i| i.pointer.hover_pos())?;
    let west = at.x < rect.left() + EDGE;
    let east = at.x > rect.right() - EDGE;
    let north = at.y < rect.top() + EDGE;
    let south = at.y > rect.bottom() - EDGE;
    Some(match (north, south, west, east) {
        (true, _, true, _) => ResizeDirection::NorthWest,
        (true, _, _, true) => ResizeDirection::NorthEast,
        (_, true, true, _) => ResizeDirection::SouthWest,
        (_, true, _, true) => ResizeDirection::SouthEast,
        (true, ..) => ResizeDirection::North,
        (_, true, ..) => ResizeDirection::South,
        (.., true, _) => ResizeDirection::West,
        (.., true) => ResizeDirection::East,
        _ => return None,
    })
}

/// Resizes the window from its edges, and outlines it (it has no system frame to show where it ends).
/// Called before anything else is drawn.
pub fn frame(ctx: &egui::Context) {
    if MAC {
        return;
    }
    if let Some(edge) = edge(ctx) {
        ctx.set_cursor_icon(match edge {
            ResizeDirection::North => egui::CursorIcon::ResizeNorth,
            ResizeDirection::South => egui::CursorIcon::ResizeSouth,
            ResizeDirection::West => egui::CursorIcon::ResizeWest,
            ResizeDirection::East => egui::CursorIcon::ResizeEast,
            ResizeDirection::NorthWest => egui::CursorIcon::ResizeNorthWest,
            ResizeDirection::NorthEast => egui::CursorIcon::ResizeNorthEast,
            ResizeDirection::SouthWest => egui::CursorIcon::ResizeSouthWest,
            ResizeDirection::SouthEast => egui::CursorIcon::ResizeSouthEast,
        });
        if ctx.input(|i| i.pointer.primary_pressed()) {
            ctx.send_viewport_cmd(ViewportCommand::BeginResize(edge));
        }
    }
    if !maximized(ctx) {
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique("window-outline")));
        painter.rect_stroke(ctx.content_rect(), 0.0, Stroke::new(1.0, INK), egui::StrokeKind::Inside);
    }
}

/// The strip along the top of the window.
pub fn title_bar(ui: &mut egui::Ui) {
    egui::Panel::top("title-bar").frame(egui::Frame::new().fill(PAPER)).exact_size(BAR).resizable(false).show_separator_line(false).show(ui, |ui| {
        drag_area(ui, ui.max_rect());
        if !MAC {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for control in [Control::Close, Control::Maximize, Control::Minimize] {
                    button(ui, control);
                }
            });
        }
    });
}

/// Makes `rect` move the window, and maximize or restore it on a double click. Called before the widgets
/// on it, which stay clickable on top of it.
pub fn drag_area(ui: &mut egui::Ui, rect: Rect) {
    let response = ui.interact(rect, ui.scope_id().with("window-drag"), Sense::click_and_drag());
    if edge(ui.ctx()).is_some() {
        return;
    }
    if response.double_clicked() {
        ui.ctx().send_viewport_cmd(ViewportCommand::Maximized(!maximized(ui.ctx())));
    } else if response.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }
}

#[derive(Clone, Copy)]
enum Control {
    Minimize,
    Maximize,
    Close,
}

fn button(ui: &mut egui::Ui, control: Control) {
    let max = maximized(ui.ctx());
    let tip = match control {
        Control::Minimize => "Minimize",
        Control::Maximize if max => "Restore",
        Control::Maximize => "Maximize",
        Control::Close => "Close",
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(BUTTON_WIDTH, BAR), Sense::click());
    let response = response.on_hover_text(tip);
    let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.12);
    let painter = ui.painter();
    let (fill, ink) = match control {
        Control::Close => (RED, lerp(INK, PAPER, hover)),
        _ => (PRESSED_PAPER, INK),
    };
    painter.rect_filled(rect, 0.0, fill.gamma_multiply(hover));
    glyph(painter, rect.center(), control, max, ink);
    if response.clicked() {
        ui.ctx().send_viewport_cmd(match control {
            Control::Minimize => ViewportCommand::Minimized(true),
            Control::Maximize => ViewportCommand::Maximized(!max),
            Control::Close => ViewportCommand::Close,
        });
    }
}

fn glyph(painter: &Painter, at: Pos2, control: Control, maximized: bool, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    let box_ = |half: f32| Rect::from_center_size(at, Vec2::splat(half * 2.0));
    match control {
        Control::Minimize => {
            painter.hline(at.x - 5.0..=at.x + 5.0, at.y, stroke);
        }
        Control::Maximize if maximized => {
            // Two windows, one behind the other.
            painter.rect_stroke(box_(4.0).translate(Vec2::new(-1.5, 1.5)), 0.0, stroke, egui::StrokeKind::Middle);
            let back = [(-2.5, -2.5), (-2.5, -5.5), (5.5, -5.5), (5.5, 2.5), (2.5, 2.5)];
            painter.line(back.iter().map(|&(x, y)| at + Vec2::new(x, y)).collect(), stroke);
        }
        Control::Maximize => {
            painter.rect_stroke(box_(5.0), 0.0, stroke, egui::StrokeKind::Middle);
        }
        Control::Close => {
            let r = box_(5.0);
            painter.line_segment([r.left_top(), r.right_bottom()], stroke);
            painter.line_segment([r.right_top(), r.left_bottom()], stroke);
        }
    }
}

fn lerp(from: Color32, to: Color32, t: f32) -> Color32 {
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color32::from_rgb(mix(from.r(), to.r()), mix(from.g(), to.g()), mix(from.b(), to.b()))
}
