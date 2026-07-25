//! Sunset pixel-icon banks (12×12 ASCII maps from sunset-explorer-icons).
//! Converted once at startup into:
//!   - Slint `FantasyPixel` models for sparse animated glyphs (settings / empty /
//!     welcome / sidebar) — NEVER mounted per file-row
//!   - A baked RGBA `Image` for folder rows (one shared texture, cheap to paint)

use slint::{Color, ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use std::collections::HashMap;

use crate::{FantasyPixel, MainWindow, SunsetIconBank, ThemePalette};

const FOLDER: [&str; 12] = [
    "..BBBB......",
    ".BooB.......",
    "BBBBBBBBBBBB",
    "BooooooooooB",
    "BOOOOOOOOOOB",
    "BppppppppppB",
    "BppppppppppB",
    "BvvvvvvvvvvB",
    "BvvvvvvvvvvB",
    "BBBBBBBBBBBB",
    "............",
    "............",
];

const POSTCARD: [&str; 12] = [
    "BBBBBBBBBBBB",
    "BwwwwwwwqqwB",
    "BwwwwwwwqqwB",
    "BwwwwwwwwwwB",
    "BwwwwwwwwwwB",
    "BwwwwOOwwwwB",
    "BwwwOppOwwwB",
    "BwwOppppOwwB",
    "BwwwwwwwwwwB",
    "BwwwwwwwwwwB",
    "BBBBBBBBBBBB",
    "............",
];

const SUN_A: [&str; 12] = [
    ".....oo.....",
    "....oYYo....",
    "...oYYYYo...",
    "..oYYYYYYo..",
    ".oYYYYYYYYo.",
    "oYYYYYYYYYYo",
    "oYYYYYYYYYYo",
    ".oYYYYYYYYo.",
    "..oYYYYYYo..",
    "...oYYYYo...",
    "....oYYo....",
    ".....oo.....",
];

const SUN_B: [&str; 12] = [
    ".....OO.....",
    "....OYYO....",
    "...OYYYYO...",
    "..OYYYYYYO..",
    ".OYYYYYYYYO.",
    "OYYYYYYYYYYO",
    "OYYYYYYYYYYO",
    ".OYYYYYYYYO.",
    "..OYYYYYYO..",
    "...OYYYYO...",
    "....OYYO....",
    ".....OO.....",
];

const SURFBOARD: [&str; 12] = [
    "....wwww....",
    "...wwOOww...",
    "..wwwOOwww..",
    "..wwwOOwww..",
    "..wwwOOwww..",
    "..wwwOOwww..",
    "..wwwOOwww..",
    "..wwwOOwww..",
    "..wwwOOwww..",
    "...wwOOww...",
    "....wwww....",
    "............",
];

const PALM_A: [&str; 12] = [
    "..g....g....",
    ".Gg..gG.....",
    ".....Tt.....",
    ".....Tt.....",
    ".....tT.....",
    ".....Tt.....",
    ".....tT.....",
    ".....Tt.....",
    "....TTtt....",
    "....OOOO....",
    "............",
    "............",
];

const PALM_B: [&str; 12] = [
    "....g....g..",
    ".....Gg..gG.",
    ".....Tt.....",
    ".....Tt.....",
    ".....tT.....",
    ".....Tt.....",
    ".....tT.....",
    ".....Tt.....",
    "....TTtt....",
    "....OOOO....",
    "............",
    "............",
];

const WAVEBIN: [&str; 12] = [
    "..qqqqqqqq..",
    ".qwqwqwqwqw.",
    ".BBBBBBBBBB.",
    ".BwwwwwwwwB.",
    ".BwqwwwwqwB.",
    ".BwqwwwwqwB.",
    ".BwqwwwwqwB.",
    ".BwqwwwwqwB.",
    ".BwwwwwwwwB.",
    "..BBBBBBBB..",
    "............",
    "............",
];

fn palette() -> HashMap<char, Color> {
    let mut m = HashMap::new();
    m.insert('B', Color::from_rgb_u8(42, 26, 26));
    m.insert('o', Color::from_rgb_u8(244, 161, 61));
    m.insert('O', Color::from_rgb_u8(242, 112, 60));
    m.insert('p', Color::from_rgb_u8(242, 85, 125));
    m.insert('v', Color::from_rgb_u8(122, 79, 174));
    m.insert('w', Color::from_rgb_u8(247, 236, 216));
    m.insert('q', Color::from_rgb_u8(47, 157, 174));
    m.insert('Y', Color::from_rgb_u8(255, 224, 102));
    m.insert('g', Color::from_rgb_u8(47, 143, 91));
    m.insert('G', Color::from_rgb_u8(31, 107, 65));
    m.insert('t', Color::from_rgb_u8(138, 90, 52));
    m.insert('T', Color::from_rgb_u8(94, 60, 31));
    m
}

fn pixels_from_map(rows: &[&str], pal: &HashMap<char, Color>) -> ModelRc<FantasyPixel> {
    let mut out = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        for (col_index, ch) in row.chars().enumerate() {
            if let Some(color) = pal.get(&ch) {
                out.push(FantasyPixel {
                    gx: col_index as i32,
                    gy: row_index as i32,
                    color: *color,
                });
            }
        }
    }
    ModelRc::new(VecModel::from(out))
}

/// Nearest-neighbor bake of a 12×12 ASCII map → one shared GPU texture.
/// List/grid folder rows paint this image instead of hundreds of pixel Rectangles.
fn image_from_map(rows: &[&str], pal: &HashMap<char, Color>, scale: u32) -> Image {
    let scale = scale.max(1);
    let w = 12 * scale;
    let h = 12 * scale;
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for (row_index, row) in rows.iter().enumerate() {
        for (col_index, ch) in row.chars().enumerate() {
            let Some(color) = pal.get(&ch) else {
                continue;
            };
            let r = color.red();
            let g = color.green();
            let b = color.blue();
            let a = color.alpha();
            for dy in 0..scale {
                for dx in 0..scale {
                    let x = col_index as u32 * scale + dx;
                    let y = row_index as u32 * scale + dy;
                    let i = ((y * w + x) * 4) as usize;
                    rgba[i] = r;
                    rgba[i + 1] = g;
                    rgba[i + 2] = b;
                    rgba[i + 3] = a;
                }
            }
        }
    }
    let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, w, h);
    Image::from_rgba8(buf)
}

/// Load ASCII pixel maps into the Slint `SunsetIconBank` global (once at startup)
/// and bake the folder glyph for cheap per-row display.
pub fn load_sunset_icon_bank(ui: &MainWindow) {
    let pal = palette();
    let bank = ui.global::<SunsetIconBank>();
    bank.set_folder(pixels_from_map(&FOLDER, &pal));
    bank.set_postcard(pixels_from_map(&POSTCARD, &pal));
    bank.set_sun_a(pixels_from_map(&SUN_A, &pal));
    bank.set_sun_b(pixels_from_map(&SUN_B, &pal));
    bank.set_surfboard(pixels_from_map(&SURFBOARD, &pal));
    bank.set_palm_a(pixels_from_map(&PALM_A, &pal));
    bank.set_palm_b(pixels_from_map(&PALM_B, &pal));
    bank.set_wavebin(pixels_from_map(&WAVEBIN, &pal));

    // 6× scale → 72² texture; crisp at list + grid sizes without per-pixel rects.
    ui.global::<ThemePalette>()
        .set_sunset_folder_image(image_from_map(&FOLDER, &pal, 6));
}
