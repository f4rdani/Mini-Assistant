# ui-native — Native (Slint) UI for Mini Assistant

Port **1:1** dari frontend WebView2 (`src/`) ke **Slint** (Rust), supaya aplikasi
tidak butuh browser engine sama sekali.

```
WebView2 (sekarang)          ->   Slint (folder ini)
─────────────────────────────────────────────────────────────────────────────
src/lib/startorch-ui.css     ->   ui/theme.slint        (token --st-* 1:1)
src/lib/Icon.svelte (52 ikon)->   src/icons.rs          (SVG verbatim)
src/lib/brandLogos.ts        ->   assets/logos/         (+ 2 mark inline)
src/lib/markdown.ts          ->   slint::StyledText     (CommonMark native)
src/routes/+page.svelte      ->   ui/MainPanel.slint
                                  ui/HudWindow.slint
                                  ui/BubbleWindow.slint
src/lib/AemeathAvatar.svelte ->   assets/avatar/*.png   (4 frame, dianimasikan)
```

`src-tauri/src/modules/*` (DPAPI, AES-256-GCM, SQLite, Win32 hotkey, GDI
capture, reqwest streaming ke 6 provider AI) **tidak diubah sama sekali** —
cuma layer view yang diganti.

---

## Status

| Komponen | Status |
|---|---|
| Design tokens (36 `--st-*` + gradient + geometry) | ✅ 1:1 |
| 52 ikon SVG (resvg, `currentColor` -> hex runtime) | ✅ |
| Brand logo AI + browser (6 + 4) | ✅ terverifikasi screenshot |
| Control Panel 900x650 (topbar, sidebar rail, 5 tab) | ✅ terverifikasi screenshot |
| Floating HUD 380x620 (top strip, chat, input bar) | ✅ terverifikasi screenshot |
| Mini bubble 92x92 (GIF murni, auto mirror/flip per edge) | ✅ terverifikasi screenshot |
| Mascot 4-frame breathing (Timer 220 ms) | ✅ |
| DWM acrylic (`backdrop-filter` blur) | ✅ |
| Rich text markdown (bold/italic/code/list/link) | ✅ |
| Native window drag (`data-tauri-drag-region`) | ✅ |
| Always-on-top pin | ✅ |
| Font Google (Space Grotesk / Plus Jakarta Sans / JetBrains Mono) | ✅ bundled, perlu install |
| **RAM (< 40 MB)** | ✅ 12,4 MB working set / 17,0 MB private (tes release) |
| **Wiring ke backend Rust asli** | ⏳ seed data dulu |

## Jalankan

```powershell
# dari root project
pnpm ui:native                 # = cargo run --release --manifest-path ui-native/Cargo.toml

# atau langsung
cd ui-native
cargo run --release
cargo run --release -- --hud      # mulai dengan Floating HUD terlihat
cargo run --release -- --bubble   # mulai dengan mini bubble terlihat
cargo run --release -- --hud --bubble
# (control panel juga ikut tampil; tombol FLOATING HUD / BUKA FLOATING HUD
#  memindahkan window seperti versi webview)
```

## Struktur

```
ui-native/
├── Cargo.toml            crate terpisah, target/ sendiri
├── build.rs              compile 1 file (App.slint) supaya global tidak dobel
├── ui/
│   ├── App.slint         entry point (re-export)
│   ├── theme.slint       Tokens + Icons globals
│   ├── model.slint       struct model (ProviderItem, BrowserItem, ...)
│   ├── components.slint  GlassCard, PrimaryButton, IconButton, OrbitalSwitch, ...
│   ├── MainPanel.slint   Control Panel
│   ├── HudWindow.slint   Floating HUD
│   └── BubbleWindow.slint Mini bubble
├── src/
│   ├── main.rs           wiring + icon cache
│   ├── icons.rs          52 ikon SVG (auto-generated dari Icon.svelte)
│   ├── logos.rs          brand logo + mascot sprite
│   ├── model.rs          seed data (shape sama dengan backend)
│   └── win32.rs          DWM acrylic, drag, always-on-top
├── assets/
│   ├── fonts/            3 variable TTF Google Fonts
│   ├── logos/            SVG + PNG brand logo
│   └── avatar/           4 frame mascot PNG
└── scripts/install-fonts.ps1
```

## Catatan penting

### 1. WAJIB pakai `renderer-software`
`Cargo.toml` memakai `renderer-software`, **bukan** `renderer-femtovg`.
OpenGL context + driver NVIDIA/Intel menambah **~62 MB** working set:

| Konfigurasi | Working Set | Private (kolom "Memory" di Task Manager) |
|---|---|---|
| WebView2 (sekarang) | ~160–220 MB | ~160–220 MB |
| Slint + femtovg (OpenGL) | 84,4 MB | 42,4 MB |
| **Slint + software renderer** | **23,9 MB** | **6,6 MB** |

### 2. `i-slint-core` ditambahkan langsung
Feature `svg` (resvg) dan `image-decoders` (PNG/JPEG) tidak diekspos oleh
facade `slint`, jadi diaktifkan lewat dependensi langsung:

```toml
i-slint-core = { version = "=1.18.1", default-features = false, features = [
    "svg", "image-decoders",
] }
```

### 3. Aturan syntax Slint yang dipakai di sini
* id element harus **dideklarasi sebelum** dipakai: `touch := TouchArea { }`
* id harus unik per component -> pakai nama berbeda per loop
* string tidak punya `.length` -> auto-size pakai Text tersembunyi (opacity 0)
* `@markdown()` hanya menerima **literal** -> markdown di-parse di Rust
  (`slint::StyledText::from_markdown`) lalu dikirim sebagai `styled-text`
* callback yang dipakai di binding harus `pure callback`
* `Rectangle` mewarisi ukuran parent -> harus di-pin eksplisit (lihat `Icon`)
* `border-radius` hanya 1 nilai (CSS 4-sudut di-approximate)
* `clip-path` tidak ada -> chamfer 45° di-approximate dengan `border-radius`

### 3b. Kendala syntax Slint yang terbukti saat porting v2
* Slint **globals per-instance komponen**: `Icons::get(&hud)` berbeda dari
  `Icons::get(&panel)` — callback `on_render` harus di-wire di SEMUA window
  yang menggambar ikon, kalau tidak ikornya kosong tanpa error.
* `rotation-angle` TIDAK ada di `Rectangle` -> diamond (gem koin `.panel-card::after`)
  digambar pakai `Path { commands: "M 0 7 L 7 0 L 14 7 L 7 14 Z" }`.
* `Text`/`StyledText` tidak punya properti `parent` -> tulis ukuran literal.
* `line-height`, `margin-*`, `background`, `border-*` TIDAK ada di `Text` ->
  bungkus dengan `Rectangle` (pill/badge selalu Rectangle + Text di dalamnya).
* `for x[i] in [0, 1, 2, 3]` (array int literal) valid -> dipakai untuk 4
  toggle-row HUD dan 4 header action.
* SVG dari CDN memakai `width/height="1em"` -> resvg resulkan 0px (blank);
  dinormalkan di `logos.rs::load_svg` (1em -> 48).
* Chat bubble mixed-role harus pakai posisi **step uniform** (`idx * 118px`);
  step per-role membuat row saling menimpa.
* `asset_dir()` fallback ke `env!("CARGO_MANIFEST_DIR")/assets` supaya jalan
  dari working directory mana pun (`--hud` dari task manager dsb).

### 4. Yang belum 1:1 (catatan jujur)
* `filter: blur()` pada elemen (33x di CSS) tidak ada di Slint -> asset
  dekoratif di-pre-blur offline.
* `clip-path: polygon(...)` (sudut terpancung) di-approximate `border-radius`
  (mode-switcher & panel-card sudah patch dengan triangle `Path`).
* Border dashed (`.st-aemeath-orb` ring bubble) dihapus; mini bubble sekarang
  hanya GIF maskot tanpa ring/star.
* Conic-gradient beam (`.hud-session-pill`, `.hud-input-capsule`) ->
  di-approximate border cyan + slab pink/gold statis (CSS animasi 5s tidak
  bisa diloop di Slint `animate`).
* Tabel markdown, heading, code block + tombol copy, blockquote -> butuh
  renderer block-level custom (CommonMark native Slint hanya inline + list).
* Animasi GIF mascot -> frame PNG + `Timer` Slint 250 ms dengan siklus
  `0,1,2,3,2,1`; varian mirror/flip dibakar saat load untuk bubble.
* `text-transform: uppercase` -> di-uppercase di Rust.

### 5. Lisensi
Slint = GPL-3.0 **atau** Royalty-free (gratis untuk revenue < $1 M). Kalau
aplikasi ini jadi komersial besar, perlu lisensi berbayar Slint.
Alternatif tanpa syarat lisensi: egui (MIT/Apache) dengan tradeoff visual.

### Flag uji (dev)

Menjalankan jalur UI yang sama dengan tombol, tanpa klik manual:

```bash
# satu pesan
mini-assistant-ui.exe --hud --send "halo"

# pilih model (deepseek-reasoner / deepseek-search)
mini-assistant-ui.exe --hud --model deepseek-reasoner --send "2+2?"

# lampiran (gambar -> vision, selain itu -> upload file)
mini-assistant-ui.exe --hud --attach "C:\path\foto.png" --send "warna apa?"

# screenshot layar lalu kirim
mini-assistant-ui.exe --hud --capture --send "teks apa yang terlihat?"

# temp chat (tidak ditulis ke SQLite)
mini-assistant-ui.exe --hud --temp --send "jangan disimpan"

# cek jumlah pesan tersimpan di topik aktif
mini-assistant-ui.exe --dump-history

# uji jalur hotkey tanpa menekan tombol
mini-assistant-ui.exe --hotkey-test

# buka kartu Pikiran lalu simpan screenshot HUD
mini-assistant-ui.exe --hud --shot --send "..."

# multi-turn: --send boleh diulang, dikirim berurutan
mini-assistant-ui.exe --hud --send "nama saya Budi" --send "siapa nama saya?"
```

Semua flag ini memanggil callback Slint yang sama dengan tombol (`invoke_send_request`,
`invoke_capture_screen`), jadi yang diuji tetap jalur frontend → app-core → provider API.
