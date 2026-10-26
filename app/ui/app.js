"use strict";

const T = window.__TAURI__;
const invoke = T.core.invoke;
const listen = T.event.listen;

const $ = (id) => document.getElementById(id);

// ---------------------------------------------------------------------------
// Formatting

const nf1 = new Intl.NumberFormat("id-ID", { maximumFractionDigits: 1 });
const nf2 = new Intl.NumberFormat("id-ID", { maximumFractionDigits: 2 });
const nf0 = new Intl.NumberFormat("id-ID", { maximumFractionDigits: 0 });

function bytes(n) {
  if (n < 1024) return `${nf0.format(n)} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024, i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${(v >= 100 ? nf0 : v >= 10 ? nf1 : nf2).format(v)} ${units[i]}`;
}

function duration(sec) {
  if (!isFinite(sec)) return "–";
  if (sec < 1) return "< 1 dtk";
  if (sec < 60) return `${Math.round(sec)} dtk`;
  const m = Math.floor(sec / 60), s = Math.round(sec % 60);
  if (m < 60) return s ? `${m} mnt ${s} dtk` : `${m} mnt`;
  return `${Math.floor(m / 60)} jam ${m % 60} mnt`;
}

const dateFmt = new Intl.DateTimeFormat("id-ID", { day: "numeric", month: "short", year: "numeric" });
const timeFmt = new Intl.DateTimeFormat("id-ID", { hour: "2-digit", minute: "2-digit" });

function when(ms) {
  const d = new Date(ms), now = new Date();
  const days = Math.floor((now.setHours(0, 0, 0, 0) - new Date(ms).setHours(0, 0, 0, 0)) / 864e5);
  if (days === 0) return `Hari ini, ${timeFmt.format(d)}`;
  if (days === 1) return `Kemarin, ${timeFmt.format(d)}`;
  return dateFmt.format(d);
}

const KIND = {
  Dokumen: ["pdf", "doc", "docx", "odt", "rtf", "txt", "md", "pages", "xls", "xlsx", "ods", "csv", "ppt", "pptx", "key"],
  Gambar: ["jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "avif", "svg", "bmp", "tif", "tiff", "psd", "raw", "cr2", "nef"],
  Video: ["mp4", "mov", "mkv", "avi", "webm", "m4v", "wmv"],
  Audio: ["mp3", "wav", "flac", "m4a", "aac", "ogg", "opus"],
  Arsip: ["czip", "zip", "7z", "rar", "gz", "xz", "zst", "tar", "tgz", "bz2"],
  Kode: ["rs", "js", "ts", "py", "go", "java", "c", "h", "cpp", "cs", "rb", "php", "html", "css", "json", "toml", "yaml", "yml", "sh"],
};

function kindOf(name) {
  const ext = name.includes(".") ? name.split(".").pop().toLowerCase() : "";
  for (const [k, list] of Object.entries(KIND)) if (list.includes(ext)) return k;
  return "File";
}

function stemOf(name) {
  const i = name.indexOf(".", 1);
  return i > 0 ? name.slice(0, i) : name;
}

// Small DOM builder; text always goes in as text nodes.
function h(tag, attrs = {}, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k === "class") el.className = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const kid of kids.flat()) if (kid != null) el.append(kid instanceof Node ? kid : String(kid));
  return el;
}

function icon(name, cls = "") {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", `ico ${cls}`);
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
  use.setAttribute("href", `#i-${name}`);
  svg.append(use);
  return svg;
}

// ---------------------------------------------------------------------------
// State

const FORMATS = [
  { id: "czip", label: ".czip", note: "Terkecil & tercepat", pw: "XChaCha20 + Argon2id" },
  { id: "zip", label: ".zip", note: "Bisa dibuka di mana saja", pw: "AES-256" },
  { id: "7z", label: ".7z", note: "Untuk pengguna 7-Zip", pw: "AES-256" },
  { id: "tar.zst", label: ".tar.zst", note: "Linux & macOS", pw: null },
];

const LEVELS = {
  fast: { title: "Cepat", sub: { czip: "Zstd 3 · paling kencang", zip: "Deflate 1", "7z": "LZMA2 1", "tar.zst": "Zstd 3" } },
  balanced: { title: "Seimbang", sub: { czip: "Zstd 9 · direkomendasikan", zip: "Deflate 6 · direkomendasikan", "7z": "LZMA2 5 · direkomendasikan", "tar.zst": "Zstd 9 · direkomendasikan" } },
  max: { title: "Maksimal", sub: { czip: "Zstd 19 + jendela 128 MB · terkecil", zip: "Deflate 9", "7z": "LZMA2 9 · lambat", "tar.zst": "Zstd 19 + jendela 128 MB" } },
};

const ERRORS = {
  io: "Tidak bisa membaca atau menulis file",
  unknown_format: "Format arsip tidak dikenali",
  corrupt: "Arsip rusak atau tidak lengkap",
  wrong_password: "Kata sandi salah",
  password_required: "Arsip ini dikunci dengan kata sandi",
  cancelled: "Dibatalkan",
  unsupported: "Tidak didukung",
  unsafe_path: "Arsip berisi path berbahaya, ekstraksi dihentikan",
  empty: "Tidak ada file untuk dikompres",
  busy: "Masih ada proses yang berjalan",
  zip: "Gagal memproses arsip zip",
  "7z": "Gagal memproses arsip 7z",
};

const state = {
  tab: "compress",
  view: "home",
  queue: [],          // scan items: { path, name, isDir, bytes, files }
  format: localStorage.getItem("czip.format") || "czip",
  level: localStorage.getItem("czip.level") || "balanced",
  name: "",
  nameTouched: false,
  outDir: "",
  job: null,          // { kind, started, samples: [[t, bytes]], title }
  result: null,       // last compress-done / extract-done payload + kind
  archive: null,      // listing from list_archive
  archivePassword: null,
  pendingArchive: null,
  cwd: "",
  selected: new Set(),
  search: "",
  dest: "",
};

function save(key, value) {
  try { localStorage.setItem(key, value); } catch (_) { /* storage may be unavailable */ }
}

function recent() {
  try { return JSON.parse(localStorage.getItem("czip.recent") || "[]"); } catch (_) { return []; }
}

// ---------------------------------------------------------------------------
// Views

const VIEWS = ["home", "queue", "progress", "done", "extract"];

function show(view) {
  state.view = view;
  for (const v of VIEWS) $(`view-${v}`).hidden = v !== view;
  const kind = view === "progress" ? state.job?.kind : view === "done" ? state.result?.kind : null;
  const tab = view === "extract" || kind === "extract" ? "extract" : "compress";
  state.tab = tab;
  for (const b of document.querySelectorAll(".tab")) {
    if (b.dataset.tab === tab) b.setAttribute("aria-current", "page");
    else b.removeAttribute("aria-current");
  }
  if (view === "home") renderRecent();
  document.querySelector("main").scrollTop = 0;
}

let toastTimer;
function toast(title, detail = "", bad = true) {
  $("toast-title").textContent = title;
  $("toast-detail").textContent = detail;
  $("toast").classList.toggle("bad", bad);
  $("toast").hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { $("toast").hidden = true; }, 7000);
}

function showError(err) {
  const code = err?.code || "io";
  const title = ERRORS[code] || "Terjadi kesalahan";
  const detail = err?.message && err.message.toLowerCase() !== title.toLowerCase() ? err.message : "";
  toast(title, detail);
}

// ---- Home

function renderRecent() {
  const items = recent();
  $("recent").hidden = items.length === 0;
  $("recent-list").replaceChildren(...items.map((r) =>
    h("li", { class: "clickable", title: r.path, onclick: () => invoke("reveal", { path: r.path }).catch(showError) },
      h("div", { class: "file-icon" }, icon("archive")),
      h("div", { class: "item-text" },
        h("span", { class: "item-name ellipsis" }, r.name),
        h("span", { class: "item-meta" }, when(r.at))),
      h("span", { class: "item-size" }, bytes(r.size)),
      r.saved > 0 ? h("span", { class: "pill ok" }, `hemat ${Math.round(r.saved * 100)}%`) : null)));
}

// ---- Queue

async function addPaths(paths) {
  if (!paths.length) return;
  const all = [...new Set([...state.queue.map((q) => q.path), ...paths])];
  try {
    const res = await invoke("scan_paths", { paths: all });
    state.queue = res.items;
    if (!state.outDir) state.outDir = await invoke("default_output_dir", { paths: all });
    if (res.skipped) toast(`${nf0.format(res.skipped)} item dilewati`, "Shortcut (symlink) dan file yang tidak bisa dibaca tidak ikut dikompres.", false);
    if (!state.nameTouched) state.name = defaultName();
    renderQueue();
    show("queue");
  } catch (e) {
    showError(e);
  }
}

function defaultName() {
  if (state.queue.length === 1) {
    const q = state.queue[0];
    return q.isDir ? q.name : stemOf(q.name);
  }
  const d = new Date();
  const pad = (n) => String(n).padStart(2, "0");
  return `Arsip-${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

function renderQueue() {
  const q = state.queue;
  if (!q.length) { show("home"); return; }
  const files = q.reduce((a, i) => a + i.files, 0);
  const total = q.reduce((a, i) => a + i.bytes, 0);
  $("q-title").textContent = `${nf0.format(q.length)} item siap dikompres`;
  $("q-summary").textContent = `${nf0.format(files)} file · total ${bytes(total)}`;
  $("queue-list").replaceChildren(...q.map((item) =>
    h("li", {},
      h("div", { class: `file-icon${item.isDir ? " folder" : ""}` }, icon(item.isDir ? "folder" : "file")),
      h("div", { class: "item-text" },
        h("span", { class: "item-name ellipsis", title: item.path }, item.name),
        h("span", { class: "item-meta" }, item.isDir ? `Folder · ${nf0.format(item.files)} file` : kindOf(item.name))),
      h("span", { class: "item-size" }, bytes(item.bytes)),
      h("button", {
        type: "button", class: "icon-btn", "aria-label": `Hapus ${item.name} dari daftar`,
        onclick: () => {
          state.queue = state.queue.filter((x) => x !== item);
          if (!state.nameTouched) state.name = defaultName();
          renderQueue();
        },
      }, icon("x")))));
  $("go-compress").firstChild.textContent = `Kompres ${nf0.format(q.length)} item`;
  renderSettings();
}

function renderSettings() {
  const fmt = FORMATS.find((f) => f.id === state.format) || FORMATS[0];
  $("arc-name").value = state.name;
  $("arc-ext").textContent = fmt.label;
  $("out-dir").textContent = state.outDir || "–";
  $("out-dir").title = state.outDir;

  $("format-options").replaceChildren(...FORMATS.map((f) =>
    h("button", {
      type: "button", class: "option", "aria-pressed": String(f.id === state.format),
      onclick: () => { state.format = f.id; save("czip.format", f.id); renderSettings(); },
    }, h("span", { class: "t" }, f.label), h("span", { class: "d" }, f.note))));

  $("level-options").replaceChildren(...Object.entries(LEVELS).map(([id, l]) =>
    h("button", {
      type: "button", class: "level", "aria-pressed": String(id === state.level),
      onclick: () => { state.level = id; save("czip.level", id); renderSettings(); },
    },
    h("span", { class: "radio" }),
    h("span", { class: "grow" }, h("span", { class: "t" }, l.title), h("span", { class: "d" }, l.sub[fmt.id])))));

  const pwOk = !!fmt.pw;
  $("use-pw").disabled = !pwOk;
  if (!pwOk) $("use-pw").checked = false;
  $("pw-hint").textContent = pwOk ? `(${fmt.pw})` : "(tidak didukung .tar.zst)";
  $("pw-fields").hidden = !$("use-pw").checked;
}

async function startCompress() {
  let password = null;
  if ($("use-pw").checked) {
    password = $("pw1").value;
    if (!password) { toast("Isi kata sandi dulu"); $("pw1").focus(); return; }
    if (password !== $("pw2").value) { toast("Kata sandi tidak sama", "Ketik ulang di kolom kedua."); $("pw2").focus(); return; }
  }
  try {
    const output = await invoke("resolve_output", { dir: state.outDir, name: state.name, format: state.format });
    const fileName = output.split(/[\\/]/).pop();
    beginJob("compress", `Mengompres ${fileName}`,
      `${LEVELS[state.level].title} · ${LEVELS[state.level].sub[state.format].split(" · ")[0]} · semua inti CPU`);
    await invoke("start_compress", {
      req: { paths: state.queue.map((q) => q.path), output, format: state.format, level: state.level, password },
    });
  } catch (e) {
    state.job = null;
    show("queue");
    showError(e);
  }
}

// ---- Progress

function beginJob(kind, title, sub) {
  state.job = { kind, started: performance.now(), samples: [], title };
  $("p-title").textContent = title;
  $("p-sub").textContent = sub;
  $("p-pct").textContent = "0%";
  $("p-bar").firstElementChild.style.width = "0%";
  $("p-bar").setAttribute("aria-valuenow", "0");
  for (const id of ["p-done", "p-speed", "p-files", "p-eta"]) $(id).textContent = "–";
  $("p-current").textContent = "Menyiapkan…";
  show("progress");
}

function onProgress(p) {
  const job = state.job;
  if (!job || job.kind !== p.kind) return;
  const now = performance.now();
  job.samples.push([now, p.doneBytes]);
  while (job.samples.length > 2 && now - job.samples[0][0] > 3000) job.samples.shift();
  const [t0, b0] = job.samples[0];
  const speed = now > t0 ? ((p.doneBytes - b0) / (now - t0)) * 1000 : 0;
  const frac = p.totalBytes ? Math.min(1, p.doneBytes / p.totalBytes) : 0;
  const pct = Math.floor(frac * 100);
  $("p-pct").textContent = `${pct}%`;
  $("p-bar").firstElementChild.style.width = `${frac * 100}%`;
  $("p-bar").setAttribute("aria-valuenow", String(pct));
  $("p-done").textContent = `${bytes(p.doneBytes)} / ${bytes(p.totalBytes)}`;
  $("p-speed").textContent = speed > 0 ? `${bytes(speed)}/dtk` : "–";
  $("p-files").textContent = nf0.format(p.filesDone);
  $("p-eta").textContent = speed > 0 && now - job.started > 1500 ? `~${duration((p.totalBytes - p.doneBytes) / speed)}` : "–";
  if (p.current) $("p-current").textContent = p.current;
}

// ---- Done

function onCompressDone(r) {
  state.job = null;
  state.result = { kind: "compress", ...r };
  const saved = r.inputBytes ? Math.max(0, 1 - r.outputBytes / r.inputBytes) : 0;
  const name = r.output.split(/[\\/]/).pop();
  $("d-title").textContent = `Selesai dalam ${duration(r.elapsedMs / 1000)}`;
  const speed = r.elapsedMs ? r.inputBytes / (r.elapsedMs / 1000) : 0;
  $("d-sub").textContent = `${nf0.format(r.files)} file · ${bytes(speed)}/dtk · integritas tiap file dicatat`;
  $("d-sizes").hidden = false;
  $("d-before").textContent = bytes(r.inputBytes);
  $("d-after").textContent = bytes(r.outputBytes);
  $("d-saved").textContent = `hemat ${Math.round(saved * 100)}%`;
  $("d-name").textContent = name;
  $("d-path").textContent = r.output;
  $("d-reveal-label").textContent = "Tampilkan di folder";
  $("d-again").textContent = "Kompres lagi";
  $("d-icon").setAttribute("href", "#i-archive");
  if (r.skipped) toast(`${nf0.format(r.skipped)} item dilewati`, "Shortcut (symlink) dan file yang tidak bisa dibaca tidak ikut dikompres.", false);

  const list = recent().filter((x) => x.path !== r.output);
  list.unshift({ name, path: r.output, size: r.outputBytes, saved, at: Date.now() });
  save("czip.recent", JSON.stringify(list.slice(0, 6)));

  state.queue = [];
  state.nameTouched = false;
  $("pw1").value = $("pw2").value = "";
  show("done");
}

function onExtractDone(r) {
  const opens = state.job?.opens || r.dest;
  state.job = null;
  state.result = { kind: "extract", ...r, opens };
  $("d-title").textContent = `Ekstrak selesai dalam ${duration(r.elapsedMs / 1000)}`;
  const verified = state.archive?.format === "czip" ? " · semua checksum cocok" : "";
  $("d-sub").textContent = `${nf0.format(r.files)} file · ${bytes(r.bytes)}${verified}`;
  $("d-icon").setAttribute("href", "#i-folder");
  $("d-sizes").hidden = true;
  $("d-name").textContent = opens.split(/[\\/]/).pop();
  $("d-path").textContent = opens;
  $("d-reveal-label").textContent = "Buka folder";
  $("d-again").textContent = "Ekstrak arsip lain";
  show("done");
  if ($("x-open-after").checked) invoke("open_folder", { path: opens }).catch(() => {});
}

function onJobError(err) {
  const kind = state.job?.kind;
  state.job = null;
  if (kind === "extract") {
    if (err.code === "password_required" || err.code === "wrong_password") {
      show("extract");
      askPassword(state.archive?.path || state.pendingArchive, err.code === "wrong_password");
      return;
    }
    show("extract");
  } else {
    show(state.queue.length ? "queue" : "home");
  }
  if (err.code === "cancelled") toast("Dibatalkan", kind === "compress" ? "Tidak ada arsip setengah jadi yang tertinggal." : "", false);
  else showError(err);
}

// ---- Extract

async function openArchive(path, password = null) {
  state.pendingArchive = path;
  try {
    const listing = await invoke("list_archive", { path, password });
    state.archive = listing;
    state.archivePassword = password;
    state.cwd = "";
    state.search = "";
    $("x-search").value = "";
    state.selected = new Set();
    state.dest = listing.suggestedDest;
    show("extract");
    renderArchive();
  } catch (e) {
    show("extract");
    if (e.code === "password_required" || e.code === "wrong_password") askPassword(path, e.code === "wrong_password" && password);
    else { showError(e); closeArchive(); }
  }
}

function askPassword(path, wrong) {
  $("x-empty").hidden = true;
  $("x-browser").hidden = true;
  $("x-password").hidden = false;
  $("x-pw-file").textContent = path.split(/[\\/]/).pop();
  $("x-pw-error").hidden = !wrong;
  $("x-pw").value = "";
  $("x-pw").focus();
}

function closeArchive() {
  state.archive = null;
  state.pendingArchive = null;
  state.archivePassword = null;
  $("x-password").hidden = true;
  $("x-browser").hidden = true;
  $("x-empty").hidden = false;
}

/** Children of the current folder (or search hits), with folder sizes rolled up. */
function visibleRows() {
  const a = state.archive;
  const q = state.search.trim().toLowerCase();
  if (q) {
    return a.entries
      .filter((e) => e.path.toLowerCase().includes(q))
      .slice(0, 2000)
      .map((e) => ({ name: e.path.split("/").pop(), path: e.path, isDir: e.isDir, size: e.size, mtime: e.mtime, hint: e.path }));
  }
  const prefix = state.cwd ? `${state.cwd}/` : "";
  const map = new Map();
  for (const e of a.entries) {
    if (!e.path.startsWith(prefix) || e.path === state.cwd) continue;
    const rest = e.path.slice(prefix.length);
    const slash = rest.indexOf("/");
    const name = slash < 0 ? rest : rest.slice(0, slash);
    const path = prefix + name;
    const isDir = slash >= 0 || e.isDir;
    let row = map.get(name);
    if (!row) { row = { name, path, isDir, size: 0, mtime: null }; map.set(name, row); }
    row.isDir = row.isDir || isDir;
    row.size += e.size;
    if (e.mtime && (!row.mtime || e.mtime > row.mtime)) row.mtime = e.mtime;
  }
  return [...map.values()].sort((x, y) => (y.isDir - x.isDir) || x.name.localeCompare(y.name, "id"));
}

/** Selected if the path or any folder above it is selected. */
function isSelected(path) {
  if (state.selected.has(path)) return true;
  for (const s of state.selected) if (path.startsWith(`${s}/`)) return true;
  return false;
}

function toggle(path, on) {
  if (on) {
    for (const s of [...state.selected]) if (s.startsWith(`${path}/`)) state.selected.delete(s);
    state.selected.add(path);
  } else if (state.selected.has(path)) {
    state.selected.delete(path);
  } else {
    // Unchecking something inside a selected folder: select its siblings instead.
    const parent = [...state.selected].find((s) => path.startsWith(`${s}/`));
    if (parent) {
      state.selected.delete(parent);
      const keep = new Set();
      for (const e of state.archive.entries) {
        if (!e.path.startsWith(`${parent}/`)) continue;
        let p = parent;
        for (const part of e.path.slice(parent.length + 1).split("/")) {
          const next = `${p}/${part}`;
          if (next === path || path.startsWith(`${next}/`)) { p = next; continue; }
          keep.add(next);
          break;
        }
      }
      for (const k of keep) state.selected.add(k);
    }
  }
  renderArchive();
}

function renderArchive() {
  const a = state.archive;
  $("x-empty").hidden = true;
  $("x-password").hidden = true;
  $("x-browser").hidden = false;
  $("x-name").textContent = a.name;
  const files = a.entries.filter((e) => !e.isDir).length;
  const unpacked = a.unpackedBytes ? ` · terbuka jadi ${bytes(a.unpackedBytes)}` : "";
  $("x-meta").textContent = `${a.format} · ${bytes(a.archiveBytes)}${unpacked} · ${nf0.format(files)} file${a.encrypted ? " · terkunci" : ""}`;
  $("x-dest").textContent = state.dest;
  $("x-dest").title = state.dest;

  // Breadcrumbs
  const crumbs = [h("button", { type: "button", onclick: () => { state.cwd = ""; renderArchive(); } }, a.name)];
  if (state.cwd) {
    const parts = state.cwd.split("/");
    parts.forEach((part, i) => {
      crumbs.push(h("span", { class: "sep", "aria-hidden": "true" }, "/"));
      const path = parts.slice(0, i + 1).join("/");
      crumbs.push(i === parts.length - 1
        ? h("span", { class: "here", "aria-current": "location" }, part)
        : h("button", { type: "button", onclick: () => { state.cwd = path; renderArchive(); } }, part));
    });
  } else {
    crumbs[0] = h("span", { class: "here", "aria-current": "location" }, a.name);
  }
  $("x-crumbs").replaceChildren(...crumbs);

  const rows = visibleRows();
  $("x-none").hidden = rows.length > 0;
  $("x-rows").replaceChildren(...rows.map((r) => {
    const on = isSelected(r.path);
    return h("tr", { class: on ? "sel" : "" },
      h("td", { class: "col-check" }, h("input", {
        type: "checkbox", checked: on, "aria-label": `Pilih ${r.name}`,
        onchange: (ev) => toggle(r.path, ev.target.checked),
      })),
      h("td", {}, h("span", { class: "name-cell" },
        icon(r.isDir ? "folder" : "file", r.isDir ? "folder" : "file"),
        r.isDir && !state.search
          ? h("button", { type: "button", class: "name-btn", onclick: () => { state.cwd = r.path; renderArchive(); } }, r.name)
          : h("span", { class: "ellipsis" }, r.name),
        r.hint && r.hint !== r.name ? h("span", { class: "path-hint ellipsis" }, r.hint) : null)),
      h("td", { class: "num" }, r.size || !r.isDir ? bytes(r.size) : "–"),
      h("td", { class: "date" }, r.mtime ? dateFmt.format(new Date(r.mtime * 1000)) : "–"));
  }));
  const allOn = rows.length > 0 && rows.every((r) => isSelected(r.path));
  $("x-all").checked = allOn;
  $("x-all").indeterminate = !allOn && rows.some((r) => isSelected(r.path));

  const n = state.selected.size;
  $("x-extract-sel").disabled = n === 0;
  $("x-extract-sel").textContent = n ? `Ekstrak ${nf0.format(n)} terpilih` : "Ekstrak terpilih";
}

async function startExtract(only) {
  const a = state.archive;
  try {
    beginJob("extract", `Mengekstrak ${a.name}`, `${a.format} · ke ${state.dest}`);
    // Extracting everything to the suggested place yields one known folder.
    state.job.opens = !only && state.dest === a.suggestedDest ? a.opens : state.dest;
    await invoke("start_extract", {
      req: { archive: a.path, dest: state.dest, password: state.archivePassword, only },
    });
  } catch (e) {
    state.job = null;
    show("extract");
    showError(e);
  }
}

// ---------------------------------------------------------------------------
// Drag & drop

async function onDrop(paths) {
  if (!paths?.length || state.view === "progress") return;
  if (state.tab === "extract" || state.view === "extract") {
    openArchive(paths[0]);
    return;
  }
  if (state.view === "home" && paths.length === 1 && await invoke("is_archive", { path: paths[0] })) {
    openArchive(paths[0]);
    return;
  }
  addPaths(paths);
}

function setupDragDrop() {
  const overlay = $("drop-overlay");
  T.webview.getCurrentWebview().onDragDropEvent((ev) => {
    const p = ev.payload;
    if (p.type === "enter" || p.type === "over") {
      if (state.view === "progress") return;
      $("drop-overlay-text").textContent = state.tab === "extract" ? "Lepaskan untuk membuka arsip" : "Lepaskan untuk menambahkan";
      overlay.hidden = false;
    } else {
      overlay.hidden = true;
      if (p.type === "drop") onDrop(p.paths);
    }
  });
}

// ---------------------------------------------------------------------------
// Wiring

const actions = {
  "pick-files": async () => addPaths(await invoke("pick_files")),
  "pick-folders": async () => addPaths(await invoke("pick_folders")),
  "clear-queue": () => { state.queue = []; state.nameTouched = false; show("home"); },
  "pick-out-dir": async () => {
    const d = await invoke("pick_dir", { start: state.outDir || null });
    if (d) { state.outDir = d; renderSettings(); }
  },
  cancel: () => invoke("cancel_job"),
  "copy-path": async () => {
    const path = $("d-path").textContent;
    try { await navigator.clipboard.writeText(path); toast("Lokasi disalin", path, false); } catch (_) { toast("Tidak bisa menyalin", path); }
  },
  "reveal-result": () => {
    const r = state.result;
    if (!r) return;
    const p = r.kind === "extract" ? invoke("open_folder", { path: r.opens }) : invoke("reveal", { path: r.output });
    p.catch(showError);
  },
  again: () => {
    if (state.result?.kind === "extract") { closeArchive(); show("extract"); } else show("home");
  },
  "clear-recent": () => { save("czip.recent", "[]"); renderRecent(); },
  "pick-archive": async () => { const p = await invoke("pick_archive"); if (p) openArchive(p); },
  "close-archive": () => closeArchive(),
  "extract-all": () => startExtract(null),
  "pick-dest": async () => {
    const d = await invoke("pick_dir", { start: state.dest || null });
    if (d) { state.dest = d; renderArchive(); }
  },
};

document.addEventListener("click", (ev) => {
  const el = ev.target.closest("[data-action]");
  if (!el) return;
  const fn = actions[el.dataset.action];
  if (fn) Promise.resolve(fn()).catch(showError);
});

for (const tab of document.querySelectorAll(".tab")) {
  tab.addEventListener("click", () => {
    if (state.view === "progress") return;
    if (tab.dataset.tab === "extract") show("extract");
    else show(state.queue.length ? "queue" : "home");
  });
}

$("toast-close").addEventListener("click", () => { $("toast").hidden = true; });
$("arc-name").addEventListener("input", (ev) => { state.name = ev.target.value; state.nameTouched = true; });
$("use-pw").addEventListener("change", () => { renderSettings(); if ($("use-pw").checked) $("pw1").focus(); });
$("go-compress").addEventListener("click", startCompress);
$("x-pw-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  const pw = $("x-pw").value;
  if (pw) openArchive(state.pendingArchive, pw);
});
$("x-search").addEventListener("input", (ev) => { state.search = ev.target.value; renderArchive(); });
$("x-all").addEventListener("change", (ev) => {
  for (const r of visibleRows()) {
    if (ev.target.checked) { if (!isSelected(r.path)) state.selected.add(r.path); }
    else state.selected.delete(r.path);
  }
  renderArchive();
});
$("x-extract-sel").addEventListener("click", () => startExtract([...state.selected]));

listen("job-progress", (ev) => onProgress(ev.payload));
listen("compress-done", (ev) => onCompressDone(ev.payload));
listen("extract-done", (ev) => onExtractDone(ev.payload));
listen("job-error", (ev) => onJobError(ev.payload));

setupDragDrop();
show("home");
if (window.__CZIP_PREVIEW__) {
  document.body.prepend(h("div", { class: "preview-banner" }, "Pratinjau browser · data contoh, tidak ada file yang diubah"));
}

// Launched with paths (e.g. double-clicking an archive or "Open with").
invoke("launch_paths").then(async (paths) => {
  if (paths.length === 1 && await invoke("is_archive", { path: paths[0] })) openArchive(paths[0]);
  else if (paths.length) addPaths(paths);
});
