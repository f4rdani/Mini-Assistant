# api-ai

Turns a **chat.deepseek.com web session** into an **OpenAI-compatible API**.
You paste a cookie (+ the `userToken` from localStorage), and get:

- `POST /v1/chat/completions` — streaming & non-streaming, OpenAI schema
- `POST /v1/files` — upload image/pdf/doc, get a file id
- `GET  /v1/models`
- `GET|POST|DELETE /admin/accounts` — account pool (round-robin)
- `GET  /health`

Features carried over from the web UI:

| Web UI | How to trigger |
| --- | --- |
| Pikir Mendalam (reasoning) | model `deepseek-reasoner` or `"thinking": true` |
| Pencarian Cerdas (web search) | model `deepseek-search` or `"search": true` |
| Attach file / vision | `image_url` or `file` content parts, or `/v1/files` + `ref_file_ids` |
| Tool calling | standard OpenAI `tools` array (prompt-injected, see below) |

Reasoning tokens come back as `message.reasoning_content` (same as DeepSeek's
official API) and as `delta.reasoning_content` while streaming.

Full guides (Indonesian):

- [docs/ambil-token.md](docs/ambil-token.md) — cara ambil token, langkah per langkah + troubleshooting
- [docs/penggunaan.md](docs/penggunaan.md) — 12 skenario pemakaian (curl, Python SDK, Node, tool loop, vision, file)

## 1. Get credentials

Open <https://chat.deepseek.com/a/chat> while logged in, open DevTools:

```js
// Console tab
copy(document.cookie)                                  // -> "cookie"
copy(JSON.parse(localStorage.getItem('userToken')).value) // -> "token"
```

`userToken` may also be a raw string; either works.

## 2. Configure

```bash
cp .env.example .env
cp accounts.example.json accounts.json   # fill in cookie + token
```

or, at runtime (accounts are written back to `accounts.json`, so they survive
a restart):

```bash
curl -X POST localhost:8787/admin/accounts \
  -H 'content-type: application/json' \
  -d '{"name":"main","cookie":"...","token":"..."}'
```

`/admin/*` is open only while `HOST` is loopback. Bind to `0.0.0.0` and you
must set `ADMIN_KEY` (send it as `Authorization: Bearer <key>` or
`x-admin-key`), otherwise those routes return 401.

## 3. Run

```bash
cargo run --release
```

## 4. Use

```bash
curl localhost:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "deepseek-reasoner",
    "messages": [{"role":"user","content":"jelaskan ownership di rust"}],
    "stream": false
  }'
```

Works with the OpenAI SDKs / LangChain / Cursor / etc:

```python
from openai import OpenAI
client = OpenAI(base_url="http://localhost:8787/v1", api_key="unused")
client.chat.completions.create(
    model="deepseek-search",
    messages=[{"role": "user", "content": "berita AI hari ini"}],
)
```

### Tool calling

```json
{
  "model": "deepseek-chat",
  "messages": [{"role": "user", "content": "cuaca di Bandung?"}],
  "tools": [{
    "type": "function",
    "function": {
      "name": "get_weather",
      "description": "Ambil cuaca terkini",
      "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}
    }
  }]
}
```

The tool schemas are injected into the system prompt; the model answers with
`<tool_call>{...}</tool_call>`, which is parsed out of the stream and returned
as a normal OpenAI `tool_calls` delta with `finish_reason: "tool_calls"`.
Send the result back as a `{"role":"tool","name":"get_weather","content":"..."}`
message and the loop continues.

## Layout

```
src/
  main.rs            router + bootstrap
  config.rs          env config
  state.rs           shared state, account pool, session cache
  deepseek/
    auth.rs          Account / AccountPool (round-robin + cooldown)
    client.rs        headers, pow, session, completion, upload
    pow.rs           DeepSeekHashV1 proof-of-work solver
    stream.rs        tolerant SSE/patch parser
  convert/
    mod.rs           OpenAI messages -> DeepSeek prompt, attachments
    tools.rs         tool prompt injection + <tool_call> stream parser
  api/
    openai.rs        /v1/chat/completions, /v1/models, /v1/files
    admin.rs         /admin/accounts
```

## Behaviour worth knowing

- **429** means every account is busy or cooling down — not a bad request.
  **503** means no account is configured.
- A failing account is cooled down for 30s and the request is retried on the
  next account (up to `accounts` attempts). Good for a mix of live and expired
  cookies.
- Upstream silence for 180s ends the stream with an `error` frame plus a
  terminal `finish_reason`, so clients do not hang.
- `image_url` urls are fetched with a 20MB cap, a 45s timeout, redirects
  disabled and private/loopback addresses rejected — a client cannot use this
  service to probe your LAN.
- An unterminated `<tool_call>` block is reported as an `error` frame instead of
  being silently dropped.
- The chat session created for each request is **deleted afterwards**
  (`DS_DELETE_SESSION`, default on), so the account's web UI does not fill up
  with API traffic. Deletion also runs when the client disconnects.

## Proof of work

`/api/v0/chat/completion` and `/api/v0/file/upload_file` are protected by a
`DeepSeekHashV1` challenge (`x-ds-pow-response`). The hash is a custom
construction — not SHA3-256 and not Keccak-256, verified by differential
testing — so instead of guessing, the 26 KB reference module is vendored in
`assets/deepseek_hash_v1.wasm` and executed with [wasmi](https://crates.io/crates/wasmi),
a pure-Rust WebAssembly interpreter. No Node.js and no C toolchain at runtime.

It costs roughly 200-600 ms per request in a release build (the interpreter is
~10x slower than native). Build with `--release`; the debug build is 5-10x
slower again and will make every call feel broken.

If DeepSeek ever changes the algorithm, replace that one file. Provenance: it
is the solver shipped by the public `deepseek-free-api` reference project.

## Verified against the live upstream

Everything below was checked end-to-end with a real account (2026-10):

| Case | Result |
| --- | --- |
| `deepseek-chat` non-stream / stream | ok |
| `deepseek-reasoner` → `reasoning_content` | ok, 20 reasoning deltas in a short answer |
| `deepseek-search` → live web results | ok |
| tool call, single + parallel, `finish_reason: "tool_calls"` | ok |
| tool result fed back via `role:"tool"` | ok, model uses the result |
| image (`image_url` data url) | ok |
| `/v1/files` + `ref_file_ids` | ok, model read the file contents |
| expired token | 502 with `40003 INVALID_TOKEN` hint |
| expired account + healthy account | rotated automatically, answer returned |
| 3 parallel calls, 2 accounts | `429`, `200`, `200` |

Known behaviour quirks of the web backend:

- The token from `localStorage.userToken` is **not** a JWT, it is a 64-char
  opaque string. `Authorization: Bearer <token>` alone is enough — the cookie
  is optional (the code still forwards it if you supply one).
- Uploads are asynchronous: `PENDING → PARSING → SUCCESS`. The service polls
  `/api/v0/file/fetch_files` until the file is usable, otherwise the completion
  fails with `invalid ref file id`.
- Streaming frames are **not** newline separated (`data: {...}event: ...`), so
  the parser delimits on the JSON object itself.
- Search answers include the UI banner text ("Found 12 web pages") in the
  content.

## Caveats

This is a reverse-engineered client of an undocumented endpoint. It will break
when DeepSeek changes their frontend. The two fragile spots are
`deepseek/pow.rs` (replace the wasm) and `deepseek/stream.rs` (the fragment /
patch protocol).

Each call creates a temporary chat session and deletes it when the response is
done. If you would rather keep one persistent session per account, set
`DS_REUSE_SESSION=true` (this disables the cleanup).

`accounts.json` holds your cookie and token in plaintext. It is gitignored —
keep it that way, and delete it when you are done with a throwaway session.

Using this against your own account is your call — it is outside DeepSeek's
terms of service.
