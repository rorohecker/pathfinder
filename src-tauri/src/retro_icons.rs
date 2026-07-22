//! Retro Arcade pixel-icon banks (12×12 ASCII maps from retro-explorer-icons).
//! Converted once at startup into Slint `FantasyPixel` models for `RetroIconBank`.

use slint::{Color, ComponentHandle, ModelRc, VecModel};
use std::collections::HashMap;

use crate::{FantasyPixel, MainWindow, RetroIconBank};

const FLOPPY: [&str; 12] = [
    "BBBBBBBBBBBB",
    "BccccccccccB",
    "BccccccccccB",
    "BBBBBBBBBBBB",
    "BwwwwwwwwwwB",
    "BwkkkkkkkkwB",
    "BwkkkkkkkkwB",
    "BwwwwwwwwwwB",
    "BbbbbbbbbbbB",
    "BbbbbbbbbbbB",
    "BbbbbbbbbbbB",
    "BBBBBBBBBBBB",
];

const CASSETTE_A: [&str; 12] = [
    "............",
    "..BBBBBBBB..",
    ".BwwwwwwwwB.",
    ".BwccwwccwB.",
    ".BwccwwccwB.",
    ".BwwwwwwwwB.",
    ".BbbbbbbbbB.",
    ".BbkkkkkkbB.",
    ".BbkkkkkkbB.",
    ".BbbbbbbbbB.",
    "..BBBBBBBB..",
    "............",
];

const CASSETTE_B: [&str; 12] = [
    "............",
    "..BBBBBBBB..",
    ".BwwwwwwwwB.",
    ".BwcCwwCcwB.",
    ".BwCcwwcCwB.",
    ".BwwwwwwwwB.",
    ".BbbbbbbbbB.",
    ".BbkkkkkkbB.",
    ".BbkkkkkkbB.",
    ".BbbbbbbbbB.",
    "..BBBBBBBB..",
    "............",
];

const TV_A: [&str; 12] = [
    "............",
    "..BBBBBBBB..",
    ".BwwwwwwwwB.",
    ".BwCCCCCCwB.",
    ".BwCkkkkCwB.",
    ".BwCkkkkCwB.",
    ".BwCCCCCCwB.",
    ".BwwwwwwwwB.",
    ".BBBBBBBBBB.",
    ".Bss....ssB.",
    "..BBBBBBBB..",
    "............",
];

const TV_B: [&str; 12] = [
    "............",
    "..BBBBBBBB..",
    ".BwwwwwwwwB.",
    ".BwCCCCCCwB.",
    ".BwCkCkkCwB.",
    ".BwCkkCkCwB.",
    ".BwCCCCCCwB.",
    ".BwwwwwwwwB.",
    ".BBBBBBBBBB.",
    ".Bss....ssB.",
    "..BBBBBBBB..",
    "............",
];

const JOYSTICK: [&str; 12] = [
    "....BBBB....",
    "....BrrB....",
    "....BrrB....",
    "....BBBB....",
    ".....gg.....",
    ".....gg.....",
    ".....gg.....",
    "...BBBBBB...",
    "..BbbbbbbB..",
    "..BkkkkkkB..",
    "..BbbbbbbB..",
    "..BBBBBBBB..",
];

const BOOMBOX: [&str; 12] = [
    ".BBBBBBBBBB.",
    "Bss......ssB",
    "Bss......ssB",
    "BBBBBBBBBBBB",
    "BCCCkkkkCCCB",
    "BCkCkkkkCkCB",
    "BCCCkkkkCCCB",
    "BBBBBBBBBBBB",
    "BbbbbbbbbbbB",
    "BbbbbbbbbbbB",
    "BBBBBBBBBBBB",
    "............",
];

const TRASH: [&str; 12] = [
    "...BBBBBB...",
    "..BrrrrrrB..",
    ".BBBBBBBBBB.",
    ".BkkkkkkkkB.",
    ".BkmkkkkmkB.",
    ".BkmkkkkmkB.",
    ".BkmkkkkmkB.",
    ".BkmkkkkmkB.",
    ".BkkkkkkkkB.",
    "..BBBBBBBB..",
    "............",
    "............",
];

fn palette() -> HashMap<char, Color> {
    let mut m = HashMap::new();
    m.insert('B', Color::from_rgb_u8(22, 16, 31));
    m.insert('c', Color::from_rgb_u8(53, 232, 255));
    m.insert('w', Color::from_rgb_u8(234, 230, 218));
    m.insert('k', Color::from_rgb_u8(28, 21, 38));
    m.insert('b', Color::from_rgb_u8(91, 47, 134));
    m.insert('r', Color::from_rgb_u8(255, 59, 107));
    m.insert('g', Color::from_rgb_u8(57, 255, 136));
    m.insert('s', Color::from_rgb_u8(185, 182, 201));
    m.insert('C', Color::from_rgb_u8(56, 242, 255));
    m.insert('m', Color::from_rgb_u8(74, 69, 96));
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

/// Load ASCII pixel maps into the Slint `RetroIconBank` global (once at startup).
pub fn load_retro_icon_bank(ui: &MainWindow) {
    let pal = palette();
    let bank = ui.global::<RetroIconBank>();
    bank.set_floppy(pixels_from_map(&FLOPPY, &pal));
    bank.set_cassette_a(pixels_from_map(&CASSETTE_A, &pal));
    bank.set_cassette_b(pixels_from_map(&CASSETTE_B, &pal));
    bank.set_tv_a(pixels_from_map(&TV_A, &pal));
    bank.set_tv_b(pixels_from_map(&TV_B, &pal));
    bank.set_joystick(pixels_from_map(&JOYSTICK, &pal));
    bank.set_boombox(pixels_from_map(&BOOMBOX, &pal));
    bank.set_trash(pixels_from_map(&TRASH, &pal));
}
