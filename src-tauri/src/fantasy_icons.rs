//! High Fantasy pixel-icon banks (12×12 ASCII maps from fantasy-explorer-icons).
//! Converted once at startup into Slint `FantasyPixel` models for `FantasyIconBank`.

use slint::{Color, ComponentHandle, ModelRc, VecModel};
use std::collections::HashMap;

use crate::{FantasyIconBank, FantasyPixel, MainWindow};

const CHEST: [&str; 12] = [
    "............",
    "..BBBBBBBB..",
    ".BggggggggB.",
    ".BgGGGGGGgB.",
    ".BgGwwwwGgB.",
    ".BgGwGGwGgB.",
    ".BBBBBBBBBB.",
    ".BooooooooB.",
    ".BobbbbbboB.",
    ".BobbbbbboB.",
    ".BooooooooB.",
    "..BBBBBBBB..",
];

const SCROLL: [&str; 12] = [
    "..wwwwwwww..",
    ".wWWWWWWWWw.",
    "wWWWWWWWWWWw",
    "wW........Ww",
    "wW.kkkkkk.Ww",
    "wW.k....k.Ww",
    "wW.k....k.Ww",
    "wW.kkkkkk.Ww",
    "wW........Ww",
    "wWWWWWWWWWWw",
    ".wWWWWWWWWw.",
    "..wwwwwwww..",
];

const GEM_A: [&str; 12] = [
    ".....cc.....",
    "....cCCc....",
    "...cCCCCc...",
    "..cCCCCCCc..",
    ".cCCCCCCCCc.",
    "cCCCCCCCCCCc",
    "cCCCCCCCCCCc",
    ".cCCCCCCCCc.",
    "..cCCCCCCc..",
    "...cCCCCc...",
    "....cCCc....",
    ".....cc.....",
];

const GEM_B: [&str; 12] = [
    ".....cc.....",
    "....cHCc....",
    "...cCCCCc...",
    "..cCCCCCCc..",
    ".cCCCCCCCCc.",
    "cCCCCCCCCCCc",
    "cCCCCCCCCCCc",
    ".cCCCCCCCCc.",
    "..cCCCCCCc..",
    "...cCCCCc...",
    "....cCCc....",
    ".....cc.....",
];

const POTION: [&str; 12] = [
    "....BB......",
    "....bb......",
    "....bb......",
    "...bbbb.....",
    "..bggggb....",
    ".bggGGggb...",
    ".bgGrrGgb...",
    ".bgGrrGgb...",
    ".bggGGggb...",
    ".bgggggb....",
    "..bbbbbb....",
    "...bbbb.....",
];

const TOWER_A: [&str; 12] = [
    "....ss......",
    "...ssss.....",
    "..ssssss....",
    "..sBBBBs....",
    "..sBddBs....",
    "..sBddBs....",
    "..sBBBBs....",
    "..sBddBs....",
    "..sBBBBs....",
    ".ssssssss...",
    ".ssssssss...",
    ".ssssssss...",
];

const TOWER_B: [&str; 12] = [
    "....ss......",
    "...ssss.....",
    "..ssssss....",
    "..sBBBBs....",
    "..sBggBs....",
    "..sBggBs....",
    "..sBBBBs....",
    "..sBggBs....",
    "..sBBBBs....",
    ".ssssssss...",
    ".ssssssss...",
    ".ssssssss...",
];

const CAULDRON: [&str; 12] = [
    "...B....B...",
    "....B..B....",
    "..BBBBBBBB..",
    ".BkkkkkkkkB.",
    "BknnnGnnnkB.",
    "BknGGGGGnkB.",
    "BknnnGnnnkB.",
    ".BkkkkkkkkB.",
    "..BBBBBBBB..",
    "...BB..BB...",
    "............",
    "............",
];

fn palette() -> HashMap<char, Color> {
    let mut m = HashMap::new();
    m.insert('B', Color::from_rgb_u8(43, 23, 16));
    m.insert('b', Color::from_rgb_u8(107, 63, 34));
    m.insert('o', Color::from_rgb_u8(169, 116, 76));
    m.insert('g', Color::from_rgb_u8(224, 178, 60));
    m.insert('G', Color::from_rgb_u8(166, 127, 34));
    m.insert('w', Color::from_rgb_u8(232, 220, 181));
    m.insert('W', Color::from_rgb_u8(242, 232, 200));
    m.insert('k', Color::from_rgb_u8(20, 20, 20));
    m.insert('c', Color::from_rgb_u8(31, 111, 130));
    m.insert('C', Color::from_rgb_u8(79, 198, 224));
    m.insert('H', Color::from_rgb_u8(230, 255, 255));
    m.insert('r', Color::from_rgb_u8(178, 60, 60));
    m.insert('s', Color::from_rgb_u8(154, 160, 166));
    m.insert('d', Color::from_rgb_u8(36, 26, 16));
    m.insert('n', Color::from_rgb_u8(60, 122, 60));
    m.insert('N', Color::from_rgb_u8(95, 174, 95));
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

/// Load ASCII pixel maps into the Slint `FantasyIconBank` global (once at startup).
pub fn load_fantasy_icon_bank(ui: &MainWindow) {
    let pal = palette();
    let bank = ui.global::<FantasyIconBank>();
    bank.set_chest(pixels_from_map(&CHEST, &pal));
    bank.set_scroll(pixels_from_map(&SCROLL, &pal));
    bank.set_gem_a(pixels_from_map(&GEM_A, &pal));
    bank.set_gem_b(pixels_from_map(&GEM_B, &pal));
    bank.set_potion(pixels_from_map(&POTION, &pal));
    bank.set_tower_a(pixels_from_map(&TOWER_A, &pal));
    bank.set_tower_b(pixels_from_map(&TOWER_B, &pal));
    bank.set_cauldron(pixels_from_map(&CAULDRON, &pal));
}
