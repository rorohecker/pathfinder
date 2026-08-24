//! UI language helpers for strings owned by Rust (sidebar, choices, commands).
//!
//! Slint `@tr()` covers markup strings. Anything pushed through `set_*` models
//! must go through [`t`] so it tracks `NativeSettings::ui_language`.

use std::sync::RwLock;

static CURRENT: RwLock<&'static str> = RwLock::new("en");

/// Set the active UI language code (`en`, `it`, `es`, …).
pub fn set_language(lang: &str) {
    let normalized = normalize(lang);
    if let Ok(mut guard) = CURRENT.write() {
        *guard = normalized;
    }
}

pub fn current() -> &'static str {
    CURRENT.read().map(|g| *g).unwrap_or("en")
}

pub fn normalize(lang: &str) -> &'static str {
    let l = lang.trim().to_ascii_lowercase();
    if l.is_empty() || l == "system" || l == "en" || l == "en-us" || l == "en_us" {
        return "en";
    }
    if l == "it" || l.starts_with("it-") || l.starts_with("it_") {
        return "it";
    }
    if l == "es" || l.starts_with("es-") || l.starts_with("es_") {
        return "es";
    }
    "en"
}

/// Resolve `system` / explicit codes to a concrete language folder name.
pub fn resolve_setting(setting: &str) -> &'static str {
    let s = setting.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("system") {
        return detect_system_language();
    }
    normalize(s)
}

fn detect_system_language() -> &'static str {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(val) = std::env::var(key) {
            let lower = val.to_ascii_lowercase();
            if lower.starts_with("it") {
                return "it";
            }
            if lower.starts_with("es") {
                return "es";
            }
            if lower.starts_with("en") {
                return "en";
            }
        }
    }
    "en"
}

/// Translate an English UI string for the active language.
pub fn t(english: &str) -> String {
    if english.is_empty() {
        return String::new();
    }
    match current() {
        "it" => it(english).unwrap_or_else(|| english.to_string()),
        "es" => es(english).unwrap_or_else(|| english.to_string()),
        _ => english.to_string(),
    }
}

fn it(en: &str) -> Option<String> {
    Some(
        match en {
            "Settings" => "Impostazioni",
            "Appearance" => "Aspetto",
            "View" => "Visualizzazione",
            "Performance" => "Prestazioni",
            "AI" => "IA",
            "Language" => "Lingua",
            "System" => "Sistema",
            "English" => "Inglese",
            "Italian" => "Italiano",
            "Spanish" => "Spagnolo",
            "Mica Dark" => "Mica scuro",
            "Cool graphite Fluent - frosted glass panels" => {
                "Fluent grafite - pannelli in vetro smerigliato"
            }
            "Mica Light" => "Mica chiaro",
            "Icy daylight - blue-tint chrome and white glass" => {
                "Luce fredda - chrome azzurro e vetro bianco"
            }
            "Warm Neutral" => "Neutro caldo",
            "Latte and oak - sepia UI for long sessions" => {
                "Latte e quercia - UI seppia per sessioni lunghe"
            }
            "Flat White" => "Bianco piatto",
            "Swiss studio - flat panels, sharp dividers" => {
                "Studio svizzero - pannelli piatti, divisori netti"
            }
            "Terminal" => "Terminale",
            "CRT green - phosphor glow, scanline grid, mono type" => {
                "Verde CRT - bagliore al fosforo, scanline, mono"
            }
            "Paper" => "Carta",
            "Ink and cotton - editorial serif warmth" => {
                "Inchiostro e cotone - calore tipografico editoriale"
            }
            "Retro Arcade" => "Arcade retrò",
            "Neon cab - purple void, gold marquee" => {
                "Cabina neon - vuoto viola, insegna dorata"
            }
            "Cyberpunk" => "Cyberpunk",
            "Synth district - magenta rail, cyan haze" => {
                "Quartiere synth - binario magenta, foschia ciano"
            }
            "High Fantasy" => "High Fantasy",
            "Moonlit archive - ink glass, aurora teal, arcane violet" => {
                "Archivio al chiaro di luna - vetro inchiostro, aurora teal, viola arcano"
            }
            "Sunset" => "Tramonto",
            "Dusk sky - aubergine dark, amber-rose glow" => {
                "Cielo al crepuscolo - melanzana, bagliore ambra-rosa"
            }
            "Blue" => "Blu",
            "Amber" => "Ambra",
            "Green" => "Verde",
            "Violet" => "Viola",
            "Rose" => "Rosa",
            "Teal" => "Verde acqua",
            "Copper" => "Rame",
            "Gold" => "Oro",
            "Indigo" => "Indaco",
            "Crimson" => "Cremisi",
            "Black" => "Nero",
            "White" => "Bianco",
            "Cozy" => "Comoda",
            "38px rows and larger icons" => "Righe da 38px e icone più grandi",
            "Comfortable" => "Confortevole",
            "Balanced row and grid sizing" => "Dimensioni griglia ed elenco bilanciate",
            "Compact" => "Compatta",
            "Dense rows and tight spacing" => "Righe dense e spaziatura stretta",
            "Low" => "Bassa",
            "Visited folders only, lowest storage" => {
                "Solo cartelle visitate, minimo spazio"
            }
            "Balanced" => "Bilanciata",
            "Desktop, Documents, Downloads, Pictures, and projects" => {
                "Desktop, Documenti, Download, Immagini e progetti"
            }
            "Fast" => "Veloce",
            "Selected roots, with common folders as fallback" => {
                "Radici selezionate, con cartelle comuni di riserva"
            }
            "Max" => "Massima",
            "All fixed drives, highest storage" => {
                "Tutte le unità fisse, massimo spazio"
            }
            "QUICK ACCESS" => "ACCESSO RAPIDO",
            "Home" => "Home",
            "Recycle Bin" => "Cestino",
            "Storage" => "Archiviazione",
            "PINNED" => "FISSATI",
            "BOOKMARKS" => "PREFERITI",
            "DRIVES" => "UNITÀ",
            "THIS PC" => "QUESTO PC",
            "Folder" => "Cartella",
            "Simple" => "Semplice",
            "Normal" => "Normale",
            "Grid" => "Griglia",
            "List" => "Elenco",
            "Gallery" => "Galleria",
            "Loading index statistics..." => "Caricamento statistiche indice...",
            "Measuring disk usage..." => "Misurazione uso disco...",
            _ => return None,
        }
        .to_string(),
    )
}

fn es(en: &str) -> Option<String> {
    Some(
        match en {
            "Settings" => "Configuración",
            "Appearance" => "Apariencia",
            "View" => "Vista",
            "Performance" => "Rendimiento",
            "AI" => "IA",
            "Language" => "Idioma",
            "System" => "Sistema",
            "English" => "Inglés",
            "Italian" => "Italiano",
            "Spanish" => "Español",
            "Mica Dark" => "Mica oscuro",
            "Cool graphite Fluent - frosted glass panels" => {
                "Fluent grafito - paneles de vidrio esmerilado"
            }
            "Mica Light" => "Mica claro",
            "Icy daylight - blue-tint chrome and white glass" => {
                "Luz fría - cromo azulado y vidrio blanco"
            }
            "Warm Neutral" => "Neutro cálido",
            "Latte and oak - sepia UI for long sessions" => {
                "Latte y roble - UI sepia para sesiones largas"
            }
            "Flat White" => "Blanco plano",
            "Swiss studio - flat panels, sharp dividers" => {
                "Estudio suizo - paneles planos, divisores nítidos"
            }
            "Terminal" => "Terminal",
            "CRT green - phosphor glow, scanline grid, mono type" => {
                "Verde CRT - brillo de fósforo, scanlines, mono"
            }
            "Paper" => "Papel",
            "Ink and cotton - editorial serif warmth" => {
                "Tinta y algodón - calidez tipográfica editorial"
            }
            "Retro Arcade" => "Arcade retro",
            "Neon cab - purple void, gold marquee" => {
                "Cabina neón - vacío púrpura, marquesina dorada"
            }
            "Cyberpunk" => "Cyberpunk",
            "Synth district - magenta rail, cyan haze" => {
                "Distrito synth - riel magenta, niebla cian"
            }
            "High Fantasy" => "High Fantasy",
            "Moonlit archive - ink glass, aurora teal, arcane violet" => {
                "Archivo bajo la luna - vidrio tinta, aurora teal, violeta arcano"
            }
            "Sunset" => "Atardecer",
            "Dusk sky - aubergine dark, amber-rose glow" => {
                "Cielo al crepúsculo - berenjena, brillo ámbar-rosa"
            }
            "Blue" => "Azul",
            "Amber" => "Ámbar",
            "Green" => "Verde",
            "Violet" => "Violeta",
            "Rose" => "Rosa",
            "Teal" => "Verde azulado",
            "Copper" => "Cobre",
            "Gold" => "Oro",
            "Indigo" => "Índigo",
            "Crimson" => "Carmesí",
            "Black" => "Negro",
            "White" => "Blanco",
            "Cozy" => "Cómodo",
            "38px rows and larger icons" => "Filas de 38px e iconos más grandes",
            "Comfortable" => "Confortable",
            "Balanced row and grid sizing" => "Tamaño equilibrado de filas y cuadrícula",
            "Compact" => "Compacto",
            "Dense rows and tight spacing" => "Filas densas y espaciado ajustado",
            "Low" => "Baja",
            "Visited folders only, lowest storage" => {
                "Solo carpetas visitadas, menor almacenamiento"
            }
            "Balanced" => "Equilibrado",
            "Desktop, Documents, Downloads, Pictures, and projects" => {
                "Escritorio, Documentos, Descargas, Imágenes y proyectos"
            }
            "Fast" => "Rápido",
            "Selected roots, with common folders as fallback" => {
                "Raíces seleccionadas, con carpetas comunes de reserva"
            }
            "Max" => "Máxima",
            "All fixed drives, highest storage" => {
                "Todas las unidades fijas, mayor almacenamiento"
            }
            "QUICK ACCESS" => "ACCESO RÁPIDO",
            "Home" => "Inicio",
            "Recycle Bin" => "Papelera",
            "Storage" => "Almacenamiento",
            "PINNED" => "FIJADOS",
            "BOOKMARKS" => "MARCADORES",
            "DRIVES" => "UNIDADES",
            "THIS PC" => "ESTE EQUIPO",
            "Folder" => "Carpeta",
            "Simple" => "Simple",
            "Normal" => "Normal",
            "Grid" => "Cuadrícula",
            "List" => "Lista",
            "Gallery" => "Galería",
            "Loading index statistics..." => "Cargando estadísticas del índice...",
            "Measuring disk usage..." => "Midiendo uso del disco...",
            _ => return None,
        }
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_variants() {
        assert_eq!(normalize("IT-it"), "it");
        assert_eq!(normalize("es_MX"), "es");
        assert_eq!(normalize("en-US"), "en");
    }
}
