# Bundled fonts

All fonts embedded in Pathfinder are licensed under the SIL Open Font License 1.1
(https://scripts.sil.org/OFL). They are shipped inside the binary so the UI never
depends on a font being installed on the user's system.

| File | Family | Source |
| --- | --- | --- |
| NotoSans-Regular.ttf | Noto Sans | https://github.com/notofonts/notofonts.github.io |
| NotoSansMono-Regular.ttf | Noto Sans Mono | https://github.com/notofonts/notofonts.github.io |
| JetBrainsMono-Variable.ttf | JetBrains Mono | https://github.com/JetBrains/JetBrainsMono |
| Inter-Regular.ttf | Inter | https://github.com/google/fonts/tree/main/ofl/inter |
| Lora-Regular.ttf | Lora | https://github.com/google/fonts/tree/main/ofl/lora |
| FiraCode-Regular.ttf | Fira Code | https://github.com/tonsky/FiraCode |
| PressStart2P-Regular.ttf | Press Start 2P | https://github.com/google/fonts/tree/main/ofl/pressstart2p |

Note: Press Start 2P is a pixel display font with limited glyph coverage. It is no
longer applied as a global UI/monospace font because the femtovg renderer has no
glyph fallback (see slint-ui/slint#3057), which blanks out unsupported glyphs and
can black out the window.
