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
    #[cfg(windows)]
    {
        if let Some(lang) = detect_windows_ui_language() {
            return lang;
        }
    }
    detect_from_env_lang()
}

#[cfg(windows)]
fn detect_windows_ui_language() -> Option<&'static str> {
    use windows::Win32::Globalization::GetUserDefaultUILanguage;
    // LANGID primary language: Italian 0x10, Spanish 0x0A, English 0x09.
    let primary = unsafe { GetUserDefaultUILanguage() } & 0x3FF;
    match primary {
        0x0010 => Some("it"),
        0x000A => Some("es"),
        0x0009 => Some("en"),
        _ => None,
    }
}

fn detect_from_env_lang() -> &'static str {
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
                        "AI: Suggested tags for selection" => "IA: tag suggeriti per la selezione",
            "Archive Browser" => "Browser archivi",
            "Batch Note Selection" => "Note batch sulla selezione",
            "Batch Rename" => "Rinomina in batch",
            "Batch Tag Selection" => "Tag batch sulla selezione",
            "Breadcrumb Siblings" => "Fratelli breadcrumb",
            "Cancel Queued Operations" => "Annulla operazioni in coda",
            "Check for Updates" => "Controlla aggiornamenti",
            "Checksum" => "Checksum",
            "Clear Local Caches" => "Svuota cache locali",
            "Clear Thumbnail Cache" => "Svuota cache miniature",
            "Close Tab" => "Chiudi scheda",
            "Cloud State" => "Stato cloud",
            "Compare Folder" => "Confronta cartella",
            "Compare Two Files" => "Confronta due file",
            "Copy" => "Copia",
            "Copy As Path" => "Copia come percorso",
            "Copy As PowerShell Path" => "Copia come percorso PowerShell",
            "Copy As URI" => "Copia come URI",
            "Create 7z Archive" => "Crea archivio 7z",
            "Create ZIP Archive" => "Crea archivio ZIP",
            "Create tar.gz Archive" => "Crea archivio tar.gz",
            "Cut" => "Taglia",
            "Delete" => "Elimina",
            "Details View" => "Vista dettagli",
            "Empty Recycle Bin" => "Svuota Cestino",
            "Empty" => "Svuota",
            "Extract Here" => "Estrai qui",
            "File Note" => "Nota file",
            "Files" => "File",
            "Find Duplicates" => "Trova duplicati",
            "Find Duplicates on Drive" => "Trova duplicati sull'unità",
            "Find duplicate images" => "Trova immagini duplicate",
            "Gallery View" => "Vista galleria",
            "Home Page" => "Pagina Home",
            "Icon View" => "Vista icone",
            "Image Tools" => "Strumenti immagine",
            "Libraries" => "Librerie",
            "Locked File Inspector" => "Ispeziona file bloccati",
            "Native Properties" => "Proprietà native",
            "Navigation" => "Navigazione",
            "New File" => "Nuovo file",
            "New Folder" => "Nuova cartella",
            "New From Template" => "Nuovo da modello",
            "New Tab" => "Nuova scheda",
            "New Window" => "Nuova finestra",
            "Open Settings" => "Apri impostazioni",
            "Open With" => "Apri con",
            "Open Workspace" => "Apri area di lavoro",
            "Open in Terminal" => "Apri nel terminale",
            "Operation Log" => "Registro operazioni",
            "Operation Queue" => "Coda operazioni",
            "Paste" => "Incolla",
            "Pause Operation Queue" => "Metti in pausa la coda",
            "Performance Debug Panel" => "Pannello debug prestazioni",
            "Permanently Delete" => "Elimina definitivamente",
            "Pin to Start" => "Fissa su Start",
            "Pin to Taskbar" => "Fissa sulla barra delle applicazioni",
            "Power Rename Presets" => "Preimpostazioni rinomina",
            "Previous Versions" => "Versioni precedenti",
            "Privacy and Storage" => "Privacy e archiviazione",
            "Rebuild Search Index" => "Ricostruisci indice di ricerca",
            "Recent" => "Recenti",
            "Recent Locations" => "Posizioni recenti",
            "Redo Last Operation" => "Ripeti ultima operazione",
            "Refresh" => "Aggiorna",
            "Rename" => "Rinomina",
            "Restore All from Recycle Bin" => "Ripristina tutto dal Cestino",
            "Restore from Recycle Bin" => "Ripristina dal Cestino",
            "Resume Operation Queue" => "Riprendi la coda",
            "Rules and Automation" => "Regole e automazione",
            "Run as Administrator" => "Esegui come amministratore",
            "Save Workspace" => "Salva area di lavoro",
            "Scan with Microsoft Defender" => "Analizza con Microsoft Defender",
            "Search Performance Settings" => "Impostazioni prestazioni ricerca",
            "Select All" => "Seleziona tutto",
            "Shell Verb Bridge" => "Ponte verbi shell",
            "Shortcut Editor" => "Editor scorciatoie",
            "Show More Options" => "Mostra altre opzioni",
            "Smart Folders" => "Cartelle smart",
            "Storage Treemap" => "Mappa archiviazione",
            "Take Ownership" => "Prendi proprietà",
            "Toggle Dual Pane" => "Attiva/disattiva doppio riquadro",
            "Toggle Preview" => "Attiva/disattiva anteprima",
            "Tools" => "Strumenti",
            "Undo Last Operation" => "Annulla ultima operazione",
                        "Pathfinder keeps a small local database of the files in folders you visit so the search bar can return matches instantly without rescanning your disk every time. Indexing modes change how aggressively that database is grown in the background:\n• Low - only the folders you actually open get added. Lightest on disk and CPU.\n• Balanced - also indexes Desktop, Documents, Downloads, Pictures, and common project folders.\n• Fast - indexes roots you choose, with common folders as fallback.\n• Max - all fixed drives. Best search coverage, uses the most disk while it catches up.\nThumbnails are stored separately and the cache is automatically pruned when it hits the budget below." => "Pathfinder mantiene un piccolo database locale dei file nelle cartelle che visiti, così la barra di ricerca trova subito le corrispondenze senza riscandire il disco ogni volta. Le modalità di indicizzazione cambiano quanto aggressivamente cresce quel database in background:\n• Bassa - solo le cartelle che apri davvero. Minimo impatto su disco e CPU.\n• Bilanciata - indicizza anche Desktop, Documenti, Download, Immagini e cartelle progetto comuni.\n• Veloce - indicizza le radici che scegli, con cartelle comuni di riserva.\n• Massima - tutte le unità fisse. Massima copertura, usa più disco mentre recupera.\nLe miniature sono separate e la cache viene ridotta automaticamente al budget sotto.",
            "Compact — smallest (~50 MB)" => "Compatta — più piccola (~50 MB)",
            "Quality — best accuracy (~99 MB)" => "Qualità — massima accuratezza (~99 MB)",
            "Balanced (recommended, ~61 MB)" => "Bilanciata (consigliata, ~61 MB)",
            "Ready — search by meaning across indexed files" => "Pronto — cerca per significato tra i file indicizzati",
            "Ready — auto-suggest tags for photos" => "Pronto — suggerisce tag per le foto",
            "Ready — near-duplicate detection uses a lightweight image hash (no model required)" => "Pronto — i quasi-duplicati usano un hash immagine leggero (nessun modello richiesto)",
            "Models up to date" => "Modelli aggiornati",
            "Delete forever" => "Elimina per sempre",
            "Move to Recycle Bin" => "Sposta nel Cestino",
            "Move to Bin" => "Sposta nel Cestino",
            "Share" => "Condividi",
            "Compact View" => "Vista compatta",
            "Open a library folder" => "Apri una cartella libreria",
            "Click a folder to open it" => "Clicca una cartella per aprirla",
            "No recent locations recorded yet." => "Nessuna posizione recente.",
            "No sibling folders available." => "Nessuna cartella gemella disponibile.",
            "Click a version, then Restore" => "Scegli una versione, poi Ripristina",
            "No shadow copies found. Enable File History or VSS snapshots." => {
                "Nessuna copia shadow. Attiva Cronologia file o gli snapshot VSS."
            }
            "OneDrive and cloud placeholders. Open in Explorer for pin controls." => {
                "Segnaposto OneDrive e cloud. Apri in Explorer per i controlli di pinning."
            }
            "Thumbnails, previews, and folder cache" => "Miniature, anteprime e cache cartelle",
            "Does not delete your files" => "Non elimina i tuoi file",
            "Click a row and press a key combination. Save to apply." => {
                "Clicca una riga e premi una combinazione. Salva per applicare."
            }
            "Click a version to restore it." => "Clicca una versione per ripristinarla.",
            "Keyboard shortcuts" => "Scorciatoie da tastiera",
            "Click a row, press a chord, then Save." => {
                "Clicca una riga, premi un accordo, poi Salva."
            }
            "Documents" => "Documenti",
            "Pictures" => "Immagini",
            "Music" => "Musica",
            "Videos" => "Video",
            "Downloads" => "Download",
            "Desktop" => "Desktop",
            "Back" => "Indietro",
            "Forward" => "Avanti",
            "Up One Level" => "Cartella superiore",
            "Focus Address Bar" => "Focus sulla barra indirizzi",
            "Focus Search" => "Focus sulla ricerca",
            "Command Palette" => "Tavolozza comandi",
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
                        "AI: Suggested tags for selection" => "IA: etiquetas sugeridas para la selección",
            "Archive Browser" => "Explorador de archivos",
            "Batch Note Selection" => "Notas por lotes en la selección",
            "Batch Rename" => "Cambio de nombre por lotes",
            "Batch Tag Selection" => "Etiquetar selección por lotes",
            "Breadcrumb Siblings" => "Hermanos de ruta",
            "Cancel Queued Operations" => "Cancelar operaciones en cola",
            "Check for Updates" => "Buscar actualizaciones",
            "Checksum" => "Checksum",
            "Clear Local Caches" => "Vaciar cachés locales",
            "Clear Thumbnail Cache" => "Vaciar caché de miniaturas",
            "Close Tab" => "Cerrar pestaña",
            "Cloud State" => "Estado en la nube",
            "Compare Folder" => "Comparar carpeta",
            "Compare Two Files" => "Comparar dos archivos",
            "Copy" => "Copiar",
            "Copy As Path" => "Copiar como ruta",
            "Copy As PowerShell Path" => "Copiar como ruta de PowerShell",
            "Copy As URI" => "Copiar como URI",
            "Create 7z Archive" => "Crear archivo 7z",
            "Create ZIP Archive" => "Crear archivo ZIP",
            "Create tar.gz Archive" => "Crear archivo tar.gz",
            "Cut" => "Cortar",
            "Delete" => "Eliminar",
            "Details View" => "Vista de detalles",
            "Empty Recycle Bin" => "Vaciar Papelera",
            "Empty" => "Vaciar",
            "Extract Here" => "Extraer aquí",
            "File Note" => "Nota de archivo",
            "Files" => "Archivos",
            "Find Duplicates" => "Buscar duplicados",
            "Find Duplicates on Drive" => "Buscar duplicados en la unidad",
            "Find duplicate images" => "Buscar imágenes duplicadas",
            "Gallery View" => "Vista de galería",
            "Home Page" => "Página de inicio",
            "Icon View" => "Vista de iconos",
            "Image Tools" => "Herramientas de imagen",
            "Libraries" => "Bibliotecas",
            "Locked File Inspector" => "Inspector de archivos bloqueados",
            "Native Properties" => "Propiedades nativas",
            "Navigation" => "Navegación",
            "New File" => "Nuevo archivo",
            "New Folder" => "Nueva carpeta",
            "New From Template" => "Nuevo desde plantilla",
            "New Tab" => "Nueva pestaña",
            "New Window" => "Nueva ventana",
            "Open Settings" => "Abrir configuración",
            "Open With" => "Abrir con",
            "Open Workspace" => "Abrir espacio de trabajo",
            "Open in Terminal" => "Abrir en terminal",
            "Operation Log" => "Registro de operaciones",
            "Operation Queue" => "Cola de operaciones",
            "Paste" => "Pegar",
            "Pause Operation Queue" => "Pausar cola",
            "Performance Debug Panel" => "Panel de depuración de rendimiento",
            "Permanently Delete" => "Eliminar permanentemente",
            "Pin to Start" => "Fijar a Inicio",
            "Pin to Taskbar" => "Fijar a la barra de tareas",
            "Power Rename Presets" => "Preajustes de cambio de nombre",
            "Previous Versions" => "Versiones anteriores",
            "Privacy and Storage" => "Privacidad y almacenamiento",
            "Rebuild Search Index" => "Reconstruir índice de búsqueda",
            "Recent" => "Recientes",
            "Recent Locations" => "Ubicaciones recientes",
            "Redo Last Operation" => "Rehacer última operación",
            "Refresh" => "Actualizar",
            "Rename" => "Cambiar nombre",
            "Restore All from Recycle Bin" => "Restaurar todo de la Papelera",
            "Restore from Recycle Bin" => "Restaurar de la Papelera",
            "Resume Operation Queue" => "Reanudar cola",
            "Rules and Automation" => "Reglas y automatización",
            "Run as Administrator" => "Ejecutar como administrador",
            "Save Workspace" => "Guardar espacio de trabajo",
            "Scan with Microsoft Defender" => "Examinar con Microsoft Defender",
            "Search Performance Settings" => "Configuración de rendimiento de búsqueda",
            "Select All" => "Seleccionar todo",
            "Shell Verb Bridge" => "Puente de verbos del shell",
            "Shortcut Editor" => "Editor de atajos",
            "Show More Options" => "Mostrar más opciones",
            "Smart Folders" => "Carpetas inteligentes",
            "Storage Treemap" => "Mapa de almacenamiento",
            "Take Ownership" => "Tomar posesión",
            "Toggle Dual Pane" => "Alternar panel dual",
            "Toggle Preview" => "Alternar vista previa",
            "Tools" => "Herramientas",
            "Undo Last Operation" => "Deshacer última operación",
                        "Pathfinder keeps a small local database of the files in folders you visit so the search bar can return matches instantly without rescanning your disk every time. Indexing modes change how aggressively that database is grown in the background:\n• Low - only the folders you actually open get added. Lightest on disk and CPU.\n• Balanced - also indexes Desktop, Documents, Downloads, Pictures, and common project folders.\n• Fast - indexes roots you choose, with common folders as fallback.\n• Max - all fixed drives. Best search coverage, uses the most disk while it catches up.\nThumbnails are stored separately and the cache is automatically pruned when it hits the budget below." => "Pathfinder mantiene una pequeña base de datos local de los archivos en las carpetas que visitas para que la barra de búsqueda encuentre coincidencias al instante sin reescanear el disco cada vez. Los modos de indexación cambian lo agresivo que crece esa base en segundo plano:\n• Baja - solo las carpetas que abres de verdad. Menor impacto en disco y CPU.\n• Equilibrado - también indexa Escritorio, Documentos, Descargas, Imágenes y carpetas de proyecto comunes.\n• Rápido - indexa las raíces que eliges, con carpetas comunes de reserva.\n• Máxima - todas las unidades fijas. Mejor cobertura, usa más disco mientras se pone al día.\nLas miniaturas se guardan aparte y la caché se recorta automáticamente al presupuesto de abajo.",
            "Compact — smallest (~50 MB)" => "Compacto — más pequeño (~50 MB)",
            "Quality — best accuracy (~99 MB)" => "Calidad — máxima precisión (~99 MB)",
            "Balanced (recommended, ~61 MB)" => "Equilibrado (recomendado, ~61 MB)",
            "Ready — search by meaning across indexed files" => "Listo — busca por significado entre archivos indexados",
            "Ready — auto-suggest tags for photos" => "Listo — sugiere etiquetas para fotos",
            "Ready — near-duplicate detection uses a lightweight image hash (no model required)" => "Listo — los casi duplicados usan un hash de imagen ligero (sin modelo)",
            "Models up to date" => "Modelos actualizados",
            "Delete forever" => "Eliminar para siempre",
            "Move to Recycle Bin" => "Mover a la Papelera",
            "Move to Bin" => "Mover a la Papelera",
            "Share" => "Compartir",
            "Compact View" => "Vista compacta",
            "Open a library folder" => "Abrir una carpeta de biblioteca",
            "Click a folder to open it" => "Haz clic en una carpeta para abrirla",
            "No recent locations recorded yet." => "Aún no hay ubicaciones recientes.",
            "No sibling folders available." => "No hay carpetas hermanas.",
            "Click a version, then Restore" => "Elige una versión y pulsa Restaurar",
            "No shadow copies found. Enable File History or VSS snapshots." => {
                "No hay copias shadow. Activa Historial de archivos o instantáneas VSS."
            }
            "OneDrive and cloud placeholders. Open in Explorer for pin controls." => {
                "Marcadores de OneDrive y la nube. Ábrelos en Explorer para anclar."
            }
            "Thumbnails, previews, and folder cache" => "Miniaturas, vistas previas y caché de carpetas",
            "Does not delete your files" => "No elimina tus archivos",
            "Click a row and press a key combination. Save to apply." => {
                "Haz clic en una fila y pulsa una combinación. Guarda para aplicar."
            }
            "Click a version to restore it." => "Haz clic en una versión para restaurarla.",
            "Keyboard shortcuts" => "Atajos de teclado",
            "Click a row, press a chord, then Save." => {
                "Haz clic en una fila, pulsa un acorde y guarda."
            }
            "Documents" => "Documentos",
            "Pictures" => "Imágenes",
            "Music" => "Música",
            "Videos" => "Vídeos",
            "Downloads" => "Descargas",
            "Desktop" => "Escritorio",
            "Back" => "Atrás",
            "Forward" => "Adelante",
            "Up One Level" => "Subir un nivel",
            "Focus Address Bar" => "Enfocar barra de direcciones",
            "Focus Search" => "Enfocar búsqueda",
            "Command Palette" => "Paleta de comandos",
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
