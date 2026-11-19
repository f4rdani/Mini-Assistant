//! Parser for DeepSeek's SSE stream.
//!
//! Captured wire format (2026-10):
//!
//!   event: ready
//!   data: {"request_message_id":1,"response_message_id":2,"model_type":"default"}
//!   event: update_session
//!   data: {"updated_at":1792656247.08}
//!   data: {"v":{"response":{"status":"WIP","fragments":[{"id":2,"type":"RESPONSE","content":"Jak"}]}}}
//!   data: {"p":"response/fragments/-1/content","o":"APPEND","v":"arta"}
//!   data: {"v":" adalah"}                          <- bare delta, belongs to the last fragment
//!   data: {"p":"response/fragments","o":"APPEND","v":[{"id":3,"type":"RESPONSE","content":"I"}]}
//!   data: {"p":"response","o":"BATCH","v":[{"p":"accumulated_token_usage","v":82},{"p":"quasi_status","v":"FINISHED"}]}
//!   data: {"p":"response/status","o":"SET","v":"FINISHED"}
//!   event: title
//!   data: {"content":"Fakta Jakarta"}
//!   event: close
//!
//! Reasoning streams are the same shape with `"type":"THINK"` on the fragment.

use serde_json::Value;

/// Hard ceiling for a single unterminated frame.
const MAX_BUFFER: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchItem {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub site_name: String,
    pub published_at: String,
    pub cite_index: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum DsEvent {
    Content(String),
    Thinking(String),
    Search(Vec<SearchItem>),
    Status(String),
    Usage { input: u64, output: u64 },
    Done,
    Ignore,
}

#[derive(Default)]
pub struct SseBuffer {
    buf: Vec<u8>,
    overflowed: bool,
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// End index (exclusive) of the JSON object starting at `start`.
fn json_object_end(buf: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    let mut i = start;
    while i < buf.len() {
        let b = buf[i];
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

impl SseBuffer {
    /// Feed raw bytes, get back the list of `data:` payloads that are complete.
    ///
    /// Frames are NOT newline separated on the wire — DeepSeek emits
    /// `data: {...}event: update_session` back to back — so we delimit on the
    /// JSON object itself instead of on newlines.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        if self.overflowed {
            return Vec::new();
        }
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > MAX_BUFFER {
            self.overflowed = true;
            self.buf.clear();
            return Vec::new();
        }
        self.drain_frames()
    }

    fn drain_frames(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            let Some(idx) = find_sub(&self.buf, b"data:") else {
                break;
            };
            let mut start = idx + 5;
            while start < self.buf.len() && (self.buf[start] == b' ' || self.buf[start] == b'\t') {
                start += 1;
            }
            if start >= self.buf.len() {
                break;
            }
            if self.buf[start..].starts_with(b"[DONE]") {
                out.push("[DONE]".to_string());
                self.buf.drain(..start + 6);
                continue;
            }
            if self.buf[start] != b'{' {
                // not a payload we understand; drop the token and keep scanning
                self.buf.drain(..start);
                continue;
            }
            match json_object_end(&self.buf, start) {
                Some(end) => {
                    let payload = String::from_utf8_lossy(&self.buf[start..end]).to_string();
                    out.push(payload);
                    self.buf.drain(..end);
                }
                None => break, // wait for more bytes
            }
        }
        out
    }

    /// A stream can end mid-frame; try one last time.
    pub fn flush(&mut self) -> Vec<String> {
        if self.overflowed {
            self.buf.clear();
            return Vec::new();
        }
        let out = self.drain_frames();
        self.buf.clear();
        out
    }

    pub fn overflowed(&self) -> bool {
        self.overflowed
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum FragmentKind {
    #[default]
    Response,
    Think,
    Search,
}

impl FragmentKind {
    fn from_type(ty: &str) -> Self {
        if ty.eq_ignore_ascii_case("THINK") {
            Self::Think
        } else if ty.eq_ignore_ascii_case("RESPONSE") {
            Self::Response
        } else {
            // Non-answer fragments (SEARCH, IMAGE_SEARCH, TOOL, STATUS, etc.)
            // must never leak internal status lines into answer content.
            Self::Search
        }
    }
}

#[derive(Default)]
struct NoiseFilter {
    pending: String,
}

impl NoiseFilter {
    fn feed(&mut self, chunk: &str) -> String {
        self.pending.push_str(chunk);
        self.pending = strip_search_noise(&self.pending);
        let hold = trailing_banner_prefix_len(&self.pending);
        let emit_len = self.pending.len() - hold;
        let out = self.pending[..emit_len].to_string();
        self.pending.drain(..emit_len);
        out
    }

    fn flush(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let cleaned = strip_search_noise(&self.pending);
        self.pending.clear();
        cleaned
    }
}

/// Stateful stream parser: knows which fragment (SEARCH / THINK / RESPONSE) deltas
/// belong to and how much of a snapshot was already emitted.
#[derive(Default)]
pub struct StreamParser {
    buf: SseBuffer,
    frag_id: Option<u64>,
    frag_kind: FragmentKind,
    emitted: usize,
    noise: NoiseFilter,
}

impl StreamParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<DsEvent> {
        let payloads = self.buf.push(chunk);
        let mut out = Vec::new();
        for p in payloads {
            out.extend(self.handle(&p));
        }
        out
    }

    pub fn flush(&mut self) -> Vec<DsEvent> {
        let payloads = self.buf.flush();
        let mut out = Vec::new();
        for p in payloads {
            out.extend(self.handle(&p));
        }
        let rem = self.noise.flush();
        if !rem.is_empty() {
            out.push(DsEvent::Content(rem));
        }
        out
    }

    pub fn overflowed(&self) -> bool {
        self.buf.overflowed()
    }

    fn emit(&mut self, text: String) -> DsEvent {
        match self.frag_kind {
            FragmentKind::Think => DsEvent::Thinking(text),
            FragmentKind::Search => DsEvent::Ignore,
            FragmentKind::Response => {
                let clean = self.noise.feed(&text);
                if clean.is_empty() {
                    DsEvent::Ignore
                } else {
                    DsEvent::Content(clean)
                }
            }
        }
    }

    fn flush_noise_into(&mut self, out: &mut Vec<DsEvent>) {
        let rem = self.noise.flush();
        if !rem.is_empty() {
            out.push(DsEvent::Content(rem));
        }
    }

    fn set_fragment(&mut self, id: Option<u64>, ty: &str) {
        if self.frag_id != id {
            self.frag_id = id;
            self.emitted = 0;
        }
        self.frag_kind = FragmentKind::from_type(ty);
    }

    fn process_fragment(&mut self, frag: &Value, out: &mut Vec<DsEvent>) {
        let Some(obj) = frag.as_object() else {
            return;
        };
        let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("RESPONSE");
        let is_search = ty.eq_ignore_ascii_case("SEARCH")
            || obj.contains_key("results")
            || obj.contains_key("search_results");

        if is_search {
            let mut items = Vec::new();
            if let Some(r) = obj.get("results") {
                collect_search(r, &mut items);
            }
            if let Some(sr) = obj.get("search_results") {
                collect_search(sr, &mut items);
            }
            if items.is_empty() && ty.eq_ignore_ascii_case("SEARCH") {
                collect_search(frag, &mut items);
            }
            if !items.is_empty() {
                out.push(DsEvent::Search(items));
            }
        }

        let id = obj.get("id").and_then(|v| v.as_u64());
        let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_some() || obj.contains_key("type") {
            self.set_fragment(id, ty);
        }
        let delta = self.take_delta(content);
        if !delta.is_empty() {
            let ev = self.emit(delta);
            if !matches!(ev, DsEvent::Ignore) {
                out.push(ev);
            }
        }
    }

    /// Emit only the part of `content` we have not sent yet.
    fn take_delta(&mut self, content: &str) -> String {
        if content.len() < self.emitted {
            self.emitted = 0;
        }
        let mut start = self.emitted.min(content.len());
        while start < content.len() && !content.is_char_boundary(start) {
            start += 1;
        }
        let delta = content[start..].to_string();
        self.emitted = content.len();
        delta
    }

    fn handle(&mut self, payload: &str) -> Vec<DsEvent> {
        let payload = payload.trim();
        if payload == "[DONE]" {
            let mut out = Vec::new();
            self.flush_noise_into(&mut out);
            out.push(DsEvent::Done);
            return out;
        }
        let value: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => return vec![DsEvent::Ignore],
        };
        let Some(obj) = value.as_object() else {
            return vec![DsEvent::Ignore];
        };

        if let Some(code) = obj.get("code").and_then(|c| c.as_i64()) {
            if code != 0 {
                let msg = obj
                    .get("msg")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown upstream error");
                return vec![DsEvent::Status(format!("ERROR:{code}:{msg}"))];
            }
        }

        // incremental patch
        if let Some(path) = obj.get("p").and_then(|p| p.as_str()) {
            let op = obj.get("o").and_then(|o| o.as_str()).unwrap_or("SET");
            let value = obj.get("v").cloned().unwrap_or(Value::Null);
            return self.patch(path, op, value);
        }

        if let Some(inner) = obj.get("v") {
            // bare delta: text appended to the fragment currently being written
            if let Some(s) = inner.as_str() {
                if s.is_empty() {
                    return vec![DsEvent::Ignore];
                }
                self.emitted += s.len();
                return vec![self.emit(s.to_string())];
            }
            // full snapshot
            if let Some(resp) = inner.get("response") {
                return self.snapshot(resp);
            }
        }

        vec![DsEvent::Ignore]
    }

    fn snapshot(&mut self, resp: &Value) -> Vec<DsEvent> {
        let mut out = Vec::new();
        let mut search_items = Vec::new();

        if let Some(sr) = resp.get("search_results").or_else(|| resp.get("results")) {
            collect_search(sr, &mut search_items);
        }

        if let Some(frags) = resp.get("fragments").and_then(|f| f.as_array()) {
            for f in frags {
                collect_fragment_search(f, &mut search_items);
            }
            if !search_items.is_empty() {
                out.push(DsEvent::Search(search_items));
            }
            if let Some(last) = frags.last() {
                let id = last.get("id").and_then(|v| v.as_u64());
                let ty = last
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("RESPONSE");
                let content = last.get("content").and_then(|v| v.as_str()).unwrap_or("");
                self.set_fragment(id, ty);
                let delta = self.take_delta(content);
                if !delta.is_empty() {
                    let ev = self.emit(delta);
                    if !matches!(ev, DsEvent::Ignore) {
                        out.push(ev);
                    }
                }
            }
        } else if !search_items.is_empty() {
            out.push(DsEvent::Search(search_items));
        }

        if let Some(status) = resp.get("status").and_then(|v| v.as_str()) {
            out.push(DsEvent::Status(status.to_string()));
        }
        if let Some(n) = resp.get("accumulated_token_usage").and_then(|v| v.as_u64()) {
            out.push(DsEvent::Usage { input: 0, output: n });
        }
        if out.is_empty() {
            out.push(DsEvent::Ignore);
        }
        out
    }

    fn patch(&mut self, path: &str, op: &str, value: Value) -> Vec<DsEvent> {
        let p = normalize_path(path);

        if op.eq_ignore_ascii_case("BATCH") {
            let mut out = Vec::new();
            if let Some(arr) = value.as_array() {
                for item in arr {
                    let ip = item
                        .get("p")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .trim_start_matches('/');
                    let io = item.get("o").and_then(|x| x.as_str()).unwrap_or("SET");
                    let iv = item.get("v").cloned().unwrap_or(Value::Null);
                    let full = if p.is_empty() {
                        ip.to_string()
                    } else if ip.is_empty() {
                        p.clone()
                    } else {
                        format!("{p}/{ip}")
                    };
                    out.extend(self.patch(&full, io, iv));
                }
            }
            return out;
        }

        if p == "search_results" || p == "results" || is_search_subpath(&p) {
            let mut items = Vec::new();
            collect_search(&value, &mut items);
            return if items.is_empty() {
                vec![DsEvent::Ignore]
            } else {
                vec![DsEvent::Search(items)]
            };
        }

        if is_fragment_content_path(&p) {
            return match value.as_str() {
                Some(s) if !s.is_empty() => {
                    self.emitted += s.len();
                    let ev = self.emit(s.to_string());
                    if matches!(ev, DsEvent::Ignore) {
                        vec![DsEvent::Ignore]
                    } else {
                        vec![ev]
                    }
                }
                _ => vec![DsEvent::Ignore],
            };
        }

        if is_fragment_type_path(&p) {
            if let Some(ty) = value.as_str() {
                self.frag_kind = FragmentKind::from_type(ty);
            }
            return vec![DsEvent::Ignore];
        }

        if is_fragment_object_path(&p) {
            let mut out = Vec::new();
            match &value {
                Value::Array(arr) => {
                    for f in arr {
                        self.process_fragment(f, &mut out);
                    }
                }
                Value::Object(_) => self.process_fragment(&value, &mut out),
                _ => {}
            }
            return if out.is_empty() {
                vec![DsEvent::Ignore]
            } else {
                out
            };
        }

        match p.as_str() {
            // a new fragment was appended or replaced: {"id":3,"type":"RESPONSE","content":"I"}
            "fragments" => {
                let mut out = Vec::new();
                if let Some(arr) = value.as_array() {
                    for f in arr {
                        self.process_fragment(f, &mut out);
                    }
                } else if value.is_object() {
                    self.process_fragment(&value, &mut out);
                }
                if out.is_empty() {
                    out.push(DsEvent::Ignore);
                }
                out
            }
            "thinking_content" => value
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| vec![DsEvent::Thinking(s.to_string())])
                .unwrap_or_else(|| vec![DsEvent::Ignore]),
            "status" => {
                let mut out = Vec::new();
                if let Some(s) = value.as_str() {
                    if s.eq_ignore_ascii_case("FINISHED") {
                        self.flush_noise_into(&mut out);
                    }
                    out.push(DsEvent::Status(s.to_string()));
                } else {
                    out.push(DsEvent::Ignore);
                }
                out
            }
            "accumulated_token_usage" | "token_usage" | "usage" => {
                let (i, o) = read_usage(&value);
                vec![DsEvent::Usage { input: i, output: o }]
            }
            _ => vec![DsEvent::Ignore],
        }
    }
}

fn normalize_path(path: &str) -> String {
    let p = path.trim_start_matches('/');
    if p == "response" {
        String::new()
    } else {
        p.strip_prefix("response/").unwrap_or(p).to_string()
    }
}

fn is_fragment_seg(seg: &str) -> bool {
    seg == "-1" || (!seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()))
}

fn is_fragment_content_path(p: &str) -> bool {
    if p == "content" {
        return true;
    }
    let mut parts = p.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some("fragments"), Some(seg), Some("content"), None) if is_fragment_seg(seg)
    )
}

fn is_fragment_type_path(p: &str) -> bool {
    let mut parts = p.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some("fragments"), Some(seg), Some("type"), None) if is_fragment_seg(seg)
    )
}

fn is_fragment_object_path(p: &str) -> bool {
    let mut parts = p.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some("fragments"), Some(seg), None) if is_fragment_seg(seg)
    )
}

fn is_search_subpath(p: &str) -> bool {
    p.ends_with("/search_results") || p.ends_with("/results")
}

fn collect_fragment_search(frag: &Value, out: &mut Vec<SearchItem>) {
    let Some(obj) = frag.as_object() else {
        return;
    };
    let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let has_search = ty.eq_ignore_ascii_case("SEARCH")
        || obj.contains_key("results")
        || obj.contains_key("search_results");
    if !has_search {
        return;
    }
    if let Some(r) = obj.get("results") {
        collect_search(r, out);
    }
    if let Some(sr) = obj.get("search_results") {
        collect_search(sr, out);
    }
    if !obj.contains_key("results") && !obj.contains_key("search_results") && ty.eq_ignore_ascii_case("SEARCH") {
        collect_search(frag, out);
    }
}

fn read_usage(v: &Value) -> (u64, u64) {
    if let Some(n) = v.as_u64() {
        return (0, n);
    }
    let get = |keys: &[&str]| -> u64 {
        for k in keys {
            if let Some(n) = v.get(*k).and_then(|x| x.as_u64()) {
                return n;
            }
        }
        0
    };
    (
        get(&["prompt_tokens", "input_tokens", "input"]),
        get(&["completion_tokens", "output_tokens", "output"]),
    )
}

fn first_str(map: &serde_json::Map<String, Value>, keys: &[&str]) -> String {
    for k in keys {
        if let Some(s) = map.get(*k).and_then(|x| x.as_str()) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

fn format_unix_date(ts: i64) -> String {
    let secs = if ts > 1_000_000_000_000 { ts / 1000 } else { ts };
    if secs <= 0 {
        return String::new();
    }
    // Howard Hinnant civil-from-days algorithm in UTC.
    let z = secs.div_euclid(86_400) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    format!("{y:04}/{m:02}/{d:02}")
}

fn extract_published_at(map: &serde_json::Map<String, Value>) -> String {
    for k in ["published_at", "publishedAt", "date", "publish_time", "publishTime"] {
        let Some(val) = map.get(k) else { continue };
        if let Some(s) = val.as_str() {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        } else if let Some(n) = val.as_i64() {
            let formatted = format_unix_date(n);
            if !formatted.is_empty() {
                return formatted;
            }
        }
    }
    String::new()
}

fn collect_search(v: &Value, out: &mut Vec<SearchItem>) {
    match v {
        Value::Array(arr) => {
            for item in arr {
                collect_search(item, out);
            }
        }
        Value::Object(map) => {
            let url = first_str(map, &["url", "link", "href"]);
            if !url.is_empty() {
                let title = first_str(map, &["title", "name"]);
                let snippet = first_str(map, &["snippet", "description", "content", "summary"]);
                let site_name = first_str(map, &["site_name", "siteName", "source"]);
                let published_at = extract_published_at(map);
                let cite_index = ["cite_index", "citeIndex", "citation_index", "index"]
                    .iter()
                    .find_map(|k| {
                        map.get(*k).and_then(|val| {
                            val.as_u64()
                                .or_else(|| val.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
                        })
                    });
                out.push(SearchItem {
                    title,
                    url,
                    snippet,
                    site_name,
                    published_at,
                    cite_index,
                });
                return;
            }
            for (_, child) in map {
                if child.is_array() || child.is_object() {
                    collect_search(child, out);
                }
            }
        }
        _ => {}
    }
}


const STATIC_SEARCH_BANNERS: &[&str] = &[
    "Searching for images...",
    "Searching for images",
    "Searching the web...",
    "Searching the web",
    "Searching...",
    "Relevant images found",
    "Relevant results found",
];

fn match_banner_at(s: &str) -> Option<usize> {
    for banner in STATIC_SEARCH_BANNERS {
        if s.starts_with(banner) {
            return Some(banner.len());
        }
    }
    if let Some(after) = s.strip_prefix("Found ") {
        let digits = after.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 {
            let tail = &after[digits..];
            for suffix in [
                " web pages",
                " web page",
                " relevant images",
                " images",
                " results",
                " result",
            ] {
                if tail.starts_with(suffix) {
                    return Some("Found ".len() + digits + suffix.len());
                }
            }
        }
    }
    if let Some(after) = s.strip_prefix("Reading ") {
        let digits = after.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 {
            let tail = &after[digits..];
            for suffix in [" web pages", " pages"] {
                if tail.starts_with(suffix) {
                    return Some("Reading ".len() + digits + suffix.len());
                }
            }
        }
    }
    None
}

fn is_partial_counted_banner(tail: &str, prefix: &str, suffixes: &[&str]) -> bool {
    if prefix.starts_with(tail) {
        return true;
    }
    let Some(after) = tail.strip_prefix(prefix) else {
        return false;
    };
    let digits = after.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return false;
    }
    let rem = &after[digits..];
    rem.is_empty() || suffixes.iter().any(|suf| suf.starts_with(rem))
}

fn is_partial_banner(tail: &str) -> bool {
    if STATIC_SEARCH_BANNERS.iter().any(|b| b.starts_with(tail)) {
        return true;
    }
    if is_partial_counted_banner(
        tail,
        "Found ",
        &[
            " web pages",
            " web page",
            " relevant images",
            " images",
            " results",
            " result",
        ],
    ) {
        return true;
    }
    is_partial_counted_banner(tail, "Reading ", &[" web pages", " pages"])
}

fn trailing_banner_prefix_len(s: &str) -> usize {
    let max_window = 36;
    let start_bound = s.len().saturating_sub(max_window);
    for (idx, _) in s.char_indices() {
        if idx < start_bound {
            continue;
        }
        let tail = &s[idx..];
        if is_partial_banner(tail) {
            return tail.len();
        }
    }
    0
}

/// The web/image search flow prefixes answers with status banners such as
/// "Searching for images...", "Relevant images found", and "Found 16 web pages".
/// They are internal UI banners upstream, so they are dropped before text reaches clients.
fn strip_search_noise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some(len) = match_banner_at(&text[i..]) {
            i += len;
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(parser: &mut StreamParser, raw: &str) -> (String, String, Vec<String>) {
        let mut content = String::new();
        let mut thinking = String::new();
        let mut status = Vec::new();
        for ev in parser.push(raw.as_bytes()) {
            match ev {
                DsEvent::Content(t) => content.push_str(&t),
                DsEvent::Thinking(t) => thinking.push_str(&t),
                DsEvent::Status(s) => status.push(s),
                _ => {}
            }
        }
        (content, thinking, status)
    }

    #[test]
    fn bare_deltas_are_content() {
        let mut p = StreamParser::new();
        let (c, t, _) = collect(&mut p, "data: {\"v\":\"hello\"}\ndata: {\"v\":\" world\"}\n\n");
        assert_eq!(c, "hello world");
        assert_eq!(t, "");
    }

    #[test]
    fn snapshot_fragment_then_append() {
        let mut p = StreamParser::new();
        let snap = "data: {\"v\":{\"response\":{\"status\":\"WIP\",\"fragments\":[{\"id\":2,\"type\":\"RESPONSE\",\"content\":\"Jak\"}]}}}\n\n";
        assert_eq!(collect(&mut p, snap).0, "Jak");
        assert_eq!(
            collect(&mut p, "data: {\"p\":\"response/fragments/-1/content\",\"o\":\"APPEND\",\"v\":\"arta\"}\n\n").0,
            "arta"
        );
        assert_eq!(collect(&mut p, "data: {\"v\":\" adalah\"}\n\n").0, " adalah");
    }

    #[test]
    fn repeated_snapshot_does_not_duplicate() {
        let mut p = StreamParser::new();
        let snap = "data: {\"v\":{\"response\":{\"fragments\":[{\"id\":2,\"type\":\"RESPONSE\",\"content\":\"Jakarta\"}]}}}\n\n";
        assert_eq!(collect(&mut p, snap).0, "Jakarta");
        assert_eq!(collect(&mut p, snap).0, "");
    }

    #[test]
    fn think_fragment_then_response_fragment() {
        let mut p = StreamParser::new();
        let (c, t, _) = collect(
            &mut p,
            "data: {\"v\":{\"response\":{\"fragments\":[{\"id\":2,\"type\":\"THINK\",\"content\":\"We\"}]}}}\n\n",
        );
        assert_eq!(t, "We");
        assert_eq!(c, "");
        assert_eq!(collect(&mut p, "data: {\"v\":\" need\"}\n\n").1, " need");
        let (c3, t3, _) = collect(
            &mut p,
            "data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"id\":3,\"type\":\"RESPONSE\",\"content\":\"I\"}]}\n\n",
        );
        assert_eq!(c3, "I");
        assert_eq!(t3, "");
        assert_eq!(collect(&mut p, "data: {\"v\":\" answer\"}\n\n").0, " answer");
    }

    #[test]
    fn batch_carries_usage_and_status() {
        let mut p = StreamParser::new();
        let mut usage = 0u64;
        let mut status = Vec::new();
        for ev in p.push(
            b"data: {\"p\":\"response\",\"o\":\"BATCH\",\"v\":[{\"p\":\"accumulated_token_usage\",\"v\":82},{\"p\":\"quasi_status\",\"v\":\"FINISHED\"}]}\n\n",
        ) {
            match ev {
                DsEvent::Usage { output, .. } => usage = output,
                DsEvent::Status(s) => status.push(s),
                _ => {}
            }
        }
        assert_eq!(usage, 82);
        // quasi_status is not a terminal status, only response/status is
        assert!(status.is_empty());
    }

    #[test]
    fn full_real_stream() {
        let raw = concat!(
            "event: ready\ndata: {\"request_message_id\":1,\"response_message_id\":2,\"model_type\":\"default\"}",
            "event: update_session\ndata: {\"updated_at\":1792656247.086736}",
            "data: {\"v\":{\"response\":{\"status\":\"WIP\",\"fragments\":[{\"id\":2,\"type\":\"RESPONSE\",\"content\":\"Jak\"}]}}}",
            "data: {\"p\":\"response/fragments/-1/content\",\"o\":\"APPEND\",\"v\":\"arta\"}",
            "data: {\"v\":\" adalah\"}\n\n",
            "data: {\"v\":\" ibu\"}\n\n",
            "data: {\"p\":\"response\",\"o\":\"BATCH\",\"v\":[{\"p\":\"accumulated_token_usage\",\"v\":82},{\"p\":\"quasi_status\",\"v\":\"FINISHED\"}]}     \n\n",
            "data: {\"p\":\"response/status\",\"o\":\"SET\",\"v\":\"FINISHED\"}\n\n",
            "event: title\ndata: {\"content\":\"Fakta Jakarta\"}",
            "event: close\ndata: {\"click_behavior\":\"none\",\"auto_resume\":false}\n\n",
        );
        let mut p = StreamParser::new();
        let (c, _, status) = collect(&mut p, raw);
        assert_eq!(c, "Jakarta adalah ibu");
        assert!(status.contains(&"FINISHED".to_string()));
    }

    #[test]
    fn upstream_error_code() {
        let mut p = StreamParser::new();
        let (_, _, status) = collect(&mut p, "data: {\"code\":40301,\"msg\":\"INVALID_POW_RESPONSE\"}\n\n");
        assert!(status[0].starts_with("ERROR:"));
    }

    #[test]
    fn search_fragment_snapshot_and_incremental_results() {
        let mut p = StreamParser::new();
        let snap = concat!(
            "data: {\"v\":{\"response\":{\"status\":\"WIP\",\"fragments\":[",
            "{\"id\":1,\"type\":\"SEARCH\",\"results\":[",
            "{\"url\":\"https://rust-lang.org/learn\",\"title\":\"Rust 1.100\",\"snippet\":\"Release notes\",\"site_name\":\"Rust\",\"published_at\":\"2026/11/10\",\"cite_index\":6}",
            "]},",
            "{\"id\":2,\"type\":\"RESPONSE\",\"content\":\"Versi stabil [reference:5].\"}",
            "]}}}\n\n"
        );
        let mut search = Vec::new();
        let mut content = String::new();
        for ev in p.push(snap.as_bytes()) {
            match ev {
                DsEvent::Search(items) => search.extend(items),
                DsEvent::Content(t) => content.push_str(&t),
                _ => {}
            }
        }
        assert_eq!(search.len(), 1);
        assert_eq!(search[0].title, "Rust 1.100");
        assert_eq!(search[0].site_name, "Rust");
        assert_eq!(search[0].published_at, "2026/11/10");
        assert_eq!(search[0].cite_index, Some(6));
        assert_eq!(content, "Versi stabil [reference:5].");

        let patch_results = concat!(
            "data: {\"p\":\"response/fragments/-1/results\",\"o\":\"APPEND\",\"v\":[",
            "{\"url\":\"https://example.com/news\",\"title\":\"Berita Terbaru\",\"snippet\":\"Detail ringkas\",\"source\":\"Example\",\"publish_time\":1794528000,\"index\":7}",
            "]}\n\n",
            "data: {\"p\":\"response\",\"o\":\"BATCH\",\"v\":[",
            "{\"p\":\"fragments\",\"o\":\"APPEND\",\"v\":[{\"id\":3,\"type\":\"SEARCH\",\"content\":\"Searching...\",\"results\":[{\"link\":\"https://docs.rs\",\"name\":\"Docs.rs\"}]}]},",
            "{\"p\":\"fragments\",\"o\":\"APPEND\",\"v\":[{\"id\":4,\"type\":\"RESPONSE\",\"content\":\" Selesai\"}]}",
            "]}\n\n"
        );
        let mut batch_search = Vec::new();
        let mut batch_content = String::new();
        for ev in p.push(patch_results.as_bytes()) {
            match ev {
                DsEvent::Search(items) => batch_search.extend(items),
                DsEvent::Content(t) => batch_content.push_str(&t),
                _ => {}
            }
        }
        assert_eq!(batch_search.len(), 2);
        assert_eq!(batch_search[0].url, "https://example.com/news");
        assert_eq!(batch_search[0].site_name, "Example");
        assert!(!batch_search[0].published_at.is_empty());
        assert_eq!(batch_search[0].cite_index, Some(7));
        assert_eq!(batch_search[1].url, "https://docs.rs");
        assert_eq!(batch_content, " Selesai");
        assert!(!batch_content.contains("Searching..."));
    }

    #[test]
    fn strips_image_search_banners_even_when_split_across_chunks() {
        let mut p = StreamParser::new();
        let raw = concat!(
            "data: {\"v\":{\"response\":{\"status\":\"WIP\",\"fragments\":[{\"id\":1,\"type\":\"IMAGE_SEARCH\",\"content\":\"Looking up image\"}]}}}\n\n",
            "data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"id\":2,\"type\":\"RESPONSE\",\"content\":\"Searching for \"}]}\n\n",
            "data: {\"v\":\"images...\"}\n\n",
            "data: {\"v\":\"Relevant \"}\n\n",
            "data: {\"v\":\"images found\"}\n\n",
            "data: {\"v\":\"Relevant images foundRelevant images \"}\n\n",
            "data: {\"v\":\"foundBerdasarkan \"}\n\n",
            "data: {\"v\":\"gambar tersebut.\"}\n\n",
            "data: {\"p\":\"response/status\",\"o\":\"SET\",\"v\":\"FINISHED\"}\n\n"
        );
        // Feed 7 bytes at a time to stress chunk boundaries as well.
        let mut content = String::new();
        for piece in raw.as_bytes().chunks(7) {
            for ev in p.push(piece) {
                if let DsEvent::Content(t) = ev {
                    content.push_str(&t);
                }
            }
        }
        for ev in p.flush() {
            if let DsEvent::Content(t) = ev {
                content.push_str(&t);
            }
        }
        assert_eq!(content, "Berdasarkan gambar tersebut.");
    }

    #[test]
    fn preserves_legitimate_prose_starting_with_found_or_searching() {
        let mut p = StreamParser::new();
        let raw = concat!(
            "data: {\"v\":\"Found \"}\n\n",
            "data: {\"v\":\"a bug while Searching \"}\n\n",
            "data: {\"v\":\"my repo\"}\n\n",
            "data: [DONE]\n\n"
        );
        let (c, _, _) = collect(&mut p, raw);
        assert_eq!(c, "Found a bug while Searching my repo");
    }
}
