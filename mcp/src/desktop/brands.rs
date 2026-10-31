// SPDX-License-Identifier: GPL-3.0-only
//! The marks shown beside each AI maker's name (see `assets/brands/README.md`): white silhouettes,
//! tinted here in each brand's colour. Custom gets a dashed box with a plus, drawn in code.

use eframe::egui::{self, Color32, Painter, Rect, Stroke, TextureHandle, pos2};

use super::theme::{self, INK};
use crate::link::Maker;

const CLAUDE: &[u8] = include_bytes!("../../../assets/brands/claude.png");
const GEMINI: &[u8] = include_bytes!("../../../assets/brands/gemini.png");
const OPENAI: &[u8] = include_bytes!("../../../assets/brands/openai.png");

/// Brand colours (Simple Icons' for Claude and Gemini; OpenAI's mark is shown in black).
const CLAUDE_ORANGE: Color32 = Color32::from_rgb(0xD9, 0x77, 0x57);
const GEMINI_PURPLE: Color32 = Color32::from_rgb(0x8E, 0x75, 0xB2);

pub struct Brands {
    claude: Option<TextureHandle>,
    gemini: Option<TextureHandle>,
    openai: Option<TextureHandle>,
}

impl Brands {
    pub fn load(ctx: &egui::Context) -> Brands {
        let load = |name: &str, png: &[u8]| {
            let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?.into_rgba8();
            let size = [image.width() as usize, image.height() as usize];
            let pixels = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
            Some(ctx.load_texture(name, pixels, egui::TextureOptions::LINEAR))
        };
        Brands { claude: load("claude", CLAUDE), gemini: load("gemini", GEMINI), openai: load("openai", OPENAI) }
    }

    /// Draws `maker`'s mark filling `rect` (a square).
    pub fn paint(&self, painter: &Painter, rect: Rect, maker: Maker) {
        let (texture, color) = match maker {
            Maker::Claude => (&self.claude, CLAUDE_ORANGE),
            Maker::Gemini => (&self.gemini, GEMINI_PURPLE),
            Maker::OpenAi => (&self.openai, INK),
            Maker::Custom => {
                let rect = rect.shrink(1.0);
                theme::dashed_rect(painter, rect, Stroke::new(1.5, INK));
                let (c, arm) = (rect.center(), rect.width() * 0.22);
                let stroke = Stroke::new(1.8, INK);
                painter.line_segment([pos2(c.x - arm, c.y), pos2(c.x + arm, c.y)], stroke);
                painter.line_segment([pos2(c.x, c.y - arm), pos2(c.x, c.y + arm)], stroke);
                return;
            }
        };
        if let Some(texture) = texture {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            painter.image(texture.id(), rect, uv, color);
        }
    }
}
