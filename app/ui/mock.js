// Browser preview only: when the page is opened outside the app there is no
// Tauri bridge, so this stands in with sample data. Inside the app it does
// nothing.
(function () {
  if (window.__TAURI__ || window.__TAURI_INTERNALS__) return;
  window.__CZIP_PREVIEW__ = true;
  const handlers = {};
  const emit = (name, payload) => (handlers[name] || []).forEach((f) => f({ payload }));
  let cancelled = false;
  const sample = [
    { path: "/home/demo/Foto Liburan 2026", name: "Foto Liburan 2026", isDir: true, bytes: 1202590843, files: 842 },
    { path: "/home/demo/proyek-website", name: "proyek-website", isDir: true, bytes: 509607936, files: 3912 },
    { path: "/home/demo/rekaman-meeting.mp4", name: "rekaman-meeting.mp4", isDir: false, bytes: 826277888, files: 1 },
    { path: "/home/demo/dataset-penjualan.csv", name: "dataset-penjualan.csv", isDir: false, bytes: 23068672, files: 1 },
    { path: "/home/demo/laporan-final.docx", name: "laporan-final.docx", isDir: false, bytes: 4404019, files: 1 },
  ];
  const now = Math.floor(Date.now() / 1000);
  const entries = [
    ["aset", true, 0], ["aset/ikon", true, 0], ["aset/ikon/ikon-app.svg", false, 18874368],
    ["aset/mockup", true, 0], ["aset/mockup/beranda.fig", false, 412000000], ["aset/mockup/checkout.fig", false, 259000000],
    ["aset/hero-banner.psd", false, 327155712], ["aset/logo-final.svg", false, 86016], ["aset/palet-warna.ase", false, 6144],
    ["font-lisensi.pdf", false, 1258291], ["catatan-klien.md", false, 9216],
  ].map(([path, isDir, size], i) => ({ path, isDir, size, mtime: now - i * 86400 * 3 }));
  function run(kind, total, done) {
    cancelled = false;
    let d = 0;
    const t = setInterval(() => {
      if (cancelled) { clearInterval(t); emit("job-error", { code: "cancelled", message: "cancelled" }); return; }
      d = Math.min(total, d + total / 40);
      emit("job-progress", { kind, totalBytes: total, doneBytes: d, filesDone: Math.round(d / total * 4757), current: "proyek-website/src/components/Header.tsx" });
      if (d >= total) { clearInterval(t); emit(`${kind}-done`, done); }
    }, 100);
  }
  const cmds = {
    scan_paths: ({ paths }) => {
      const items = sample.filter((s) => paths.includes(s.path));
      return { items, totalBytes: items.reduce((a, i) => a + i.bytes, 0), files: items.reduce((a, i) => a + i.files, 0), skipped: 0 };
    },
    default_output_dir: () => "/home/demo/Documents",
    resolve_output: ({ dir, name, format }) => `${dir}/${name}.${format}`,
    start_compress: ({ req }) => run("compress", 2566949378, { output: req.output, files: 4757, inputBytes: 2566949378, outputBytes: 2003000000, elapsedMs: 4100, skipped: 0 }),
    start_extract: ({ req }) => run("extract", 1018000000, { dest: req.dest, files: 8, bytes: 1018000000, elapsedMs: 2300 }),
    cancel_job: () => { cancelled = true; },
    list_archive: ({ path, password }) => {
      if (path.includes("rahasia") && password !== "123") throw { code: password ? "wrong_password" : "password_required", message: "" };
      return { path, name: path.split("/").pop(), format: "czip", encrypted: path.includes("rahasia"), archiveBytes: 432013312, unpackedBytes: 1018399000, entries, suggestedDest: "/home/demo/Downloads/proyek-desain", opens: "/home/demo/Downloads/proyek-desain" };
    },
    is_archive: ({ path }) => /\.(czip|zip|7z|zst|gz|xz)$/i.test(path),
    pick_files: () => sample.filter((s) => !s.isDir).map((s) => s.path),
    pick_folders: () => sample.filter((s) => s.isDir).map((s) => s.path),
    pick_dir: () => "/home/demo/Desktop",
    pick_archive: () => "/home/demo/Downloads/proyek-desain.czip",
    reveal: () => {}, open_folder: () => {},
    launch_paths: () => [],
  };
  window.__TAURI__ = {
    core: { invoke: async (cmd, args) => { const f = cmds[cmd]; if (!f) throw { code: "io", message: `no mock for ${cmd}` }; return f(args || {}); } },
    event: { listen: async (name, f) => { (handlers[name] = handlers[name] || []).push(f); return () => {}; } },
    webview: { getCurrentWebview: () => ({ onDragDropEvent: () => Promise.resolve(() => {}) }) },
  };
})();
