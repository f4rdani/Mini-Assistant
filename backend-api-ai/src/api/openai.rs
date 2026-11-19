use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Multipart, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api::authorize;
use crate::convert::{self, tools::ToolParser};
use crate::deepseek::auth::Account;
use crate::deepseek::client::ByteStream;
use crate::deepseek::stream::{DsEvent, SearchItem, StreamParser};
use crate::error::AppError;
use crate::state::AppState;
use crate::util::{approx_tokens, now_secs, truncate};

/// If the upstream goes silent for this long, give up instead of holding the
/// connection (and the account) forever.
const IDLE_TIMEOUT: Duration = Duration::from_secs(180);

/// Keeps the account marked busy for as long as the stream lives.
struct BusyGuard(Arc<Account>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0
            .busy
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

fn search_results_json(items: &[SearchItem]) -> Value {
    let list: Vec<Value> = items
        .iter()
        .map(|i| {
            let mut obj = json!({
                "title": i.title,
                "url": i.url,
                "snippet": i.snippet,
            });
            if !i.site_name.is_empty() {
                obj["site_name"] = json!(i.site_name);
            }
            if !i.published_at.is_empty() {
                obj["published_at"] = json!(i.published_at);
            }
            if let Some(idx) = i.cite_index {
                obj["cite_index"] = json!(idx);
            }
            obj
        })
        .collect();
    json!({ "search_results": list })
}

pub(crate) fn is_retryable(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    [
        "40001",
        "40002",
        "40003",
        "40301",
        "401",
        "403",
        "429",
        "500",
        "502",
        "503",
        "504",
        "rate limit",
        "too many",
        "quota",
        "exceeded",
        "reach",
        "limit",
        "frequency",
        "frequently",
        "try again later",
        "busy",
        "invalid token",
        "authorization failed",
        "timed out",
        "timeout",
        "error sending request",
        "connection",
        "unexpected content-type",
    ]
    .iter()
    .any(|m| lower.contains(m))
}

pub(crate) fn cooldown_secs_for(msg: &str) -> u64 {
    let lower = msg.to_ascii_lowercase();
    if [
        "40002",
        "40003",
        "401",
        "403",
        "invalid token",
        "authorization failed",
    ]
    .iter()
    .any(|m| lower.contains(m))
    {
        return 300;
    }
    if [
        "429",
        "rate limit",
        "too many",
        "quota",
        "exceeded",
        "reach",
        "limit",
        "frequency",
        "frequently",
        "try again later",
    ]
    .iter()
    .any(|m| lower.contains(m))
    {
        return 90;
    }
    30
}

/// Everything that has to succeed before the first token can flow.
async fn build(
    st: &AppState,
    acc: &Account,
    body: &Value,
) -> Result<(ByteStream, String, String, String), AppError> {
    let prepared = convert::prepare(body, &st.ds, acc, &st.cfg.default_model).await?;
    let prompt_text = prepared.prompt.clone();
    let model = prepared.model.clone();
    let session_id = st.session_id(acc).await?;

    let upstream_body = json!({
        "chat_session_id": session_id,
        "parent_message_id": Value::Null,
        "prompt": prepared.prompt,
        "ref_file_ids": prepared.ref_file_ids,
        "thinking_enabled": prepared.thinking,
        "search_enabled": prepared.search,
    });

    let upstream = match st.ds.completion(acc, &upstream_body).await {
        Ok(u) => u,
        Err(e) => {
            // The session already exists upstream; without this it would show up
            // in the account's web history forever.
            if st.cfg.delete_session && !st.cfg.reuse_session {
                if let Err(de) = st.ds.delete_session(acc, &session_id).await {
                    tracing::debug!("cleanup after failed completion: {de}");
                }
            }
            return Err(e.into());
        }
    };
    Ok((upstream, prompt_text, model, session_id))
}

/// Deletes the upstream chat session when the response is done (or the client
/// disconnects), so the account's web UI does not fill up with API traffic.
struct SessionJanitor {
    st: Arc<AppState>,
    acc: Arc<Account>,
    sid: String,
    enabled: bool,
}

impl SessionJanitor {
    fn new(st: Arc<AppState>, acc: Arc<Account>, sid: String, enabled: bool) -> Self {
        Self { st, acc, sid, enabled }
    }

    /// Deletes the session now, before the response is finished. The Drop impl
    /// stays as a backstop for aborted streams.
    async fn delete_now(&mut self) {
        if !self.enabled {
            return;
        }
        self.enabled = false;
        if let Err(e) = self.st.ds.delete_session(&self.acc, &self.sid).await {
            tracing::debug!("delete_session({}) failed: {e}", self.sid);
        } else {
            tracing::debug!("deleted session {}", self.sid);
        }
    }
}

impl Drop for SessionJanitor {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }
        let st = self.st.clone();
        let acc = self.acc.clone();
        let sid = self.sid.clone();
        tokio::spawn(async move {
            // let the upstream finish persisting the assistant message first
            tokio::time::sleep(Duration::from_millis(800)).await;
            if let Err(e) = st.ds.delete_session(&acc, &sid).await {
                tracing::debug!("delete_session({sid}) failed: {e}");
            } else {
                tracing::debug!("deleted session {sid}");
            }
        });
    }
}

struct PrimedStream {
    upstream: ByteStream,
    sp: StreamParser,
    pre_events: Vec<DsEvent>,
    eof: bool,
}

/// Waits for the first meaningful upstream SSE signal before committing HTTP 200.
/// Any `ERROR:...` event received before real output triggers account cooldown & fallback.
async fn prime_stream(mut upstream: ByteStream) -> Result<PrimedStream, String> {
    let mut sp = StreamParser::new();
    let mut buffered: Vec<DsEvent> = Vec::new();
    loop {
        let item = match tokio::time::timeout(IDLE_TIMEOUT, upstream.next()).await {
            Ok(Some(item)) => item,
            Ok(None) => {
                for ev in sp.flush() {
                    if let DsEvent::Status(ref s) = ev {
                        if s.starts_with("ERROR:")
                            && !buffered.iter().any(|b| {
                                matches!(
                                    b,
                                    DsEvent::Thinking(_) | DsEvent::Content(_) | DsEvent::Search(_)
                                )
                            })
                        {
                            return Err(s.clone());
                        }
                    }
                    buffered.push(ev);
                }
                return Ok(PrimedStream {
                    upstream,
                    sp,
                    pre_events: buffered,
                    eof: true,
                });
            }
            Err(_) => return Err("upstream timed out".to_string()),
        };

        let bytes = item.map_err(|e| format!("stream error: {e}"))?;
        let events = sp.push(&bytes);
        if sp.overflowed() {
            return Err("upstream frame too large".to_string());
        }

        let mut saw_real_output = false;
        let mut saw_done = false;
        let mut early_error: Option<String> = None;
        for ev in events {
            match &ev {
                DsEvent::Thinking(_) | DsEvent::Content(_) | DsEvent::Search(_) => {
                    saw_real_output = true;
                    buffered.push(ev);
                }
                DsEvent::Status(s) if s.starts_with("ERROR:") => {
                    if !saw_real_output {
                        early_error = Some(s.clone());
                        break;
                    }
                    buffered.push(ev);
                }
                DsEvent::Done => {
                    saw_done = true;
                    buffered.push(ev);
                }
                DsEvent::Usage { .. } => buffered.push(ev),
                DsEvent::Status(_) | DsEvent::Ignore => {}
            }
        }
        if let Some(err) = early_error {
            return Err(err);
        }
        if saw_real_output || saw_done {
            return Ok(PrimedStream {
                upstream,
                sp,
                pre_events: buffered,
                eof: false,
            });
        }
    }
}

pub async fn chat_completions(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Response, AppError> {
    authorize(&st.cfg, &headers)?;

    let Json(body) = body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            AppError::TooLarge("request body too large (max 4 MB)".into())
        } else {
            AppError::BadRequest(format!("invalid json body: {}", e.body_text()))
        }
    })?;

    let has_messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !has_messages {
        return Err(AppError::BadRequest("messages[] is required".into()));
    }

    let want_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Priority-first fallback: always try Key 1 first; if Key 1 is rate-limited
    // or errors (before real output), cool it down and fall back to Key 2, Key 3...
    let attempts = st.pool.len();
    if attempts == 0 {
        return Err(AppError::NoAccount);
    }
    let mut last: Option<String> = None;
    let mut tried: Vec<String> = Vec::with_capacity(attempts);

    for _ in 0..attempts {
        let acc = match st.pool.try_acquire_excluding(&tried) {
            Some(a) => a,
            None => {
                return Err(match last {
                    Some(m) => AppError::Upstream(m),
                    None => AppError::Busy,
                })
            }
        };
        let guard = BusyGuard(acc.clone());

        match build(&st, &acc, &body).await {
            Ok((upstream, prompt_text, model, session_id)) => {
                let mut janitor = SessionJanitor::new(
                    st.clone(),
                    acc.clone(),
                    session_id,
                    st.cfg.delete_session && !st.cfg.reuse_session,
                );
                match prime_stream(upstream).await {
                    Ok(primed) => {
                        acc.clear_error();
                        let id = format!("chatcmpl-{}", Uuid::new_v4().simple());
                        let created = now_secs();
                        return Ok(if want_stream {
                            sse_response(id, model, created, primed, guard, janitor, prompt_text)
                        } else {
                            collect_response(id, model, created, primed, guard, janitor, prompt_text)
                                .await
                        });
                    }
                    Err(err) => {
                        janitor.delete_now().await;
                        if st.cfg.reuse_session {
                            st.forget_session(&acc.name).await;
                        }
                        acc.set_error(err.clone());
                        drop(guard);
                        if attempts > 1 && is_retryable(&err) {
                            acc.set_cooldown(cooldown_secs_for(&err));
                            tracing::warn!("account {} failed on stream start, trying next: {err}", acc.name);
                            tried.push(acc.name.clone());
                            last = Some(err);
                            continue;
                        }
                        return Err(AppError::Upstream(err));
                    }
                }
            }
            Err(e) => {
                let msg = format!("{e}");
                let retryable = !matches!(e, AppError::BadRequest(_)) && is_retryable(&msg);
                acc.set_error(msg.clone());
                if st.cfg.reuse_session {
                    st.forget_session(&acc.name).await;
                }
                drop(guard);
                if attempts > 1 && retryable {
                    acc.set_cooldown(cooldown_secs_for(&msg));
                    tracing::warn!("account {} failed, trying next: {msg}", acc.name);
                    tried.push(acc.name.clone());
                    last = Some(msg);
                    continue;
                }
                return Err(e);
            }
        }
    }

    Err(AppError::Upstream(
        last.unwrap_or_else(|| "all accounts failed".into()),
    ))
}

fn chunk(id: &str, model: &str, created: u64, delta: Value, finish: Option<&str>) -> Event {
    let payload = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "logprobs": Value::Null,
            "finish_reason": finish,
        }],
    });
    Event::default().data(payload.to_string())
}

fn error_event(msg: &str) -> Event {
    Event::default().data(
        json!({ "error": { "message": msg, "type": "upstream_error" } }).to_string(),
    )
}

fn usage_event(id: &str, model: &str, created: u64, pt: u64, ct: u64) -> Event {
    Event::default().data(
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [],
            "usage": {
                "prompt_tokens": pt,
                "completion_tokens": ct,
                "total_tokens": pt + ct,
            },
        })
        .to_string(),
    )
}

fn sse_response(
    id: String,
    model: String,
    created: u64,
    primed: PrimedStream,
    guard: BusyGuard,
    janitor: SessionJanitor,
    prompt_text: String,
) -> Response {
    let stream = async_stream::stream! {
        let _guard = guard;
        let mut janitor = janitor;
        let PrimedStream {
            mut upstream,
            mut sp,
            mut pre_events,
            eof,
        } = primed;
        let mut parser = ToolParser::new();
        let mut aborted = false;
        let mut usage_in = 0u64;
        let mut usage_out = 0u64;
        let mut visible_text = String::new();
        let mut reasoning = String::new();

        yield Ok::<Event, Infallible>(chunk(
            &id,
            &model,
            created,
            json!({ "role": "assistant", "content": "" }),
            None,
        ));

        while !aborted {
            let events = if !pre_events.is_empty() {
                std::mem::take(&mut pre_events)
            } else if eof {
                break;
            } else {
                let item = match tokio::time::timeout(IDLE_TIMEOUT, upstream.next()).await {
                    Ok(Some(item)) => item,
                    Ok(None) => break,
                    Err(_) => {
                        yield Ok::<Event, Infallible>(error_event("upstream timed out"));
                        aborted = true;
                        break;
                    }
                };

                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => {
                        yield Ok::<Event, Infallible>(error_event(&format!("stream error: {e}")));
                        aborted = true;
                        break;
                    }
                };

                let evs = sp.push(&bytes);
                if sp.overflowed() {
                    yield Ok::<Event, Infallible>(error_event("upstream frame too large"));
                    aborted = true;
                    break;
                }
                evs
            };

            {
                for ev in events {
                    match ev {
                        DsEvent::Thinking(t) => {
                            reasoning.push_str(&t);
                            yield Ok(chunk(&id, &model, created, json!({ "reasoning_content": t }), None));
                        }
                        DsEvent::Content(t) => {
                            // always feed: the parser holds the tool block back
                            let visible = parser.feed(&t);
                            if !visible.is_empty() {
                                visible_text.push_str(&visible);
                                yield Ok(chunk(&id, &model, created, json!({ "content": visible }), None));
                            }
                        }
                        DsEvent::Search(items) => {
                            yield Ok(chunk(&id, &model, created, search_results_json(&items), None));
                        }
                        DsEvent::Status(s) => {
                            if s.starts_with("ERROR:") {
                                yield Ok(error_event(&s));
                                aborted = true;
                            }
                        }
                        DsEvent::Usage { input, output } => {
                            usage_in = input;
                            usage_out = output;
                        }
                        DsEvent::Done | DsEvent::Ignore => {}
                    }
                    if aborted {
                        break;
                    }
                }
            }
        }

        if !aborted {
            {
                for ev in sp.flush() {
                    match ev {
                        DsEvent::Content(t) => {
                            let visible = parser.feed(&t);
                            if !visible.is_empty() {
                                visible_text.push_str(&visible);
                                yield Ok(chunk(&id, &model, created, json!({ "content": visible }), None));
                            }
                        }
                        DsEvent::Thinking(t) => {
                            reasoning.push_str(&t);
                            yield Ok(chunk(&id, &model, created, json!({ "reasoning_content": t }), None));
                        }
                        DsEvent::Search(items) => {
                            yield Ok(chunk(&id, &model, created, search_results_json(&items), None));
                        }
                        _ => {}
                    }
                }
            }

            let tail = parser.finish();
            if !tail.is_empty() {
                visible_text.push_str(&tail);
                yield Ok(chunk(&id, &model, created, json!({ "content": tail }), None));
            }
            if let Some(raw) = parser.take_incomplete() {
                yield Ok::<Event, Infallible>(error_event(&format!(
                    "incomplete tool call dropped: {}",
                    truncate(&raw, 200)
                )));
            }

            let calls = parser.take_calls();
            if !calls.is_empty() {
                let arr: Vec<Value> = calls
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        json!({
                            "index": i,
                            "id": format!("call_{}", Uuid::new_v4().simple()),
                            "type": "function",
                            "function": { "name": c.name, "arguments": c.arguments },
                        })
                    })
                    .collect();
                yield Ok(chunk(&id, &model, created, json!({ "tool_calls": arr }), None));
                yield Ok(chunk(&id, &model, created, json!({}), Some("tool_calls")));
            } else {
                yield Ok(chunk(&id, &model, created, json!({}), Some("stop")));
            }

            let pt = if usage_in > 0 { usage_in } else { approx_tokens(&prompt_text) };
            let ct = if usage_out > 0 {
                usage_out
            } else {
                approx_tokens(&visible_text) + approx_tokens(&reasoning)
            };
            yield Ok(usage_event(&id, &model, created, pt, ct));
        } else {
            // keep the stream well formed for strict clients
            yield Ok(chunk(&id, &model, created, json!({}), Some("stop")));
        }

        // Clean up before the terminal frame: an explicit await cannot be lost
        // when the client disconnects right after the answer.
        janitor.delete_now().await;
        yield Ok::<Event, Infallible>(Event::default().data("[DONE]"));
    };

    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

struct CollectedPrimed {
    content: String,
    reasoning: String,
    usage_in: u64,
    usage_out: u64,
    incomplete: Option<String>,
    calls: Vec<crate::convert::tools::ParsedCall>,
}

async fn drain_primed(primed: PrimedStream) -> Result<CollectedPrimed, String> {
    let PrimedStream {
        mut upstream,
        mut sp,
        mut pre_events,
        eof,
    } = primed;
    let mut parser = ToolParser::new();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut usage_in = 0u64;
    let mut usage_out = 0u64;
    let mut error: Option<String> = None;

    loop {
        let events = if !pre_events.is_empty() {
            std::mem::take(&mut pre_events)
        } else if eof {
            break;
        } else {
            let item = match tokio::time::timeout(IDLE_TIMEOUT, upstream.next()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    error = Some("upstream timed out".into());
                    break;
                }
            };
            let bytes = match item {
                Ok(b) => b,
                Err(e) => {
                    error = Some(e.to_string());
                    break;
                }
            };
            let evs = sp.push(&bytes);
            if sp.overflowed() && error.is_none() {
                error = Some("upstream frame too large".to_string());
                break;
            }
            evs
        };

        for ev in events {
            match ev {
                DsEvent::Thinking(t) => reasoning.push_str(&t),
                DsEvent::Content(t) => {
                    let visible = parser.feed(&t);
                    content.push_str(&visible);
                }
                DsEvent::Usage { input, output } => {
                    usage_in = input;
                    usage_out = output;
                }
                DsEvent::Status(s) if s.starts_with("ERROR:") => {
                    error = Some(s);
                }
                _ => {}
            }
            if error.is_some() {
                break;
            }
        }
        if sp.overflowed() && error.is_none() {
            error = Some("upstream frame too large".to_string());
        }
        if error.is_some() {
            break;
        }
    }

    if let Some(e) = error {
        return Err(e);
    }

    for ev in sp.flush() {
        match ev {
            DsEvent::Content(t) => content.push_str(&parser.feed(&t)),
            DsEvent::Thinking(t) => reasoning.push_str(&t),
            DsEvent::Usage { input, output } => {
                usage_in = input;
                usage_out = output;
            }
            _ => {}
        }
    }

    content.push_str(&parser.finish());
    let incomplete = parser.take_incomplete();
    let calls = parser.take_calls();
    Ok(CollectedPrimed {
        content,
        reasoning,
        usage_in,
        usage_out,
        incomplete,
        calls,
    })
}

/// Probes a single named account directly without falling back to any other key.
pub(crate) async fn probe_account(
    st: &Arc<AppState>,
    acc: &Arc<Account>,
) -> Result<(String, u128), String> {
    if acc
        .busy
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_err()
    {
        return Err("Akun sedang sibuk dipakai chat, coba lagi sebentar".into());
    }
    let guard = BusyGuard(acc.clone());
    let started = std::time::Instant::now();
    let probe_body = json!({
        "model": st.cfg.default_model,
        "messages": [{ "role": "user", "content": "Balas satu kata: OK" }],
        "thinking": false,
        "search": false,
        "stream": false,
    });

    let res = match build(st, acc, &probe_body).await {
        Ok((upstream, _, _, session_id)) => {
            let mut janitor = SessionJanitor::new(
                st.clone(),
                acc.clone(),
                session_id,
                st.cfg.delete_session && !st.cfg.reuse_session,
            );
            match prime_stream(upstream).await {
                Ok(primed) => match drain_primed(primed).await {
                    Ok(collected) => {
                        drop(guard);
                        janitor.delete_now().await;
                        acc.clear_error();
                        let head: String = collected.content.trim().chars().take(40).collect();
                        Ok((
                            if head.is_empty() { "OK".into() } else { head },
                            started.elapsed().as_millis(),
                        ))
                    }
                    Err(err) => {
                        drop(guard);
                        janitor.delete_now().await;
                        Err(err)
                    }
                },
                Err(err) => {
                    drop(guard);
                    janitor.delete_now().await;
                    Err(err)
                }
            }
        }
        Err(e) => {
            drop(guard);
            Err(format!("{e}"))
        }
    };

    if let Err(ref err) = res {
        if st.cfg.reuse_session {
            st.forget_session(&acc.name).await;
        }
        acc.set_error(err.clone());
        acc.set_cooldown(cooldown_secs_for(err));
    }
    res
}

async fn collect_response(
    id: String,
    model: String,
    created: u64,
    primed: PrimedStream,
    guard: BusyGuard,
    mut janitor: SessionJanitor,
    prompt_text: String,
) -> Response {
    let drained = drain_primed(primed).await;
    drop(guard);
    janitor.delete_now().await;

    let CollectedPrimed {
        content,
        reasoning,
        usage_in,
        usage_out,
        incomplete,
        calls,
    } = match drained {
        Ok(d) => d,
        Err(e) => return AppError::Upstream(e).into_response(),
    };

    let mut message = json!({
        "role": "assistant",
        "content": if content.is_empty() && !calls.is_empty() { Value::Null } else { json!(content) },
    });
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if let Some(raw) = incomplete {
        message["incomplete_tool_call"] = json!(truncate(&raw, 2000));
    }
    if !calls.is_empty() {
        let arr: Vec<Value> = calls
            .iter()
            .map(|c| {
                json!({
                    "id": format!("call_{}", Uuid::new_v4().simple()),
                    "type": "function",
                    "function": { "name": c.name, "arguments": c.arguments },
                })
            })
            .collect();
        message["tool_calls"] = json!(arr);
    }

    let finish = if !calls.is_empty() { "tool_calls" } else { "stop" };
    let pt = if usage_in > 0 { usage_in } else { approx_tokens(&prompt_text) };
    let ct = if usage_out > 0 {
        usage_out
    } else {
        approx_tokens(&content) + approx_tokens(&reasoning)
    };

    Json(json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "logprobs": Value::Null,
            "finish_reason": finish,
        }],
        "usage": {
            "prompt_tokens": pt,
            "completion_tokens": ct,
            "total_tokens": pt + ct,
        },
    }))
    .into_response()
}

pub async fn models(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    authorize(&st.cfg, &headers)?;
    let created = now_secs();
    let mk = |id: &str, owned: &str| {
        json!({
            "id": id,
            "object": "model",
            "created": created,
            "owned_by": owned,
        })
    };
    let mut seen = std::collections::HashSet::new();
    let ids: Vec<String> = [
        st.cfg.default_model.clone(),
        "deepseek-chat".to_string(),
        "deepseek-reasoner".to_string(),
        "deepseek-search".to_string(),
        "deepseek-vision".to_string(),
    ]
    .into_iter()
    .filter(|id| seen.insert(id.clone()))
    .collect();

    Ok(Json(json!({
        "object": "list",
        "data": ids.iter().map(|id| mk(id, "deepseek-web")).collect::<Vec<_>>(),
    })))
}

/// OpenAI-compatible file upload -> DeepSeek file id (use it in ref_file_ids
/// or as {"type":"file","file":{"file_id":"..."}} in message content).
pub async fn upload_file(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    authorize(&st.cfg, &headers)?;

    let mut picked: Option<(String, String, Vec<u8>)> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("multipart: {e}")))?
    {
        // skip non-file parts (some clients send extra text fields first)
        if field.name() != Some("file") && field.file_name().is_none() {
            continue;
        }
        let filename = field.file_name().unwrap_or("upload.bin").to_string();
        let mime = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_string();
        let data = field
            .bytes()
            .await
            .map_err(|e| AppError::BadRequest(format!("multipart body: {e}")))?
            .to_vec();
        picked = Some((filename, mime, data));
        break;
    }

    let Some((filename, mime, data)) = picked else {
        return Err(AppError::BadRequest("no file field in form".into()));
    };
    let bytes = data.len();

    let attempts = st.pool.len();
    if attempts == 0 {
        return Err(AppError::NoAccount);
    }
    let mut last: Option<String> = None;
    let mut tried: Vec<String> = Vec::with_capacity(attempts);

    for _ in 0..attempts {
        let acc = match st.pool.try_acquire_excluding(&tried) {
            Some(a) => a,
            None => {
                return Err(match last {
                    Some(m) => AppError::Upstream(m),
                    None => AppError::Busy,
                })
            }
        };
        let guard = BusyGuard(acc.clone());

        match st
            .ds
            .upload_file(&acc, filename.clone(), mime.clone(), data.clone())
            .await
        {
            Ok(id) => {
                acc.clear_error();
                return Ok(Json(json!({
                    "id": id,
                    "object": "file",
                    "bytes": bytes,
                    "filename": filename,
                    "purpose": "assistants",
                    "account": acc.name,
                })));
            }
            Err(e) => {
                let msg = format!("{e}");
                acc.set_error(msg.clone());
                drop(guard);
                if attempts > 1 && is_retryable(&msg) {
                    acc.set_cooldown(cooldown_secs_for(&msg));
                    tracing::warn!("upload failed on {}, trying next: {msg}", acc.name);
                    tried.push(acc.name.clone());
                    last = Some(msg);
                    continue;
                }
                return Err(AppError::from(e));
            }
        }
    }

    Err(AppError::Upstream(
        last.unwrap_or_else(|| "all accounts failed".into()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::deepseek::auth::AccountSpec;
    use crate::deepseek::client::DeepSeekClient;
    use axum::{routing::post, Router};
    use axum::http::header::CONTENT_TYPE;

    #[test]
    fn retryable_and_cooldown_cover_limits_auth_and_upstream() {
        assert!(is_retryable("completion 429 Too Many Requests"));
        assert!(is_retryable("ERROR:40003:token invalid"));
        assert!(is_retryable("quota exceeded for today"));
        assert!(is_retryable("HTTP 503 Service Unavailable"));
        assert!(!is_retryable("messages[] is required"));

        assert_eq!(cooldown_secs_for("upstream 40003: invalid"), 300);
        assert_eq!(cooldown_secs_for("completion 429 rate limit"), 90);
        assert_eq!(cooldown_secs_for("upstream timed out"), 30);
    }

    async fn spawn_mock_deepseek() -> String {
        async fn pow(headers: HeaderMap) -> Response {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if auth.contains("tok-chrome-limit") {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({ "code": 429, "msg": "rate limit" })),
                )
                    .into_response();
            }
            Json(json!({
                "data": {
                    "biz_data": {
                        "challenge": {
                            "algorithm": "DeepSeekHashV1",
                            "challenge": "abc",
                            "salt": "s",
                            "difficulty": 1,
                            "expire_at": 9999999999u64,
                            "signature": "sig",
                            "target_path": "/api/v0/chat/completion"
                        }
                    }
                }
            }))
            .into_response()
        }

        async fn create_session() -> Json<Value> {
            Json(json!({ "data": { "biz_data": { "chat_session": { "id": "sess-1" } } } }))
        }

        async fn delete_session() -> Json<Value> {
            Json(json!({ "code": 0 }))
        }

        async fn completion(headers: HeaderMap) -> Response {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if auth.contains("tok-chrome-limit") {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({ "code": 429, "msg": "rate limit" })),
                )
                    .into_response();
            }
            if auth.contains("tok-chrome-sse-err") {
                let body = "data: {\"code\":40003,\"msg\":\"Authorization Failed\"}\n\n";
                return ([(CONTENT_TYPE, "text/event-stream")], body).into_response();
            }
            let body = "data: {\"v\":{\"response\":{\"fragments\":[{\"id\":1,\"type\":\"RESPONSE\",\"content\":\"OK from Edge\"}]}}}\n\nevent: finish\ndata: {}\n\n";
            ([(CONTENT_TYPE, "text/event-stream")], body).into_response()
        }

        let app = Router::new()
            .route("/api/v0/chat/create_pow_challenge", post(pow))
            .route("/api/v0/chat_session/create", post(create_session))
            .route("/api/v0/chat_session/delete", post(delete_session))
            .route("/api/v0/chat/completion", post(completion));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn mock_state(upstream: String, chrome_token: &str) -> Arc<AppState> {
        let cfg = Arc::new(Config {
            host: "127.0.0.1".into(),
            port: 0,
            api_keys: Vec::new(),
            admin_key: None,
            upstream,
            user_agent: "ua".into(),
            app_version: "1".into(),
            client_version: "1".into(),
            completion_path: "/api/v0/chat/completion".into(),
            pow_enabled: false,
            reuse_session: false,
            delete_session: true,
            accounts_file: "accounts.mock.json".into(),
            default_model: "deepseek-chat".into(),
        });
        let ds = DeepSeekClient::new(cfg.clone()).unwrap();
        let pool = crate::deepseek::auth::AccountPool::new();
        pool.add(AccountSpec {
            name: "chrome:Default".into(),
            cookie: String::new(),
            token: Some(chrome_token.into()),
            source: "chrome".into(),
            profile: "Chrome".into(),
            email: "a@gmail.com".into(),
        });
        pool.add(AccountSpec {
            name: "edge:Default".into(),
            cookie: String::new(),
            token: Some("tok-edge-ok".into()),
            source: "edge".into(),
            profile: "Edge".into(),
            email: "b@gmail.com".into(),
        });
        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        Arc::new(AppState {
            cfg,
            pool,
            ds,
            sessions: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            shutdown_tx,
        })
    }

    #[tokio::test]
    async fn chat_completions_falls_back_from_chrome_to_edge_on_http_limit_and_initial_sse_error() {
        let upstream = spawn_mock_deepseek().await;

        let st2 = mock_state(upstream.clone(), "tok-chrome-sse-err");
        let stream_req = serde_json::to_vec(&json!({
            "model": "deepseek-chat",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        let res_stream = chat_completions(
            State(st2.clone()),
            HeaderMap::new(),
            Ok(Json(serde_json::from_slice(&stream_req).unwrap())),
        )
        .await
        .expect("streaming fallback should succeed via Edge");
        assert_eq!(res_stream.status(), StatusCode::OK);
        let sse_bytes = axum::body::to_bytes(res_stream.into_body(), 1024 * 1024).await.unwrap();
        let sse_text = String::from_utf8_lossy(&sse_bytes);
        assert!(sse_text.contains("OK from Edge"), "got sse: {sse_text}");
        // Chrome must now be in cooldown while Edge stays clean.
        assert!(!st2.pool.snapshot()[0].cooldown_ok());
        assert!(st2.pool.snapshot()[1].cooldown_ok());

        // Case 1b: Non-streaming initial SSE error also falls back.
        let st_collect = mock_state(upstream.clone(), "tok-chrome-sse-err");
        let body = json!({
            "model": "deepseek-chat",
            "stream": false,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let res = chat_completions(
            State(st_collect.clone()),
            HeaderMap::new(),
            Ok(Json(serde_json::from_slice(&bytes).unwrap())),
        )
        .await
        .expect("non-stream fallback should succeed via Edge");
        assert_eq!(res.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        let parsed: Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(parsed["choices"][0]["message"]["content"], "OK from Edge");

        // Case 2: HTTP 429 directly from /completion on Chrome falls back to Edge cleanly.
        let st1 = mock_state(upstream.clone(), "tok-chrome-limit");
        let res_limit = chat_completions(
            State(st1.clone()),
            HeaderMap::new(),
            Ok(Json(serde_json::from_slice(&bytes).unwrap())),
        )
        .await
        .expect("429 on completion should fall back to Edge");
        assert_eq!(res_limit.status(), StatusCode::OK);
        let limit_bytes = axum::body::to_bytes(res_limit.into_body(), 1024 * 1024).await.unwrap();
        let limit_json: Value = serde_json::from_slice(&limit_bytes).unwrap();
        assert_eq!(limit_json["choices"][0]["message"]["content"], "OK from Edge");
        assert!(!st1.pool.snapshot()[0].cooldown_ok());
        assert!(st1.pool.snapshot()[1].cooldown_ok());
    }
}
