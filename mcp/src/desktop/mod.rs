// SPDX-License-Identifier: GPL-3.0-only
//! The desktop app: find the Switch, pair with the code on its screen, link the AI app, then watch what
//! the agent is doing. The network side is in [`worker`]; this draws the [`worker::Model`] and sends
//! [`worker::Cmd`]s.

mod art;
mod brands;
mod overview;
mod screens;
mod theme;
mod window;
mod worker;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use eframe::egui::{self, Frame, Margin, Rect, Sense, Stroke, Vec2};
use tokio::sync::mpsc::UnboundedSender;

use theme::{Face, ink, muted, paper, RED, rule};
use worker::{Cmd, Model};

pub fn run() -> Result<()> {
    let icon = image::load_from_memory_with_format(crate::install::window_icon(), image::ImageFormat::Png).ok().map(|image| {
        let image = image.into_rgba8();
        egui::IconData { width: image.width(), height: image.height(), rgba: image.into_raw() }
    });
    let viewport = egui::ViewportBuilder::default()
        .with_title("Kickstand")
        .with_app_id("kickstand")
        .with_inner_size([1200.0, 760.0])
        // The smallest size every screen fits in side by side; the window itself doesn't scroll.
        .with_min_inner_size([920.0, 748.0]);
    let mut viewport = window::viewport(viewport);
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
    /// Recording and replaying macros.
    Macros,
}

/// What a confirmation asks before doing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    Reboot,
    Unpair,
    UpdatePc,
    UpdateSwitch,
}

impl Screen {
    /// Its place in the order Find, Pair, Link, Home: a later screen comes in from the right.
    fn order(&self) -> usize {
        match self {
            Screen::Find => 0,
            Screen::Pair { .. } => 1,
            Screen::Link => 2,
            Screen::Home => 3,
            Screen::Macros => 4,
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
    /// The macros page's name and note fields.
    macro_name: String,
    macro_note: String,
    /// The macro whose Delete was clicked once, waiting for the second click.
    deleting: Option<String>,
    /// A confirmation is open, and which (kept while it fades out).
    confirming: bool,
    confirm: Confirm,
    /// The agent question last brought to the front, so the window is raised once per question.
    raised: Option<String>,
    /// The Link screen's tab.
    tab: crate::link::Maker,
    /// Which makers' groups are folded in the home screen's linked apps (by [`crate::link::Maker::index`]).
    folded: [bool; 4],
    /// Dark or light as chosen with the header's sun/moon button; `None` follows the system.
    dark: Option<bool>,
    /// A theme change under way.
    flood: Option<Flood>,
    /// A group just opened there, to scroll into view once its rows are laid out.
    reveal: Option<crate::link::Maker>,
    brands: brands::Brands,
    /// The screen being shown's [`Screen::order`] (`None` before the first frame), when it came in, and
    /// from which side (-1 left, 1 right, 0 fading in place).
    shown: Option<usize>,
    entered: f64,
    from: f32,
    /// Frames a new screen is still drawn unseen, so it's laid out before it fades in.
    settling: u8,
    /// When the Link screen's tab last changed.
    tab_at: f64,
    /// Activity entries seen so far: when each happened (seconds since 1970) and when it appeared here,
    /// so new ones fade in.
    seen: Vec<(f64, f64)>,
    /// The overview (the header's Kickstand opens it), its tab, and when that tab was chosen.
    overview: bool,
    overview_tab: usize,
    overview_at: f64,
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
            macro_name: String::new(),
            macro_note: String::new(),
            deleting: None,
            confirming: false,
            confirm: Confirm::Reboot,
            raised: None,
            tab,
            folded: [false; 4],
            dark: crate::config::dark_theme(),
            flood: None,
            reveal: None,
            brands,
            shown: None,
            entered: 0.0,
            from: 0.0,
            settling: 0,
            tab_at: f64::NEG_INFINITY,
            seen: Vec::new(),
            overview: false,
            overview_tab: 0,
            overview_at: f64::NEG_INFINITY,
        }
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    /// Opens the confirmation for `what`.
    fn ask(&mut self, what: Confirm) {
        self.confirm = what;
        self.confirming = true;
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
        // Under the title bar, which makes up the rest of the space above.
        let margin = Margin { left: theme::GUTTER as i8, right: theme::GUTTER as i8, top: 0, bottom: 16 };
        let frame = Frame::new().fill(paper()).inner_margin(margin);
        let panel = egui::Panel::top("header").frame(frame).resizable(false).show_separator_line(false).show(ui, |ui| {
            // The header moves the window too.
            window::drag_area(ui, ui.max_rect() + margin);
            ui.horizontal(|ui| {
                // The same height on every screen (the console button is the tallest), so the line under
                // it doesn't jump when the screen changes.
                ui.set_min_height(44.0);
                ui.spacing_mut().item_spacing.x = 12.0;
                // The mark and name open the overview.
                let name = ui.painter().layout_job(theme::job("Kickstand", Face::Heavy, 22.0, ink(), -0.01));
                let (rect, response) = ui.allocate_exact_size(Vec2::new(36.0 + 12.0 + name.size().x + 10.0, 36.0), Sense::click());
                let response = response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("What Kickstand does, and its commands");
                let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.12);
                let painter = ui.painter();
                painter.rect_filled(rect.expand(4.0), 0.0, theme::pressed_paper().gamma_multiply(hover));
                art::mark(painter, Rect::from_min_size(rect.min, Vec2::splat(36.0)));
                painter.galley(egui::pos2(rect.left() + 48.0, rect.center().y - name.size().y / 2.0), name, ink());
                if response.clicked() {
                    self.overview = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match &self.screen {
                        Screen::Home | Screen::Macros => {
                            if self.selector(ui, model).clicked() {
                                self.go(model, Screen::Find);
                            }
                        }
                        screen => {
                            if let Some(next) = stepper(ui, screen, model) {
                                self.go(model, next);
                            }
                        }
                    }
                    ui.add_space(8.0);
                    if self.theme_button(ui).clicked() {
                        // Back to following the system when the choice matches it.
                        let dark = !theme::dark();
                        let system = ui.ctx().system_theme() == Some(egui::Theme::Dark);
                        self.dark = (dark != system).then_some(dark);
                        if let Err(e) = crate::config::save_dark_theme(self.dark) {
                            model.notice = Some((format!("couldn't save the theme: {e:#}"), true));
                        }
                    }
                });
            });
        });
        let rect = panel.response.rect;
        ui.painter().hline(rect.x_range(), rect.bottom(), Stroke::new(1.5, ink()));
    }

    /// A moon in light (to switch to dark), a sun in dark.
    fn theme_button(&self, ui: &mut egui::Ui) -> egui::Response {
        let dark = theme::dark();
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
        let tip = if dark { "Light mode" } else { "Dark mode" };
        let response = response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip);
        let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.12);
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, theme::pressed_paper().gamma_multiply(hover));
        if dark {
            art::sun(painter, rect.center(), theme::ink());
        } else {
            art::moon(painter, rect.center(), theme::ink());
        }
        response
    }

    /// The header's button naming the console in use; it leads back to the list.
    fn selector(&self, ui: &mut egui::Ui, model: &Model) -> egui::Response {
        let name = model.name.clone().unwrap_or_else(|| "Your Switch".into());
        let host = model.host.clone().unwrap_or_default();
        let name_galley = ui.painter().layout_job(theme::job(&name, Face::Semi, 15.0, ink(), 0.0));
        let host_galley = ui.painter().layout_job(theme::job(&host, Face::Mono, 12.0, muted(), 0.0));
        let width = 14.0 + name_galley.size().x + 10.0 + host_galley.size().x + 10.0 + 12.0 + 14.0;
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 44.0), Sense::click());
        let response = response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Use a different Switch");
        let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.12);
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, theme::pressed_paper().gamma_multiply(hover));
        painter.rect_stroke(rect, 0.0, Stroke::new(1.5, ink()), egui::StrokeKind::Inside);
        let mut x = rect.left() + 14.0;
        let cy = rect.center().y;
        painter.galley(egui::pos2(x, cy - name_galley.size().y / 2.0), name_galley.clone(), ink());
        x += name_galley.size().x + 10.0;
        painter.galley(egui::pos2(x, cy - host_galley.size().y / 2.0), host_galley.clone(), muted());
        x += host_galley.size().x + 10.0;
        // An arrow, not a menu's chevron: it goes to the list of consoles.
        art::fold(painter, egui::pos2(x, cy - 6.0), 0.0, ink());
        response
    }

    fn footer(&mut self, ui: &mut egui::Ui, model: &Model) {
        let frame = Frame::new().fill(paper()).inner_margin(Margin::symmetric(theme::GUTTER as i8, 14));
        let panel = egui::Panel::bottom("footer").frame(frame).resizable(false).show_separator_line(false).show(ui, |ui| {
            ui.horizontal(|ui| {
                let console = match &model.reach {
                    worker::Reach::Connected(live) => format!(" · SWITCH {} · PROTOCOL {}", live.info.version.to_uppercase(), live.info.protocol),
                    _ => String::new(),
                };
                let left = format!("KICKSTAND {}{console}", env!("CARGO_PKG_VERSION"));
                theme::label(ui, &left, false, 12.0, muted());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    theme::label(ui, "GPL-3.0", false, 12.0, muted());
                    if let Some(release) = &model.update {
                        ui.add_space(18.0);
                        let text = format!("UPDATE TO {}", release.version);
                        if crate::update::from_source() {
                            theme::label(ui, &format!("{} IS OUT", release.version), false, 12.0, ink()).on_hover_text("This copy was built from source: update it with git pull and cargo build");
                        } else if theme::link(ui, &text, 12.0, Face::Mono).clicked() && model.busy.is_none() {
                            self.ask(Confirm::UpdatePc);
                        }
                    }
                });
            });
        });
        let rect = panel.response.rect;
        ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, rule()));
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
        let (face, color) = if i == current { (Face::MonoSemi, ink()) } else { (Face::Mono, muted()) };
        let galley = ui.painter().layout_job(theme::job(step, face, 13.0, color, 0.04));
        // Room for the underline below the text and as much above, so the text is centred in the header.
        let (rect, response) = ui.allocate_exact_size(galley.size() + Vec2::new(0.0, 14.0), sense);
        let color = if response.hovered() && target.is_some() { ink() } else { color };
        ui.painter().galley(rect.min + Vec2::new(0.0, 7.0), galley, color);
        let bar = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 3.0), rect.max);
        if i == current {
            underline = Some(bar);
        } else if response.hovered() && target.is_some() {
            ui.painter().rect_filled(bar.with_min_y(bar.max.y - 1.5), 0.0, ink());
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
        if model.unpaired {
            model.unpaired = false;
            self.screen = Screen::Find;
        }
        // Before anything is drawn, so the whole frame has one palette. A change floods in from the top
        // (see `flood`), except as the window opens.
        let dark = self.dark.unwrap_or(ui.ctx().system_theme() == Some(egui::Theme::Dark));
        let now = ui.input(|i| i.time);
        if self.flood.is_none() && dark != theme::dark() {
            if now < 0.5 {
                theme::set_dark(ui.ctx(), dark);
            } else {
                // Capture the window as it is (this frame, still in the old colours).
                let shot = Arc::new(Mutex::new(None));
                let (sink, ctx) = (shot.clone(), ui.ctx().clone());
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |image| {
                    *sink.lock().unwrap() = Some(image);
                    ctx.request_repaint();
                })));
                self.flood = Some(Flood { to_dark: dark, asked: now, shot, old: None });
            }
        }
        if let Some(flood) = &mut self.flood
            && flood.old.is_none()
        {
            let image = flood.shot.lock().unwrap().take();
            if let Some(image) = image {
                flood.old = Some((ui.ctx().load_texture("theme-old", image, egui::TextureOptions::LINEAR), now));
                theme::set_dark(ui.ctx(), flood.to_dark);
            } else if now - flood.asked > SHOT_TIMEOUT_S {
                theme::set_dark(ui.ctx(), flood.to_dark);
                self.flood = None;
            } else {
                ui.ctx().request_repaint();
            }
        }
        window::frame(ui.ctx());
        window::title_bar(ui);
        self.header(ui, &mut model);
        self.footer(ui, &model);
        // A new screen fades in, sliding from the side it's on in Find, Pair, Link, Home.
        let now = ui.input(|i| i.time);
        let order = self.screen.order();
        if self.shown != Some(order) {
            self.from = self.shown.map_or(0.0, |shown| if order > shown { 1.0 } else { -1.0 });
            self.shown = Some(order);
            // egui sizes things (columns, text, right-aligned rows) over a frame or two: until then the
            // screen is drawn unseen, and the fade starts after.
            self.settling = 2;
        }
        let t = if self.settling > 0 {
            self.settling -= 1;
            self.entered = now;
            0.0
        } else {
            eased(now - self.entered, ENTER_S)
        };
        if t < 1.0 {
            ui.ctx().request_repaint();
        }
        let frame = Frame::new().fill(paper());
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
                    Screen::Macros => screens::macros(self, ui, &mut model),
                }
            });
        });
        let words = screens::confirm_words(self.confirm, &model);
        let question = model.questions.first().cloned();
        if let Some(exe) = model.restart.take() {
            // The new copy opens its own window as this one closes.
            match std::process::Command::new(&exe).arg("desktop").stdin(std::process::Stdio::null()).spawn() {
                Ok(_) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                Err(e) => model.notice = Some((format!("Updated, but couldn't start it again ({e}): open Kickstand again"), true)),
            }
        }
        drop(model);
        // Shown while it fades out too.
        let open = ui.ctx().animate_bool_with_time_and_easing(egui::Id::unique("confirm-open"), self.confirming, OPEN_S, egui::emath::easing::cubic_out);
        if open > 0.0 {
            screens::confirm(self, ui.ctx(), open, words);
        }
        if let Some(question) = &question {
            // A new question: bring the window to the user.
            if self.raised.as_ref() != Some(&question.id) {
                self.raised = Some(question.id.clone());
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Critical));
            }
            screens::question(self, ui.ctx(), question);
        }
        let open = ui.ctx().animate_bool_with_time_and_easing(egui::Id::unique("overview-open"), self.overview, OPEN_S, egui::emath::easing::cubic_out);
        if open > 0.0 {
            overview::show(self, ui.ctx(), open);
        }
        if let Some(Flood { old: Some((old, start)), .. }) = &self.flood {
            if wave(ui.ctx(), old, (now - start) as f32) {
                ui.ctx().request_repaint();
            } else {
                self.flood = None;
            }
        }
        // Times and the "looking" bars move on their own.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(400));
    }
}

/// A theme change. The window as it was is captured, the palette changes at once, and the capture is
/// laid over the window and peeled away from the top in a wave, showing the window in its new colours
/// behind it.
struct Flood {
    to_dark: bool,
    asked: f64,
    /// Filled by the screenshot callback.
    shot: Arc<Mutex<Option<Arc<egui::ColorImage>>>>,
    /// The capture, and when the wave started.
    old: Option<(egui::TextureHandle, f64)>,
}

const WAVE_S: f32 = 1.1;
/// How long to wait for the capture before changing without the wave.
const SHOT_TIMEOUT_S: f64 = 0.5;

/// Draws the old window below a wave `elapsed` seconds in. False once it's past the bottom.
fn wave(ctx: &egui::Context, old: &egui::TextureHandle, elapsed: f32) -> bool {
    if elapsed >= WAVE_S {
        return false;
    }
    const SWELL: f32 = 14.0;
    let rect = ctx.content_rect();
    let t = egui::emath::easing::cubic_in_out(elapsed / WAVE_S);
    let front = rect.top() - SWELL + t * (rect.height() + 2.0 * SWELL);
    let crest = |x: f32| {
        let wave = (x / 70.0 + elapsed * 5.0).sin() * 0.65 + (x / 37.0 - elapsed * 3.0).sin() * 0.35;
        (front + wave * SWELL).clamp(rect.top(), rect.bottom())
    };
    let mut mesh = egui::Mesh::with_texture(old.id());
    let steps = (rect.width() / 8.0).ceil() as u32;
    for i in 0..=steps {
        let u = i as f32 / steps as f32;
        let x = rect.left() + rect.width() * u;
        let y = crest(x);
        let v = (y - rect.top()) / rect.height();
        mesh.vertices.push(egui::epaint::Vertex { pos: egui::pos2(x, y), uv: egui::pos2(u, v), color: egui::Color32::WHITE });
        mesh.vertices.push(egui::epaint::Vertex { pos: egui::pos2(x, rect.bottom()), uv: egui::pos2(u, 1.0), color: egui::Color32::WHITE });
        if i > 0 {
            let (a, b, c, d) = (2 * i - 2, 2 * i - 1, 2 * i, 2 * i + 1);
            mesh.add_triangle(a, b, c);
            mesh.add_triangle(b, d, c);
        }
    }
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Debug, egui::Id::unique("theme-wave")));
    painter.add(egui::Shape::mesh(mesh));
    true
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
