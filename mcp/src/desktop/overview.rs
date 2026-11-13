// SPDX-License-Identifier: GPL-3.0-only
//! The overview the header's Kickstand opens: what Kickstand does, the agent's tools by area, and the
//! terminal commands (read from the command line's own definition, so they stay current).

use clap::CommandFactory;
use eframe::egui::{self, Margin, Rect, Sense, Stroke, Ui, Vec2};

use super::Kickstand;
use super::theme::{self, body, Face, ink, muted, RED, rule, rule_light};

/// An area of tools: its name, a line about it, and each tool with a line on what it does.
type Area = (&'static str, &'static str, &'static [(&'static str, &'static str)]);

/// The agent's tools by area. A test checks every tool is here.
const TOOLS: &[Area] = &[
    (
        "Develop & run",
        "Build, run and manage software on the Switch.",
        &[
            ("test_build", "Build a project on this computer, send it over, run it and watch: one step from code change to result"),
            ("projects", "The projects test_build knows (~/.config/kickstand/projects.json)"),
            ("run_nro", "Run homebrew with full memory (or in the Album for small tools), streaming what it prints"),
            ("reload_nro", "Hot-reload homebrew in place without going back through the Home Menu"),
            ("answer_hint", "Pass on your answer when you said whether to link libkickstand in the chat instead"),
            ("nro_output", "What the running homebrew has printed"),
            ("supervise", "Watch the running homebrew until it exits, crashes or hangs"),
            ("open_hbmenu", "Open the Homebrew Menu"),
            ("quit_app", "Quit the running game or homebrew, as from the home menu"),
            ("force_quit_app", "Force-quit a stuck game or homebrew"),
            ("list_apps", "The installed games and apps"),
            ("launch_title", "Start an installed game or app"),
        ],
    ),
    (
        "See & play",
        "Watch the screen and use a virtual controller.",
        &[
            ("screenshot", "A screenshot, scaled or cropped, with an optional coordinate grid"),
            ("wait_screen", "Wait until the screen changes or settles"),
            ("record_gif", "Record a few seconds of the screen"),
            ("audio_replay", "Save and analyze the last 5 seconds of console audio"),
            ("record_audio", "Record live audio from the Switch and analyze it"),
            ("audio_control", "Check or set the Switch's audio volume and mute state"),
            ("press", "Press buttons"),
            ("input", "Timed input: buttons held, sticks and touch"),
            ("input_until", "Hold, mash or replay input until something appears or goes away on screen"),
            ("macros", "The macros you recorded"),
            ("record_macro", "Record you playing, to replay later"),
            ("play_macro", "Replay a macro, e.g. back to the same spot in a game"),
            ("save_macro", "Write a macro from input steps"),
            ("control_macro", "Pause, resume or stop the macro playing"),
            ("type_text", "Type on a virtual keyboard"),
        ],
    ),
    (
        "Debug",
        "Track down crashes, hangs and memory problems.",
        &[
            ("crash_reports", "Atmosphère's crash reports and fatal errors"),
            ("crash_report", "One of them, summarized, with the screenshot taken at the crash"),
            ("symbolize", "Crash addresses as function names and source lines"),
            ("threads", "Every thread's backtrace in the running homebrew: what a hang waits on"),
            ("profile", "Sample the running software's CPU call stacks and save an interactive SVG flamegraph"),
            ("memory_map", "The running software's memory layout"),
            ("read_memory", "Read its memory, by address or symbol"),
            ("write_memory", "Change homebrew's memory (never a game's)"),
        ],
    ),
    (
        "Files",
        "Browse the SD card. Outside Kickstand's folders and your projects, you're asked first.",
        &[
            ("list_dir", "List a folder"),
            ("stat", "A file's size and type"),
            ("read_file", "Read a file"),
            ("tail_file", "A log's last lines, then what's added"),
            ("find", "Find files by name, searched on the Switch"),
            ("grep", "Find lines in files, searched on the Switch"),
            ("download", "Copy a file to this computer"),
            ("upload", "Copy a file to the SD card"),
            ("usb_upload", "Copy a large file to the SD card over USB"),
            ("delete", "Delete a file or folder"),
            ("answer_access", "Pass on your answer when you allowed a folder in the chat instead"),
        ],
    ),
    (
        "Console",
        "Check on the Switch and look after it.",
        &[
            ("status", "What's running, a fatal error, keep-awake, other agents, and a small screenshot"),
            ("ping", "Check the Switch answers"),
            ("resources", "Memory use of the background service"),
            ("keep_awake", "Keep the screen on past 30 minutes, or let it sleep"),
            ("sysmodule", "Install or restart the background service"),
            ("hekate_entries", "hekate's boot entries"),
            ("reboot", "Restart into hekate: a boot entry, USB mass storage or its menu"),
        ],
    ),
];

/// The tabs: the overview, the tool areas, the command line.
fn tab_names() -> Vec<&'static str> {
    std::iter::once("Overview").chain(TOOLS.iter().map(|(name, _, _)| *name)).chain(std::iter::once("Command line")).collect()
}

/// Draws the overview; `open` runs from 0 to 1 as it opens and back as it closes.
pub fn show(app: &mut Kickstand, ctx: &egui::Context, open: f32) {
    // The frame fades with what's in it.
    let frame = egui::Frame::new().fill(theme::paper().gamma_multiply(open)).stroke(Stroke::new(1.5, ink().gamma_multiply(open))).inner_margin(Margin::same(32));
    let id = egui::Id::unique("overview");
    let area = egui::Modal::default_area(id).fade_in(false).anchor(egui::Align2::CENTER_CENTER, Vec2::new(0.0, (1.0 - open) * 14.0));
    let screen = ctx.content_rect();
    let modal = egui::Modal::new(id).area(area).frame(frame).backdrop_color(theme::scrim().gamma_multiply(open)).show(ctx, |ui| {
        ui.multiply_opacity(open);
        let width = (screen.width() - 140.0).clamp(480.0, 760.0);
        ui.set_width(width);
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.horizontal(|ui| {
            theme::heading(ui, "Kickstand", 34.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::link(ui, "Close", 15.0, Face::Semi).clicked() {
                    app.overview = false;
                }
            });
        });
        ui.add_space(6.0);
        theme::text(ui, "Let an AI agent use your Switch.", Face::Regular, 17.0, body());
        ui.add_space(20.0);
        tabs(app, ui);
        ui.add_space(4.0);
        // The same height for every tab, so the dialog doesn't jump.
        let height = (screen.height() - 300.0).clamp(220.0, 460.0);
        egui::ScrollArea::vertical().id_salt(("overview", app.overview_tab)).max_height(height).min_scrolled_height(height).auto_shrink([false, false]).show(
            ui,
            |ui| {
                let t = super::eased(ui.input(|i| i.time) - app.overview_at, super::ENTER_S);
                if t < 1.0 {
                    ui.ctx().request_repaint();
                }
                ui.multiply_opacity(t);
                match app.overview_tab {
                    0 => overview(ui),
                    i if i <= TOOLS.len() => {
                        let (_, about, tools) = TOOLS[i - 1];
                        ui.add_space(16.0);
                        theme::text(ui, about, Face::Regular, 15.0, body());
                        ui.add_space(8.0);
                        rows(ui, tools.iter().map(|(name, what)| (name.to_string(), what.to_string())));
                    }
                    _ => commands(ui),
                }
            },
        );
    });
    if modal.should_close() {
        app.overview = false;
    }
}

/// The tab strip, the chosen one underlined in red (the line slides over).
fn tabs(app: &mut Kickstand, ui: &mut Ui) {
    let mut underline = None;
    let start = ui.cursor().left();
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(20.0, 4.0);
        for (i, name) in tab_names().into_iter().enumerate() {
            let chosen = app.overview_tab == i;
            let galley = ui.painter().layout_job(theme::job(name, if chosen { Face::Bold } else { Face::Semi }, 15.0, ink(), 0.0));
            let (rect, response) = ui.allocate_exact_size(Vec2::new(galley.size().x, 34.0), Sense::click());
            let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
            let color = if chosen || response.hovered() { ink() } else { muted() };
            ui.painter().galley(egui::pos2(rect.left(), rect.top() + 6.0), galley, color);
            if chosen {
                underline = Some(rect);
            }
            if response.clicked() && !chosen {
                app.overview_tab = i;
                app.overview_at = ui.input(|i| i.time);
            }
        }
    });
    if let Some(rect) = underline {
        let ctx = ui.ctx();
        // Measured from the strip, so it only moves when the tab changes, not while the dialog rises in.
        let left = start + ctx.animate_value_with_time(egui::Id::unique("overview-tab-left"), rect.left() - start, 0.2);
        let right = start + ctx.animate_value_with_time(egui::Id::unique("overview-tab-right"), rect.right() - start, 0.2);
        ui.painter().rect_filled(Rect::from_x_y_ranges(left..=right, rect.bottom() - 3.0..=rect.bottom()), 0.0, RED);
    }
    theme::hrule(ui, 1.5, ink());
}

fn overview(ui: &mut Ui) {
    let section = |ui: &mut Ui, title: &str| {
        ui.add_space(26.0);
        theme::label(ui, title, true, 13.0, ink());
        ui.add_space(10.0);
        theme::hrule(ui, 1.0, rule());
    };
    ui.add_space(18.0);
    theme::text(
        ui,
        "Kickstand lets an AI agent work on your Switch for you. It can copy homebrew over, run it, watch the screen, press buttons and read crash reports, so it can test and debug on real hardware while you're away.",
        Face::Regular,
        16.0,
        body(),
    );
    section(ui, "HOW IT WORKS");
    ui.add_space(18.0);
    diagram(ui);
    ui.add_space(16.0);
    theme::text(
        ui,
        "This window sets it all up: it finds your Switch, pairs with it and links your AI apps. After that, it shows you what the agents are doing.",
        Face::Regular,
        15.0,
        body(),
    );
    section(ui, "STAYING SAFE");
    rows(
        ui,
        [
            ("SD card", "Kickstand's folders and your projects are fair game. For anything else, you're asked first."),
            ("System files", "Atmosphère, hekate and Nintendo's files, keys and NAND are never touched."),
            ("Games", "Games always run with full memory, and their memory is never edited."),
            ("Restarts", "The Switch only ever restarts into hekate. It's never powered off, so an RCM setup can always get back."),
            ("Sleep", "The Switch stays awake for 30 minutes after the agent's last action, then sleeps as usual."),
        ]
        .map(|(a, b)| (a.to_string(), b.to_string())),
    );
    ui.add_space(8.0);
}

/// Your AI app → Kickstand on this computer → your Switch.
fn diagram(ui: &mut Ui) {
    const BOXES: [(&str, &str); 3] = [
        ("Your AI app", "Claude Code, Antigravity, Codex, LM Studio and others"),
        ("Kickstand", "Runs on this computer and gives the agent its tools"),
        ("Your Switch", "Kickstand's background service does what the agent asks"),
    ];
    const LINKS: [&str; 2] = ["TOOLS", "WI-FI"];
    const GAP: f32 = 64.0;
    const PAD: f32 = 14.0;
    let width = ui.available_width();
    let box_width = (width - 2.0 * GAP) / 3.0;
    let painter = ui.painter().clone();
    let laid: Vec<_> = BOXES
        .iter()
        .map(|(title, what)| {
            let title = painter.layout_job(theme::job(title, Face::Bold, 16.0, ink(), 0.0));
            let mut job = theme::job(what, Face::Regular, 13.5, body(), 0.0);
            job.wrap.max_width = box_width - 2.0 * PAD;
            (title, painter.layout_job(job))
        })
        .collect();
    let height = laid.iter().map(|(title, what)| title.size().y + 6.0 + what.size().y).fold(0.0, f32::max) + 2.0 * PAD;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    for (i, (title, what)) in laid.into_iter().enumerate() {
        let left = rect.left() + i as f32 * (box_width + GAP);
        let cell = Rect::from_min_size(egui::pos2(left, rect.top()), Vec2::new(box_width, height));
        painter.rect_stroke(cell, 0.0, Stroke::new(1.5, ink()), egui::StrokeKind::Inside);
        let title_height = title.size().y;
        painter.galley(cell.min + Vec2::splat(PAD), title, ink());
        painter.galley(cell.min + Vec2::new(PAD, PAD + title_height + 6.0), what, body());
        if let Some(link) = LINKS.get(i) {
            // Both ways: the agent asks, the Switch answers.
            let y = cell.center().y;
            let (from, to) = (cell.right() + 8.0, cell.right() + GAP - 8.0);
            painter.line_segment([egui::pos2(from, y), egui::pos2(to, y)], Stroke::new(1.5, ink()));
            for (tip, dir) in [(to, 1.0), (from, -1.0)] {
                let back = tip - dir * 6.0;
                painter.line_segment([egui::pos2(tip, y), egui::pos2(back, y - 4.0)], Stroke::new(1.5, ink()));
                painter.line_segment([egui::pos2(tip, y), egui::pos2(back, y + 4.0)], Stroke::new(1.5, ink()));
            }
            let label = painter.layout_job(theme::job(link, Face::Mono, 11.0, muted(), 0.06));
            painter.galley(egui::pos2((from + to) / 2.0 - label.size().x / 2.0, y - 8.0 - label.size().y), label, muted());
        }
    }
}

/// The terminal commands, as `kickstand --help` lists them.
fn commands(ui: &mut Ui) {
    ui.add_space(16.0);
    theme::text(ui, "Everything here works from a terminal too. Add --help to any command for the details.", Face::Regular, 15.0, body());
    ui.add_space(8.0);
    let cli = crate::Cli::command();
    let commands = cli.get_subcommands().filter(|c| !c.is_hide_set()).map(|c| {
        let about = c.get_about().map(|a| a.to_string()).unwrap_or_default();
        (c.get_name().to_string(), about)
    });
    rows(ui, commands.collect::<Vec<_>>());
}

/// Name and description rows, the name in a column of its own.
fn rows(ui: &mut Ui, rows: impl IntoIterator<Item = (String, String)>) {
    for (name, what) in rows {
        ui.add_space(11.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            let galley = ui.painter().layout_job(theme::job(&name, Face::MonoSemi, 13.5, ink(), 0.0));
            // Centred on the description's first line.
            let line = ui.painter().layout_job(theme::job("Ag", Face::Regular, 15.0, body(), 0.0)).size().y;
            let (rect, _) = ui.allocate_exact_size(Vec2::new(130.0, line), Sense::hover());
            ui.painter().galley(egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0), galley, ink());
            ui.add(egui::Label::new(theme::job(&what, Face::Regular, 15.0, body(), 0.0)).wrap());
        });
        ui.add_space(11.0);
        theme::hrule(ui, 1.0, rule_light());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_tool_is_described_once() {
        let mut described: Vec<&str> = super::TOOLS.iter().flat_map(|(_, _, tools)| tools.iter().map(|(name, _)| *name)).collect();
        described.sort();
        let mut tools = crate::tools::tool_names();
        tools.sort();
        assert_eq!(described, tools);
    }
}
