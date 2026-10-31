// SPDX-License-Identifier: GPL-3.0-only
//! The desktop app: find the Switch, pair with the code on its screen, link the AI app, then watch what
//! the agent is doing. The network side is in [`worker`]; this draws the [`worker::Model`] and sends
//! [`worker::Cmd`]s.

mod art;
mod brands;
mod screens;
mod theme;
mod worker;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use eframe::egui::{self, Frame, Margin, Sense, Stroke, Vec2};
use tokio::sync::mpsc::UnboundedSender;

use theme::{Face, INK, MUTED, PAPER, RED, RULE};
use worker::{Cmd, Model};

/// The window's icon (the mark on its ink tile).
const ICON: &[u8] = include_bytes!("../../../assets/logo/kickstand-icon.jpg");

pub fn run() -> Result<()> {
    let icon = image::load_from_memory_with_format(ICON, image::ImageFormat::Jpeg).ok().map(|image| {
        let image = image.into_rgba8();
        egui::IconData { width: image.width(), height: image.height(), rgba: image.into_raw() }
    });
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Kickstand")
        .with_app_id("kickstand")
        .with_inner_size([1200.0, 760.0])
        // The smallest size every screen fits in side by side; the window itself doesn't scroll.
        .with_min_inner_size([920.0, 740.0]);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("Kickstand", options, Box::new(|cc| Ok(Box::new(Kickstand::new(&cc.egui_ctx)))))
        .map_err(|e| anyhow!("couldn't open the window ({e}); `kickstand serve` runs the MCP server without one"))
}

#[derive(Clone, PartialEq, Eq)]
pub enum Screen {
    Find,
    /// Typing the code shown by the console at `addr`, called `name`.
    Pair { addr: SocketAddr, name: String },
    Link,
    Home,
}

pub struct Kickstand {
    model: Arc<Mutex<Model>>,
    tx: UnboundedSender<Cmd>,
    screen: Screen,
    /// The pairing code typed so far.
    code: String,
    /// "Enter its address" is open, and what's typed in it.
    address: Option<String>,
    confirm_reboot: bool,
    /// The Link screen's tab.
    tab: crate::link::Maker,
    /// Which makers' groups are folded in the home screen's linked apps (by [`crate::link::Maker::index`]).
    folded: [bool; 4],
    brands: brands::Brands,
}

impl Kickstand {
    fn new(ctx: &egui::Context) -> Self {
        theme::install(ctx);
        let (model, tx) = worker::spawn(ctx.clone());
        let (screen, tab) = {
            let model = model.lock().unwrap();
            let screen = if model.host.is_some() { Screen::Home } else { Screen::Find };
            // The tab of an app that's linked, else of one that's installed, else Claude's.
            let linked = model.links.iter().find(|(_, s)| matches!(s, crate::link::State::Linked { .. }));
            let installed = || model.links.iter().find(|(_, s)| !matches!(s, crate::link::State::Missing));
            (screen, linked.or_else(installed).map_or(crate::link::Maker::Claude, |(app, _)| app.maker()))
        };
        let brands = brands::Brands::load(ctx);
        Kickstand { model, tx, screen, code: String::new(), address: None, confirm_reboot: false, tab, folded: [false; 4], brands }
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    /// Shows `screen`. (`model` is the locked model the frame is drawn from.)
    fn go(&mut self, model: &mut Model, screen: Screen) {
        if let Screen::Pair { .. } = screen {
            self.code.clear();
            model.pairing = worker::Pairing::Idle;
        }
        model.notice = None;
        self.screen = screen;
    }

    fn header(&mut self, ui: &mut egui::Ui, model: &mut Model) {
        let frame = Frame::new().fill(PAPER).inner_margin(Margin::symmetric(theme::GUTTER as i8, 20));
        let panel = egui::Panel::top("header").frame(frame).resizable(false).show_separator_line(false).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
                art::mark(ui.painter(), rect);
                ui.label(theme::job("Kickstand", Face::Heavy, 22.0, INK, -0.01));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match &self.screen {
                    Screen::Home => {
                        if self.selector(ui, model).clicked() {
                            self.go(model, Screen::Find);
                        }
                    }
                    screen => {
                        if let Some(next) = stepper(ui, screen, model) {
                            self.go(model, next);
                        }
                    }
                });
            });
        });
        let rect = panel.response.rect;
        ui.painter().hline(rect.x_range(), rect.bottom(), Stroke::new(1.5, INK));
    }

    /// The header's button naming the console in use; it leads back to the list.
    fn selector(&self, ui: &mut egui::Ui, model: &Model) -> egui::Response {
        let name = model.name.clone().unwrap_or_else(|| "Your Switch".into());
        let host = model.host.clone().unwrap_or_default();
        let name_galley = ui.painter().layout_job(theme::job(&name, Face::Semi, 15.0, INK, 0.0));
        let host_galley = ui.painter().layout_job(theme::job(&host, Face::Mono, 12.0, MUTED, 0.0));
        let width = 14.0 + name_galley.size().x + 10.0 + host_galley.size().x + 10.0 + 12.0 + 14.0;
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 44.0), Sense::click());
        let response = response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Use a different Switch");
        let painter = ui.painter();
        if response.hovered() {
            painter.rect_filled(rect, 0.0, theme::PRESSED_PAPER);
        }
        painter.rect_stroke(rect, 0.0, Stroke::new(1.5, INK), egui::StrokeKind::Inside);
        let mut x = rect.left() + 14.0;
        let cy = rect.center().y;
        painter.galley(egui::pos2(x, cy - name_galley.size().y / 2.0), name_galley.clone(), INK);
        x += name_galley.size().x + 10.0;
        painter.galley(egui::pos2(x, cy - host_galley.size().y / 2.0), host_galley.clone(), MUTED);
        x += host_galley.size().x + 10.0;
        art::chevron(painter, egui::pos2(x, cy - 4.0));
        response
    }

    fn footer(&self, ui: &mut egui::Ui, model: &Model) {
        let frame = Frame::new().fill(PAPER).inner_margin(Margin::symmetric(theme::GUTTER as i8, 14));
        let panel = egui::Panel::bottom("footer").frame(frame).resizable(false).show_separator_line(false).show(ui, |ui| {
            ui.horizontal(|ui| {
                let console = match &model.reach {
                    worker::Reach::Connected(live) => format!(" · SWITCH {} · PROTOCOL {}", live.info.version.to_uppercase(), live.info.protocol),
                    _ => String::new(),
                };
                let left = format!("KICKSTAND {}{console}", env!("CARGO_PKG_VERSION"));
                theme::label(ui, &left, false, 12.0, MUTED);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| theme::label(ui, "GPL-3.0", false, 12.0, MUTED));
            });
        });
        let rect = panel.response.rect;
        ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, RULE));
    }
}

/// 01 FIND · 02 PAIR · 03 LINK, the current one underlined in red; the others are links to their screen.
/// Pair needs a Switch showing a code (the first one found). Returns the screen clicked.
fn stepper(ui: &mut egui::Ui, screen: &Screen, model: &Model) -> Option<Screen> {
    let current = match screen {
        Screen::Find => 0,
        Screen::Pair { .. } => 1,
        _ => 2,
    };
    let showing_code = model.found.iter().find(|f| f.announce.pairing).map(|f| Screen::Pair {
        addr: f.addr,
        name: if f.announce.nickname.is_empty() { "Switch".into() } else { f.announce.nickname.clone() },
    });
    let targets = [Some(Screen::Find), showing_code, Some(Screen::Link)];
    let mut clicked = None;
    ui.spacing_mut().item_spacing.x = 28.0;
    // Right to left.
    for (i, step) in ["01 FIND", "02 PAIR", "03 LINK"].iter().enumerate().rev() {
        let target = if i == current { None } else { targets[i].clone() };
        let sense = if target.is_some() { Sense::click() } else { Sense::hover() };
        let (face, color) = if i == current { (Face::MonoSemi, INK) } else { (Face::Mono, MUTED) };
        let galley = ui.painter().layout_job(theme::job(step, face, 13.0, color, 0.04));
        let (rect, response) = ui.allocate_exact_size(galley.size() + Vec2::new(0.0, 7.0), sense);
        let color = if response.hovered() && target.is_some() { INK } else { color };
        ui.painter().galley(rect.min, galley, color);
        let bar = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 3.0), rect.max);
        if i == current {
            ui.painter().rect_filled(bar, 0.0, RED);
        } else if response.hovered() && target.is_some() {
            ui.painter().rect_filled(bar.with_min_y(bar.max.y - 1.5), 0.0, INK);
        }
        if target.is_some() {
            if response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                clicked = target;
            }
        } else if i == 1 && i != current {
            response.on_hover_text("Open Kickstand from the Homebrew Menu on your Switch: its screen shows a code to pair with");
        }
    }
    clicked
}

impl eframe::App for Kickstand {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let model = self.model.clone();
        let mut model = model.lock().unwrap();
        model.discovering = self.screen == Screen::Find;
        if model.pairing == worker::Pairing::Done && matches!(self.screen, Screen::Pair { .. }) {
            model.pairing = worker::Pairing::Idle;
            let linked = model.links.iter().any(|(_, s)| matches!(s, crate::link::State::Linked { .. }));
            self.screen = if linked { Screen::Home } else { Screen::Link };
        }
        self.header(ui, &mut model);
        self.footer(ui, &model);
        let frame = Frame::new().fill(PAPER);
        egui::CentralPanel::no_frame().frame(frame).show(ui, |ui| {
            // The window doesn't scroll; lists that can grow (the activity) scroll on their own.
            match self.screen.clone() {
                Screen::Find => screens::find(self, ui, &mut model),
                Screen::Pair { addr, name } => screens::pair(self, ui, &mut model, addr, &name),
                Screen::Link => screens::link(self, ui, &mut model),
                Screen::Home => screens::home(self, ui, &mut model),
            }
        });
        drop(model);
        if self.confirm_reboot {
            screens::confirm_reboot(self, ui.ctx());
        }
        // Times and the "looking" bars move on their own.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(400));
    }
}

/// Lays out columns side by side when they fit at their minimum widths, else one above the other.
/// `weights` share the width left after the gaps.
fn columns(ui: &mut egui::Ui, weights: &[f32], mins: &[f32], gap: f32, mut add: impl FnMut(&mut egui::Ui, usize)) -> bool {
    let width = ui.available_width();
    let total_min: f32 = mins.iter().sum::<f32>() + gap * (weights.len() - 1) as f32;
    if width < total_min {
        ui.vertical(|ui| {
            for i in 0..weights.len() {
                if i > 0 {
                    ui.add_space(gap * 0.6);
                }
                add(ui, i);
            }
        });
        return false;
    }
    let share = (width - gap * (weights.len() - 1) as f32) / weights.iter().sum::<f32>();
    let spacing = ui.spacing().item_spacing;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (i, w) in weights.iter().enumerate() {
            if i > 0 {
                ui.add_space(gap);
            }
            // The full height, so a column can fill what's left (the activity list scrolls in it).
            ui.allocate_ui_with_layout(Vec2::new(share * w, ui.available_height()), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(share * w);
                ui.spacing_mut().item_spacing = spacing;
                add(ui, i);
            });
        }
    });
    true
}
