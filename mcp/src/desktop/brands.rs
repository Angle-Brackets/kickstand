// SPDX-License-Identifier: GPL-3.0-only
//! The marks shown beside each AI maker's name (see `assets/brands/README.md`): white silhouettes,
//! tinted here in each brand's colour. Custom gets a dashed box with a plus, drawn in code.
//!
//! Each is rendered at the exact size it's drawn ([`SIZE`] points, at 1x and 2x) and placed on whole
//! pixels: a bigger image shrunk on the GPU comes out jagged, or soft with mipmaps.

use eframe::egui::emath::GuiRounding as _;
use eframe::egui::{self, Color32, Painter, Rect, Stroke, TextureHandle, pos2};

use super::theme::{self, ink};
use crate::link::Maker;

/// The marks' size, in points.
pub const SIZE: f32 = 20.0;

/// (1x, 2x) renders of each.
const CLAUDE: [&[u8]; 2] = [include_bytes!("../../../assets/brands/claude-20.png"), include_bytes!("../../../assets/brands/claude-40.png")];
const GEMINI: [&[u8]; 2] = [include_bytes!("../../../assets/brands/gemini-20.png"), include_bytes!("../../../assets/brands/gemini-40.png")];
const OPENAI: [&[u8]; 2] = [include_bytes!("../../../assets/brands/openai-20.png"), include_bytes!("../../../assets/brands/openai-40.png")];

/// Brand colours (Simple Icons' for Claude and Gemini; OpenAI's mark is shown in black).
const CLAUDE_ORANGE: Color32 = Color32::from_rgb(0xD9, 0x77, 0x57);
const GEMINI_PURPLE: Color32 = Color32::from_rgb(0x8E, 0x75, 0xB2);

/// A mark at 1x and 2x.
type Mark = [Option<TextureHandle>; 2];

pub struct Brands {
    claude: Mark,
    gemini: Mark,
    openai: Mark,
}

impl Brands {
    pub fn load(ctx: &egui::Context) -> Brands {
        let load_one = |name: String, png: &[u8]| {
            let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?.into_rgba8();
            let size = [image.width() as usize, image.height() as usize];
            let pixels = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
            // Mipmaps only matter on displays between 1x and 2x, where the 2x one is shrunk a little.
            let options = egui::TextureOptions::LINEAR.with_mipmap_mode(Some(egui::TextureFilter::Linear));
            Some(ctx.load_texture(name, pixels, options))
        };
        let load = |name: &str, pngs: [&[u8]; 2]| [load_one(format!("{name}-1x"), pngs[0]), load_one(format!("{name}-2x"), pngs[1])];
        Brands { claude: load("claude", CLAUDE), gemini: load("gemini", GEMINI), openai: load("openai", OPENAI) }
    }

    /// Draws `maker`'s mark in `rect` (a square, [`SIZE`] points across).
    pub fn paint(&self, painter: &Painter, rect: Rect, maker: Maker) {
        let ppp = painter.pixels_per_point();
        let rect = rect.round_to_pixels(ppp);
        let (mark, color) = match maker {
            Maker::Claude => (&self.claude, CLAUDE_ORANGE),
            Maker::Gemini => (&self.gemini, GEMINI_PURPLE),
            Maker::OpenAi => (&self.openai, ink()),
            Maker::Custom => {
                let rect = rect.shrink(1.0);
                theme::dashed_rect(painter, rect, Stroke::new(1.5, ink()));
                let (c, arm) = (rect.center(), rect.width() * 0.22);
                let stroke = Stroke::new(1.8, ink());
                painter.line_segment([pos2(c.x - arm, c.y), pos2(c.x + arm, c.y)], stroke);
                painter.line_segment([pos2(c.x, c.y - arm), pos2(c.x, c.y + arm)], stroke);
                return;
            }
        };
        if let Some(texture) = &mark[usize::from(ppp > 1.0)] {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            painter.image(texture.id(), rect, uv, color);
        }
    }
}
