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

/// How long a screen takes to come in, and dialogs and folding groups to open.
const ENTER_S: f32 = 0.24;
const OPEN_S: f32 = 0.18;

#[derive(Clone, PartialEq, Eq)]
pub enum Screen {
    Find,
    /// Typing the code shown by the console at `addr`, called `name`.
    Pair { addr: SocketAddr, name: String },
    Link,
    Home,
}

impl Screen {
    /// Its place in the order Find, Pair, Link, Home: a later screen comes in from the right.
    fn order(&self) -> usize {
        match self {
            Screen::Find => 0,
            Screen::Pair { .. } => 1,
            Screen::Link => 2,
            Screen::Home => 3,
        }
    }
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
    /// A group just opened there, to scroll into view once its rows are laid out.
    reveal: Option<crate::link::Maker>,
    brands: brands::Brands,
    /// The screen being shown's [`Screen::order`] (`None` before the first frame), when it came in, and
    /// from which side (-1 left, 1 right, 0 fading in place).
    shown: Option<usize>,
    entered: f64,
    from: f32,
    /// When the Link screen's tab last changed.
    tab_at: f64,
    /// Activity entries seen so far: when each happened (seconds since 1970) and when it appeared here,
    /// so new ones fade in.
    seen: Vec<(f64, f64)>,
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
        Kickstand {
            model,
            tx,
            screen,
            code: String::new(),
            address: None,
            confirm_reboot: false,
            tab,
            folded: [false; 4],
            reveal: None,
            brands,
            shown: None,
            entered: 0.0,
            from: 0.0,
            tab_at: f64::NEG_INFINITY,
            seen: Vec::new(),
        }
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
        let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.12);
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, theme::PRESSED_PAPER.gamma_multiply(hover));
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
    let mut underline = None;
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
            underline = Some(bar);
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
    // Slides to the step shown.
    if let Some(bar) = underline {
        let ctx = ui.ctx();
        let left = ctx.animate_value_with_time(egui::Id::unique("step-left"), bar.left(), 0.2);
        let right = ctx.animate_value_with_time(egui::Id::unique("step-right"), bar.right(), 0.2);
        ui.painter().rect_filled(egui::Rect::from_x_y_ranges(left..=right, bar.y_range()), 0.0, RED);
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
        // A new screen fades in, sliding from the side it's on in Find, Pair, Link, Home.
        let now = ui.input(|i| i.time);
        let order = self.screen.order();
        if self.shown != Some(order) {
            self.from = self.shown.map_or(0.0, |shown| if order > shown { 1.0 } else { -1.0 });
            self.shown = Some(order);
            self.entered = now;
        }
        let t = eased(now - self.entered, ENTER_S);
        if t < 1.0 {
            ui.ctx().request_repaint();
        }
        let frame = Frame::new().fill(PAPER);
        egui::CentralPanel::no_frame().frame(frame).show(ui, |ui| {
            let rect = ui.max_rect().translate(Vec2::new((1.0 - t) * 36.0 * self.from, 0.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                ui.multiply_opacity(t);
                // The window doesn't scroll; lists that can grow (the activity) scroll on their own.
                match self.screen.clone() {
                    Screen::Find => screens::find(self, ui, &mut model),
                    Screen::Pair { addr, name } => screens::pair(self, ui, &mut model, addr, &name),
                    Screen::Link => screens::link(self, ui, &mut model),
                    Screen::Home => screens::home(self, ui, &mut model),
                }
            });
        });
        drop(model);
        // Shown while it fades out too.
        let open = ui.ctx().animate_bool_with_time_and_easing(egui::Id::unique("confirm-reboot"), self.confirm_reboot, OPEN_S, egui::emath::easing::cubic_out);
        if open > 0.0 {
            screens::confirm_reboot(self, ui.ctx(), open);
        }
        // Times and the "looking" bars move on their own.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(400));
    }
}

/// How far along an animation `elapsed` seconds into `length` is, eased out (fast, then settling).
fn eased(elapsed: f64, length: f32) -> f32 {
    egui::emath::easing::cubic_out((elapsed as f32 / length).clamp(0.0, 1.0))
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
