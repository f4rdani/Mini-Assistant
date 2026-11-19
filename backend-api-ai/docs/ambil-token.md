# Ambil token DeepSeek

Service ini **tidak butuh password**. Yang dibutuhkan cuma satu nilai: `userToken`
dari localStorage browser kamu.

> Cookie tidak wajib. Sudah diuji: dengan `Authorization: Bearer <token>` saja,
> endpoint chat, upload, dan delete session semuanya jalan. Kalau kamu tetap
> mengirim cookie, service akan meneruskannya — tidak masalah.

## Langkah

### 1. Pastikan tab yang aktif benar

Buka <https://chat.deepseek.com/a/chat> dan pastikan kamu **sudah login**.
DevTools harus dijalankan di tab itu, bukan di tab lain.

### 2. Buka Console

- Tekan **F12**, lalu klik tab **Console**, atau
- Langsung **Ctrl+Shift+J** (Windows/Linux) / **Cmd+Option+J** (macOS).

### 3. Izinkan paste (sekali saja)

Kalau muncul peringatan "Warning: Don't paste code you don't understand…":

1. Klik di baris ketik paling bawah Console,
2. ketik `allow pasting`,
3. tekan **Enter**.

Ini hanya muncul di Chrome/Edge dan hanya sekali per profil.

### 4. Jalankan snippet

Copy seluruh blok di bawah, paste ke Console, tekan **Enter**:

```js
(() => {
  let raw = localStorage.getItem('userToken');
  if (!raw) return console.warn('userToken kosong — belum login?');
  let token;
  try { token = JSON.parse(raw).value; } catch { token = raw; }
  if (!token) return console.warn('userToken tidak berisi .value');
  const out = JSON.stringify({ name: 'main', token }, null, 2);
  console.log(out);
  try { copy(out); console.log('%c✓ sudah dicopy', 'color:#0a0'); }
  catch { console.log('copy manual dari teks di atas'); }
})()
```

### 5. Yang harus muncul

```json
{
  "name": "main",
  "token": "DEMOONLY00000000000000000000000000000000000000000000000000000000"
}
```

Token DeepSeek **bukan JWT** — bentuknya string acak 64 karakter, bukan `eyJ...`.
Kalau yang keluar `eyJ...`, itu token lain, bukan yang dipakai service ini.

### 6. Daftarkan ke service

Lewat API (disimpan ke `accounts.json`, bertahan setelah restart):

```bash
curl -X POST http://127.0.0.1:8787/admin/accounts \
  -H 'content-type: application/json' \
  -d '{"name":"main","token":"TEMPEL_TOKEN_DI_SINI"}'
```

Atau lewat `.env` (satu akun, tidak ditulis ke disk):

```env
DS_TOKEN=TEMPEL_TOKEN_DI_SINI
```

Cek berhasil:

```bash
curl -s http://127.0.0.1:8787/admin/accounts
# {"accounts":[{"name":"main","has_token":true,"available":true,...}]}
```

## Kalau gagal

| Gejala | Penyebab | Solusi |
| --- | --- | --- |
| `token: null` | belum login / baru logout | login ulang, refresh, ulangi |
| `userToken kosong` | beda domain atau incognito | pastikan di chat.deepseek.com |
| `copy is not defined` | Console tidak fokus | klik di Console dulu, ulangi |
| Paste ditolak | proteksi Chrome | ketik allow pasting |
| `40003 INVALID_TOKEN` | token kedaluwarsa | ambil token baru |
| `40002 Missing Token` | token tidak terkirim | cek `has_token` di `/admin/accounts` |

Token biasanya berlaku sekitar 24 jam. Kalau semua request tiba-tiba gagal
dengan `40003`, ambil token baru — akun lain di pool tetap dipakai sampai
semuanya habis.

## Keamanan

- Token ini setara **akses penuh** ke akun kamu, bukan cuma chat.
- Jangan commit `accounts.json` atau `.env` (keduanya sudah ada di `.gitignore`).
- Kalau token bocor: **logout** di browser lalu login lagi — token lama langsung mati.
- Kalau cuma mau coba-coba, pakai akun percobaan.
