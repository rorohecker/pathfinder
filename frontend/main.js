const tauriCore = window.__TAURI__?.core;
const invoke = tauriCore?.invoke
  ? tauriCore.invoke
  : async () => {
      throw new Error("Tauri IPC is unavailable. Run this app with Tauri.");
    };

const TAGS = [
  { id: "red", label: "Urgent", color: "#e5484d" },
  { id: "orange", label: "Important", color: "#e3862a" },
  { id: "yellow", label: "Review", color: "#d7b125" },
  { id: "green", label: "Done", color: "#2aa96b" },
  { id: "blue", label: "Personal", color: "#4f9cff" },
  { id: "violet", label: "Code", color: "#8b6cff" },
];

const THEMES = [
  { id: "mica-dark", name: "Mica Dark", desc: "Windows-style dark Fluent", swatch: ["#101318", "#4f9cff", "#f2f6fb"] },
  { id: "mica-light", name: "Mica Light", desc: "Windows-style light Mica", swatch: ["#eef2f7", "#2f78d4", "#19202a"] },
  { id: "warm", name: "Warm Neutral", desc: "Soft desktop workspace", swatch: ["#f2eee6", "#d07920", "#2a241d"] },
  { id: "flat", name: "Flat White", desc: "Quiet and minimal", swatch: ["#ffffff", "#4f6fdc", "#161a20"] },
  { id: "terminal", name: "Terminal", desc: "Phosphor green command room", swatch: ["#07110d", "#7cff9d", "#c8ffd8"] },
  { id: "paper", name: "Paper", desc: "Letterpress and ink", swatch: ["#eadfc9", "#9f3f2c", "#332617"] },
  { id: "retro", name: "Retro Arcade", desc: "Fun 16-bit gaming energy", swatch: ["#24205e", "#ffcf3f", "#aeeaff"] },
  { id: "fantasy", name: "High Fantasy", desc: "Parchment and gilded UI", swatch: ["#d9c38f", "#9b6716", "#2f2113"] },
  { id: "cyberpunk", name: "Cyberpunk", desc: "Neon city file grid", swatch: ["#100727", "#ff39bc", "#00ecff"] },
];

const ACCENTS = [
  { id: "blue", color: "#4f9cff" },
  { id: "amber", color: "#d98a24" },
  { id: "green", color: "#2aa96b" },
  { id: "violet", color: "#8b6cff" },
  { id: "rose", color: "#e45578" },
  { id: "teal", color: "#1aa6a6" },
];

const WALLPAPERS = [
  { id: "none", name: "None", css: "" },
  { id: "dawn", name: "Dawn", css: "linear-gradient(135deg, #f7bd86, #df7fa4 54%, #8068ca)" },
  { id: "forest", name: "Forest", css: "linear-gradient(135deg, #173f35, #4f896c 58%, #c1d98c)" },
  { id: "ocean", name: "Ocean", css: "linear-gradient(135deg, #19345f, #267fad 58%, #6bd5e5)" },
  { id: "dusk", name: "Dusk", css: "linear-gradient(160deg, #21114c, #71308b 52%, #d77b27)" },
  { id: "ink", name: "Ink", css: "radial-gradient(circle at 24% 18%, #323238, #06070a 70%)" },
  { id: "neon", name: "Neon", css: "linear-gradient(135deg, #13052d, #562086 58%, #d83fa7)" },
];

const ICONS = {
  back: "M15 18l-6-6 6-6",
  forward: "M9 18l6-6-6-6",
  up: ["M12 19V5", "M5 12l7-7 7 7"],
  refresh: ["M3 12a9 9 0 0 1 15-6.7L21 8", "M21 3v5h-5", "M21 12a9 9 0 0 1-15 6.7L3 16", "M3 21v-5h5"],
  plus: "M12 5v14 M5 12h14",
  x: "M18 6L6 18 M6 6l12 12",
  search: ["M11 19a8 8 0 1 0 0-16 8 8 0 0 0 0 16z", "M21 21l-4.3-4.3"],
  grid: ["M3 3h7v7H3z", "M14 3h7v7h-7z", "M3 14h7v7H3z", "M14 14h7v7h-7z"],
  list: ["M8 6h13", "M8 12h13", "M8 18h13", "M3 6h.01", "M3 12h.01", "M3 18h.01"],
  gallery: ["M4 5h16v14H4z", "M4 15l4-4 4 4 3-3 5 5", "M9 9h.01"],
  panel: ["M3 4h18v16H3z", "M14 4v16"],
  palette: ["M12 22a10 10 0 1 1 10-10c0 2-1 3-3 3h-1.5a2 2 0 0 0-2 2c0 1.5-1 5-3.5 5z", "M7.5 10.5h.01", "M10.5 7.5h.01", "M14.5 7.5h.01", "M17 11h.01"],
  settings: ["M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z", "M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 0 1-4 0v-.1a1.7 1.7 0 0 0-1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 0 1 0-4h.1a1.7 1.7 0 0 0 1.5-1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3h.1a1.7 1.7 0 0 0 1-1.5V3a2 2 0 0 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8v.1a1.7 1.7 0 0 0 1.5 1h.1a2 2 0 0 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"],
  folder: "M3 7.5A2.5 2.5 0 0 1 5.5 5H10l2 2h6.5A2.5 2.5 0 0 1 21 9.5v7A2.5 2.5 0 0 1 18.5 19h-13A2.5 2.5 0 0 1 3 16.5z",
  file: ["M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z", "M14 2v6h6"],
  drive: ["M22 12H2", "M5.5 5.2 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.5-6.8A2 2 0 0 0 16.7 4H7.3a2 2 0 0 0-1.8 1.2z"],
  home: "M3 10l9-7 9 7v10a2 2 0 0 1-2 2h-4v-7H9v7H5a2 2 0 0 1-2-2z",
  desktop: ["M3 4h18v12H3z", "M8 20h8", "M12 16v4"],
  download: ["M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4", "M7 10l5 5 5-5", "M12 15V3"],
  image: ["M21 15V5a2 2 0 0 0-2-2H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2z", "M8 11a2 2 0 1 0 0-4 2 2 0 0 0 0 4z", "M21 19l-6-6-9 9"],
  music: ["M9 18V5l12-2v13", "M9 18a3 3 0 1 1-3-3", "M21 16a3 3 0 1 1-3-3"],
  video: ["M23 7l-7 5 7 5V7z", "M14 5H3a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h11a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2z"],
  star: "M12 2l3.1 6.3 6.9 1-5 4.9 1.2 6.8L12 17.8 5.8 21l1.2-6.8-5-4.9 6.9-1z",
  copy: ["M20 9h-9a2 2 0 0 0-2 2v9a2 2 0 0 0 2 2h9a2 2 0 0 0 2-2v-9a2 2 0 0 0-2-2z", "M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"],
  cut: ["M6 9a3 3 0 1 0 0-6 3 3 0 0 0 0 6z", "M6 21a3 3 0 1 0 0-6 3 3 0 0 0 0 6z", "M20 4 8.1 15.9", "M14.5 14.5 20 20", "M8.1 8.1 12 12"],
  paste: ["M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2", "M9 2h6v4H9z"],
  rename: ["M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7", "M18.5 2.5a2.1 2.1 0 0 1 3 3L12 15l-4 1 1-4z"],
  trash: ["M3 6h18", "M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6", "M10 11v6", "M14 11v6", "M9 6V3h6v3"],
  tag: ["M20.6 13.4 13 21a2 2 0 0 1-2.8 0L3 13.6V3h10.6l7 7a2 2 0 0 1 0 2.8z", "M7 7h.01"],
  open: ["M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6", "M15 3h6v6", "M10 14 21 3"],
  info: ["M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18z", "M12 8h.01", "M11 12h1v4h1"],
  check: "M5 12l5 5L20 7",
  upload: ["M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4", "M17 8l-5-5-5 5", "M12 3v12"],
};

const state = {
  currentPath: "",
  files: [],
  searchResults: null,
  query: "",
  activeTag: null,
  loading: false,
  loadError: null,
  selected: new Set(),
  lastSelectedPath: null,
  renamingPath: null,
  clipboard: null,
  view: "grid",
  sortBy: "name",
  sortDir: "asc",
  showPreview: true,
  tabs: [],
  activeTabId: null,
  knownFolders: [],
  drives: [],
  bookmarks: [],
  tags: loadJson("fe-tags", {}),
  prefs: loadJson("fe-prefs", {
    theme: "mica-dark",
    accent: "blue",
    density: "cozy",
    wallpaper: "none",
    customWallpaper: null,
  }),
  previewCache: new Map(),
  previewLoading: new Set(),
  previewToken: 0,
  thumbnailCache: new Map(),
  menuActions: [],
  settingsTab: "themes",
  paletteItems: [],
  paletteIndex: 0,
  loadToken: 0,
  searchToken: 0,
  modalResolve: null,
  aiCapabilities: null,
  useIndexedSearch: true,
  virtual: {
    scrollTop: 0,
    paneHeight: 0,
    paneWidth: 0,
  },
};

const els = {};

function loadJson(key, fallback) {
  try {
    const value = localStorage.getItem(key);
    return value ? JSON.parse(value) : fallback;
  } catch {
    return fallback;
  }
}

function saveJson(key, value) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Ignore storage failures; the app still works for the session.
  }
}

function icon(name, size = 16, stroke = 1.7) {
  const paths = ICONS[name];
  const body = (Array.isArray(paths) ? paths : [paths])
    .map((d) => `<path d="${d}"></path>`)
    .join("");
  return `<svg class="icon" width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="${stroke}" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
}

function escapeHtml(value) {
  return String(value ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#039;");
}

function basename(path) {
  const trimmed = String(path || "").replace(/[\\\/]+$/, "");
  const parts = trimmed.split(/[\\\/]/);
  return parts.pop() || path || "";
}

function dirname(path) {
  const normalized = String(path || "").replace(/[\\\/]+$/, "");
  const idx = Math.max(normalized.lastIndexOf("\\"), normalized.lastIndexOf("/"));
  if (idx <= 0) return normalized;
  if (/^[A-Za-z]:$/.test(normalized.slice(0, idx))) return normalized.slice(0, idx + 1);
  return normalized.slice(0, idx);
}

function displayPath(path) {
  if (/^[A-Za-z]:[\\/]/.test(path)) return path.replace(/\//g, "\\");
  return path;
}

function samePath(left, right) {
  return displayPath(left).toLowerCase() === displayPath(right).toLowerCase();
}

const RESERVED_WINDOWS_NAMES = new Set([
  "CON",
  "PRN",
  "AUX",
  "NUL",
  "COM1",
  "COM2",
  "COM3",
  "COM4",
  "COM5",
  "COM6",
  "COM7",
  "COM8",
  "COM9",
  "LPT1",
  "LPT2",
  "LPT3",
  "LPT4",
  "LPT5",
  "LPT6",
  "LPT7",
  "LPT8",
  "LPT9",
]);

function validateWindowsName(name) {
  const value = name.trim();
  if (!value) return "Name cannot be empty.";
  if (/[<>:"/\\|?*\x00-\x1F]/.test(value)) return 'Names cannot include < > : " / \\ | ? or *.';
  if (/[. ]$/.test(value)) return "Names cannot end with a space or period.";
  if (RESERVED_WINDOWS_NAMES.has(value.split(".")[0].toUpperCase())) return "That name is reserved by Windows.";
  return "";
}

function splitExtension(name) {
  const index = name.lastIndexOf(".");
  if (index <= 0) return { stem: name, ext: "" };
  return { stem: name.slice(0, index), ext: name.slice(index) };
}

function copyNameVariant(name, index) {
  const { stem, ext } = splitExtension(name);
  const suffix = index === 1 ? " - Copy" : ` - Copy (${index})`;
  return `${stem}${suffix}${ext}`;
}

async function nextAvailableChild(parent, preferredName, mode = "copy") {
  for (let attempt = 0; attempt < 1000; attempt += 1) {
    const name = attempt === 0
      ? preferredName
      : mode === "number"
        ? `${preferredName} (${attempt + 1})`
        : copyNameVariant(preferredName, attempt);
    const path = await invoke("join_path", { parent, child: name });
    if (!(await invoke("path_exists", { path }))) return { name, path };
  }
  throw new Error(`Could not find an available name for ${preferredName}`);
}

function pathParts(path) {
  const raw = String(path || "");
  if (/^[A-Za-z]:[\\/]/.test(raw)) {
    const drive = raw.slice(0, 2);
    const rest = raw.slice(3).split(/[\\\/]+/).filter(Boolean);
    const parts = [{ label: drive, path: `${drive}\\` }];
    let acc = `${drive}\\`;
    for (const segment of rest) {
      acc = acc.endsWith("\\") ? `${acc}${segment}` : `${acc}\\${segment}`;
      parts.push({ label: segment, path: acc });
    }
    return parts;
  }

  if (raw.startsWith("\\\\")) {
    const rest = raw.replace(/^\\\\/, "").split(/[\\\/]+/).filter(Boolean);
    const root = `\\\\${rest.slice(0, 2).join("\\")}\\`;
    const parts = [{ label: root.replace(/\\$/, ""), path: root }];
    let acc = root;
    for (const segment of rest.slice(2)) {
      acc = `${acc}${segment}\\`;
      parts.push({ label: segment, path: acc.replace(/\\$/, "") });
    }
    return parts;
  }

  const rest = raw.split(/[\\\/]+/).filter(Boolean);
  const parts = [{ label: "Root", path: "/" }];
  let acc = "";
  for (const segment of rest) {
    acc += `/${segment}`;
    parts.push({ label: segment, path: acc });
  }
  return parts;
}

function normalizeEntry(entry) {
  return {
    ...entry,
    extension: (entry.extension || "").toLowerCase(),
    kind: entry.kind || "Other",
  };
}

function isDirectory(entry) {
  return entry?.kind === "Directory" || entry?.kind === "Symlink";
}

function fileType(entry) {
  if (!entry) return "File";
  if (entry.kind === "Directory") return "Folder";
  if (entry.kind === "Symlink") return "Shortcut";
  const ext = (entry.extension || "").toLowerCase();
  if (!ext) return "File";
  const names = {
    jpg: "JPEG image",
    jpeg: "JPEG image",
    png: "PNG image",
    gif: "GIF image",
    webp: "WebP image",
    svg: "SVG image",
    pdf: "PDF document",
    md: "Markdown",
    txt: "Text document",
    zip: "ZIP archive",
    rar: "Archive",
    "7z": "7z archive",
    mp3: "Audio",
    wav: "Audio",
    mp4: "Video",
    mov: "Video",
    js: "JavaScript",
    ts: "TypeScript",
    rs: "Rust source",
    html: "HTML document",
    css: "CSS stylesheet",
    json: "JSON",
    toml: "TOML",
    csv: "CSV",
    doc: "Word document",
    docx: "Word document",
    xls: "Excel workbook",
    xlsx: "Excel workbook",
  };
  return names[ext] || `${ext.toUpperCase()} file`;
}

function iconForKnownFolder(id) {
  return {
    home: "home",
    desktop: "desktop",
    documents: "file",
    downloads: "download",
    pictures: "image",
    music: "music",
    videos: "video",
  }[id] || "folder";
}

function formatSize(bytes) {
  if (!bytes || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let size = bytes;
  let index = 0;
  while (size >= 1024 && index < units.length - 1) {
    size /= 1024;
    index += 1;
  }
  return `${index > 0 && size < 10 ? size.toFixed(1) : Math.round(size)} ${units[index]}`;
}

function formatDate(epochSecs) {
  if (!epochSecs) return "";
  const date = new Date(epochSecs * 1000);
  const now = new Date();
  const diff = now - date;
  if (diff < 60 * 1000) return "just now";
  if (diff < 60 * 60 * 1000) return `${Math.floor(diff / 60000)} min ago`;
  if (diff < 24 * 60 * 60 * 1000) return `${Math.floor(diff / 3600000)} hr ago`;
  if (diff < 7 * 24 * 60 * 60 * 1000) {
    const days = Math.floor(diff / 86400000);
    return `${days} day${days === 1 ? "" : "s"} ago`;
  }
  return date.toLocaleDateString("en-US", { month: "short", day: "numeric", year: "numeric" });
}

function cssNumber(name, fallback) {
  const value = Number.parseFloat(getComputedStyle(document.documentElement).getPropertyValue(name));
  return Number.isFinite(value) ? value : fallback;
}

function updateVirtualMetrics() {
  if (!els.filePane) return;
  state.virtual.scrollTop = els.filePane.scrollTop;
  state.virtual.paneHeight = els.filePane.clientHeight;
  state.virtual.paneWidth = els.filePane.clientWidth;
}

function visibleGridSlice(files) {
  updateVirtualMetrics();
  const pad = cssNumber("--pad", 16);
  const gap = 9;
  const gridW = cssNumber("--grid-w", 132);
  const gridH = state.view === "gallery" ? 154 : cssNumber("--grid-h", 118);
  const contentWidth = Math.max(gridW, state.virtual.paneWidth - pad * 2);
  const columns = Math.max(1, Math.floor((contentWidth + gap) / (gridW + gap)));
  const rowHeight = gridH + gap;
  const totalRows = Math.ceil(files.length / columns);
  const startRow = Math.max(0, Math.floor(state.virtual.scrollTop / rowHeight) - 2);
  const visibleRows = Math.ceil((state.virtual.paneHeight || 600) / rowHeight) + 5;
  const endRow = Math.min(totalRows, startRow + visibleRows);
  const start = startRow * columns;
  const end = Math.min(files.length, endRow * columns);
  return {
    files: files.slice(start, end),
    top: startRow * rowHeight,
    height: totalRows * rowHeight,
    start,
    end,
  };
}

function visibleListSlice(files) {
  updateVirtualMetrics();
  const rowHeight = cssNumber("--row-h", 36);
  const headerHeight = 32;
  const scrollTop = Math.max(0, state.virtual.scrollTop - headerHeight);
  const start = Math.max(0, Math.floor(scrollTop / rowHeight) - 6);
  const visible = Math.ceil((state.virtual.paneHeight || 600) / rowHeight) + 12;
  const end = Math.min(files.length, start + visible);
  return {
    files: files.slice(start, end),
    top: start * rowHeight,
    height: files.length * rowHeight,
    start,
    end,
  };
}

function ensureIndexVisible(index) {
  if (index < 0 || !els.filePane) return;
  if (state.view === "list") {
    const rowHeight = cssNumber("--row-h", 36);
    const top = 32 + index * rowHeight;
    const bottom = top + rowHeight;
    if (top < els.filePane.scrollTop) els.filePane.scrollTop = top;
    else if (bottom > els.filePane.scrollTop + els.filePane.clientHeight) {
      els.filePane.scrollTop = bottom - els.filePane.clientHeight;
    }
  } else {
    const gap = 9;
    const pad = cssNumber("--pad", 16);
    const gridW = cssNumber("--grid-w", 132);
    const gridH = state.view === "gallery" ? 154 : cssNumber("--grid-h", 118);
    const contentWidth = Math.max(gridW, els.filePane.clientWidth - pad * 2);
    const columns = Math.max(1, Math.floor((contentWidth + gap) / (gridW + gap)));
    const row = Math.floor(index / columns);
    const top = row * (gridH + gap);
    const bottom = top + gridH;
    if (top < els.filePane.scrollTop) els.filePane.scrollTop = top;
    else if (bottom > els.filePane.scrollTop + els.filePane.clientHeight) {
      els.filePane.scrollTop = bottom - els.filePane.clientHeight;
    }
  }
  state.virtual.scrollTop = els.filePane.scrollTop;
}

function getTag(path) {
  return state.tags[path] || null;
}

function setTag(path, tag) {
  if (tag) state.tags[path] = tag;
  else delete state.tags[path];
  saveJson("fe-tags", state.tags);
  render();
}

function tagById(id) {
  return TAGS.find((tag) => tag.id === id);
}

function nodeVisual(entry, size = "large") {
  if (entry.kind === "Directory") {
    return `<div class="node-visual folder ${size === "small" ? "small" : ""}"><span class="folder-shape"></span></div>`;
  }
  const ext = (entry.extension || "file").slice(0, 4).toUpperCase();
  return `<div class="node-visual file ${size === "small" ? "small" : ""}"><span class="file-shape"><span class="file-ext">${escapeHtml(ext || "FILE")}</span></span></div>`;
}

function applyPrefs() {
  const { theme, accent, density, wallpaper, customWallpaper } = state.prefs;
  document.documentElement.dataset.theme = theme || "mica-dark";
  document.documentElement.dataset.accent = accent || "blue";
  document.documentElement.dataset.density = density || "cozy";

  if (wallpaper === "custom" && customWallpaper) {
    document.body.dataset.wallpaper = "custom";
    document.body.style.setProperty("--wallpaper", `url("${customWallpaper}")`);
  } else {
    const preset = WALLPAPERS.find((item) => item.id === wallpaper);
    if (preset?.css) {
      document.body.dataset.wallpaper = preset.id;
      document.body.style.setProperty("--wallpaper", preset.css);
    } else {
      document.body.removeAttribute("data-wallpaper");
      document.body.style.removeProperty("--wallpaper");
    }
  }
  saveJson("fe-prefs", state.prefs);
}

function activeTab() {
  return state.tabs.find((tab) => tab.id === state.activeTabId);
}

function updateTabPath(path, pushHistory) {
  const tab = activeTab();
  if (!tab) return;
  tab.path = path;
  tab.name = basename(path) || path;
  if (pushHistory) {
    tab.history = tab.history.slice(0, tab.hIndex + 1);
    tab.history.push(path);
    tab.hIndex = tab.history.length - 1;
  }
}

async function navigateTo(path, options = {}) {
  const { pushHistory = true, keepQuery = false } = options;
  const token = ++state.loadToken;
  state.loading = true;
  state.loadError = null;
  if (!keepQuery) {
    state.query = "";
    state.searchResults = null;
    state.activeTag = null;
    state.searchToken += 1;
    els.searchInput.value = "";
  }
  state.selected.clear();
  state.virtual.scrollTop = 0;
  if (els.filePane) els.filePane.scrollTop = 0;
  render();

  try {
    const files = (await invoke("list_directory", { path })).map(normalizeEntry);
    if (token !== state.loadToken) return;
    state.files = files;
    state.currentPath = path;
    state.loading = false;
    updateTabPath(path, pushHistory);
    render();
    invoke("watch_paths", { paths: [path] }).catch(() => {});
    const likely = files.filter(isDirectory).slice(0, 12).map((file) => file.path);
    if (likely.length) invoke("prefetch_paths", { paths: likely }).catch(() => {});
  } catch (error) {
    if (token !== state.loadToken) return;
    state.loading = false;
    state.loadError = String(error);
    toast(String(error));
    render();
  }
}

async function refreshCurrent() {
  if (!state.currentPath) return;
  const query = state.query;
  await navigateTo(state.currentPath, { pushHistory: false, keepQuery: true });
  if (query) performSearch(query);
}

function goBack() {
  const tab = activeTab();
  if (!tab || tab.hIndex <= 0) return;
  tab.hIndex -= 1;
  navigateTo(tab.history[tab.hIndex], { pushHistory: false });
}

function goForward() {
  const tab = activeTab();
  if (!tab || tab.hIndex >= tab.history.length - 1) return;
  tab.hIndex += 1;
  navigateTo(tab.history[tab.hIndex], { pushHistory: false });
}

async function goUp() {
  if (!state.currentPath) return;
  const parent = await invoke("get_parent_path", { path: state.currentPath });
  if (parent && parent !== state.currentPath) navigateTo(parent);
}

function createTab(path) {
  const id = `tab-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
  state.tabs.push({
    id,
    path,
    name: basename(path) || path,
    history: [path],
    hIndex: 0,
  });
  state.activeTabId = id;
  renderTabs();
  navigateTo(path, { pushHistory: false });
}

function closeTab(id) {
  if (state.tabs.length === 1) return;
  const index = state.tabs.findIndex((tab) => tab.id === id);
  state.tabs = state.tabs.filter((tab) => tab.id !== id);
  if (state.activeTabId === id) {
    const next = state.tabs[Math.max(0, index - 1)] || state.tabs[0];
    state.activeTabId = next.id;
    navigateTo(next.path, { pushHistory: false });
  } else {
    renderTabs();
  }
}

function switchTab(id) {
  const tab = state.tabs.find((item) => item.id === id);
  if (!tab || id === state.activeTabId) return;
  state.activeTabId = id;
  navigateTo(tab.path, { pushHistory: false });
}

function renderTabs() {
  els.tabs.innerHTML = state.tabs
    .map((tab) => {
      const active = tab.id === state.activeTabId ? "active" : "";
      return `
        <button class="tab ${active}" data-tab="${escapeHtml(tab.id)}" role="tab" aria-selected="${active ? "true" : "false"}">
          ${nodeVisual({ kind: "Directory" }, "small")}
          <span class="tab-name">${escapeHtml(tab.name)}</span>
          <span class="tab-close" data-close-tab="${escapeHtml(tab.id)}">${icon("x", 12)}</span>
        </button>`;
    })
    .join("");
}

function renderSidebar() {
  const tagCounts = TAGS.reduce((acc, tag) => {
    acc[tag.id] = Object.values(state.tags).filter((id) => id === tag.id).length;
    return acc;
  }, {});

  const section = (title, body, plusAction = "") => `
    <div class="side-section">
      <div class="side-title"><span>${title}</span>${plusAction}</div>
      ${body}
    </div>`;

  const known = state.knownFolders
    .map((folder) => {
      const active = folder.path === state.currentPath && !state.activeTag ? "active" : "";
      return `<button class="side-item ${active}" data-nav="${escapeHtml(folder.path)}">
        <span>${icon(iconForKnownFolder(folder.id), 15)}</span>
        <span class="side-label">${escapeHtml(folder.name)}</span>
        <span></span>
      </button>`;
    })
    .join("");

  const bookmarks = state.bookmarks
    .map((bookmark, index) => {
      const active = bookmark.path === state.currentPath && !state.activeTag ? "active" : "";
      return `<button class="side-item ${active}" data-bookmark="${index}">
        <span>${icon("star", 14)}</span>
        <span class="side-label">${escapeHtml(bookmark.name)}</span>
        <span></span>
      </button>`;
    })
    .join("");

  const tags = TAGS.map((tag) => {
    const active = state.activeTag === tag.id ? "active" : "";
    return `<button class="side-item ${active}" data-tag-filter="${tag.id}">
      <span class="tag-dot" style="background:${tag.color}"></span>
      <span class="side-label">${escapeHtml(tag.label)}</span>
      <span class="side-count">${tagCounts[tag.id] || 0}</span>
    </button>`;
  }).join("");

  const drives = state.drives
    .map((drive) => {
      const active = drive.path === state.currentPath && !state.activeTag ? "active" : "";
      return `<button class="side-item ${active}" data-nav="${escapeHtml(drive.path)}">
        <span>${icon("drive", 15)}</span>
        <span class="side-label">${escapeHtml(drive.name)}</span>
        <span class="side-count">${escapeHtml(drive.path)}</span>
      </button>`;
    })
    .join("");

  els.sidebar.innerHTML =
    section("Places", known) +
    section("Bookmarks", bookmarks || `<div class="side-item"><span></span><span class="side-label">No bookmarks</span><span></span></div>`, `<button data-add-bookmark title="Bookmark current folder">${icon("plus", 12)}</button>`) +
    section("Tags", tags) +
    section("Drives", drives);
}

function renderBreadcrumb() {
  const parts = pathParts(state.currentPath);
  els.breadcrumb.innerHTML = parts
    .map((part, index) => {
      const cls = index === 0 ? "crumb-root" : "crumb";
      const sep = index === 0 ? "" : `<span class="crumb-sep">\\</span>`;
      return `${sep}<button class="${cls}" data-crumb="${escapeHtml(part.path)}">${escapeHtml(part.label)}</button>`;
    })
    .join("");
  els.addressBar.value = displayPath(state.currentPath);
}

function getDisplayFiles() {
  let files = state.searchResults || state.files;
  if (state.activeTag) {
    files = files.filter((file) => getTag(file.path) === state.activeTag);
  }

  const sorted = files.slice().sort((a, b) => {
    if (a.kind === "Directory" && b.kind !== "Directory") return -1;
    if (a.kind !== "Directory" && b.kind === "Directory") return 1;
    let result = 0;
    if (state.sortBy === "size") result = (a.size || 0) - (b.size || 0);
    else if (state.sortBy === "modified") result = (a.modified || 0) - (b.modified || 0);
    else if (state.sortBy === "kind") result = fileType(a).localeCompare(fileType(b));
    else result = a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: "base" });
    return state.sortDir === "asc" ? result : -result;
  });

  return sorted;
}

function searchTerms() {
  return state.query
    .replace(/\b\w+:[^\s]+/g, "")
    .trim()
    .split(/\s+/)
    .filter(Boolean)
    .map((term) => term.toLowerCase());
}

function highlightedName(name) {
  const terms = searchTerms();
  if (!terms.length) return escapeHtml(name);
  let output = escapeHtml(name);
  for (const term of terms) {
    const safe = term.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    output = output.replace(new RegExp(`(${safe})`, "ig"), '<mark class="highlight">$1</mark>');
  }
  return output;
}

function renderFiles() {
  if (state.loading) {
    els.filePane.innerHTML = `<div class="loading-state"><div><div class="spinner"></div><div>Loading ${escapeHtml(displayPath(state.currentPath))}</div></div></div>`;
    els.filePane.classList.remove("virtualized");
    return;
  }

  if (state.loadError) {
    els.filePane.innerHTML = `<div class="error-state"><div>${escapeHtml(state.loadError)}</div></div>`;
    els.filePane.classList.remove("virtualized");
    return;
  }

  const files = getDisplayFiles();
  els.filePane.classList.toggle("gallery", state.view === "gallery");
  els.filePane.classList.toggle("details", state.view === "list");

  if (!files.length) {
    const label = state.query || state.activeTag ? "No matching files" : "This folder is empty";
    els.filePane.innerHTML = `<div class="empty-state"><div>${escapeHtml(label)}</div></div>`;
    els.filePane.classList.remove("virtualized");
    return;
  }

  const shouldVirtualize = files.length > 220;
  els.filePane.classList.toggle("virtualized", shouldVirtualize);

  if (state.view === "list") {
    const heads = [
      ["name", "Name"],
      ["size", "Size"],
      ["modified", "Modified"],
      ["kind", "Type"],
      ["tag", "Tag"],
    ];
    const header = `<div class="list-head">${heads.map(([key, label]) => `<button data-sort="${key}">${label}${state.sortBy === key ? (state.sortDir === "asc" ? " ^" : " v") : ""}</button>`).join("")}</div>`;
    if (shouldVirtualize) {
      const slice = visibleListSlice(files);
      const rows = slice.files.map((file) => fileRow(file)).join("");
      els.filePane.innerHTML = `${header}<div class="virtual-list" style="height:${slice.height}px"><div class="virtual-window" style="transform:translateY(${slice.top}px)">${rows}</div></div>`;
      warmVisiblePreviews(slice.files);
    } else {
      const rows = files.map((file) => fileRow(file)).join("");
      els.filePane.innerHTML = header + rows;
      warmVisiblePreviews(files.slice(0, 80));
    }
  } else {
    if (shouldVirtualize) {
      const slice = visibleGridSlice(files);
      els.filePane.innerHTML = `<div class="view-grid virtual-grid" style="height:${slice.height}px"><div class="virtual-window grid-window" style="transform:translateY(${slice.top}px)">${slice.files.map((file) => fileCard(file)).join("")}</div></div>`;
      warmVisiblePreviews(slice.files);
      warmVisibleThumbnails(slice.files);
    } else {
      els.filePane.innerHTML = `<div class="view-grid">${files.map((file) => fileCard(file)).join("")}</div>`;
      warmVisiblePreviews(files.slice(0, 80));
      warmVisibleThumbnails(files.slice(0, 80));
    }
  }

  if (state.renamingPath) {
    const input = els.filePane.querySelector(".rename-input");
    if (input) {
      input.focus();
      input.select();
    }
  }
}

function fileCard(file) {
  const selected = state.selected.has(file.path) ? "selected" : "";
  const focused = state.lastSelectedPath === file.path ? "focused" : "";
  const cut = state.clipboard?.mode === "cut" && state.clipboard.paths.includes(file.path) ? "cut" : "";
  const tag = getTag(file.path);
  const tagHtml = tag ? `<span class="tag-dot" style="background:${tagById(tag)?.color || "var(--accent)"}"></span>` : "";
  const nameHtml = state.renamingPath === file.path
    ? `<input class="rename-input" value="${escapeHtml(file.name)}" data-rename="${escapeHtml(file.path)}" />`
    : highlightedName(file.name);

  const thumbUrl = state.thumbnailCache.get(file.path);
  const visual = thumbUrl
    ? `<div class="node-visual thumb"><img src="${thumbUrl}" alt="" loading="lazy" decoding="async" /></div>`
    : nodeVisual(file);

  return `<div class="file-card ${selected} ${focused} ${cut}" data-path="${escapeHtml(file.path)}">
    ${tagHtml}
    ${visual}
    <div class="name">${nameHtml}</div>
  </div>`;
}

function fileRow(file) {
  const selected = state.selected.has(file.path) ? "selected" : "";
  const focused = state.lastSelectedPath === file.path ? "focused" : "";
  const cut = state.clipboard?.mode === "cut" && state.clipboard.paths.includes(file.path) ? "cut" : "";
  const tag = getTag(file.path);
  const tagMeta = tagById(tag);
  const tagHtml = tagMeta ? `<span class="tag-dot" style="background:${tagMeta.color}"></span>` : "";
  const nameHtml = state.renamingPath === file.path
    ? `<input class="rename-input" value="${escapeHtml(file.name)}" data-rename="${escapeHtml(file.path)}" />`
    : highlightedName(file.name);

  return `<div class="file-row ${selected} ${focused} ${cut}" data-path="${escapeHtml(file.path)}">
    <div class="name-cell">${nodeVisual(file, "small")}<span class="row-name">${nameHtml}</span>${tagHtml}</div>
    <div class="mono">${file.kind === "Directory" ? "" : formatSize(file.size)}</div>
    <div class="mono">${formatDate(file.modified)}</div>
    <div class="mono">${escapeHtml(fileType(file))}</div>
    <div class="mono">${tagMeta ? escapeHtml(tagMeta.label) : ""}</div>
  </div>`;
}

function renderPreview() {
  els.main.classList.toggle("with-preview", state.showPreview);
  els.previewToggle.classList.toggle("active", state.showPreview);
  if (!state.showPreview) return;

  const selected = selectedEntries();
  const file = selected[0];
  if (!file) {
    els.previewPane.innerHTML = `
      <div class="preview-head">
        <div class="preview-title">Preview</div>
      </div>
      <div class="preview-empty">
        <div>
          <div style="margin-bottom:10px">${icon("panel", 34)}</div>
          <div>Select a file to preview</div>
          <div style="margin-top:4px;font-size:11px">Press Space for Quick Look</div>
        </div>
      </div>`;
    return;
  }

  const cached = state.previewCache.get(file.path);
  if (cached) {
    els.previewPane.innerHTML = previewMarkup(file, cached);
    return;
  }

  els.previewPane.innerHTML = `
    <div class="preview-head">
      <div class="preview-title">${escapeHtml(file.name)}</div>
      <button class="tool-btn" data-close-preview>${icon("x", 14)}</button>
    </div>
    <div class="preview-body"><div class="spinner"></div></div>`;
  loadPreview(file);
}

async function loadPreview(file) {
  if (state.previewLoading.has(file.path)) return;
  state.previewLoading.add(file.path);
  const token = ++state.previewToken;
  try {
    const preview = await invoke("read_preview", { path: file.path, maxBytes: 524288 });
    state.previewCache.set(file.path, preview);
    if (token === state.previewToken) renderPreview();
  } catch (error) {
    state.previewCache.set(file.path, { kind: "error", text: String(error), truncated: false });
    if (token === state.previewToken) renderPreview();
  } finally {
    state.previewLoading.delete(file.path);
  }
}

function warmVisiblePreviews(files) {
  const paths = files
    .filter((file) => file.kind !== "Directory" && !state.previewCache.has(file.path))
    .slice(0, 48)
    .map((file) => file.path);
  if (paths.length) {
    invoke("warm_preview_cache", { paths, maxBytes: 262144 }).catch(() => {});
  }
}

const IMAGE_EXTS = new Set(["jpg", "jpeg", "png", "gif", "webp", "bmp"]);
function isImageFile(file) {
  return file.kind !== "Directory" && IMAGE_EXTS.has((file.extension || "").toLowerCase());
}

let _thumbFrame = null;
function warmVisibleThumbnails(files) {
  const paths = files
    .filter((file) => isImageFile(file) && !state.thumbnailCache.has(file.path))
    .slice(0, 40)
    .map((file) => file.path);
  if (!paths.length) return;
  if (_thumbFrame) cancelAnimationFrame(_thumbFrame);
  _thumbFrame = requestAnimationFrame(async () => {
    _thumbFrame = null;
    try {
      const result = await invoke("fetch_thumbnails", { paths, size: 160 });
      let changed = false;
      for (const [path, dataUrl] of Object.entries(result)) {
        if (!state.thumbnailCache.has(path)) {
          state.thumbnailCache.set(path, dataUrl);
          changed = true;
        }
      }
      if (changed) renderFiles();
    } catch {
      // thumbnail fetch failed — silently continue with generic icons
    }
  });
}

function previewMarkup(file, preview) {
  const tag = tagById(getTag(file.path));
  const art = preview.kind === "image" && preview.data_url
    ? `<img class="preview-img" src="${preview.data_url}" alt="${escapeHtml(file.name)}" />`
    : `<div class="preview-art">${nodeVisual(file)}</div>`;
  const body = preview.kind === "text"
    ? `<div style="margin-top:16px"><div class="group-label">Contents${preview.truncated ? " (truncated)" : ""}</div><pre class="preview-code">${escapeHtml(preview.text || "")}</pre></div>`
    : preview.kind === "error"
      ? `<div class="preview-code">${escapeHtml(preview.text || "Preview failed")}</div>`
      : "";

  return `
    <div class="preview-head">
      <div class="preview-title">${escapeHtml(file.name)}</div>
      <button class="tool-btn" data-close-preview>${icon("x", 14)}</button>
    </div>
    <div class="preview-body">
      ${art}
      <dl class="meta">
        <dt>Type</dt><dd>${escapeHtml(fileType(file))}</dd>
        <dt>Size</dt><dd>${file.kind === "Directory" ? "Folder" : formatSize(file.size)}</dd>
        <dt>Modified</dt><dd>${formatDate(file.modified)}</dd>
        <dt>Location</dt><dd>${escapeHtml(displayPath(dirname(file.path)))}</dd>
        ${tag ? `<dt>Tag</dt><dd><span class="tag-dot" style="display:inline-block;background:${tag.color};vertical-align:-1px"></span> ${escapeHtml(tag.label)}</dd>` : ""}
      </dl>
      ${body}
    </div>`;
}

function renderStatus() {
  const files = getDisplayFiles();
  const selected = selectedEntries();
  const totalSize = files.reduce((sum, file) => sum + (file.kind === "Directory" ? 0 : file.size || 0), 0);
  const selectedSize = selected.reduce((sum, file) => sum + (file.kind === "Directory" ? 0 : file.size || 0), 0);
  const status = state.query
    ? `${files.length} search result${files.length === 1 ? "" : "s"}`
    : `${files.length} item${files.length === 1 ? "" : "s"}`;

  els.statusBar.innerHTML = `
    <span>${escapeHtml(status)}</span>
    <span>${selected.length ? `${selected.length} selected (${formatSize(selectedSize)})` : `${formatSize(totalSize)} visible`}</span>
    <span>${escapeHtml(displayPath(state.currentPath))}</span>
    <span class="status-spacer"></span>
    <span>Local</span>
    <span class="progress"><span></span></span>`;
}

function renderToolbar() {
  const tab = activeTab();
  els.backBtn.disabled = !tab || tab.hIndex <= 0;
  els.forwardBtn.disabled = !tab || tab.hIndex >= tab.history.length - 1;
  els.viewGrid.classList.toggle("active", state.view === "grid");
  els.viewList.classList.toggle("active", state.view === "list");
  els.viewGallery.classList.toggle("active", state.view === "gallery");
  renderBreadcrumb();
}

function render() {
  applyPrefs();
  renderTabs();
  renderSidebar();
  renderToolbar();
  renderFiles();
  renderPreview();
  renderStatus();
}

function selectedEntries() {
  const all = getDisplayFiles();
  return [...state.selected]
    .map((path) => all.find((file) => file.path === path) || state.files.find((file) => file.path === path) || state.searchResults?.find((file) => file.path === path))
    .filter(Boolean);
}

function selectFile(path, event = {}) {
  const files = getDisplayFiles();
  if (event.shiftKey && state.lastSelectedPath) {
    const a = files.findIndex((file) => file.path === state.lastSelectedPath);
    const b = files.findIndex((file) => file.path === path);
    if (a >= 0 && b >= 0) {
      const [start, end] = [Math.min(a, b), Math.max(a, b)];
      state.selected = new Set(files.slice(start, end + 1).map((file) => file.path));
    }
  } else if (event.ctrlKey) {
    if (state.selected.has(path)) state.selected.delete(path);
    else state.selected.add(path);
    state.lastSelectedPath = path;
  } else {
    state.selected = new Set([path]);
    state.lastSelectedPath = path;
  }
  render();
}

function clearSelection() {
  state.selected.clear();
  state.lastSelectedPath = null;
  render();
}

function findEntry(path) {
  return getDisplayFiles().find((file) => file.path === path) || state.files.find((file) => file.path === path) || state.searchResults?.find((file) => file.path === path);
}

async function openEntry(entry) {
  if (!entry) return;
  if (isDirectory(entry)) {
    await navigateTo(entry.path);
  } else {
    await invoke("open_file", { path: entry.path });
  }
}

async function revealEntry(entry) {
  if (!entry) return;
  await invoke("reveal_in_folder", { path: entry.path });
}

async function writeClipboardText(text) {
  if (navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return;
    } catch {
      // Fall back to the older selection API in WebView environments.
    }
  }

  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.left = "-9999px";
  document.body.appendChild(textarea);
  textarea.select();
  const copied = document.execCommand?.("copy");
  textarea.remove();
  if (!copied) throw new Error("Could not copy to clipboard");
}

async function copyPathsToClipboard(entries = selectedEntries()) {
  const paths = entries.map((entry) => displayPath(entry.path));
  if (!paths.length) return;
  await writeClipboardText(paths.join("\r\n"));
  toast(paths.length === 1 ? "Path copied" : `${paths.length} paths copied`);
}

async function performSearch(rawQuery) {
  const token = ++state.searchToken;
  state.query = rawQuery;
  const tagMatch = rawQuery.match(/\btag:([^\s]+)/i);
  const tag = tagMatch ? tagMatch[1].toLowerCase() : null;
  const backendQuery = rawQuery.replace(/\btag:[^\s]+/ig, "").trim();

  if (!rawQuery.trim()) {
    state.searchResults = null;
    state.activeTag = null;
    state.loading = false;
    render();
    return;
  }

  state.loading = true;
  render();
  try {
    let results = [];
    if (backendQuery) {
      results = (await invoke("search_files", {
        query: backendQuery,
        path: state.currentPath,
        maxResults: 600,
        useIndexed: state.useIndexedSearch,
      })).map(normalizeEntry);
    } else if (tag) {
      results = await entriesForTag(tag);
    }

    if (tag && backendQuery) {
      results = results.filter((file) => getTag(file.path) === tag);
    }

    if (token !== state.searchToken) return;
    state.searchResults = results;
    state.activeTag = tag || null;
    state.virtual.scrollTop = 0;
    if (els.filePane) els.filePane.scrollTop = 0;
  } catch (error) {
    if (token !== state.searchToken) return;
    toast(`Search failed: ${error}`);
  } finally {
    if (token === state.searchToken) {
      state.loading = false;
      render();
    }
  }
}

async function entriesForTag(tag) {
  const paths = Object.entries(state.tags)
    .filter(([, tagId]) => tagId === tag)
    .map(([path]) => path);
  const entries = [];
  for (const path of paths) {
    try {
      entries.push(normalizeEntry(await invoke("get_file_info", { path })));
    } catch {
      delete state.tags[path];
    }
  }
  saveJson("fe-tags", state.tags);
  return entries;
}

function debounce(fn, ms) {
  let timer = null;
  return (...args) => {
    clearTimeout(timer);
    timer = setTimeout(() => fn(...args), ms);
  };
}

const debouncedSearch = debounce(performSearch, 250);

async function promptNewFolder() {
  let next = { name: "New folder" };
  try {
    next = await nextAvailableChild(state.currentPath, "New folder", "number");
  } catch {
    // The create command still reports the exact filesystem error if the fallback exists.
  }
  showNameDialog({
    title: "New folder",
    label: "Folder name",
    value: next.name,
    submitText: "Create",
    validate: validateWindowsName,
    onSubmit: async (name) => {
      const path = await invoke("join_path", { parent: state.currentPath, child: name });
      await invoke("create_directory", { path });
      await refreshCurrent();
      state.selected = new Set([path]);
      toast(`Created ${name}`);
    },
  });
}

async function promptRename(entry) {
  if (!entry) return;
  showNameDialog({
    title: "Rename",
    label: "New name",
    value: entry.name,
    submitText: "Rename",
    validate: validateWindowsName,
    onSubmit: async (name) => {
      const newPath = await invoke("rename_file", { path: entry.path, newName: name });
      if (state.tags[entry.path]) {
        state.tags[newPath] = state.tags[entry.path];
        delete state.tags[entry.path];
        saveJson("fe-tags", state.tags);
      }
      await refreshCurrent();
      state.selected = new Set([newPath]);
      toast(`Renamed to ${name}`);
    },
  });
}

function copySelected(mode) {
  const paths = selectedEntries().map((entry) => entry.path);
  if (!paths.length) return;
  state.clipboard = { mode, paths };
  toast(`${mode === "cut" ? "Cut" : "Copied"} ${paths.length} item${paths.length === 1 ? "" : "s"}`);
  renderFiles();
}

async function pasteClipboard() {
  if (!state.clipboard) return;
  const pasted = [];
  const skipped = [];
  for (const path of state.clipboard.paths) {
    const name = basename(path);
    let destination = await invoke("join_path", { parent: state.currentPath, child: name });
    if (state.clipboard.mode === "copy") {
      destination = (await nextAvailableChild(state.currentPath, name, "copy")).path;
      await invoke("copy_file", { from: path, to: destination });
      if (state.tags[path]) state.tags[destination] = state.tags[path];
    } else {
      if (samePath(path, destination)) {
        skipped.push(name);
        continue;
      }
      if (await invoke("path_exists", { path: destination })) {
        destination = (await nextAvailableChild(state.currentPath, name, "copy")).path;
      }
      await invoke("move_file", { from: path, to: destination });
      if (state.tags[path]) {
        state.tags[destination] = state.tags[path];
        delete state.tags[path];
      }
    }
    pasted.push(destination);
  }
  if (state.clipboard.mode === "cut") state.clipboard = null;
  saveJson("fe-tags", state.tags);
  await refreshCurrent();
  state.selected = new Set(pasted);
  if (pasted.length) render();
  if (pasted.length && skipped.length) toast(`Pasted ${pasted.length}; skipped ${skipped.length} unchanged item${skipped.length === 1 ? "" : "s"}`);
  else if (pasted.length) toast(`Pasted ${pasted.length} item${pasted.length === 1 ? "" : "s"}`);
  else if (skipped.length) toast("Nothing to paste here");
}

async function deleteSelected() {
  const entries = selectedEntries();
  if (!entries.length) return;
  const confirmed = await showConfirmDialog({
    title: "Move to Recycle Bin?",
    message: entries.length === 1
      ? `Move "${entries[0].name}" to the Recycle Bin?`
      : `Move ${entries.length} items to the Recycle Bin?`,
    confirmText: "Move",
    danger: true,
  });
  if (!confirmed) return;
  for (const entry of entries) {
    await invoke("delete_file", { path: entry.path });
    delete state.tags[entry.path];
  }
  saveJson("fe-tags", state.tags);
  state.selected.clear();
  await refreshCurrent();
  toast("Moved to Recycle Bin");
}

async function saveBookmarks() {
  await invoke("save_bookmarks", { bookmarks: state.bookmarks });
}

function addBookmark(path = state.currentPath, defaultName = basename(state.currentPath) || "Bookmark") {
  showNameDialog({
    title: "Add bookmark",
    label: "Bookmark name",
    value: defaultName,
    submitText: "Add",
    onSubmit: async (name) => {
      if (!state.bookmarks.some((bookmark) => bookmark.path === path)) {
        state.bookmarks.push({ name, path });
        await saveBookmarks();
        renderSidebar();
        toast("Bookmark added");
      }
    },
  });
}

function showNameDialog({ title, label, value, submitText, onSubmit, validate }) {
  els.modalRoot.innerHTML = `
    <div class="modal-scrim">
      <form class="dialog" id="name-dialog">
        <div class="dialog-head">
          <div class="dialog-title">${escapeHtml(title)}</div>
          <button type="button" class="tool-btn" data-modal-close>${icon("x", 14)}</button>
        </div>
        <div class="dialog-body">
          <label class="field-label" for="name-field">${escapeHtml(label)}</label>
          <input id="name-field" class="text-field" value="${escapeHtml(value)}" autocomplete="off" />
          <div id="name-error" class="field-error" hidden></div>
        </div>
        <div class="dialog-actions">
          <button type="button" class="secondary-btn" data-modal-close>Cancel</button>
          <button type="submit" class="primary-btn" id="name-submit">${escapeHtml(submitText)}</button>
        </div>
      </form>
    </div>`;
  const form = document.getElementById("name-dialog");
  const input = document.getElementById("name-field");
  const errorEl = document.getElementById("name-error");
  const submit = document.getElementById("name-submit");
  const showError = (message) => {
    errorEl.textContent = message;
    errorEl.hidden = false;
    input.focus();
  };
  input.focus();
  input.select();
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const next = input.value.trim();
    if (!next) {
      showError("Name cannot be empty.");
      return;
    }
    const validationError = validate?.(next);
    if (validationError) {
      showError(validationError);
      return;
    }
    submit.disabled = true;
    errorEl.hidden = true;
    try {
      await onSubmit(next);
      closeModal();
    } catch (error) {
      submit.disabled = false;
      showError(String(error));
    }
  });
}

function showConfirmDialog({ title, message, confirmText = "OK", danger = false }) {
  return new Promise((resolve) => {
    state.modalResolve?.(false);
    state.modalResolve = resolve;
    els.modalRoot.innerHTML = `
      <div class="modal-scrim">
        <form class="dialog" id="confirm-dialog">
          <div class="dialog-head">
            <div class="dialog-title">${escapeHtml(title)}</div>
            <button type="button" class="tool-btn" data-modal-close>${icon("x", 14)}</button>
          </div>
          <div class="dialog-body">
            <p class="dialog-message">${escapeHtml(message)}</p>
          </div>
          <div class="dialog-actions">
            <button type="button" class="secondary-btn" data-modal-close>Cancel</button>
            <button type="submit" class="primary-btn ${danger ? "danger" : ""}" id="confirm-submit">${escapeHtml(confirmText)}</button>
          </div>
        </form>
      </div>`;
    document.getElementById("confirm-dialog").addEventListener("submit", (event) => {
      event.preventDefault();
      closeModal(true);
    });
    document.getElementById("confirm-submit").focus();
  });
}

function closeModal(result = false) {
  if (state.modalResolve) {
    const resolve = state.modalResolve;
    state.modalResolve = null;
    resolve(result);
  }
  els.modalRoot.innerHTML = "";
}

function showContextMenu(x, y, items) {
  state.menuActions = items;
  const html = items.map((item, index) => {
    if (item.type === "sep") return `<div class="menu-sep"></div>`;
    if (item.type === "tags") {
      return `<div class="tag-row">${TAGS.map((tag) => `<button class="tag-swatch ${item.active === tag.id ? "active" : ""}" style="background:${tag.color}" data-menu-tag="${tag.id}" title="${tag.label}"></button>`).join("")}<button class="tag-swatch ${!item.active ? "active" : ""}" data-menu-tag="" title="No tag" style="background:transparent;border:1px dashed var(--border-strong)"></button></div>`;
    }
    const disabled = item.disabled ? "disabled" : "";
    const danger = item.danger ? "danger" : "";
    return `<button class="menu-item ${disabled} ${danger}" data-menu-index="${index}">
      <span>${item.icon ? icon(item.icon, 14) : ""}</span>
      <span class="menu-label">${escapeHtml(item.label)}</span>
      ${item.kbd ? `<kbd>${escapeHtml(item.kbd)}</kbd>` : ""}
    </button>`;
  }).join("");

  els.contextMenuRoot.innerHTML = `<div class="context-menu" style="left:${x}px;top:${y}px">${html}</div>`;
  const menu = els.contextMenuRoot.querySelector(".context-menu");
  const rect = menu.getBoundingClientRect();
  const nx = Math.min(x, window.innerWidth - rect.width - 8);
  const ny = Math.min(y, window.innerHeight - rect.height - 8);
  menu.style.left = `${Math.max(8, nx)}px`;
  menu.style.top = `${Math.max(8, ny)}px`;
}

function closeContextMenu() {
  els.contextMenuRoot.innerHTML = "";
  state.menuActions = [];
}

function contextItemsFor(entry) {
  const selected = selectedEntries();
  const targets = entry && state.selected.has(entry.path) ? selected : entry ? [entry] : [];
  if (!entry) {
    return [
      { label: "New folder", icon: "plus", kbd: "Ctrl+Shift+N", action: promptNewFolder },
      { label: "Paste", icon: "paste", kbd: "Ctrl+V", disabled: !state.clipboard, action: pasteClipboard },
      { type: "sep" },
      { label: "Refresh", icon: "refresh", kbd: "F5", action: refreshCurrent },
      { label: "Add current folder to Bookmarks", icon: "star", action: () => addBookmark() },
      { label: "Personalization", icon: "settings", action: () => openSettings("themes") },
    ];
  }

  const activeTag = getTag(entry.path);
  return [
    { label: isDirectory(entry) ? "Open" : "Open", icon: "open", kbd: "Enter", action: () => openEntry(entry) },
    { label: "Show in Windows Explorer", icon: "folder", action: () => revealEntry(entry) },
    { type: "sep" },
    { label: "Cut", icon: "cut", kbd: "Ctrl+X", action: () => copySelected("cut") },
    { label: "Copy", icon: "copy", kbd: "Ctrl+C", action: () => copySelected("copy") },
    { label: targets.length > 1 ? "Copy paths" : "Copy path", icon: "copy", action: () => copyPathsToClipboard(targets) },
    { label: "Paste", icon: "paste", kbd: "Ctrl+V", disabled: !state.clipboard, action: pasteClipboard },
    { type: "sep" },
    { label: "Rename", icon: "rename", kbd: "F2", disabled: targets.length !== 1, action: () => promptRename(entry) },
    { type: "tags", active: activeTag, action: (tag) => targets.forEach((target) => setTag(target.path, tag)) },
    { label: "Add to Bookmarks", icon: "star", disabled: !isDirectory(entry), action: () => addBookmark(entry.path, entry.name) },
    { type: "sep" },
    { label: targets.length > 1 ? `Move ${targets.length} items to Recycle Bin` : "Move to Recycle Bin", icon: "trash", kbd: "Delete", danger: true, action: deleteSelected },
    { label: "Properties", icon: "info", action: () => { state.showPreview = true; renderPreview(); } },
  ];
}

function openQuickLook(entry = selectedEntries()[0]) {
  if (!entry) return;
  const cached = state.previewCache.get(entry.path);
  const body = cached
    ? quickLookBody(entry, cached)
    : `<div class="spinner"></div>`;
  els.modalRoot.innerHTML = `
    <div class="modal-scrim" data-quicklook>
      <div class="quicklook">
        <div class="quicklook-head">
          <div class="dialog-title">${escapeHtml(entry.name)}</div>
          <button class="tool-btn" data-modal-close>${icon("x", 14)}</button>
        </div>
        <div class="quicklook-body">${body}</div>
      </div>
    </div>`;
  if (!cached) {
    loadPreview(entry).then(() => {
      const preview = state.previewCache.get(entry.path);
      const container = els.modalRoot.querySelector(".quicklook-body");
      if (container && preview) container.innerHTML = quickLookBody(entry, preview);
    });
  }
}

function quickLookBody(entry, preview) {
  if (preview.kind === "image" && preview.data_url) {
    return `<img src="${preview.data_url}" alt="${escapeHtml(entry.name)}" />`;
  }
  if (preview.kind === "text") {
    return `<pre class="preview-code">${escapeHtml(preview.text || "")}</pre>`;
  }
  return `<div class="preview-empty"><div>${nodeVisual(entry)}<div style="margin-top:12px">${escapeHtml(fileType(entry))}</div><div style="margin-top:4px">${formatSize(entry.size)}</div></div></div>`;
}

function openSettings(tab = "themes") {
  state.settingsTab = tab;
  renderSettings();
}

function renderSettings() {
  const tabButton = (id, label) => `<button class="settings-tab ${state.settingsTab === id ? "active" : ""}" data-settings-tab="${id}">${label}</button>`;
  const body = settingsBody();
  els.modalRoot.innerHTML = `
    <div class="modal-scrim">
      <section class="settings-panel">
        <header class="settings-head">
          <div>
            <div class="settings-title">Personalization</div>
            <div class="settings-subtitle">Themes, accent color, density, and local image backgrounds.</div>
          </div>
          <button class="tool-btn" data-modal-close>${icon("x", 14)}</button>
        </header>
        <nav class="settings-tabs">
          ${tabButton("themes", "Themes")}
          ${tabButton("accent", "Accent")}
          ${tabButton("background", "Background")}
          ${tabButton("density", "Density")}
          ${tabButton("ai", "AI")}
        </nav>
        <div class="settings-body">${body}</div>
        <footer class="settings-foot">
          <span style="margin-right:auto;color:var(--text-faint);font-size:12px">Saved automatically on this PC</span>
          <button class="primary-btn" data-modal-close>Done</button>
        </footer>
      </section>
    </div>`;
}

function settingsBody() {
  if (state.settingsTab === "themes") {
    return `<p class="settings-help">Choose a practical Windows Mica style or switch into the fun retro, fantasy, and cyberpunk directions from the design.</p>
      <div class="theme-grid">${THEMES.map((theme) => `
        <button class="theme-card ${state.prefs.theme === theme.id ? "active" : ""}" data-pref-theme="${theme.id}">
          <div class="theme-preview" style="background:${theme.swatch[0]}">
            <div class="theme-window">
              <span style="background:${theme.swatch[2]}"></span>
              <span style="background:${theme.swatch[2]}"></span>
              <span style="background:${theme.swatch[1]}"></span>
            </div>
          </div>
          <div class="theme-meta">
            <div class="theme-name">${theme.name}</div>
            <div class="theme-desc">${theme.desc}</div>
          </div>
        </button>`).join("")}</div>`;
  }

  if (state.settingsTab === "accent") {
    return `<p class="settings-help">Accent color updates selection, active controls, file badges, focus rings, and progress highlights.</p>
      <div class="accent-grid">${ACCENTS.map((accent) => `
        <button class="accent-card ${state.prefs.accent === accent.id ? "active" : ""}" data-pref-accent="${accent.id}">
          <span class="accent-dot" style="background:${accent.color}"></span>
          <span>${accent.id}</span>
        </button>`).join("")}</div>`;
  }

  if (state.settingsTab === "background") {
    return `<p class="settings-help">Use a preset or import a local image. Custom images stay in browser storage for this app; nothing leaves your machine.</p>
      <div class="wallpaper-grid">${WALLPAPERS.map((wallpaper) => `
        <button class="wallpaper-card ${state.prefs.wallpaper === wallpaper.id ? "active" : ""}" data-pref-wallpaper="${wallpaper.id}" style="--wallpaper-preview:${wallpaper.css || "linear-gradient(135deg, var(--panel-alt), var(--panel-solid))"}">
          <span class="wallpaper-name">${wallpaper.name}</span>
        </button>`).join("")}
        <button class="wallpaper-card upload-card ${state.prefs.wallpaper === "custom" ? "active" : ""}" data-upload-wallpaper>
          <span>${icon("upload", 22)}</span>
          <span>Import image</span>
        </button>
      </div>`;
  }

  if (state.settingsTab === "ai") {
    const caps = state.aiCapabilities;
    const enabled = Boolean(caps?.semantic_search);
    const status = caps ? caps.reason : "Checking local hardware and runtime support...";
    const rows = [
      ["Semantic search", caps?.semantic_search],
      ["Automatic summaries", caps?.automatic_summaries],
      ["Image classification", caps?.image_classification],
      ["Local embeddings", caps?.local_embeddings],
    ];
    return `<p class="settings-help">${escapeHtml(status)}</p>
      <div class="ai-status ${enabled ? "enabled" : "disabled"}">
        <div class="ai-chip">${enabled ? "NPU AI enabled" : caps?.npu_available ? "Runtime needed" : "NPU not detected"}</div>
        <div class="ai-rows">
          ${rows.map(([label, value]) => `<div><span>${escapeHtml(label)}</span><strong>${value ? "Enabled" : "Disabled"}</strong></div>`).join("")}
        </div>
      </div>`;
  }

  return `<p class="settings-help">Density adjusts rows, targets, and icon spacing across the file list.</p>
    <div class="density-grid">${["cozy", "comfortable", "compact"].map((density) => `
      <button class="density-card ${state.prefs.density === density ? "active" : ""}" data-pref-density="${density}">
        <span>${icon("list", 16)}</span>
        <span>${density}</span>
      </button>`).join("")}</div>`;
}

function openPalette() {
  state.paletteIndex = 0;
  renderPalette("");
}

function paletteActions() {
  const home = state.knownFolders.find((folder) => folder.id === "home");
  const selected = selectedEntries();
  return [
    { type: "action", label: "New folder", hint: "Ctrl+Shift+N", icon: "plus", run: promptNewFolder },
    { type: "action", label: "Refresh", hint: "F5", icon: "refresh", run: refreshCurrent },
    ...(selected.length ? [{ type: "action", label: selected.length === 1 ? "Copy selected path" : "Copy selected paths", hint: "", icon: "copy", run: () => copyPathsToClipboard(selected) }] : []),
    { type: "action", label: "Toggle preview pane", hint: "Ctrl+I", icon: "panel", run: () => { state.showPreview = !state.showPreview; render(); } },
    { type: "action", label: "Icon view", hint: "Ctrl+1", icon: "grid", run: () => setView("grid") },
    { type: "action", label: "Details view", hint: "Ctrl+2", icon: "list", run: () => setView("list") },
    { type: "action", label: "Gallery view", hint: "Ctrl+3", icon: "gallery", run: () => setView("gallery") },
    { type: "action", label: "Personalization", hint: "Ctrl+,", icon: "settings", run: () => openSettings("themes") },
    { type: "action", label: "AI feature status", hint: "", icon: "settings", run: () => openSettings("ai") },
    ...(home ? [{ type: "action", label: "Go to Home", hint: "", icon: "home", run: () => navigateTo(home.path) }] : []),
  ];
}

function renderPalette(query) {
  const q = query.trim().toLowerCase();
  const actions = paletteActions().filter((item) => item.label.toLowerCase().includes(q));
  const files = getDisplayFiles()
    .filter((file) => file.name.toLowerCase().includes(q) || file.path.toLowerCase().includes(q))
    .slice(0, 12)
    .map((file) => ({ type: "file", file }));
  state.paletteItems = [...actions, ...files];

  els.modalRoot.innerHTML = `
    <div class="modal-scrim">
      <section class="palette">
        <div class="palette-input">
          ${icon("search", 18)}
          <input id="palette-input" placeholder="Type a command, file, or path" value="${escapeHtml(query)}" />
          <kbd>Esc</kbd>
        </div>
        <div class="palette-list">
          ${actions.length ? `<div class="palette-section">Actions</div>` : ""}
          ${actions.map((item, i) => paletteRow(item, i)).join("")}
          ${files.length ? `<div class="palette-section">Files and folders</div>` : ""}
          ${files.map((item, i) => paletteRow(item, actions.length + i)).join("")}
          ${!state.paletteItems.length ? `<div class="empty-state" style="height:110px">No matches</div>` : ""}
        </div>
      </section>
    </div>`;
  const input = document.getElementById("palette-input");
  input.focus();
  input.setSelectionRange(input.value.length, input.value.length);
}

function paletteRow(item, index) {
  const active = state.paletteIndex === index ? "active" : "";
  if (item.type === "file") {
    return `<button class="palette-row ${active}" data-palette-index="${index}">
      <span>${nodeVisual(item.file, "small")}</span>
      <span class="label">${escapeHtml(item.file.name)}</span>
      <span class="mono">${escapeHtml(displayPath(dirname(item.file.path)))}</span>
    </button>`;
  }
  return `<button class="palette-row ${active}" data-palette-index="${index}">
    <span>${icon(item.icon, 15)}</span>
    <span class="label">${escapeHtml(item.label)}</span>
    ${item.hint ? `<kbd>${escapeHtml(item.hint)}</kbd>` : "<span></span>"}
  </button>`;
}

function runPaletteItem(index = state.paletteIndex) {
  const item = state.paletteItems[index];
  if (!item) return;
  closeModal();
  if (item.type === "file") {
    if (isDirectory(item.file)) navigateTo(item.file.path);
    else {
      state.selected = new Set([item.file.path]);
      state.lastSelectedPath = item.file.path;
      render();
    }
  } else {
    item.run();
  }
}

function setView(view) {
  state.view = view;
  state.virtual.scrollTop = 0;
  if (els.filePane) els.filePane.scrollTop = 0;
  render();
}

function toast(message) {
  const id = `toast-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
  const node = document.createElement("div");
  node.className = "toast";
  node.id = id;
  node.innerHTML = `${icon("check", 15)}<span>${escapeHtml(message)}</span>`;
  els.toastRoot.appendChild(node);
  setTimeout(() => node.remove(), 4200);
}

function setupEvents() {
  els.tabs.addEventListener("click", (event) => {
    const close = event.target.closest("[data-close-tab]");
    if (close) {
      event.stopPropagation();
      closeTab(close.dataset.closeTab);
      return;
    }
    const tab = event.target.closest("[data-tab]");
    if (tab) switchTab(tab.dataset.tab);
  });

  els.newTabBtn.addEventListener("click", () => createTab(state.currentPath));
  els.backBtn.addEventListener("click", goBack);
  els.forwardBtn.addEventListener("click", goForward);
  els.upBtn.addEventListener("click", goUp);
  els.refreshBtn.addEventListener("click", refreshCurrent);
  els.viewGrid.addEventListener("click", () => setView("grid"));
  els.viewList.addEventListener("click", () => setView("list"));
  els.viewGallery.addEventListener("click", () => setView("gallery"));
  els.previewToggle.addEventListener("click", () => { state.showPreview = !state.showPreview; render(); });
  els.paletteBtn.addEventListener("click", openPalette);
  els.settingsBtn.addEventListener("click", () => openSettings("themes"));

  els.minimizeBtn.addEventListener("click", () => invoke("minimize_window", {}));
  els.maximizeBtn.addEventListener("click", () => invoke("toggle_maximize_window", {}));
  els.closeBtn.addEventListener("click", () => invoke("close_window", {}));

  els.pathControl.addEventListener("click", (event) => {
    if (event.target.closest("[data-crumb]")) return;
    editAddress();
  });
  els.breadcrumb.addEventListener("click", (event) => {
    const crumb = event.target.closest("[data-crumb]");
    if (crumb) navigateTo(crumb.dataset.crumb);
  });
  els.addressBar.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      els.pathControl.classList.remove("editing");
      navigateTo(els.addressBar.value.trim());
    }
    if (event.key === "Escape") {
      els.pathControl.classList.remove("editing");
      els.addressBar.value = displayPath(state.currentPath);
    }
  });
  els.addressBar.addEventListener("blur", () => els.pathControl.classList.remove("editing"));

  els.searchInput.addEventListener("input", (event) => {
    state.query = event.target.value;
    debouncedSearch(event.target.value);
  });

  let scrollFrame = null;
  els.filePane.addEventListener("scroll", () => {
    updateVirtualMetrics();
    if (scrollFrame) return;
    scrollFrame = requestAnimationFrame(() => {
      scrollFrame = null;
      if (getDisplayFiles().length > 220) renderFiles();
    });
  });

  els.sidebar.addEventListener("click", async (event) => {
    const nav = event.target.closest("[data-nav]");
    if (nav) {
      navigateTo(nav.dataset.nav);
      return;
    }
    const bookmark = event.target.closest("[data-bookmark]");
    if (bookmark) {
      const item = state.bookmarks[Number(bookmark.dataset.bookmark)];
      if (item) navigateTo(item.path);
      return;
    }
    const tag = event.target.closest("[data-tag-filter]");
    if (tag) {
      const tagId = tag.dataset.tagFilter;
      state.activeTag = state.activeTag === tagId ? null : tagId;
      state.query = state.activeTag ? `tag:${state.activeTag}` : "";
      els.searchInput.value = state.query;
      if (state.activeTag) {
        state.loading = true;
        render();
        state.searchResults = await entriesForTag(state.activeTag);
        state.loading = false;
      } else {
        state.searchResults = null;
      }
      render();
      return;
    }
    if (event.target.closest("[data-add-bookmark]")) addBookmark();
  });

  els.filePane.addEventListener("click", (event) => {
    if (event.target.closest(".rename-input")) return;
    const entryEl = event.target.closest("[data-path]");
    if (!entryEl) {
      clearSelection();
      return;
    }
    selectFile(entryEl.dataset.path, event);
  });

  els.filePane.addEventListener("dblclick", (event) => {
    const entryEl = event.target.closest("[data-path]");
    if (!entryEl || event.target.closest(".rename-input")) return;
    const entry = findEntry(entryEl.dataset.path);
    openEntry(entry).catch((error) => toast(String(error)));
  });

  els.filePane.addEventListener("contextmenu", (event) => {
    event.preventDefault();
    const entryEl = event.target.closest("[data-path]");
    if (entryEl) {
      const entry = findEntry(entryEl.dataset.path);
      if (!state.selected.has(entry.path)) {
        state.selected = new Set([entry.path]);
        state.lastSelectedPath = entry.path;
        renderFiles();
        renderPreview();
      }
      showContextMenu(event.clientX, event.clientY, contextItemsFor(entry));
    } else {
      clearSelection();
      showContextMenu(event.clientX, event.clientY, contextItemsFor(null));
    }
  });

  els.filePane.addEventListener("focusout", async (event) => {
    const input = event.target.closest(".rename-input");
    if (!input) return;
    const entry = findEntry(input.dataset.rename);
    state.renamingPath = null;
    if (entry && input.value.trim() && input.value.trim() !== entry.name) {
      try {
        await invoke("rename_file", { path: entry.path, newName: input.value.trim() });
        await refreshCurrent();
      } catch (error) {
        toast(String(error));
      }
    } else {
      renderFiles();
    }
  });

  els.filePane.addEventListener("keydown", (event) => {
    const input = event.target.closest(".rename-input");
    if (!input) return;
    if (event.key === "Enter") input.blur();
    if (event.key === "Escape") {
      state.renamingPath = null;
      renderFiles();
    }
  });

  els.filePane.addEventListener("click", (event) => {
    const sort = event.target.closest("[data-sort]");
    if (!sort) return;
    const key = sort.dataset.sort;
    if (state.sortBy === key) state.sortDir = state.sortDir === "asc" ? "desc" : "asc";
    else {
      state.sortBy = key;
      state.sortDir = "asc";
    }
    renderFiles();
  });

  els.previewPane.addEventListener("click", (event) => {
    if (event.target.closest("[data-close-preview]")) {
      state.showPreview = false;
      render();
    }
  });

  els.contextMenuRoot.addEventListener("click", async (event) => {
    const tag = event.target.closest("[data-menu-tag]");
    if (tag) {
      const item = state.menuActions.find((action) => action.type === "tags");
      item?.action(tag.dataset.menuTag || null);
      closeContextMenu();
      return;
    }
    const button = event.target.closest("[data-menu-index]");
    if (!button) return;
    const item = state.menuActions[Number(button.dataset.menuIndex)];
    closeContextMenu();
    try {
      await item?.action?.();
    } catch (error) {
      toast(String(error));
    }
  });

  document.addEventListener("mousedown", (event) => {
    if (!event.target.closest(".context-menu")) closeContextMenu();
  });

  els.modalRoot.addEventListener("click", (event) => {
    if (event.target.classList.contains("modal-scrim") || event.target.closest("[data-modal-close]")) {
      closeModal();
      return;
    }
    const tab = event.target.closest("[data-settings-tab]");
    if (tab) {
      state.settingsTab = tab.dataset.settingsTab;
      renderSettings();
      return;
    }
    const theme = event.target.closest("[data-pref-theme]");
    if (theme) {
      state.prefs.theme = theme.dataset.prefTheme;
      applyPrefs();
      renderSettings();
      return;
    }
    const accent = event.target.closest("[data-pref-accent]");
    if (accent) {
      state.prefs.accent = accent.dataset.prefAccent;
      applyPrefs();
      renderSettings();
      return;
    }
    const density = event.target.closest("[data-pref-density]");
    if (density) {
      state.prefs.density = density.dataset.prefDensity;
      applyPrefs();
      renderSettings();
      render();
      return;
    }
    const wallpaper = event.target.closest("[data-pref-wallpaper]");
    if (wallpaper) {
      state.prefs.wallpaper = wallpaper.dataset.prefWallpaper;
      applyPrefs();
      renderSettings();
      return;
    }
    if (event.target.closest("[data-upload-wallpaper]")) {
      els.wallpaperFile.click();
      return;
    }
    const paletteRowEl = event.target.closest("[data-palette-index]");
    if (paletteRowEl) {
      runPaletteItem(Number(paletteRowEl.dataset.paletteIndex));
    }
  });

  els.modalRoot.addEventListener("input", (event) => {
    if (event.target.id === "palette-input") {
      state.paletteIndex = 0;
      renderPalette(event.target.value);
    }
  });

  els.modalRoot.addEventListener("keydown", (event) => {
    if (event.target.id !== "palette-input") return;
    if (event.key === "Escape") closeModal();
    if (event.key === "ArrowDown") {
      event.preventDefault();
      state.paletteIndex = Math.min(state.paletteIndex + 1, state.paletteItems.length - 1);
      renderPalette(event.target.value);
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      state.paletteIndex = Math.max(state.paletteIndex - 1, 0);
      renderPalette(event.target.value);
    }
    if (event.key === "Enter") {
      event.preventDefault();
      runPaletteItem();
    }
  });

  els.wallpaperFile.addEventListener("change", (event) => {
    const file = event.target.files?.[0];
    if (!file) return;
    const reader = new FileReader();
    reader.onload = () => {
      state.prefs.wallpaper = "custom";
      state.prefs.customWallpaper = reader.result;
      applyPrefs();
      renderSettings();
      toast("Background image imported");
    };
    reader.readAsDataURL(file);
  });

  setupRubberBand();
  setupKeyboard();
}

function editAddress() {
  els.pathControl.classList.add("editing");
  els.addressBar.value = displayPath(state.currentPath);
  els.addressBar.focus();
  els.addressBar.select();
}

function setupRubberBand() {
  let start = null;
  let initial = null;
  const rubber = document.getElementById("rubber-band");

  els.filePane.addEventListener("pointerdown", (event) => {
    if (event.button !== 0 || event.target.closest("[data-path], .list-head, .rename-input")) return;
    start = { x: event.clientX, y: event.clientY };
    initial = event.ctrlKey ? new Set(state.selected) : new Set();
    if (!event.ctrlKey) state.selected.clear();
    rubber.hidden = false;
    rubber.style.left = `${start.x}px`;
    rubber.style.top = `${start.y}px`;
    rubber.style.width = "0px";
    rubber.style.height = "0px";
    els.filePane.setPointerCapture(event.pointerId);
  });

  els.filePane.addEventListener("pointermove", (event) => {
    if (!start) return;
    const left = Math.min(start.x, event.clientX);
    const top = Math.min(start.y, event.clientY);
    const width = Math.abs(start.x - event.clientX);
    const height = Math.abs(start.y - event.clientY);
    rubber.style.left = `${left}px`;
    rubber.style.top = `${top}px`;
    rubber.style.width = `${width}px`;
    rubber.style.height = `${height}px`;

    const band = { left, top, right: left + width, bottom: top + height };
    const next = new Set(initial);
    els.filePane.querySelectorAll("[data-path]").forEach((node) => {
      const rect = node.getBoundingClientRect();
      const overlaps = !(rect.right < band.left || rect.left > band.right || rect.bottom < band.top || rect.top > band.bottom);
      if (overlaps) next.add(node.dataset.path);
    });
    state.selected = next;
    renderFiles();
    renderPreview();
    rubber.hidden = false;
  });

  function stop(event) {
    if (!start) return;
    start = null;
    initial = null;
    rubber.hidden = true;
    try {
      els.filePane.releasePointerCapture(event.pointerId);
    } catch {
      // Pointer may already be released.
    }
  }

  els.filePane.addEventListener("pointerup", stop);
  els.filePane.addEventListener("pointercancel", stop);
}

function setupKeyboard() {
  document.addEventListener("keydown", async (event) => {
    const target = event.target;
    const inInput = target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable);

    if (event.key === "Escape") {
      closeContextMenu();
      if (els.modalRoot.innerHTML) closeModal();
      if (inInput) target.blur();
      return;
    }

    if (event.ctrlKey && event.key.toLowerCase() === "p") {
      event.preventDefault();
      openPalette();
      return;
    }
    if (event.ctrlKey && event.key === ",") {
      event.preventDefault();
      openSettings("themes");
      return;
    }
    if (event.ctrlKey && event.key.toLowerCase() === "f") {
      event.preventDefault();
      els.searchInput.focus();
      els.searchInput.select();
      return;
    }
    if (event.ctrlKey && event.key.toLowerCase() === "l") {
      event.preventDefault();
      editAddress();
      return;
    }
    if (event.ctrlKey && event.key.toLowerCase() === "t") {
      event.preventDefault();
      createTab(state.currentPath);
      return;
    }

    if (inInput) return;

    if (event.altKey && event.key === "ArrowLeft") {
      event.preventDefault();
      goBack();
      return;
    }
    if (event.altKey && event.key === "ArrowRight") {
      event.preventDefault();
      goForward();
      return;
    }
    if (event.altKey && event.key === "ArrowUp") {
      event.preventDefault();
      goUp();
      return;
    }

    if (event.key === "F5") {
      event.preventDefault();
      refreshCurrent();
      return;
    }
    if (event.key === "F2") {
      const entry = selectedEntries()[0];
      if (entry) {
        event.preventDefault();
        state.renamingPath = entry.path;
        renderFiles();
      }
      return;
    }
    if (event.key === "Delete") {
      event.preventDefault();
      deleteSelected().catch((error) => toast(String(error)));
      return;
    }
    if (event.key === " " && selectedEntries()[0]) {
      event.preventDefault();
      openQuickLook();
      return;
    }
    if (event.key === "Enter" && selectedEntries()[0]) {
      event.preventDefault();
      openEntry(selectedEntries()[0]).catch((error) => toast(String(error)));
      return;
    }

    if (event.ctrlKey && event.shiftKey && event.key.toLowerCase() === "c") {
      const selected = selectedEntries();
      if (selected.length) {
        event.preventDefault();
        copyPathsToClipboard(selected).catch((error) => toast(String(error)));
      }
      return;
    }
    if (event.ctrlKey && event.key.toLowerCase() === "c") {
      event.preventDefault();
      copySelected("copy");
      return;
    }
    if (event.ctrlKey && event.key.toLowerCase() === "x") {
      event.preventDefault();
      copySelected("cut");
      return;
    }
    if (event.ctrlKey && event.key.toLowerCase() === "v") {
      event.preventDefault();
      pasteClipboard().catch((error) => toast(String(error)));
      return;
    }
    if (event.ctrlKey && event.key.toLowerCase() === "a") {
      event.preventDefault();
      state.selected = new Set(getDisplayFiles().map((file) => file.path));
      render();
      return;
    }
    if (event.ctrlKey && event.shiftKey && event.key.toLowerCase() === "n") {
      event.preventDefault();
      promptNewFolder();
      return;
    }
    if (event.ctrlKey && event.key.toLowerCase() === "i") {
      event.preventDefault();
      state.showPreview = !state.showPreview;
      render();
      return;
    }
    if (event.ctrlKey && ["1", "2", "3"].includes(event.key)) {
      event.preventDefault();
      setView({ "1": "grid", "2": "list", "3": "gallery" }[event.key]);
      return;
    }

    if (["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight"].includes(event.key)) {
      const files = getDisplayFiles();
      if (!files.length) return;
      event.preventDefault();
      const current = files.findIndex((file) => file.path === state.lastSelectedPath);
      const columns = state.view === "list" ? 1 : Math.max(1, Math.floor(els.filePane.clientWidth / Number.parseInt(getComputedStyle(document.documentElement).getPropertyValue("--grid-w"), 10)));
      let next = current < 0 ? 0 : current;
      if (event.key === "ArrowDown") next = Math.min(files.length - 1, next + columns);
      if (event.key === "ArrowUp") next = Math.max(0, next - columns);
      if (event.key === "ArrowRight") next = Math.min(files.length - 1, next + 1);
      if (event.key === "ArrowLeft") next = Math.max(0, next - 1);
      state.selected = new Set([files[next].path]);
      state.lastSelectedPath = files[next].path;
      ensureIndexVisible(next);
      render();
      [...els.filePane.querySelectorAll("[data-path]")]
        .find((node) => node.dataset.path === files[next].path)
        ?.scrollIntoView({ block: "nearest" });
    }
  });
}

async function init() {
  Object.assign(els, {
    tabs: document.getElementById("tabs"),
    newTabBtn: document.getElementById("new-tab-btn"),
    sidebar: document.getElementById("sidebar"),
    backBtn: document.getElementById("back-btn"),
    forwardBtn: document.getElementById("forward-btn"),
    upBtn: document.getElementById("up-btn"),
    refreshBtn: document.getElementById("refresh-btn"),
    pathControl: document.getElementById("path-control"),
    breadcrumb: document.getElementById("breadcrumb"),
    addressBar: document.getElementById("address-bar"),
    searchInput: document.getElementById("search-input"),
    viewGrid: document.getElementById("view-grid"),
    viewList: document.getElementById("view-list"),
    viewGallery: document.getElementById("view-gallery"),
    previewToggle: document.getElementById("preview-toggle"),
    paletteBtn: document.getElementById("palette-btn"),
    settingsBtn: document.getElementById("settings-btn"),
    filePane: document.getElementById("file-pane"),
    previewPane: document.getElementById("preview-pane"),
    main: document.getElementById("main"),
    statusBar: document.getElementById("status-bar"),
    contextMenuRoot: document.getElementById("context-menu-root"),
    modalRoot: document.getElementById("modal-root"),
    toastRoot: document.getElementById("toast-root"),
    wallpaperFile: document.getElementById("wallpaper-file"),
    minimizeBtn: document.getElementById("minimize-btn"),
    maximizeBtn: document.getElementById("maximize-btn"),
    closeBtn: document.getElementById("close-btn"),
  });

  document.getElementById("new-tab-btn").innerHTML = icon("plus", 14);
  document.getElementById("back-btn").innerHTML = icon("back", 16);
  document.getElementById("forward-btn").innerHTML = icon("forward", 16);
  document.getElementById("up-btn").innerHTML = icon("up", 15);
  document.getElementById("refresh-btn").innerHTML = icon("refresh", 15);
  document.getElementById("view-grid").innerHTML = icon("grid", 15);
  document.getElementById("view-list").innerHTML = icon("list", 15);
  document.getElementById("view-gallery").innerHTML = icon("gallery", 15);
  document.getElementById("preview-toggle").innerHTML = icon("panel", 15);
  document.getElementById("palette-btn").innerHTML = icon("palette", 15);
  document.getElementById("settings-btn").innerHTML = icon("settings", 15);
  document.getElementById("search-icon").innerHTML = icon("search", 14);

  applyPrefs();
  setupEvents();

  try {
    const [knownFolders, drives, bookmarks, home] = await Promise.all([
      invoke("get_known_folders"),
      invoke("get_drives"),
      invoke("get_bookmarks"),
      invoke("get_home_directory"),
    ]);
    state.knownFolders = knownFolders;
    state.drives = drives;
    state.bookmarks = bookmarks;
    state.currentPath = home;
    state.tabs = [{ id: "tab-home", path: home, name: basename(home) || "Home", history: [home], hIndex: 0 }];
    state.activeTabId = "tab-home";
    await navigateTo(home, { pushHistory: false });
    const commonPaths = [...new Set([
      home,
      ...knownFolders.map((folder) => folder.path),
      ...bookmarks.map((bookmark) => bookmark.path),
    ].filter(Boolean))];
    invoke("prefetch_paths", { paths: commonPaths }).catch(() => {});
    invoke("watch_paths", { paths: commonPaths }).catch(() => {});
    invoke("get_ai_capabilities", {}).then((capabilities) => {
      state.aiCapabilities = capabilities;
      if (state.settingsTab === "ai" && els.modalRoot.innerHTML) renderSettings();
    }).catch(() => {
      state.aiCapabilities = {
        npu_available: false,
        semantic_search: false,
        automatic_summaries: false,
        image_classification: false,
        local_embeddings: false,
        reason: "Could not inspect local AI hardware. Local AI features are disabled.",
      };
    });
  } catch (error) {
    state.loadError = String(error);
    render();
    toast(String(error));
  }
}

document.addEventListener("DOMContentLoaded", init);
