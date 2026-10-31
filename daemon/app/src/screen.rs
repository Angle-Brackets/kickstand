// SPDX-License-Identifier: GPL-3.0-only
//! The NRO's screen: what Kickstand is doing, for someone looking at the console. Whether an agent is
//! connected and what it's doing in plain words, how to reach this Switch, whether the background service
//! is installed, and the daemon log underneath. Drawn in the system font (`pl:u`), with the button glyphs
//! the home menu uses; the 8x8 bitmap font stands in if the shared fonts can't be loaded.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

use ab_glyph::{Font as _, ScaleFont as _};
use nx::gpu::canvas::{AlphaBlend, Canvas, CanvasManager, Font, RGBA8};
use nx::gpu::{self, BlockLinearHeights};
use nx::ipc::sf::vi::LayerStackId;
use nx::svc;
use nx::sync::RwLock;

use daemon_core::activity::{self, Client};
use daemon_core::{log, sd};
use proto::path::{SYSMODULE_BOOT2_FLAG, SYSMODULE_NSP};
use services::pl::{self, SharedFonts};

/// `kickstand-mark-512.png` at 64x64, straight-alpha RGBA (`assets/logo/gen_nro_mark.py`).
static MARK: &[u8; MARK_SIZE * MARK_SIZE * 4] = include_bytes!("mark.rgba");
const MARK_SIZE: usize = 64;

const BACKGROUND: RGBA8 = RGBA8::new_scaled(0x12, 0x12, 0x16, 0xff);
const CARD: RGBA8 = RGBA8::new_scaled(0x1e, 0x1e, 0x24, 0xff);
const TEXT: RGBA8 = RGBA8::new_scaled(0xec, 0xec, 0xf1, 0xff);
const MUTED: RGBA8 = RGBA8::new_scaled(0x9a, 0x9a, 0xa6, 0xff);
const GREEN: RGBA8 = RGBA8::new_scaled(0x3d, 0xdc, 0x84, 0xff);
const AMBER: RGBA8 = RGBA8::new_scaled(0xff, 0xb0, 0x20, 0xff);
/// The mark's accent.
const RED: RGBA8 = RGBA8::new_scaled(0xff, 0x45, 0x54, 0xff);

/// NintendoExt glyphs (the home menu's button icons).
const GLYPH_PLUS: char = '\u{E0EF}';
const GLYPH_UP: char = '\u{E0EB}';
const GLYPH_DOWN: char = '\u{E0EC}';

/// Log lines kept for scrolling.
const LOG_KEEP: usize = 200;
const LOG_ROWS: usize = 9;
/// How often the screen is redrawn with nothing new (for the "12 s ago" times).
const REFRESH_MS: u64 = 1000;
/// How often the SD card and network are checked again.
const CHECK_MS: u64 = 3000;
/// A request this recent counts as what the agent is doing now.
const BUSY_MS: u64 = 3000;
/// Input is polled this often while nothing needs drawing.
const IDLE_SLEEP_NS: i64 = 33_000_000;

struct Fonts {
    standard: Font<'static>,
    buttons: Font<'static>,
}

/// What the screen shows besides the log, refreshed every [`CHECK_MS`].
#[derive(Default)]
struct Checks {
    ip: Option<Ipv4Addr>,
    installed: bool,
    boot2: bool,
}

impl Checks {
    fn now() -> Self {
        Checks {
            ip: daemon_core::server::listening_ip(),
            installed: sd::stat(SYSMODULE_NSP).is_ok(),
            boot2: sd::stat(SYSMODULE_BOOT2_FLAG).is_ok(),
        }
    }

    /// Whether the background service is installed to start at boot.
    fn starts_at_boot(&self) -> bool {
        self.installed && self.boot2
    }
}

pub struct Screen {
    /// `None` if the display couldn't be set up; the daemon still runs, just without a picture.
    canvas: Option<CanvasManager<RGBA8>>,
    fonts: Option<Fonts>,
    log: VecDeque<String>,
    /// Lines scrolled back from the newest.
    scroll: usize,
    checks: Checks,
    checked_ms: u64,
    drawn_ms: Option<u64>,
}

impl Screen {
    pub fn new() -> Self {
        // With vi:m the layer can join the screenshot stack, so the agent's screenshots show this screen too.
        let canvas = layer(gpu::ViServiceKind::Manager)
            .map(|mut c| {
                if let Err(rc) = c.surface.push_layer_stack(LayerStackId::Screenshot) {
                    log!("this screen won't show in screenshots: {rc}");
                }
                c
            })
            .or_else(|_| layer(gpu::ViServiceKind::System));
        let canvas = canvas.map_err(|rc| log!("no screen output: {rc}")).ok();
        let fonts = canvas.as_ref().and_then(|_| match load_fonts() {
            Ok(fonts) => Some(fonts),
            Err(e) => {
                log!("system fonts unavailable ({e}); using the bitmap font");
                None
            }
        });
        Screen {
            canvas,
            fonts,
            log: VecDeque::new(),
            scroll: 0,
            checks: Checks::default(),
            checked_ms: 0,
            drawn_ms: None,
        }
    }

    /// Whether the background service should be running once this NRO exits: it's installed to start at boot.
    pub fn starts_at_boot(&self) -> bool {
        self.checks.starts_at_boot()
    }

    /// Takes new log lines and redraws when something changed (or every [`REFRESH_MS`]); otherwise waits a
    /// little, so the caller can poll input.
    pub fn frame(&mut self, scroll_up: bool, scroll_down: bool) {
        let now = daemon_core::uptime_ms();
        let mut changed = false;
        for line in log::take_recent() {
            if self.log.len() == LOG_KEEP {
                self.log.pop_front();
            }
            self.log.push_back(String::from(line.trim_end()));
            if self.scroll > 0 {
                // Keep the same lines in view while scrolled back.
                self.scroll += 1;
            }
            changed = true;
        }
        let max_scroll = self.log.len().saturating_sub(LOG_ROWS);
        if scroll_up && self.scroll < max_scroll {
            self.scroll += 1;
            changed = true;
        } else if scroll_down && self.scroll > 0 {
            self.scroll -= 1;
            changed = true;
        }
        self.scroll = self.scroll.min(max_scroll);
        if self.drawn_ms.is_none() || now - self.checked_ms >= CHECK_MS {
            self.checks = Checks::now();
            self.checked_ms = now;
        }

        let due = self.drawn_ms.is_none_or(|t| now - t >= REFRESH_MS);
        let Some(canvas) = self.canvas.as_mut().filter(|_| changed || due) else {
            let _ = svc::sleep_thread(IDLE_SLEEP_NS);
            return;
        };
        let view = View {
            fonts: self.fonts.as_ref(),
            has_token: daemon_core::tokens::any(),
            pair_code: daemon_core::pairing::code(),
            checks: &self.checks,
            clients: activity::clients(),
            log: &self.log,
            scroll: self.scroll,
            now,
        };
        if let Err(rc) = canvas.render(Some(BACKGROUND), |c| {
            view.draw(&mut Painter { canvas: c, fonts: view.fonts });
            Ok(())
        }) {
            log!("drawing the screen failed: {rc}");
            self.canvas = None;
        }
        self.drawn_ms = Some(now);
    }
}

/// nx's canvas takes the GPU context as `Arc<RwLock<_>>`; it stays on this thread.
#[allow(clippy::arc_with_non_send_sync)]
fn layer(vi: gpu::ViServiceKind) -> nx::result::Result<CanvasManager<RGBA8>> {
    let ctx = gpu::Context::new(gpu::NvDrvServiceKind::Applet, vi, 0x40000)?;
    CanvasManager::new_stray(Arc::new(RwLock::new(ctx)), Default::default(), 3, BlockLinearHeights::OneGob)
}

fn load_fonts() -> Result<Fonts, String> {
    // Copied out so the session and mapping can be given back (see `SharedFonts`); the copies live as long
    // as this NRO's heap.
    let shared = SharedFonts::map().map_err(|rc| format!("pl:u: {rc}"))?;
    let load = |kind| -> Result<Font<'static>, String> {
        let data = shared.font(kind).map_err(|rc| format!("font {kind}: {rc}"))?;
        let data: &'static [u8] = alloc::boxed::Box::leak(data.into());
        Font::try_from_slice(data).map_err(|_| format!("font {kind} isn't TrueType"))
    };
    Ok(Fonts { standard: load(pl::FONT_STANDARD)?, buttons: load(pl::FONT_NINTENDO_EXT)? })
}

struct View<'a> {
    fonts: Option<&'a Fonts>,
    has_token: bool,
    /// The pairing code, as digits.
    pair_code: Option<String>,
    checks: &'a Checks,
    clients: Vec<Client>,
    log: &'a VecDeque<String>,
    scroll: usize,
    now: u64,
}

const WIDTH: i32 = 1280;
const MARGIN: i32 = 48;
const PAD: i32 = 28;

impl View<'_> {
    fn draw<C: Canvas<ColorFormat = RGBA8>>(&self, p: &mut Painter<'_, C>) {
        self.header(p);
        self.status(p, MARGIN, 120, 744, 250);
        self.console(p, 816, 120, WIDTH - MARGIN - 816, 250);
        self.activity(p, MARGIN, 394, WIDTH - 2 * MARGIN, 262);
        self.footer(p, 676);
    }

    fn header<C: Canvas<ColorFormat = RGBA8>>(&self, p: &mut Painter<'_, C>) {
        p.image(MARK, MARK_SIZE as i32, MARGIN, 30);
        p.text("Kickstand", 26.0, MARGIN + 84, 28, TEXT);
        p.text("Lets an AI agent test homebrew on this Switch", 13.0, MARGIN + 86, 72, MUTED);
        let version = format!("v{}  ·  protocol {}", daemon_core::VERSION, proto::PROTOCOL_VERSION);
        p.text_right(&version, 13.0, WIDTH - MARGIN, 48, MUTED);
    }

    /// The headline: is an agent connected, and what is it doing.
    fn status<C: Canvas<ColorFormat = RGBA8>>(&self, p: &mut Painter<'_, C>, x: i32, y: i32, w: i32, h: i32) {
        p.card(x, y, w, h);
        let (dot, headline, lines): (RGBA8, String, Vec<(String, RGBA8)>) = if !self.clients.is_empty() {
            let headline = match self.clients.len() {
                1 => "Agent connected".into(),
                n => format!("{n} agents connected"),
            };
            let mut lines = Vec::new();
            for client in self.clients.iter().take(2) {
                let idle = self.now.saturating_sub(client.last_ms);
                let doing = if idle < BUSY_MS {
                    format!("Now: {}", client.doing)
                } else {
                    format!("Last: {}, {} ago", client.doing, duration(idle))
                };
                lines.push((doing, TEXT));
                let requests = if client.requests == 1 { "1 request".into() } else { format!("{} requests", client.requests) };
                let connected = duration(self.now.saturating_sub(client.connected_ms));
                lines.push((format!("From {}  ·  connected {connected}  ·  {requests}", client.ip), MUTED));
            }
            (GREEN, headline, lines)
        } else if !self.has_token {
            (
                RED,
                "Pair a PC to start".into(),
                match &self.pair_code {
                    Some(code) => alloc::vec![
                        (format!("Pairing code:  {}", spaced(code)), TEXT),
                        (format!("On your PC, on the same Wi-Fi, run  kickstand pair {code}"), MUTED),
                    ],
                    None => alloc::vec![
                        ("No PC is paired yet, and pairing isn't available.".into(), TEXT),
                        (format!("Put a token in {} on the SD card instead.", proto::path::TOKEN_PATH), MUTED),
                    ],
                },
            )
        } else if self.checks.ip.is_none() {
            (
                AMBER,
                "Not on a network".into(),
                alloc::vec![
                    ("Connect this Switch to Wi-Fi so your agent can reach it.".into(), TEXT),
                    ("System Settings > Internet".into(), MUTED),
                ],
            )
        } else {
            let ip = self.checks.ip.map(|ip| format!("{ip}")).unwrap_or_default();
            (
                GREEN,
                "Ready for your agent".into(),
                alloc::vec![
                    ("Nothing is connected right now.".into(), TEXT),
                    (format!("Your PC must be on the same Wi-Fi to reach {ip}."), MUTED),
                ],
            )
        };
        p.dot(x + PAD + 10, y + PAD + 20, 10, dot);
        p.text(&headline, 22.0, x + PAD + 34, y + PAD, TEXT);
        let mut line_y = y + PAD + 64;
        for (i, (line, color)) in lines.iter().enumerate() {
            p.text(&p.fit(line, 15.0, w - 2 * PAD), 15.0, x + PAD, line_y, *color);
            // Pairs of lines, with a gap between clients.
            line_y += if i % 2 == 1 { 44 } else { 32 };
        }
        if self.clients.len() < 2 {
            let hint = if self.checks.starts_at_boot() {
                "You can leave this screen with +: Kickstand keeps working in the background."
            } else {
                "Kickstand works only while this screen is open."
            };
            p.text(hint, 12.0, x + PAD, y + h - PAD - 16, MUTED);
        }
    }

    /// How to reach this console, and the background service.
    fn console<C: Canvas<ColorFormat = RGBA8>>(&self, p: &mut Painter<'_, C>, x: i32, y: i32, w: i32, h: i32) {
        p.card(x, y, w, h);
        p.text("This Switch", 15.0, x + PAD, y + PAD - 4, MUTED);
        let ip = self.checks.ip.map(|ip| format!("{ip}"));
        let service = match (self.checks.installed, self.checks.boot2) {
            (true, true) => ("Starts at boot", GREEN),
            (true, false) => ("Installed, off at boot", AMBER),
            (false, _) => ("Not installed", MUTED),
        };
        let paired = if self.has_token { ("Paired", GREEN) } else { ("Not yet", AMBER) };
        let rows: [(&str, String, RGBA8); 5] = [
            ("Address", ip.clone().unwrap_or_else(|| "No network".into()), if ip.is_some() { TEXT } else { AMBER }),
            ("Port", format!("{}", proto::DEFAULT_PORT), TEXT),
            ("Paired", paired.0.into(), paired.1),
            ("Pairing code", self.pair_code.as_deref().map_or_else(|| "Off".into(), spaced), TEXT),
            ("Service", service.0.into(), service.1),
        ];
        let mut row_y = y + PAD + 34;
        for (label, value, color) in rows {
            p.text(label, 15.0, x + PAD, row_y, MUTED);
            p.text_right(&value, 15.0, x + w - PAD, row_y, color);
            row_y += 36;
        }
    }

    fn activity<C: Canvas<ColorFormat = RGBA8>>(&self, p: &mut Painter<'_, C>, x: i32, y: i32, w: i32, h: i32) {
        p.card(x, y, w, h);
        p.text("Activity", 15.0, x + PAD, y + PAD - 8, MUTED);
        if self.scroll > 0 {
            let back = format!("{} older", self.scroll);
            p.text_right(&back, 12.0, x + w - PAD, y + PAD - 6, AMBER);
        }
        let end = self.log.len() - self.scroll;
        let start = end.saturating_sub(LOG_ROWS);
        let mut line_y = y + PAD + 26;
        for line in self.log.range(start..end) {
            // `[12.345] message`: the time muted, the message bright.
            let (time, message) = match line.split_once("] ") {
                Some((time, message)) if time.starts_with('[') => (clock(&time[1..]), message),
                _ => (String::new(), line.as_str()),
            };
            p.text_right(&time, 12.0, x + PAD + 88, line_y + 2, MUTED);
            p.text(&p.fit(message, 13.0, w - 2 * PAD - 104), 13.0, x + PAD + 104, line_y, TEXT);
            line_y += 24;
        }
    }

    fn footer<C: Canvas<ColorFormat = RGBA8>>(&self, p: &mut Painter<'_, C>, y: i32) {
        let mut x = MARGIN;
        x += p.button(GLYPH_PLUS, "+", 15.0, x, y, TEXT) + 8;
        let exit = if self.checks.starts_at_boot() { "Exit (Kickstand keeps running in the background)" } else { "Exit" };
        p.text(exit, 14.0, x, y + 2, TEXT);

        let scroll = "Scroll activity";
        let mut x = WIDTH - MARGIN - p.width(scroll, 14.0);
        p.text(scroll, 14.0, x, y + 2, TEXT);
        x -= 8;
        x -= p.button_width(GLYPH_DOWN, "v", 15.0);
        p.button(GLYPH_DOWN, "v", 15.0, x, y, TEXT);
        x -= p.button_width(GLYPH_UP, "^", 15.0);
        p.button(GLYPH_UP, "^", 15.0, x, y, TEXT);
    }
}

/// `482913` as `482 913`, easier to read off the screen.
fn spaced(code: &str) -> String {
    let (head, tail) = code.split_at(code.len() / 2);
    format!("{head} {tail}")
}

/// A log line's `seconds.millis` since boot as `h:mm:ss`.
fn clock(uptime: &str) -> String {
    let Ok(s) = uptime.split('.').next().unwrap_or("").parse::<u64>() else { return String::from(uptime) };
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// `12 s`, `3 min`, `1 h 5 min`.
fn duration(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min", s / 60),
        _ => format!("{} h {} min", s / 3600, s / 60 % 60),
    }
}

/// Drawing helpers over nx's canvas. Sizes are in points, as nx's font drawing takes them; `y` is the top
/// of the line.
struct Painter<'a, C> {
    canvas: &'a mut C,
    fonts: Option<&'a Fonts>,
}

impl<C: Canvas<ColorFormat = RGBA8>> Painter<'_, C> {
    fn bitmap_scale(size: f32) -> u32 {
        ((size / 8.0) as u32).max(1)
    }

    fn measure(font: &Font, text: &str, size: f32) -> i32 {
        let font = font.as_scaled(font.pt_to_px_scale(size).unwrap_or(size.into()));
        let mut width = 0.0;
        let mut previous = None;
        for c in text.chars() {
            let id = font.glyph_id(c);
            if let Some(previous) = previous {
                width += font.kern(previous, id);
            }
            width += font.h_advance(id);
            previous = Some(id);
        }
        width as i32
    }

    fn width(&self, text: &str, size: f32) -> i32 {
        match self.fonts {
            Some(fonts) => Self::measure(&fonts.standard, text, size),
            None => (text.chars().count() as u32 * 8 * Self::bitmap_scale(size)) as i32,
        }
    }

    fn text(&mut self, text: &str, size: f32, x: i32, y: i32, color: RGBA8) {
        match self.fonts {
            Some(fonts) => self.canvas.draw_font_text(&fonts.standard, text, color, size, x, y, AlphaBlend::Source),
            None => self.canvas.draw_ascii_bitmap_text(text, color, Self::bitmap_scale(size), x, y + 4, AlphaBlend::Source),
        }
    }

    fn text_right(&mut self, text: &str, size: f32, right: i32, y: i32, color: RGBA8) {
        let width = self.width(text, size);
        self.text(text, size, right - width, y, color);
    }

    /// `text`, cut short with an ellipsis to fit in `max` pixels.
    fn fit(&self, text: &str, size: f32, max: i32) -> String {
        if self.width(text, size) <= max {
            return String::from(text);
        }
        let mut cut: String = String::from(text);
        while !cut.is_empty() {
            cut.pop();
            let candidate = format!("{}…", cut.trim_end());
            if self.width(&candidate, size) <= max {
                return candidate;
            }
        }
        String::new()
    }

    /// The home menu's glyph for a button, or `fallback` in brackets without the system fonts.
    fn button(&mut self, glyph: char, fallback: &str, size: f32, x: i32, y: i32, color: RGBA8) -> i32 {
        match self.fonts.filter(|f| f.buttons.glyph_id(glyph).0 != 0) {
            Some(fonts) => {
                let mut buf = [0; 4];
                self.canvas.draw_font_text(&fonts.buttons, glyph.encode_utf8(&mut buf), color, size, x, y, AlphaBlend::Source);
            }
            None => self.text(&format!("[{fallback}]"), size, x, y, color),
        }
        self.button_width(glyph, fallback, size)
    }

    fn button_width(&self, glyph: char, fallback: &str, size: f32) -> i32 {
        match self.fonts.filter(|f| f.buttons.glyph_id(glyph).0 != 0) {
            Some(fonts) => Self::measure(&fonts.buttons, glyph.encode_utf8(&mut [0; 4]), size),
            None => self.width(&format!("[{fallback}]"), size),
        }
    }

    /// A card: a rounded rectangle.
    fn card(&mut self, x: i32, y: i32, w: i32, h: i32) {
        const R: i32 = 14;
        self.canvas.draw_rect(x + R, y, (w - 2 * R) as u32, h as u32, CARD, AlphaBlend::None);
        self.canvas.draw_rect(x, y + R, R as u32, (h - 2 * R) as u32, CARD, AlphaBlend::None);
        self.canvas.draw_rect(x + w - R, y + R, R as u32, (h - 2 * R) as u32, CARD, AlphaBlend::None);
        for (cx, cy) in [(x + R, y + R), (x + w - R, y + R), (x + R, y + h - R), (x + w - R, y + h - R)] {
            self.canvas.draw_circle_filled(cx, cy, R as u32, CARD, AlphaBlend::None);
        }
    }

    /// A status dot with a soft halo.
    fn dot(&mut self, cx: i32, cy: i32, r: u32, color: RGBA8) {
        self.canvas.draw_circle_filled(cx, cy, r + 6, color.with_a(0x30), AlphaBlend::Source);
        self.canvas.draw_circle_filled(cx, cy, r, color, AlphaBlend::None);
    }

    /// A square straight-alpha RGBA image.
    fn image(&mut self, rgba: &[u8], size: i32, x: i32, y: i32) {
        for (i, px) in rgba.as_chunks::<4>().0.iter().enumerate() {
            if px[3] != 0 {
                let (dx, dy) = (i as i32 % size, i as i32 / size);
                self.canvas.draw_single(x + dx, y + dy, RGBA8::new_scaled(px[0], px[1], px[2], px[3]), AlphaBlend::Source);
            }
        }
    }
}
