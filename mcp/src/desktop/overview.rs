// SPDX-License-Identifier: GPL-3.0-only
//! The overview the header's Kickstand opens: what Kickstand does, the agent's tools by area, and the
//! terminal commands (read from the command line's own definition, so they stay current).

use clap::CommandFactory;
use eframe::egui::{self, Margin, Rect, Sense, Stroke, Ui, Vec2};

use super::Kickstand;
use super::theme::{self, BODY, Face, INK, MUTED, RED, RULE, RULE_LIGHT};

/// An area of tools: its name, a line about it, and each tool with a line on what it does.
type Area = (&'static str, &'static str, &'static [(&'static str, &'static str)]);

/// The agent's tools by area. A test checks every tool is here.
const TOOLS: &[Area] = &[
    (
        "Develop & run",
        "From a code change to the result on the Switch, and the software on it.",
        &[
            ("test_build", "Build a project on this computer, send it over, run it and watch: one step from code change to result"),
            ("projects", "The projects test_build knows (~/.config/kickstand/projects.json)"),
            ("run_nro", "Run homebrew with full memory (or in the Album for small tools), streaming what it prints"),
            ("nro_output", "What the running homebrew has printed"),
            ("supervise", "Watch the running homebrew until it exits, crashes or hangs"),
            ("open_hbmenu", "Open the Homebrew Menu"),
            ("close_app", "Close the running game or homebrew"),
            ("list_apps", "The installed games and apps"),
            ("launch_title", "Start an installed game or app"),
        ],
    ),
    (
        "See & play",
        "The screen, and a virtual controller.",
        &[
            ("screenshot", "A screenshot, scaled or cropped, with an optional coordinate grid"),
            ("wait_screen", "Wait until the screen changes or settles"),
            ("record_gif", "Record a few seconds of the screen"),
            ("press", "Press buttons"),
            ("input", "Timed input: buttons held, sticks and touch"),
            ("input_until", "Hold, mash or replay input until something appears or goes away on screen"),
        ],
    ),
    (
        "Debug",
        "Crashes, hangs and memory.",
        &[
            ("crash_reports", "Atmosphère's crash reports and fatal errors"),
            ("crash_report", "One of them, summarized, with the screenshot taken at the crash"),
            ("symbolize", "Crash addresses as function names and source lines"),
            ("threads", "Every thread's backtrace in the running homebrew: what a hang waits on"),
            ("memory_map", "The running software's memory layout"),
            ("read_memory", "Read its memory, by address or symbol"),
            ("write_memory", "Change homebrew's memory (never a game's)"),
        ],
    ),
    (
        "Files",
        "The SD card. Outside Kickstand's folders and your projects', your AI app asks you first.",
        &[
            ("list_dir", "List a folder"),
            ("stat", "A file's size and type"),
            ("read_file", "Read a file"),
            ("tail_file", "A log's last lines, then what's added"),
            ("download", "Copy a file to this computer"),
            ("upload", "Copy a file to the SD card"),
            ("delete", "Delete a file or folder"),
        ],
    ),
    (
        "Console",
        "The Switch itself.",
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
    let frame = egui::Frame::new().fill(theme::PAPER).stroke(Stroke::new(1.5, INK)).inner_margin(Margin::same(32));
    let id = egui::Id::unique("overview");
    let area = egui::Modal::default_area(id).fade_in(false).anchor(egui::Align2::CENTER_CENTER, Vec2::new(0.0, (1.0 - open) * 14.0));
    let screen = ctx.content_rect();
    let modal = egui::Modal::new(id).area(area).frame(frame).backdrop_color(INK.gamma_multiply(0.35 * open)).show(ctx, |ui| {
        ui.multiply_opacity(open);
        if !app.overview {
            ui.disable();
        }
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
        theme::text(ui, "Your Switch, for an AI agent on this computer.", Face::Regular, 17.0, BODY);
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
                        theme::text(ui, about, Face::Regular, 15.0, BODY);
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
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(20.0, 4.0);
        for (i, name) in tab_names().into_iter().enumerate() {
            let chosen = app.overview_tab == i;
            let galley = ui.painter().layout_job(theme::job(name, if chosen { Face::Bold } else { Face::Semi }, 15.0, INK, 0.0));
            let (rect, response) = ui.allocate_exact_size(Vec2::new(galley.size().x, 34.0), Sense::click());
            let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
            let color = if chosen || response.hovered() { INK } else { MUTED };
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
        let left = ctx.animate_value_with_time(egui::Id::unique("overview-tab-left"), rect.left(), 0.2);
        let right = ctx.animate_value_with_time(egui::Id::unique("overview-tab-right"), rect.right(), 0.2);
        let top = ctx.animate_value_with_time(egui::Id::unique("overview-tab-top"), rect.bottom() - 3.0, 0.2);
        ui.painter().rect_filled(Rect::from_x_y_ranges(left..=right, top..=top + 3.0), 0.0, RED);
    }
    theme::rule(ui, 1.5, INK);
}

fn overview(ui: &mut Ui) {
    let section = |ui: &mut Ui, title: &str| {
        ui.add_space(22.0);
        theme::label(ui, title, true, 13.0, INK);
        ui.add_space(10.0);
        theme::rule(ui, 1.0, RULE);
    };
    ui.add_space(18.0);
    theme::text(
        ui,
        "Kickstand lets an AI agent use your Switch the way you would at a desk: copy homebrew over, run it, look at the screen, press buttons, read logs and crash reports, and get the console back when something hangs. So it can test and debug on real hardware without you there.",
        Face::Regular,
        16.0,
        BODY,
    );
    section(ui, "HOW IT FITS TOGETHER");
    rows(
        ui,
        [
            ("The Switch", "Kickstand's background service answers on your Wi-Fi (or the Kickstand app, opened from the Homebrew Menu)."),
            ("Your AI app", "Runs Kickstand's MCP server, which gives the agent its tools: Claude Code, Antigravity, Codex, LM Studio and others."),
            ("This window", "Finds the Switch, pairs with it, links your AI apps, and shows what agents are doing."),
        ]
        .map(|(a, b)| (a.to_string(), b.to_string())),
    );
    section(ui, "GETTING STARTED");
    rows(
        ui,
        [
            ("01 Find", "Open Kickstand from the Homebrew Menu on the Switch, on the same Wi-Fi as this computer."),
            ("02 Pair", "Type the code on the Switch's screen."),
            ("03 Link", "Add Kickstand to your AI app, then start a new session in it."),
            ("Then", "Ask the agent to use the Switch: \"run my build and tell me if it crashes\"."),
        ]
        .map(|(a, b)| (a.to_string(), b.to_string())),
    );
    section(ui, "WHAT IT WON'T DO");
    rows(
        ui,
        [
            ("Your files", "Outside Kickstand's folders and your projects', the agent asks you before using the SD card."),
            ("The system", "Never touches Atmosphère's, hekate's or Nintendo's files, keys or NAND."),
            ("Games", "Always run with full memory, and their memory is never changed."),
            ("Restarts", "Only into hekate, never a power-off: a console injected over RCM couldn't come back on its own."),
            ("Sleep", "The Switch stays awake 30 minutes after the agent's last request, then sleeps as usual."),
        ]
        .map(|(a, b)| (a.to_string(), b.to_string())),
    );
    ui.add_space(8.0);
}

/// The terminal commands, as `kickstand --help` lists them.
fn commands(ui: &mut Ui) {
    ui.add_space(16.0);
    theme::text(ui, "Everything from a terminal too: kickstand <command>, and kickstand <command> --help for the details.", Face::Regular, 15.0, BODY);
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
            let galley = ui.painter().layout_job(theme::job(&name, Face::MonoSemi, 13.5, INK, 0.0));
            // Centred on the description's first line.
            let line = ui.painter().layout_job(theme::job("Ag", Face::Regular, 15.0, BODY, 0.0)).size().y;
            let (rect, _) = ui.allocate_exact_size(Vec2::new(130.0, line), Sense::hover());
            ui.painter().galley(egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0), galley, INK);
            ui.add(egui::Label::new(theme::job(&what, Face::Regular, 15.0, BODY, 0.0)).wrap());
        });
        ui.add_space(11.0);
        theme::rule(ui, 1.0, RULE_LIGHT);
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
