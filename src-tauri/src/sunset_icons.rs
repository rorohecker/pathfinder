//! Sunset pixel-icon banks (12×12 ASCII maps from sunset-explorer-icons).
//! Converted once at startup into Slint `FantasyPixel` models for `SunsetIconBank`.

use slint::{Color, ComponentHandle, ModelRc, VecModel};
use std::collections::HashMap;

use crate::{FantasyPixel, MainWindow, SunsetIconBank};

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

/// Load ASCII pixel maps into the Slint `SunsetIconBank` global (once at startup).
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
}
