# Contoh penggunaan

Base URL: `http://127.0.0.1:8787/v1` — kompatibel dengan OpenAI Chat Completions.
Kalau `API_KEYS` diisi, kirim `Authorization: Bearer <key>` (boleh string apa saja
kalau `API_KEYS` kosong).

Daftar model:

| Model | Efek |
| --- | --- |
| `deepseek-chat` | chat biasa |
| `deepseek-reasoner` | Pikir Mendalam, hasil di `reasoning_content` |
| `deepseek-search` | Pencarian Cerdas (web) |
| `deepseek-vision` | sama seperti chat, untuk kirim gambar |

---

## 1. Chat paling sederhana

```bash
curl -s http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "deepseek-chat",
    "messages": [{"role":"user","content":"Jelaskan ownership di Rust dalam 2 kalimat."}]
  }'
```

---

## 2. Streaming

```bash
curl -N http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "deepseek-chat",
    "stream": true,
    "messages": [{"role":"user","content":"Hitung 1 sampai 10."}]
  }'
```

Formatnya SSE standar OpenAI: potongan `data: {...}` diakhiri `data: [DONE]`.
Chunk terakhir membawa `usage`.

---

## 3. Pikir Mendalam (reasoning)

```bash
curl -s http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "deepseek-reasoner",
    "messages": [{"role":"user","content":"Ibu kota Indonesia?"}]
  }'
```

Respons berisi dua field:

```json
{
  "message": {
    "reasoning_content": "We need answer Indonesian...",
    "content": "Ibu kota Indonesia saat ini adalah Jakarta..."
  }
}
```

Saat streaming, potongan reasoning datang sebagai `delta.reasoning_content`.

Bisa juga dipaksa lewat flag tanpa ganti model:

```json
{"model": "deepseek-chat", "thinking": true, "messages": [...]}
```

---

## 4. Pencarian Cerdas (web search)

```bash
curl -s http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "deepseek-search",
    "messages": [{"role":"user","content":"Berita AI terbaru hari ini?"}]
  }'
```

Atau `"search": true` dengan model biasa. Jawaban biasanya diawali teks banner
dari UI DeepSeek ("Found 12 web pages") — itu berasal dari upstream.

---

## 5. Tool calling

### 5a. Satu tool

```bash
curl -s http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "deepseek-chat",
    "messages": [{"role":"user","content":"Cuaca di Bandung sekarang?"}],
    "tools": [{
      "type": "function",
      "function": {
        "name": "get_weather",
        "description": "Ambil cuaca terkini sebuah kota",
        "parameters": {
          "type": "object",
          "properties": {"city": {"type": "string"}},
          "required": ["city"]
        }
      }
    }]
  }'
```

Hasil:

```json
{
  "choices": [{
    "finish_reason": "tool_calls",
    "message": {
      "role": "assistant",
      "content": null,
      "tool_calls": [{
        "id": "call_...",
        "type": "function",
        "function": {"name": "get_weather", "arguments": "{\"city\":\"Bandung\"}"}
      }]
    }
  }]
}
```

### 5b. Dua tool sekaligus (paralel)

Sudah diuji: model bisa mengembalikan **dua** `tool_calls` dalam satu balasan.

```bash
curl -s http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "deepseek-chat",
    "messages": [{"role":"user","content":"Cuaca Bandung sekarang, dan forecast Jakarta 3 hari. Pakai kedua tool."}],
    "tools": [
      {"type":"function","function":{"name":"get_weather","description":"Cuaca saat ini","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}},
      {"type":"function","function":{"name":"get_forecast","description":"Forecast 3 hari","parameters":{"type":"object","properties":{"city":{"type":"string"},"days":{"type":"integer"}},"required":["city"]}}}
    ]
  }'
```

### 5c. Kirim balik hasil tool

```bash
curl -s http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "deepseek-chat",
    "messages": [
      {"role":"user","content":"Cuaca di Bandung? Pakai tool."},
      {"role":"assistant","content":null,"tool_calls":[
        {"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Bandung\"}"}}
      ]},
      {"role":"tool","tool_call_id":"call_1","content":"27C, cerah"}
    ],
    "tools": [{"type":"function","function":{"name":"get_weather","description":"Cuaca","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}]
  }'
```

`tool_call_id` dipakai untuk mencocokkan hasil dengan pemanggilnya, jadi beberapa
hasil paralel tidak tertukar.

---

## 6. Vision (kirim gambar)

Gambar boleh data URL atau URL publik:

```bash
curl -s http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "deepseek-vision",
    "messages": [{"role":"user","content":[
      {"type":"text","text":"Apa isi gambar ini?"},
      {"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo..."}}
    ]}]
  }'
```

URL `http(s)` diunduh oleh service (maks 20 MB, 45 detik, alamat privat
ditolak), lalu diunggah ke DeepSeek.

---

## 7. Upload file lalu pakai di chat

```bash
# 1. upload
curl -s -X POST http://127.0.0.1:8787/v1/files \
  -F "file=@C:/data/laporan.pdf;type=application/pdf"
# {"id":"file-3f2a...","object":"file","bytes":51234,"filename":"laporan.pdf"}

# 2. pakai id-nya
curl -s http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "deepseek-chat",
    "messages": [{"role":"user","content":"Ringkas isi file ini."}],
    "ref_file_ids": ["file-3f2a..."]
  }'
```

Upload bersifat asinkron di sisi DeepSeek (`PENDING → PARSING → SUCCESS`).
Service menunggu sampai file siap, jadi `id` yang dikembalikan langsung bisa dipakai.

---

## 8. Percakapan multi-turn

Kirim seluruh riwayat seperti biasa — service merangkainya jadi satu prompt:

```json
{
  "model": "deepseek-chat",
  "messages": [
    {"role": "system", "content": "Jawab singkat dalam bahasa Indonesia."},
    {"role": "user", "content": "Siapa presiden pertama Indonesia?"},
    {"role": "assistant", "content": "Soekarno."},
    {"role": "user", "content": "Tahun berapa dia lahir?"}
  ]
}
```

---

## 9. Python (SDK OpenAI resmi)

```bash
pip install openai
```

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8787/v1", api_key="not-needed")

# streaming + reasoning
stream = client.chat.completions.create(
    model="deepseek-reasoner",
    messages=[{"role": "user", "content": "Jelaskan borrow checker."}],
    stream=True,
)
for chunk in stream:
    d = chunk.choices[0].delta
    if getattr(d, "reasoning_content", None):
        print(d.reasoning_content, end="", flush=True)
    elif d.content:
        print(d.content, end="", flush=True)
```

Contoh tool-calling lengkap (loop sampai model berhenti minta tool):

```python
import json
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8787/v1", api_key="not-needed")

TOOLS = [{
    "type": "function",
    "function": {
        "name": "get_weather",
        "description": "Cuaca terkini sebuah kota",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
    },
}]

def get_weather(city: str) -> str:
    return f"27C cerah di {city}"

messages = [{"role": "user", "content": "Cuaca di Bandung?"}]

while True:
    r = client.chat.completions.create(model="deepseek-chat", messages=messages, tools=TOOLS)
    msg = r.choices[0].message
    messages.append(msg.model_dump(exclude_none=True))

    if not msg.tool_calls:
        print(msg.content)
        break

    for call in msg.tool_calls:
        args = json.loads(call.function.arguments)
        result = get_weather(**args)
        messages.append({"role": "tool", "tool_call_id": call.id, "content": result})
```

---

## 10. Node / TypeScript

```ts
import OpenAI from "openai";

const client = new OpenAI({ baseURL: "http://127.0.0.1:8787/v1", apiKey: "not-needed" });

const res = await client.chat.completions.create({
  model: "deepseek-search",
  messages: [{ role: "user", content: "Berita AI hari ini?" }],
});
console.log(res.choices[0].message.content);
```

---

## 11. Banyak akun (pool)

```bash
# tambah
curl -X POST http://127.0.0.1:8787/admin/accounts \
  -H 'content-type: application/json' \
  -d '{"name":"akun2","token":"..."}'

# lihat status
curl -s http://127.0.0.1:8787/admin/accounts

# hapus
curl -X DELETE http://127.0.0.1:8787/admin/accounts/akun2
```

Request diputar bergiliran (round-robin). Akun yang gagal karena token mati atau
rate limit otomatis di-cooldown 30 detik dan request dicoba ke akun berikutnya.
Kalau semua akun sibuk, jawabannya `429`.

---

## 12. Cek kesehatan

```bash
curl -s http://127.0.0.1:8787/health
# {"status":"ok","upstream":"https://chat.deepseek.com","accounts":1,"pow":true}
```

---

## Error yang mungkin muncul

| HTTP | `code` | Arti | Tindakan |
| --- | --- | --- | --- |
| 400 | `invalid_request_error` | body salah / `messages[]` kosong / token tidak valid saat didaftarkan | perbaiki request |
| 401 | `authentication_error` | API key service salah (berlaku juga untuk `/v1/models`) | cek `API_KEYS` |
| 413 | `invalid_request_error` | body > 16 MB (chat) atau > 64 MB (upload) | kirim lebih kecil |
| 429 | `rate_limit_exceeded` | semua akun sibuk / cooldown | ulangi sebentar lagi |
| 502 | `upstream_error` | DeepSeek menolak | lihat pesan, biasanya token |
| 503 | `no_account` | belum ada akun | `POST /admin/accounts` |

Kalau `POST /admin/accounts` menolak token, biasanya karena token mengandung
newline/karakter non-ASCII (salah copy) — ambil ulang dari Console, harus satu baris.

Pesan `502` yang sering:

- `40003 INVALID_TOKEN` → token kedaluwarsa, ambil yang baru
- `40301 INVALID_POW_RESPONSE` → algoritma PoW berubah, ganti `assets/deepseek_hash_v1.wasm`
- `invalid ref file id` → file belum selesai diproses (service sudah menunggu; kalau muncul, laporkan)

---

## Environment variable

| Variabel | Default | Fungsi |
| --- | --- | --- |
| `HOST` | `127.0.0.1` | alamat bind |
| `PORT` | `8787` | port |
| `API_KEYS` | kosong | daftar key dipisah koma; kosong = tanpa auth |
| `ADMIN_KEY` | kosong | wajib kalau bind non-loopback |
| `DEFAULT_MODEL` | `deepseek-chat` | model default |
| `DS_TOKEN` / `DS_COOKIE` | kosong | satu akun lewat env |
| `DS_ACCOUNTS_FILE` | `accounts.json` | lokasi penyimpanan akun |
| `DS_DELETE_SESSION` | `true` | hapus chat session tiap request (riwayat web tetap bersih) |
| `DS_REUSE_SESSION` | `false` | pakai satu sesi per akun (kalau `true`, cleanup dimatikan) |
| `DS_POW` | `on` | matikan hanya kalau DeepSeek berhenti minta PoW |

---

## Catatan

- Pakai build **release**. Build debug 5-10x lebih lambat karena PoW dijalankan
  lewat interpreter WebAssembly.
- Setiap request membuat chat session di akun, lalu **dihapus otomatis** setelah
  selesai (termasuk kalau client memutus koneksi), jadi riwayat web tidak penuh.
- `temperature`, `top_p`, `max_tokens`, `n` diabaikan — endpoint web tidak punya
  parameter itu. Yang berpengaruh hanya `model`, `stream`, `tools`, `ref_file_ids`,
  `thinking`, dan `search`.
