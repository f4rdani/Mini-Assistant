# PRODUCT REQUIREMENT DOCUMENT (PRD)
## Mini Assistant: Universal Desktop Companion & Floating HUD Overlay
**Platform**: Windows Native Desktop (Slint + Rust, tanpa WebView2)  
**Status**: Approved & Implemented

---

## 1. Executive Summary & Visi Produk

### 1.1 Visi Produk
Aplikasi **Native Desktop Companion** lokal berdesain modern (Dark Obsidian / Glassmorphism) dengan performa ultra-ringan (RAM idle < 35 MB, CPU < 0.1%). Beroperasi 100% lokal tanpa API key berbayar dengan menjembatani session web AI (**Grok, Claude, Gemini, ChatGPT**) melalui ekstraksi cookies otomatis via Windows DPAPI.

### 1.2 Dua Model Antarmuka Utama (Two Core Modes)
Aplikasi memiliki **2 Model Penggunaan Terpisah** yang terhubung secara seamless melalui Segmented Switcher di header atas:
1. **Model 1: Control Panel (Aplikasi Utama / Desktop Dashboard)**:
   * Jendela aplikasi native penuh dengan navigasi rail di sisi kiri.
   * Berfungsi untuk manajemen penyedia AI & urutan prioritas failover, pemilihan profil browser, pengujian dekripsi DPAPI cookies, manajemen arsip topik/percakapan, dan pemantauan diagnostik sistem.
2. **Model 2: Floating HUD & Bubble Chat (Gaming / Screen Companion)**:
   * Widget mengambang dengan latar belakang *dark frosted glass* (`backdrop-filter: blur(24px)`), border tipis bergradasi, dan aksen *Emerald Glow (`#10B981`)*.
   * **Sub-Mode A (Floating Mini Bubble)**: Ikon bundar mengambang dengan efek *glow* yang dapat digeser di atas game/layar.
   * **Sub-Mode B (Floating Companion HUD Window)**: Widget percakapan dengan *top strip draggable*, tombol **Pin** (agar tetap melayang di atas game tanpa terhalang), strip deteksi game aktif, tombol toggle *Auto-Capture*, dan vision attachment.

### 1.3 Standar Visual & Ikonografi
* **Zero Browser Scrollbar**: Scrollbar browser default dihilangkan sepenuhnya; viewport terkunci 100% di dalam bingkai jendela native tanpa horizontal scrollbar.
* **Zero Emoji Policy**: Tidak menggunakan emoji teks sama sekali pada antarmuka.
* **SVG Vector Icons**: Seluruh ikon menggunakan grafis vektor SVG native (gaya Fluent / Lucide) yang tajam pada semua skala resolusi layar.

---

## 2. Arsitektur Teknis & Tech Stack

| Komponen | Teknologi | Keunggulan & Rationale |
| :--- | :--- | :--- |
| **Backend Core** | **Rust (app-core, lib in-process)** | RAM < 45 MB, akses low-level Win32 API instan, tanpa runtime NodeJS/Electron dan tanpa IPC. |
| **GUI Framework** | **Slint 1.18 (software renderer)** | Tanpa browser engine: hemat ~60 MB working set dibanding WebView2/OpenGL. |
| **Screen Capture** | **Win32 GDI Desktop Grabber** | Mengambil frame layar dalam < 12 milidetik tanpa lag pada game AAA. |
| **Cookie Decryptor**| **Windows DPAPI + AES-256-GCM** | Ekstraksi 100% otomatis dari SQLite Chrome (v10/v20) di startup. |
| **Local Database** | **SQLite (rusqlite)** | Penyimpanan riwayat topik dan konfigurasi secara offline di komputer pengguna. |

---

## 3. Spesifikasi Fungsional Rinci

### 3.1 Model 1: Control Panel (Aplikasi Utama)
* **AI Providers & Fallback Priority**:
  * Pengaturan urutan prioritas eksekusi: `#1 Grok` $\rightarrow$ `#2 Claude` $\rightarrow$ `#3 Gemini` $\rightarrow$ `#4 ChatGPT`.
  * Tombol kontrol panah atas/bawah untuk memindahkan hierarki fallback.
  * Logika auto-failover: Jika AI utama mengalami *rate-limit (429)* atau sesi terganggu, payload langsung dialihkan ke AI berikutnya.
* **Browser & DPAPI Cookie Sync & Diagnostik Sesi**:
  * Mendeteksi instalasi Google Chrome, Microsoft Edge, Brave, dan Mozilla Firefox secara otomatis.
  * Kartu profil browser interaktif dengan status validitas SQLite.
  * **Mesin Diagnostik Sesi & Status Ekstraksi**:
    * Pengujian per-provider AI (`Grok`, `ChatGPT`, `Claude`, `Gemini`) atau 1-klik untuk menguji semua 4 provider sekaligus (`cmd_test_all_cookies`).
    * Deteksi status berkas: Mendeteksi apakah berkas sedang dikunci proses browser aktif (`LOCKED`), terproteksi Chromium 127+ App-Bound Encryption (`APP_BOUND`), atau belum login (`NO_COOKIES`).
    * Verifikasi token autentikasi sesi aktif (`auth_token`, `ct0`, `__Secure-next-auth.session-token`, `SID`, dll.).
    * Rincian chip item cookie yang ditemukan lengkap dengan tipe enkripsi (`DPAPI v10`, `Plaintext`, `App-Bound v20`).
* **Gaming & Context Engine**:
  * Menampilkan informasi jendela aktif di latar depan (*Foreground Window Hook*): Nama Aplikasi, Process Executable, dan Kategori (*Gaming*, *Design*, *Study*, *Development*).
  * Menampilkan hasil rekomendasi topik otomatis.
* **Arsip Percakapan**:
  * Melihat daftar topik dan riwayat percakapan yang tersimpan di SQLite lokal.
* **Diagnostik Sistem**:
  * Pemantauan footprint RAM, CPU, dan latensi screen capture.

---

### 3.2 Model 2: Floating HUD & Bubble Chat
* **Top Strip Draggable**:
  * Area drag window (`data-tauri-drag-region`).
  * **Pin Button**: Mengunci jendela overlay agar selalu berada di lapisan teratas (`always-on-top`) saat bermain game.
  * **Minimize Button**: Mengubah tampilan menjadi *Floating Mini Bubble* bundar.
  * **Settings Button**: Membuka kembali Control Panel utama.
* **Active App Strip**:
  * Menampilkan badge game yang sedang berjalan (contoh: *Medieval Dynasty*) dengan ikon controller game vektor dan tombol refresh instan.
* **Smart Topic Selector**:
  * Dropdown topik dengan opsi rekomendasi otomatis dari aplikasi aktif, riwayat topik terdahulu, atau input topik kustom.
* **Quick Tools Bar**:
  * **Toggle Temp Chat (`Shield` Vector)**: Mode percakapan sekali pakai (*ephemeral/incognito*) yang tidak disimpan ke SQLite.
  * **Toggle Switch (Auto-Capture)**: Sakelar geser untuk otomatis menjepret layar setiap kali tombol kirim ditekan.
  * **Manual Capture Button (`Camera` Vector)**: Menjepret layar desktop saat itu juga untuk di-review sebelum dikirim.
* **Chat Body & Vision Rendering**:
  * Gelembung pesan user dan assistant dengan badge provider (`GROK`, `CLAUDE`, dll.) dan tag *Failover* jika terjadi pergantian provider.
  * Thumbnail tangkapan layar desktop yang dapat diklik untuk memperbesar (*Zoom Modal*).
  * **Dukungan AI Image Generation Output**: Merender gambar hasil kreasi AI dengan tombol aksi cepat *Perbesar* dan *Salin ke Clipboard*.

---

### 3.3 Keybinding & Summon Engine (Pintasan Keyboard Global)
* **`Alt + Q` (Global Windows Hotkey)**:
  * Menggunakan Win32 API low-level `RegisterHotKey` di backend Rust pada background thread.
  * Berfungsi secara global bahkan saat game DirectX/Vulkan layar penuh (seperti *Medieval Dynasty*) sedang aktif dan fokus.
  * Membawa jendela aplikasi ke latar depan (`HWND_TOPMOST`), membuka gelembung obrolan (*Bubble Chat*), dan memfokuskan kursor pada kotak input pesan secara instan.
* **`Esc`**: Mengecilkan jendela obrolan kembali ke *Floating Mini Bubble* bundar tanpa menutup percakapan yang sedang berjalan.
* **`Enter`**: Mengirim pesan prompt sekaligus menyertakan screenshot otomatis jika fitur *Auto-Capture* aktif.
* **`Shift + Enter`**: Membuat baris baru (*multiline*) di dalam kotak input teks tanpa mengirim pesan.

---

### 3.4 Halaman Pengaturan & Kustomisasi Pintasan (Settings Tab)
* **Kustomisasi Tombol Pintasan (Dynamic Win32 Re-registration)**:
  * Pengguna dapat memilih kombinasi modifier (`Alt`, `Ctrl`, `Ctrl + Shift`, `Alt + Shift`, `Ctrl + Alt`) dan tombol pemicu (`Q`, `Spacebar`, `~`, `F9`, `F10`, `F11`, `Tab`, `Z`, `X`, `G`).
  * Backend Rust mendukung pendaftaran ulang hotkey secara instan tanpa perlu restart aplikasi melalui `PostThreadMessageW(thread_id, WM_USER + 101, 0, 0)`.
  * UI native menyinkronkan event listener in-app dan label tooltip di seluruh aplikasi.
* **Preferensi Overlay HUD**:
  * Pengaturan *Always-on-Top Pin* (`isPinned`) untuk memastikan HUD tetap melayang di atas jendela game.
  * Toggle *Auto-Capture Layar Otomatis* (`autoScreenshot`) saat mengirim pesan chat.
* **Manajemen Privasi & Database SQLite**:
  * Opsi satu klik untuk menghapus seluruh riwayat obrolan dan lampiran visual secara bersih dari SQLite lokal (`cmd_clear_all_history`).

---

## 4. Keamanan & Anti-Cheat Compliance

* **100% Local Processing**: Tidak ada data percakapan, token, atau screenshot yang dikirim ke server pihak ketiga. Semua komunikasi hanya terjadi secara direct antara PC pengguna dan domain resmi web AI.
* **Anti-Cheat Safe**: Jendela overlay beroperasi di level Windows Desktop (`HWND_TOPMOST`), **bukan melalui injeksi DLL ke proses game DirectX/Vulkan**, sehingga sepenuhnya aman dari deteksi anti-cheat seperti Riot Vanguard, Easy Anti-Cheat, dan BattlEye.


---

## 6. Implementasi Saat Ini (v0.1.0)

| Komponen | Status |
| :--- | :--- |
| `ui-native` | Slint 1.18, software renderer, panel + HUD + bubble. Data live dari `app-core`. |
| `app-core` | Lib in-process: cookie extractor, SQLite, capture, hotkey, window detector, metrics, client provider. |
| `backend-api-ai` | Provider API OpenAI-compatible (DeepSeek). Dokumentasi di `backend-api-ai/docs/`. |
| WebView2 / Tauri / SvelteKit | **Dihapus** dari build. Arsip di `_old/`. |

Provider yang didukung saat ini: DeepSeek (lewat provider API). Provider lain
(Grok/Claude/Gemini/ChatGPT/Kimi) tidak lagi dibundel di aplikasi.
