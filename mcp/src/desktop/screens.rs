// SPDX-License-Identifier: GPL-3.0-only
//! The four screens: find, pair, link and home.

use std::net::SocketAddr;

use eframe::egui::{self, Event, Key, Margin, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use proto::DaemonKind;

use super::theme::{self, BODY, FIELD, Face, INK, Kind, MUTED, RED, RULE, RULE_LIGHT};
use super::worker::{Cmd, Live, Model, Pairing, Reach, capitalize};
use super::{Kickstand, Screen, art, columns};
use crate::link::{Maker, State};

/// The page's padding.
fn page(ui: &mut Ui, top: f32, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new().inner_margin(Margin { left: theme::GUTTER as i8, right: theme::GUTTER as i8, top: top as i8, bottom: 40 }).show(ui, |ui| {
        ui.set_max_width(ui.available_width().min(1120.0));
        add(ui);
    });
}

pub fn find(app: &mut Kickstand, ui: &mut Ui, model: &mut Model) {
    page(ui, 56.0, |ui| {
        columns(ui, &[1.0, 1.3], &[300.0, 420.0], 56.0, |ui, column| {
            if column == 0 {
                ui.spacing_mut().item_spacing.y = 20.0;
                theme::heading(ui, "Find your Switch", 52.0);
                ui.set_max_width(ui.available_width().min(420.0));
                theme::text(
                    ui,
                    "Open Kickstand from the Homebrew Menu on your Switch. It appears here as soon as it's on the same Wi-Fi as this computer.",
                    Face::Regular,
                    18.0,
                    BODY,
                );
                ui.add_space(14.0);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(260.0, 170.0), Sense::hover());
                art::side(ui.painter(), rect, true);
            } else {
                consoles(app, ui, model);
            }
        });
    });
}

fn consoles(app: &mut Kickstand, ui: &mut Ui, model: &mut Model) {
    ui.spacing_mut().item_spacing.y = 0.0;
    ui.horizontal(|ui| {
        theme::label(ui, "ON THIS NETWORK", true, 13.0, INK);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            theme::label(ui, "LOOKING", false, 13.0, MUTED);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(18.0, 12.0), Sense::hover());
            art::looking(ui.painter(), rect.min, ui.input(|i| i.time));
        });
    });
    ui.add_space(12.0);
    theme::rule(ui, 1.5, INK);

    let paired = model.host.clone();
    for found in &model.found.clone() {
        let ip = found.addr.ip().to_string();
        let is_paired = paired.as_deref() == Some(ip.as_str());
        let name = if found.announce.nickname.is_empty() { "Switch".to_string() } else { found.announce.nickname.clone() };
        ui.add_space(22.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                // Leaves room for the button, so the details wrap instead of running under it.
                ui.set_max_width(ui.available_width() - 130.0);
                ui.spacing_mut().item_spacing.y = 6.0;
                ui.horizontal(|ui| {
                    ui.label(theme::job(&name, Face::Bold, 22.0, INK, 0.0));
                    if found.announce.pairing {
                        theme::tag(ui, "SHOWING A CODE", false);
                    }
                    if is_paired {
                        theme::tag(ui, "PAIRED", true);
                    }
                });
                let mut meta = format!("{ip} · Kickstand {}", found.announce.daemon_version);
                if found.announce.kind == DaemonKind::Sysmodule {
                    meta.push_str(" · background service");
                }
                ui.add(egui::Label::new(theme::job(&meta, Face::Mono, 14.0, MUTED, 0.0)).wrap());
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if is_paired {
                    if theme::button(ui, "Open", Kind::Outline, true, true).clicked() {
                        app.send(Cmd::Open { host: ip.clone() });
                        app.go(model, Screen::Home);
                    }
                } else if found.announce.pairing {
                    if theme::button(ui, "Pair", Kind::Primary, true, true).clicked() {
                        app.go(model, Screen::Pair { addr: found.addr, name: name.clone() });
                    }
                } else {
                    theme::button(ui, "Pair", Kind::Outline, true, false)
                        .on_hover_text("Open Kickstand from the Homebrew Menu on this Switch: its screen shows the code");
                }
            });
        });
        ui.add_space(22.0);
        theme::rule(ui, 1.0, RULE);
    }
    if model.found.is_empty() {
        ui.add_space(22.0);
        let text = match (&model.find_error, model.searched) {
            (Some(e), _) => format!("Couldn't look on this network: {e}"),
            (None, true) => "Nothing yet. Kickstand has to be open on the Switch, on the same Wi-Fi as this computer.".into(),
            (None, false) => "Looking…".into(),
        };
        theme::text(ui, &text, Face::Regular, 16.0, BODY);
        ui.add_space(22.0);
        theme::rule(ui, 1.0, RULE);
    }

    ui.add_space(20.0);
    let Some(mut address) = app.address.take() else {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            theme::text(ui, "Not listed?", Face::Regular, 15.0, BODY);
            if theme::link(ui, "Enter its address", 15.0, Face::Regular).clicked() {
                app.address = Some(String::new());
            }
            theme::text(ui, "instead.", Face::Regular, 15.0, BODY);
        });
        return;
    };
    theme::label(ui, "THE SWITCH'S IP ADDRESS", true, 13.0, INK);
    ui.add_space(10.0);
    let mut go = false;
    ui.horizontal(|ui| {
        let edit = egui::TextEdit::singleline(&mut address)
            .hint_text("192.168.1.20")
            .font(Face::Mono.size(16.0))
            .margin(Margin::symmetric(12, 12))
            .desired_width(240.0);
        let response = ui.add(edit);
        go = response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        ui.add_space(6.0);
        go |= theme::button(ui, "Continue", Kind::Outline, true, !address.trim().is_empty()).clicked();
    });
    let parsed = parse_address(&address);
    if go && let Some(addr) = parsed {
        app.go(model, Screen::Pair { addr, name: "Switch".into() });
        return;
    }
    if !address.trim().is_empty() && parsed.is_none() {
        ui.add_space(8.0);
        theme::text(ui, "That isn't an IP address, like 192.168.1.20.", Face::Regular, 14.0, MUTED);
    }
    app.address = Some(address);
}

/// "192.168.1.20" or "192.168.1.20:28790".
fn parse_address(text: &str) -> Option<SocketAddr> {
    let text = text.trim();
    text.parse().ok().or_else(|| text.parse().ok().map(|ip| SocketAddr::new(ip, proto::DEFAULT_PORT)))
}

pub fn pair(app: &mut Kickstand, ui: &mut Ui, model: &mut Model, addr: SocketAddr, name: &str) {
    let running = model.pairing == Pairing::Running;
    // A wrong code is typed again from the start.
    if matches!(model.pairing, Pairing::Failed(_)) && app.code.len() == proto::PAIR_CODE_LEN {
        app.code.clear();
    }
    if !running {
        take_digits(app, ui, model, addr);
    }
    page(ui, 56.0, |ui| {
        columns(ui, &[1.2, 1.0], &[460.0, 300.0], 64.0, |ui, column| {
            if column == 1 {
                ui.spacing_mut().item_spacing.y = 14.0;
                let width = ui.available_width().min(460.0);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(width, width * 230.0 / 440.0), Sense::hover());
                art::front(ui.painter(), rect, "482 913");
                theme::label(ui, "WHERE TO LOOK: KICKSTAND ON THE SWITCH", false, 12.0, MUTED);
                return;
            }
            ui.spacing_mut().item_spacing.y = 22.0;
            theme::label(ui, &format!("{} · {}", name.to_uppercase(), addr.ip()), false, 13.0, MUTED);
            theme::heading(ui, "Type the code from your Switch", 52.0);
            ui.add_space(4.0);
            theme::label(ui, "PAIRING CODE", true, 13.0, INK);
            digit_boxes(ui, &app.code, !running);
            ui.set_max_width(ui.available_width().min(460.0));
            theme::text(
                ui,
                &format!("Six digits, on the Kickstand screen on your Switch. After {} wrong tries it shows a new one.", proto::PAIR_MAX_TRIES),
                Face::Regular,
                15.0,
                BODY,
            );
            if let Pairing::Failed(error) = &model.pairing {
                problem(ui, error);
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 14.0;
                let ready = app.code.len() == proto::PAIR_CODE_LEN && !running;
                let label = if running { "Pairing…" } else { "Pair" };
                if theme::button(ui, label, Kind::Primary, true, ready).clicked() {
                    app.send(Cmd::Pair { addr, code: app.code.clone() });
                }
                ui.add_space(4.0);
                if theme::link(ui, "Back", 16.0, Face::Semi).clicked() {
                    app.go(model, Screen::Find);
                }
            });
        });
    });
}

/// Digits typed or pasted anywhere in the window go into the code; Enter pairs.
fn take_digits(app: &mut Kickstand, ui: &mut Ui, model: &mut Model, addr: SocketAddr) {
    let events = ui.input(|i| i.events.clone());
    for event in events {
        match event {
            Event::Text(text) | Event::Paste(text) => {
                for digit in text.chars().filter(char::is_ascii_digit) {
                    if app.code.len() < proto::PAIR_CODE_LEN {
                        app.code.push(digit);
                    }
                }
                model.pairing = Pairing::Idle;
            }
            Event::Key { key: Key::Backspace, pressed: true, .. } => {
                app.code.pop();
            }
            Event::Key { key: Key::Escape, pressed: true, .. } => app.code.clear(),
            Event::Key { key: Key::Enter, pressed: true, .. } if app.code.len() == proto::PAIR_CODE_LEN => {
                app.send(Cmd::Pair { addr, code: app.code.clone() });
            }
            _ => {}
        }
    }
}

/// Six keycaps, a gap after the third, the next one to fill outlined in red.
fn digit_boxes(ui: &mut Ui, code: &str, active: bool) {
    let natural = 6.0 * 64.0 + 5.0 * 10.0 + 14.0;
    let s = (ui.available_width() / natural).min(1.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(natural, 86.0) * s, Sense::hover());
    let painter = ui.painter();
    let blink = (ui.input(|i| i.time) * 1.6) as i64 % 2 == 0;
    let digits: Vec<char> = code.chars().collect();
    let mut x = rect.left();
    for i in 0..proto::PAIR_CODE_LEN {
        if i == 3 {
            x += 14.0 * s;
        }
        let cap = Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(64.0, 86.0) * s);
        let current = active && i == digits.len();
        let color = if current {
            RED
        } else if i < digits.len() {
            INK
        } else {
            RULE
        };
        painter.rect_filled(cap, 0.0, FIELD);
        painter.rect_stroke(cap, 0.0, Stroke::new(if current { 2.5 } else { 1.5 }, color), StrokeKind::Inside);
        painter.rect_filled(Rect::from_min_max(egui::pos2(cap.left(), cap.bottom() - 6.0 * s), cap.max), 0.0, color);
        let face = cap.with_max_y(cap.bottom() - 6.0 * s);
        if let Some(d) = digits.get(i) {
            painter.text(face.center(), egui::Align2::CENTER_CENTER, d, Face::MonoSemi.size(40.0 * s), INK);
        } else if current && blink {
            let caret = Rect::from_center_size(face.center(), Vec2::new(2.5, 40.0) * s);
            painter.rect_filled(caret, 0.0, INK);
        }
        x += 74.0 * s;
    }
}

/// A red-edged note saying what went wrong.
fn problem(ui: &mut Ui, text: &str) {
    egui::Frame::new().inner_margin(Margin { left: 14, right: 0, top: 2, bottom: 2 }).show(ui, |ui| {
        let response = theme::text(ui, &capitalize(text), Face::Regular, 15.0, INK);
        let rect = response.rect;
        let bar = Rect::from_min_max(egui::pos2(rect.left() - 14.0, rect.top() - 2.0), egui::pos2(rect.left() - 10.0, rect.bottom() + 2.0));
        ui.painter().rect_filled(bar, 0.0, RED);
    });
}

pub fn link(app: &mut Kickstand, ui: &mut Ui, model: &mut Model) {
    page(ui, 56.0, |ui| {
        columns(ui, &[1.0, 1.1], &[320.0, 380.0], 64.0, |ui, column| {
            if column == 0 {
                ui.spacing_mut().item_spacing.y = 20.0;
                theme::heading(ui, "Link your AI app", 52.0);
                ui.set_max_width(ui.available_width().min(440.0));
                theme::text(
                    ui,
                    "Kickstand gives the AI app on this computer its tools for the Switch: it can copy homebrew over, run it, see the screen and press buttons. Linking adds Kickstand to the app's settings.",
                    Face::Regular,
                    18.0,
                    BODY,
                );
                return;
            }
            ui.spacing_mut().item_spacing.y = 0.0;
            tabs(app, ui, model);
            let tab = app.tab;
            app_rows(app, ui, model, tab);
            if tab == Maker::Custom {
                any_app(ui, model);
            }
            notice(ui, model);
            ui.add_space(28.0);
            let any = model.links.iter().any(|(_, s)| matches!(s, State::Linked { .. }));
            if any {
                if theme::button(ui, "Done", Kind::Primary, true, true).clicked() {
                    app.go(model, Screen::Home);
                }
            } else if theme::link(ui, "Skip for now", 16.0, Face::Semi).clicked() {
                app.go(model, Screen::Home);
            }
        });
    });
}

/// Claude, Gemini, OpenAI, Custom: each with its mark, the chosen one underlined in red; a dot marks a tab
/// with something linked.
fn tabs(app: &mut Kickstand, ui: &mut Ui, model: &Model) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 22.0;
        for maker in Maker::ALL {
            let chosen = app.tab == maker;
            let color = if chosen { INK } else { MUTED };
            let galley = ui.painter().layout_job(theme::job(maker.name(), if chosen { Face::Bold } else { Face::Semi }, 16.0, color, 0.0));
            let icon = super::brands::SIZE;
            let size = Vec2::new(icon + 8.0 + galley.size().x, 40.0);
            let (rect, response) = ui.allocate_exact_size(size, Sense::click());
            let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
            let color = if response.hovered() { INK } else { color };
            let mid = rect.top() + 14.0;
            app.brands.paint(ui.painter(), Rect::from_center_size(egui::pos2(rect.left() + icon / 2.0, mid), Vec2::splat(icon)), maker);
            ui.painter().galley(egui::pos2(rect.left() + icon + 8.0, mid - galley.size().y / 2.0), galley, color);
            let linked = model.links.iter().any(|(a, s)| a.maker() == maker && matches!(s, State::Linked { .. }));
            if linked {
                ui.painter().circle_filled(egui::pos2(rect.right() + 6.0, mid - 7.0), 3.0, RED);
            }
            if chosen {
                ui.painter().rect_filled(Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 3.0), rect.right_bottom()), 0.0, RED);
            }
            if response.clicked() {
                app.tab = maker;
            }
        }
    });
    theme::rule(ui, 1.5, INK);
}

/// One row per app of `maker`: linked (with Remove), a Link button, or not installed.
fn app_rows(app: &mut Kickstand, ui: &mut Ui, model: &Model, maker: Maker) {
    for (which, state) in model.links.iter().filter(|(a, _)| a.maker() == maker) {
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.set_min_height(44.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.label(theme::job(which.name(), Face::Semi, 19.0, INK, 0.0));
                ui.label(theme::job(which.detail(), Face::Mono, 12.0, MUTED, 0.0));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match state {
                State::Linked { command } => {
                    if theme::link(ui, "Remove", 13.0, Face::Mono).clicked() {
                        app.send(Cmd::Unlink(*which));
                    }
                    linked_mark(ui, command);
                }
                State::NotLinked => {
                    if theme::button(ui, "Link", Kind::Primary, false, true).clicked() {
                        app.send(Cmd::Link(*which));
                    }
                }
                State::Missing => {
                    theme::label(ui, "NOT INSTALLED", false, 13.0, MUTED).on_hover_text(format!("Install {}, then come back", which.name()));
                }
            });
        });
        ui.add_space(14.0);
        theme::rule(ui, 1.0, RULE);
    }
}

/// ✓ LINKED, naming the command it runs on hover. (Right to left.)
fn linked_mark(ui: &mut Ui, command: &str) {
    let response = theme::label(ui, "LINKED", false, 13.0, INK);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(14.0, 11.0), Sense::hover());
    art::check(ui.painter(), rect.min, INK);
    response.on_hover_text(format!("Runs {command}"));
}

/// The Custom tab's part for apps Kickstand doesn't know: the command, and the JSON most of them take.
fn any_app(ui: &mut Ui, model: &mut Model) {
    let Ok((command, args)) = crate::link::server_command() else { return };
    let line = std::iter::once(command.as_str()).chain(args.iter().map(String::as_str)).collect::<Vec<_>>().join(" ");
    let config = serde_json::json!({ "mcpServers": { crate::link::NAME: { "command": command, "args": args } } });
    // Shown on one line to keep the window still; copied laid out.
    let (shown, json) = (config.to_string(), serde_json::to_string_pretty(&config).unwrap_or_default());
    ui.add_space(28.0);
    theme::label(ui, "ANY OTHER MCP APP", true, 13.0, INK);
    ui.add_space(10.0);
    theme::text(ui, "Cursor, VS Code, Jan, or a local model's chat app: add a server that runs this command (stdio).", Face::Regular, 15.0, BODY);
    for (shown, copied, what) in [(line.clone(), line, "the command"), (shown, json, "the JSON")] {
        ui.add_space(12.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            if theme::button(ui, "Copy", Kind::Outline, false, true).clicked() {
                ui.ctx().copy_text(copied);
                model.notice = Some((format!("Copied {what}"), false));
            }
            let frame = egui::Frame::new().fill(theme::FIELD).stroke(Stroke::new(1.0, RULE)).inner_margin(Margin::symmetric(12, 10));
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                frame.show(ui, |ui| {
                    ui.set_width(ui.available_width() - 12.0);
                    ui.add(egui::Label::new(theme::job(&shown, Face::Mono, 12.5, INK, 0.0)).wrap());
                });
            });
        });
    }
}

fn notice(ui: &mut Ui, model: &Model) {
    if let Some(busy) = model.busy {
        ui.add_space(16.0);
        theme::label(ui, &format!("{}…", busy.to_uppercase()), false, 13.0, MUTED);
    } else if let Some((text, error)) = &model.notice {
        ui.add_space(16.0);
        if *error {
            problem(ui, text);
        } else {
            theme::text(ui, text, Face::Mono, 13.0, BODY);
        }
    }
}

pub fn home(app: &mut Kickstand, ui: &mut Ui, model: &mut Model) {
    let top = ui.cursor().top();
    let side_by_side = columns(ui, &[1.25, 1.0], &[440.0, 320.0], 0.0, |ui, column| {
        egui::Frame::new().inner_margin(Margin::symmetric(theme::GUTTER as i8, 44)).show(ui, |ui| {
            if column == 0 {
                agent(app, ui, model);
            } else {
                console(app, ui, model);
            }
        });
    });
    if side_by_side {
        // The rule between the columns, down to the footer.
        let x = ui.min_rect().left() + (ui.available_width()) * 1.25 / 2.25;
        let bottom = ui.clip_rect().bottom();
        ui.painter().vline(x, top..=bottom, Stroke::new(1.5, INK));
    }
}

/// The left column: the console's state and what the agent is up to.
fn agent(app: &mut Kickstand, ui: &mut Ui, model: &mut Model) {
    ui.spacing_mut().item_spacing.y = 18.0;
    let live = match &model.reach {
        Reach::Connected(live) => Some(live),
        _ => None,
    };
    let agents = live.and_then(|l| l.activity.as_ref()).map(|a| a.clients.iter().filter(|c| !c.you).collect::<Vec<_>>()).unwrap_or_default();
    let busiest = agents.iter().min_by_key(|c| c.idle_ms);
    let (status, status_color, heading, standing) = match (&model.reach, busiest) {
        (Reach::Unknown, _) => ("CONNECTING", MUTED, "Getting in touch".to_string(), false),
        (Reach::Unreachable(_), _) => ("CAN'T REACH IT", MUTED, "Your Switch isn't answering".to_string(), false),
        (Reach::Connected(_), Some(c)) if c.idle_ms < 90_000 => ("AGENT CONNECTED", RED, capitalize(&c.doing), true),
        (Reach::Connected(_), Some(_)) => ("AGENT CONNECTED", RED, "Waiting on the agent".to_string(), true),
        (Reach::Connected(_), None) => ("READY", INK, "Ready for an agent".to_string(), true),
    };

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 28.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(150.0, 98.0), Sense::hover());
        art::side(ui.painter(), Rect::from_min_size(rect.min - Vec2::new(0.0, 12.0), Vec2::new(150.0, 98.0)), standing);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.add_space(8.0);
            theme::status(ui, status, status_color);
            theme::heading(ui, &heading, 46.0);
        });
    });

    match (&model.reach, live) {
        (Reach::Unreachable(error), _) => {
            ui.set_max_width(ui.available_width().min(520.0));
            theme::text(
                ui,
                "Wake it up and check it's on the same Wi-Fi as this computer. Without the background service, Kickstand also has to be open on it (Homebrew Menu, then Kickstand).",
                Face::Regular,
                16.0,
                BODY,
            );
            theme::text(ui, error, Face::Mono, 12.0, MUTED);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 18.0;
                if theme::button(ui, "Try again", Kind::Primary, true, true).clicked() {
                    app.send(Cmd::Retry);
                }
                if theme::link(ui, "Find it on the network", 16.0, Face::Semi).clicked() {
                    app.go(model, Screen::Find);
                }
            });
        }
        (_, Some(live)) => activity(ui, model, live, busiest.copied()),
        _ => {}
    }
}

fn activity(ui: &mut Ui, model: &Model, live: &Live, busiest: Option<&proto::ClientInfo>) {
    let Some(report) = &live.activity else {
        theme::text(ui, "Update Kickstand on the Switch to see what the agent is doing here.", Face::Regular, 16.0, BODY);
        return;
    };
    let me = report.clients.iter().find(|c| c.you).map(|c| c.ip);
    let who = |ip: [u8; 4]| if Some(ip) == me { "This computer".to_string() } else { std::net::Ipv4Addr::from(ip).to_string() };
    let meta = match busiest {
        Some(c) => format!("{} · connected {} · {} requests", who(c.ip), duration(c.connected_ms), c.requests),
        None => {
            let linked = model.links.iter().find(|(_, s)| matches!(s, State::Linked { .. }));
            match linked {
                Some((app, _)) => format!("Ask {} to use the Switch and it shows up here", app.name()),
                None => "Link an AI app, then ask it to use the Switch".into(),
            }
        }
    };
    ui.label(theme::job(&meta, Face::Mono, 14.0, MUTED, 0.0));

    ui.add_space(14.0);
    ui.spacing_mut().item_spacing.y = 0.0;
    theme::rule(ui, 1.5, INK);
    ui.add_space(12.0);
    theme::label(ui, "ACTIVITY", true, 13.0, INK);
    ui.add_space(12.0);
    theme::rule(ui, 1.0, RULE);
    if report.recent.is_empty() {
        ui.add_space(12.0);
        theme::text(ui, "Nothing yet.", Face::Mono, 13.0, MUTED);
    }
    let now = chrono::Local::now();
    let elapsed = live.at.elapsed().as_millis() as i64;
    // The only part of the window that scrolls: it takes whatever height the column has left.
    egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(ui.available_height()).show(ui, |ui| {
        for event in &report.recent {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                let at = now - chrono::TimeDelta::milliseconds(event.ago_ms as i64 + elapsed);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(72.0, 16.0), Sense::hover());
                ui.painter().text(rect.left_center(), egui::Align2::LEFT_CENTER, at.format("%H:%M:%S"), Face::Mono.size(13.0), MUTED);
                let mut what = capitalize(&event.what);
                if Some(event.ip) != me {
                    what = format!("{what} ({})", who(event.ip));
                }
                ui.label(theme::job(&what, Face::Mono, 13.0, INK, 0.0));
            });
            ui.add_space(10.0);
            theme::rule(ui, 1.0, RULE_LIGHT);
        }
    });
}

/// "3 min", "1 h 5 min", "20 s".
fn duration(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min", s / 60),
        _ => format!("{} h {} min", s / 3600, s / 60 % 60),
    }
}

/// The right column: linked apps, the console's details and the buttons.
fn console(app: &mut Kickstand, ui: &mut Ui, model: &mut Model) {
    ui.spacing_mut().item_spacing.y = 0.0;
    let mut go_link = false;
    ui.horizontal(|ui| {
        theme::label(ui, "LINKED TO", true, 13.0, INK);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            go_link = theme::link(ui, "Link an app", 13.0, Face::Mono).clicked();
        });
    });
    ui.add_space(10.0);
    theme::rule(ui, 1.5, INK);
    let mut linked = model.links.iter().filter(|(_, s)| matches!(s, State::Linked { .. })).peekable();
    if linked.peek().is_none() {
        ui.add_space(14.0);
        theme::text(ui, "No AI app yet.", Face::Regular, 16.0, BODY);
        ui.add_space(14.0);
        theme::rule(ui, 1.0, RULE);
    }
    // Compact, so several linked apps still fit above the buttons.
    for (which, state) in linked {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.set_min_height(32.0);
            ui.spacing_mut().item_spacing.x = 10.0;
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(super::brands::SIZE), Sense::hover());
            app.brands.paint(ui.painter(), rect, which.maker());
            ui.label(theme::job(which.name(), Face::Semi, 17.0, INK, 0.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let State::Linked { command } = state {
                    linked_mark(ui, command);
                }
            });
        });
        ui.add_space(8.0);
        theme::rule(ui, 1.0, RULE);
    }
    ui.add_space(34.0);
    theme::label(ui, "THIS SWITCH", true, 13.0, INK);
    ui.add_space(12.0);
    theme::rule(ui, 1.5, INK);
    let live = match &model.reach {
        Reach::Connected(live) => Some(live),
        _ => None,
    };
    let unknown = || "—".to_string();
    let rows: [(&str, String, bool); 4] = [
        ("Address", model.host.clone().unwrap_or_else(unknown), true),
        ("Kickstand", live.map_or_else(unknown, |l| format!("{} · protocol {}", l.info.version, l.info.protocol)), true),
        (
            "Background service",
            live.map_or_else(unknown, |l| match l.info.kind {
                DaemonKind::Sysmodule => "Running".into(),
                DaemonKind::Nro => "Off: Kickstand is open on the Switch".into(),
            }),
            false,
        ),
        (
            "Kept awake",
            live.and_then(|l| l.activity.as_ref()).map_or_else(unknown, |a| match a.keep_awake_s {
                0 => "No".into(),
                s => format!("{} min left", s.div_ceil(60)),
            }),
            true,
        ),
    ];
    for (name, value, mono) in rows {
        ui.add_space(11.0);
        ui.horizontal(|ui| {
            ui.label(theme::job(name, Face::Regular, 15.0, MUTED, 0.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let face = if mono { Face::Mono } else { Face::Regular };
                ui.label(theme::job(&value, face, if mono { 14.0 } else { 15.0 }, INK, 0.0));
            });
        });
        ui.add_space(11.0);
        theme::rule(ui, 1.0, RULE_LIGHT);
    }

    ui.add_space(34.0);
    let ready = live.is_some() && model.busy.is_none();
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(12.0, 12.0);
        if theme::button(ui, "Screenshot", Kind::Outline, false, ready).clicked() {
            app.send(Cmd::Screenshot);
        }
        if theme::button(ui, "Keep awake 1 h", Kind::Outline, false, ready).clicked() {
            app.send(Cmd::KeepAwake { minutes: 60 });
        }
        if theme::button(ui, "Restart into hekate…", Kind::Dashed, false, ready).clicked() {
            app.confirm_reboot = true;
        }
    });
    notice(ui, model);
    if go_link {
        app.go(model, Screen::Link);
    }
}

pub fn confirm_reboot(app: &mut Kickstand, ctx: &egui::Context) {
    let frame = egui::Frame::new().fill(theme::PAPER).stroke(Stroke::new(1.5, INK)).inner_margin(Margin::same(28));
    let modal = egui::Modal::new(egui::Id::unique("reboot")).frame(frame).backdrop_color(INK.gamma_multiply(0.35)).show(ctx, |ui| {
        ui.set_max_width(ui.available_width().min(440.0));
        ui.spacing_mut().item_spacing.y = 16.0;
        theme::heading(ui, "Restart into hekate?", 30.0);
        theme::text(
            ui,
            "The Switch restarts into hekate's menu. Whatever is running closes without saving. To come back, boot Atmosphère from hekate.",
            Face::Regular,
            16.0,
            BODY,
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            if theme::button(ui, "Restart", Kind::Primary, true, true).clicked() {
                app.send(Cmd::Reboot);
                app.confirm_reboot = false;
            }
            if theme::link(ui, "Cancel", 16.0, Face::Semi).clicked() {
                app.confirm_reboot = false;
            }
        });
    });
    if modal.should_close() {
        app.confirm_reboot = false;
    }
}
