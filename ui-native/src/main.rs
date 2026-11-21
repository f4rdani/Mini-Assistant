#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Native (Slint) UI for Mini Assistant.
//!
//! View layer only. Every side effect goes through `app_core::Core`, which is
//! linked in-process: cookie extraction, SQLite storage, Win32 hotkeys, GDI
//! screen capture, foreground-window detection and the provider-API client.
//! There is no browser engine and no IPC in this process.
//!
//! Run with:  cargo run --release
//!
//! Uses Slint's `renderer-software` (`softbuffer` + Direct2D color emoji
//! rasterization via `emoji.rs`) for crisp hinted fonts, true Win32 desktop
//! per-pixel transparency on `HudWindow`/`BubbleWindow`, and <40 MB RAM.

mod bubble_scroll;
mod emoji;
mod icons;
mod logos;
mod model;
mod win32;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{Global, Model, ModelRc, SharedString, VecModel};

slint::include_modules!();

/// Debug-only diagnostic log macro: compiles to a complete no-op in `--release`
/// builds so no stdout/stderr formatting or console output ever runs in production.
macro_rules! dlog {
    ($($arg:tt)*) => {
        #[cfg(debug_assertions)]
        eprintln!($($arg)*);
    };
}

type SingStopHook = Rc<dyn Fn(bool)>;

// ---------------------------------------------------------------------------
//  Icon cache — rasterise each (icon, tint) pair once.
// ---------------------------------------------------------------------------
thread_local! {
    static ICON_CACHE: RefCell<HashMap<String, slint::Image>> = RefCell::new(HashMap::new());
    // Any component handle reaches the shared globals; the HUD lives for the
    // whole process, so it doubles as the handle for ChatLayout updates.
    static HUD_HANDLE: RefCell<Option<slint::Weak<HudWindow>>> = const { RefCell::new(None) };
    // Set once the control panel is wired; called after a message is stored so
    // the session list picks up a brand-new topic without a restart.
    static SESSIONS_REFRESH: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
    // UI-thread hook to immediately stop singing playback & sprites on window
    // switches, AI tasks, or shutdown without threading controllers everywhere.
    static SING_STOP_HOOK: RefCell<Option<SingStopHook>> = const { RefCell::new(None) };
    // UI-thread hook to reset the BubbleWindow human-paced auto-scroll state
    // whenever bubble content starts, expands to HUD, or collapses.
    static BUBBLE_SCROLL_RESET: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
    #[cfg(feature = "embedded-api")]
    static EMBEDDED_API: RefCell<Option<api_ai::EmbeddedServer>> = const { RefCell::new(None) };
}

fn reset_bubble_scroll_global() {
    BUBBLE_SCROLL_RESET.with(|h| {
        if let Some(cb) = h.borrow().as_ref().cloned() {
            cb();
        }
    });
}

fn stop_singing_global(reschedule: bool) {
    SING_STOP_HOOK.with(|h| {
        if let Some(cb) = h.borrow().as_ref().cloned() {
            cb(reschedule);
        }
    });
}

/// Re-stacks the chat rows and publishes the new content height to the HUD's
/// ScrollView (absolute rows do not grow it on their own).
fn sync_chat_height(model: &slint::VecModel<HudMessage>) {
    sync_chat_height_at(model, None);
}

/// Same, but does NOT invoke scroll_to_bottom. Used when user toggles dropdowns (Thought, Detail)
/// so their reading scroll position is preserved without being violently thrown to the bottom.
fn sync_chat_height_preserve(model: &slint::VecModel<HudMessage>, dirty: Option<usize>) {
    let bottom = model::relayout_dirty(model, dirty);
    HUD_HANDLE.with(|w| {
        if let Some(h) = w.borrow().as_ref().and_then(|weak| weak.upgrade()) {
            ChatLayout::get(&h).set_content_h(bottom);
        }
    });
}

/// Same, but only `dirty` is re-measured. Used by the stream pump: every other
/// bubble is unchanged, so re-wrapping their markdown would be wasted work.
fn sync_chat_height_at(model: &slint::VecModel<HudMessage>, dirty: Option<usize>) {
    let bottom = model::relayout_dirty(model, dirty);
    HUD_HANDLE.with(|w| {
        if let Some(h) = w.borrow().as_ref().and_then(|weak| weak.upgrade()) {
            ChatLayout::get(&h).set_content_h(bottom);
            h.invoke_scroll_to_bottom();
        }
    });
}

/// Schedules bottom-scroll right after Slint recomputes `chat-layout.preferred-height`,
/// so opening a long chat, loading a topic, or streaming tokens automatically lands at the bottom.
fn scroll_chat_to_bottom_deferred(h: &HudWindow) {
    h.invoke_scroll_to_bottom();
    for delay_ms in [16u64, 64] {
        let weak = h.as_weak();
        slint::Timer::single_shot(Duration::from_millis(delay_ms), move || {
            if let Some(h) = weak.upgrade() {
                h.invoke_scroll_to_bottom();
            }
        });
    }
}

/// Updates the minimized speech bubble text during streaming while preserving the
/// top reading position so `BubbleAutoScroll` paces downward movement dynamically.
fn update_bubble_stream_text(bubble: &BubbleWindow, markdown: &str, done: bool) {
    if !markdown.is_empty() && bubble.get_translate_text().as_str() != markdown {
        bubble.set_translate_text(markdown.into());
        bubble.set_translate_styled(model::styled(markdown));
    }
    if done {
        bubble.set_translate_busy(false);
    }
    sync_bubble_window_size(bubble);
}

/// One worker-thread result for the diagnostics page.
enum DiagMsg {
    Result { label: String, ms: u128, head: String },
    Fail { label: String, err: String },
    KeyTesting { id: String },
    KeyResult {
        id: String,
        label: String,
        ok: bool,
        ms: Option<u64>,
        detail: String,
        badge: String,
        tone: String,
    },
    KeySummary { ok_count: usize, total: usize, best_ms: Option<u64>, msg: String },
    Extract { ok: bool, msg: String },
    /// Cookie-vault action finished (browser extraction or manual token).
    Vault { ok: bool, msg: String },
    /// Provider toggle finished connecting: which card, and how it went.
    Connect { idx: i32, ok: bool, msg: String },
    Done,
}

/// Search box + category chip state for the session page.
#[derive(Default)]
struct SessionFilter {
    cat: String,
    query: String,
}

/// Rebuilds the session list (panel cards, category chips and the HUD's topic
/// dropdown) from SQLite.
///
/// Filtering happens here rather than in SQL so the chips always show the full
/// per-category counts, not the counts of the already-filtered rows.
fn refresh_sessions(
    core: &app_core::Core,
    panel: &MainPanel,
    hud: &HudWindow,
    filter: &SessionFilter,
) {
    let all = core.topics().unwrap_or_default();
    let counts: HashMap<String, usize> = core
        .topic_counts()
        .unwrap_or_default()
        .into_iter()
        .map(|(name, n)| (name, n.max(0) as usize))
        .collect();
    let current = hud.get_current_topic().to_string();
    let mut rows = model::topics_from(&all, Some(&current), &|n| *counts.get(n).unwrap_or(&0));
    // A new chat is named after the window before anything is typed, so it has
    // no row yet. Listing it anyway keeps the pill and the dropdown identical
    // instead of showing the previous session as "Aktif".
    if !current.trim().is_empty() && !all.iter().any(|t| t.name == current) {
        rows.retain(|r| r.name != current);
        rows.insert(
            0,
            model::pending_topic(&current, &hud.get_detected_category()),
        );
    }

    panel.set_topic_categories(ModelRc::new(VecModel::from(model::topic_categories(
        &rows,
        &filter.cat,
    ))));

    let q = filter.query.to_lowercase();
    let visible: Vec<TopicItem> = rows
        .into_iter()
        .filter(|r| {
            let by_cat = filter.cat.is_empty()
                || filter.cat == "Semua"
                || filter.cat == "All"
                || r.category == filter.cat;
            let by_text = q.is_empty()
                || r.name.to_lowercase().contains(q.as_str())
                || r.category.to_lowercase().contains(q.as_str());
            by_cat && by_text
        })
        .collect();

    hud.set_topics(ModelRc::new(VecModel::from(
        visible.iter().map(|r| r.name.clone()).collect::<Vec<_>>(),
    )));
    panel.set_topics(ModelRc::new(VecModel::from(visible)));
    panel.set_topic_query(filter.query.clone().into());
}

/// Restores a saved session into the HUD: bubbles (including saved thought,
/// timing meta, and web search sources) plus the turn history sent next turn.
fn load_topic(
    core: &app_core::Core,
    name: &str,
    messages: &slint::VecModel<HudMessage>,
    turns: &RefCell<Vec<Turn>>,
) -> usize {
    let loaded = core.messages(name).unwrap_or_default();
    messages.set_vec(
        loaded
            .iter()
            .map(|m| {
                let user_who = app_core::tr("ANDA", "YOU");
                let who = if m.role == "user" { user_who } else { "AEMEATH" };
                if m.role == "user" {
                    model::message("user", user_who, &m.content, &m.meta, false)
                } else {
                    let src_items: Vec<SearchSourceItem> = m
                        .sources
                        .iter()
                        .map(|s| {
                            model::build_search_source_item(
                                &s.title,
                                &s.url,
                                &s.snippet,
                                &s.site_name,
                                &s.date,
                            )
                        })
                        .collect();
                    let think_label = if m.thought.is_empty() {
                        ""
                    } else {
                        app_core::tr("Proses berpikir", "Thinking process")
                    };
                    model::message_full(
                        &m.role,
                        who,
                        &m.content,
                        &m.meta,
                        true,
                        &m.thought,
                        think_label,
                        false,
                        src_items.len() as i32,
                        false,
                        &src_items,
                    )
                }
            })
            .collect::<Vec<_>>(),
    );
    let mut history = turns.borrow_mut();
    history.clear();
    for m in &loaded {
        history.push(Turn {
            role: m.role.clone(),
            text: m.content.clone(),
            files: Vec::new(),
        });
    }
    loaded.len()
}

/// Topic rows are keyed by name, so a second "Sesi Baru" in the same window
/// would silently append to the first one. Numbering keeps each new chat its
/// own row without inventing a title the user did not see.
fn unique_topic(core: &app_core::Core, base: &str) -> String {
    let taken: Vec<String> = core
        .topics()
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.name)
        .collect();
    if !taken.iter().any(|n| n == base) {
        return base.to_string();
    }
    (2..1000)
        .map(|i| format!("{base} ({i})"))
        .find(|c| !taken.iter().any(|n| n == c))
        .unwrap_or_else(|| base.to_string())
}

/// Starts an empty session named after the window the user is looking at
/// ("[Game] Lethal Company", "[Dev] mini-assist", ...). Nothing is written to
/// SQLite yet: the topic row is created with the first message, so abandoned
/// new chats never litter the history.
fn start_new_session(
    core: &app_core::Core,
    hud: &HudWindow,
    messages: &slint::VecModel<HudMessage>,
) -> String {
    let win = core.current_window();
    let name = win
        .as_ref()
        .map(|w| app_core::topic_label(&w.suggested_topic))
        .filter(|s| !s.trim().is_empty())
        .map(|base| unique_topic(core, &base))
        .unwrap_or_else(|| app_core::tr("Percakapan Baru", "New Conversation").to_string());
    if let Some(w) = win {
        // The category travels with the name: the session card and its chip
        // come from the same window the title came from.
        hud.set_detected_window(w.process_name.into());
        hud.set_detected_category(w.category.to_uppercase().into());
    }
    hud.set_current_topic(name.clone().into());
    messages.set_vec(Vec::new());
    sync_chat_height(messages);
    SESSIONS_REFRESH.with(|w| {
        if let Some(f) = w.borrow().as_ref() {
            f();
        }
    });
    name
}

/// Category stored with a session: the window's own classification, so the
/// history chips ("DEVELOPMENT", "GAMING") are real instead of one "GENERAL"
/// bucket for every conversation.
fn session_category(hud: &HudWindow) -> String {
    let c = hud.get_detected_category().to_string();
    if c.trim().is_empty() {
        "General".to_string()
    } else {
        c
    }
}

/// "1 DARI 1 AKTIF · GOOGLE CHROME" / "1 OF 1 ACTIVE · GOOGLE CHROME"
fn provider_count_text_for(
    lang: app_core::AppLang,
    active: usize,
    total: usize,
    browser: &str,
) -> String {
    let b = browser.to_uppercase();
    match lang {
        app_core::AppLang::Id => format!("{active} DARI {total} AKTIF · {b}"),
        app_core::AppLang::En => format!("{active} OF {total} ACTIVE · {b}"),
    }
}

fn provider_count_text(active: usize, total: usize, browser: &str) -> String {
    provider_count_text_for(app_core::current_lang(), active, total, browser)
}

fn color_to_hex(c: slint::Color) -> String {
    format!("#{:02X}{:02X}{:02X}", c.red(), c.green(), c.blue())
}

/// Formats the Browser & Cookies vault banner without misleading `0 cookie` counts
/// when authentication succeeded via Chrome's unencrypted LocalStorage `userToken`.
fn format_vault_fetch_status_for(
    lang: app_core::AppLang,
    report: &app_core::ConnectReport,
) -> String {
    if !report.success {
        return if lang.is_en() {
            format!("Failed: {}", report.message)
        } else {
            format!("Gagal: {}", report.message)
        };
    }
    if report.locked_v20 > 0 && report.cookies_found <= 1 {
        if lang.is_en() {
            format!(
                "Connected: {}-char session token from {} LocalStorage · {} v20-locked cookies.",
                report.token_chars, report.browser, report.locked_v20
            )
        } else {
            format!(
                "Terpasang: token sesi {} karakter dari LocalStorage {} · {} cookie terkunci v20.",
                report.token_chars, report.browser, report.locked_v20
            )
        }
    } else if report.cookies_found == 0 {
        if lang.is_en() {
            format!(
                "Connected: {}-char session token from {} LocalStorage.",
                report.token_chars, report.browser
            )
        } else {
            format!(
                "Terpasang: token sesi {} karakter dari LocalStorage {}.",
                report.token_chars, report.browser
            )
        }
    } else if lang.is_en() {
        format!(
            "Connected: {} cookies + {}-char token from {}.",
            report.cookies_found, report.token_chars, report.browser
        )
    } else {
        format!(
            "Terpasang: {} cookie + token {} karakter dari {}.",
            report.cookies_found, report.token_chars, report.browser
        )
    }
}

fn format_vault_fetch_status(report: &app_core::ConnectReport) -> String {
    format_vault_fetch_status_for(app_core::current_lang(), report)
}

fn format_vault_fetch_all_status_for(
    lang: app_core::AppLang,
    reports: &[app_core::ConnectReport],
) -> (bool, String) {
    if reports.is_empty() {
        return (
            false,
            app_core::tr_for(
                lang,
                "Tidak ada profil browser terdeteksi.",
                "No browser profiles detected.",
            )
            .to_string(),
        );
    }
    let ok_names: Vec<String> = reports
        .iter()
        .filter(|r| r.success)
        .map(|r| r.browser.clone())
        .collect();
    if ok_names.is_empty() {
        let fallback = app_core::tr_for(
            lang,
            "Tidak ditemukan sesi DeepSeek.",
            "No DeepSeek session found.",
        );
        let first_err = reports
            .first()
            .map(|r| r.message.as_str())
            .unwrap_or(fallback);
        return (
            false,
            if lang.is_en() {
                format!("Failed: {first_err}")
            } else {
                format!("Gagal: {first_err}")
            },
        );
    }
    let missed: Vec<String> = reports
        .iter()
        .filter(|r| !r.success)
        .map(|r| r.browser.clone())
        .collect();
    if missed.is_empty() {
        (
            true,
            if lang.is_en() {
                format!("Connected {} sessions from {}.", ok_names.len(), ok_names.join(" & "))
            } else {
                format!("Terpasang {} sesi dari {}.", ok_names.len(), ok_names.join(" & "))
            },
        )
    } else {
        (
            true,
            if lang.is_en() {
                format!(
                    "Connected from {} (no session yet in {}).",
                    ok_names.join(", "),
                    missed.join(", ")
                )
            } else {
                format!(
                    "Terpasang dari {} (belum ada sesi di {}).",
                    ok_names.join(", "),
                    missed.join(", ")
                )
            },
        )
    }
}

fn format_vault_fetch_all_status(reports: &[app_core::ConnectReport]) -> (bool, String) {
    format_vault_fetch_all_status_for(app_core::current_lang(), reports)
}

fn apply_active_browser(
    index: usize,
    profiles: &[app_core::BrowserProfileInfo],
    browser_logos: &[slint::Image],
    panel: &MainPanel,
    core: Option<&app_core::Core>,
) -> usize {
    let idx = index.min(profiles.len().saturating_sub(1));
    panel.set_browsers(ModelRc::new(VecModel::from(
        model::browsers_from_with_active(profiles, idx),
    )));
    let picked = profiles.get(idx);
    let browser_name = picked
        .map(|b| b.browser_name.clone())
        .unwrap_or_else(|| app_core::tr("Tidak ada browser", "No browser").to_string());
    let logo_idx = picked
        .map(|b| model::browser_logo_index(&b.browser_type))
        .unwrap_or(0)
        .min(browser_logos.len().saturating_sub(1));
    if let Some(logo) = browser_logos.get(logo_idx) {
        panel.set_active_browser_logo(logo.clone());
    }
    panel.set_active_browser(browser_name.clone().into());
    let main_provider = panel.get_active_provider().to_string();
    panel.set_vault_badge(
        if profiles.is_empty() {
            app_core::tr("TIDAK ADA BROWSER TERDETEKSI", "NO BROWSER DETECTED").to_string()
        } else if app_core::is_en() {
            format!(
                "ACTIVE: {} · PRIMARY: {}",
                browser_name.to_uppercase(),
                main_provider.to_uppercase()
            )
        } else {
            format!(
                "AKTIF: {} · UTAMA: {}",
                browser_name.to_uppercase(),
                main_provider.to_uppercase()
            )
        }
        .into(),
    );
    let provs = panel.get_providers();
    let active = (0..provs.row_count())
        .filter(|&i| provs.row_data(i).map(|c| c.enabled).unwrap_or(false))
        .count();
    panel.set_provider_count(provider_count_text(active, provs.row_count(), &browser_name).into());
    if let (Some(c), Some(prof)) = (core, picked) {
        if !ui_test_active() {
            let _ = c.save_setting("active-browser-id", &app_core::account_slot_name(prof));
        }
    }
    idx
}

fn apply_key_diag_overlay(panel: &MainPanel, results: &HashMap<String, (String, String)>) {
    let model_rc = panel.get_deepseek_keys();
    let mut items: Vec<VaultKeyItem> = (0..model_rc.row_count())
        .filter_map(|i| model_rc.row_data(i))
        .collect();
    for item in &mut items {
        if let Some((text, tone)) = results.get(item.id.as_str()) {
            item.diag_text = text.clone().into();
            item.diag_tone = tone.clone().into();
        } else {
            item.diag_text = "".into();
            item.diag_tone = "".into();
        }
    }
    panel.set_deepseek_keys(ModelRc::new(VecModel::from(items)));
}

fn refresh_vault_accounts_with_diag(
    core: &app_core::Core,
    panel: &MainPanel,
    results: &HashMap<String, (String, String)>,
) -> Vec<app_core::AccountEntry> {
    let entries = core.deepseek_accounts();
    let mut items = model::vault_keys_from(&entries);
    model::apply_key_diag_results(&mut items, results);
    panel.set_deepseek_keys(ModelRc::new(VecModel::from(items)));
    panel.set_deepseek_connect_count(entries.len() as i32);
    let acc_text = entries
        .first()
        .map(|k| format!("Key 1: {}", k.email))
        .unwrap_or_default();
    panel.set_deepseek_accounts_text(acc_text.into());

    let connected = !entries.is_empty();
    let provs = panel.get_providers();
    for i in 0..provs.row_count() {
        if let Some(mut card) = provs.row_data(i) {
            if card.id == "deepseek" {
                card.status = if connected { "Connected" } else { "Offline" }.into();
                card.tone = if connected { "green" } else { "amber" }.into();
                card.tone_color = if connected {
                    slint::Color::from_rgb_u8(0x2C, 0xE5, 0x9B)
                } else {
                    slint::Color::from_rgb_u8(0xFF, 0xD1, 0x66)
                };
                provs.set_row_data(i, card);
                break;
            }
        }
    }
    let active = (0..provs.row_count())
        .filter(|&i| provs.row_data(i).map(|c| c.enabled).unwrap_or(false))
        .count();
    panel.set_provider_count(
        provider_count_text(active, provs.row_count(), &panel.get_active_browser()).into(),
    );
    entries
}

fn refresh_vault_accounts(core: &app_core::Core, panel: &MainPanel) -> Vec<app_core::AccountEntry> {
    refresh_vault_accounts_with_diag(core, panel, &HashMap::new())
}

fn key_probe_label(idx: usize, acc: &app_core::AccountEntry) -> String {
    let src = match acc.source.to_ascii_lowercase().as_str() {
        "chrome" => "Chrome",
        "edge" => "Edge",
        "brave" => "Brave",
        "firefox" => "Firefox",
        _ => "Manual",
    };
    let who = if !acc.email.trim().is_empty() {
        acc.email.as_str()
    } else {
        acc.name.as_str()
    };
    format!("Key {} ({src} \u{00b7} {who})", idx + 1)
}

fn probe_single_key(
    core: &app_core::Core,
    idx: usize,
    acc: &app_core::AccountEntry,
) -> (bool, Option<u64>, DiagMsg) {
    let label = key_probe_label(idx, acc);
    match core.test_deepseek_account(&acc.name) {
        Ok(res) if res.ok => {
            let ms = res.latency_ms;
            let detail: String = res
                .reply
                .as_deref()
                .unwrap_or("OK")
                .trim()
                .chars()
                .take(40)
                .collect();
            let (badge, tone) = model::format_key_test_badge(&res);
            (
                true,
                ms,
                DiagMsg::KeyResult {
                    id: acc.name.clone(),
                    label,
                    ok: true,
                    ms,
                    detail: if detail.is_empty() { "OK".into() } else { detail },
                    badge,
                    tone,
                },
            )
        }
        Ok(res) => {
            let detail: String = res
                .error
                .as_deref()
                .unwrap_or_else(|| app_core::tr("Gagal menguji key", "Failed to test key"))
                .chars()
                .take(120)
                .collect();
            let (badge, tone) = model::format_key_test_badge(&res);
            (
                false,
                None,
                DiagMsg::KeyResult {
                    id: acc.name.clone(),
                    label,
                    ok: false,
                    ms: None,
                    detail,
                    badge,
                    tone,
                },
            )
        }
        Err(e) => {
            let detail: String = e.chars().take(120).collect();
            let synth = app_core::AccountTestResult {
                ok: false,
                name: acc.name.clone(),
                latency_ms: None,
                reply: None,
                error: Some(detail.clone()),
            };
            let (badge, tone) = model::format_key_test_badge(&synth);
            (
                false,
                None,
                DiagMsg::KeyResult {
                    id: acc.name.clone(),
                    label,
                    ok: false,
                    ms: None,
                    detail,
                    badge,
                    tone,
                },
            )
        }
    }
}

fn demo_vault_keys() -> Vec<VaultKeyItem> {
    vec![
        VaultKeyItem {
            id: "chrome:Default".into(),
            badge: "Key 1".into(),
            email: "demo.chrome@example.com".into(),
            source: "chrome".into(),
            profile: "Google Chrome (Default)".into(),
            status_text: app_core::tr("Utama · Siap", "Primary · Ready").into(),
            tone: "green".into(),
            is_primary: true,
            diag_text: "".into(),
            diag_tone: "".into(),
        },
        VaultKeyItem {
            id: "edge:Default".into(),
            badge: "Key 2".into(),
            email: "demo.edge@example.com".into(),
            source: "edge".into(),
            profile: "Microsoft Edge (Personal)".into(),
            status_text: app_core::tr("Cadangan · Siap", "Backup · Ready").into(),
            tone: "cyan".into(),
            is_primary: false,
            diag_text: "".into(),
            diag_tone: "".into(),
        },
    ]
}

fn apply_app_language(
    lang: app_core::AppLang,
    panel: &MainPanel,
    hud: &HudWindow,
    bubble: &BubbleWindow,
) {
    app_core::set_lang(lang);
    let code = lang.code();
    I18n::get(panel).set_lang(code.into());
    I18n::get(hud).set_lang(code.into());
    I18n::get(bubble).set_lang(code.into());

    let loading = bubble.get_loading_label();
    if matches!(
        loading.as_str(),
        "" | "Menerjemahkan layar..." | "Translating screen..."
    ) {
        bubble.set_loading_label(
            app_core::tr_for(lang, "Menerjemahkan layar...", "Translating screen...").into(),
        );
    } else if matches!(
        loading.as_str(),
        "Aemeath sedang berpikir..." | "Aemeath is thinking..."
    ) {
        bubble.set_loading_label(
            app_core::tr_for(lang, "Aemeath sedang berpikir...", "Aemeath is thinking...").into(),
        );
    }

    let diag = panel.get_diag_status();
    if matches!(
        diag.as_str(),
        "" | "Menunggu pengujian..." | "Waiting for test..."
    ) {
        panel.set_diag_status(
            app_core::tr_for(lang, "Menunggu pengujian...", "Waiting for test...").into(),
        );
    }

    let topic = hud.get_current_topic();
    if matches!(topic.as_str(), "Percakapan Baru" | "New Conversation") {
        hud.set_current_topic(
            app_core::tr_for(lang, "Percakapan Baru", "New Conversation").into(),
        );
    }
}

static SHUTTING_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn switch_panel_to_hud(
    panel_weak: &slint::Weak<MainPanel>,
    hud_weak: &slint::Weak<HudWindow>,
    bubble_weak: &slint::Weak<BubbleWindow>,
) {
    stop_singing_global(true);
    // Show target window BEFORE hiding the last visible window so Slint's event loop stays alive.
    if let Some(h) = hud_weak.upgrade() {
        let _ = h.show();
    }
    if let Some(p) = panel_weak.upgrade() {
        p.set_close_modal_open(false);
        p.set_panel_visible(false);
        let _ = p.hide();
    }
    if let Some(b) = bubble_weak.upgrade() {
        let _ = b.hide();
    }
    schedule_trim_working_set();
}

/// Opens the Control Panel directly to the Close/Exit modal (e.g. on Alt+F4 in HUD/Bubble).
fn open_panel_exit_modal(
    panel_weak: &slint::Weak<MainPanel>,
    hud_weak: &slint::Weak<HudWindow>,
    bubble_weak: &slint::Weak<BubbleWindow>,
) {
    stop_singing_global(true);
    if let Some(p) = panel_weak.upgrade() {
        p.set_close_modal_open(true);
        p.set_panel_visible(true);
        let _ = p.show();
    }
    if let Some(h) = hud_weak.upgrade() {
        let _ = h.hide();
    }
    if let Some(b) = bubble_weak.upgrade() {
        let _ = b.hide();
    }
    schedule_trim_working_set();
}

/// Closes all windows, releases global hotkeys, and gracefully shuts down `api-ai.exe`.
fn perform_system_shutdown(
    core: &app_core::Core,
    panel_weak: &slint::Weak<MainPanel>,
    hud_weak: &slint::Weak<HudWindow>,
    bubble_weak: &slint::Weak<BubbleWindow>,
) {
    stop_singing_global(false);
    SHUTTING_DOWN.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(p) = panel_weak.upgrade() {
        p.set_close_modal_open(false);
        p.set_panel_visible(false);
        let _ = p.hide();
    }
    if let Some(h) = hud_weak.upgrade() {
        let _ = h.hide();
    }
    if let Some(b) = bubble_weak.upgrade() {
        let _ = b.hide();
    }
    let mut had_embedded = false;
    #[cfg(feature = "embedded-api")]
    EMBEDDED_API.with(|slot| {
        if let Some(mut srv) = slot.borrow_mut().take() {
            srv.shutdown();
            had_embedded = true;
        }
    });
    core.shutdown_system_with_api(!had_embedded);
    let _ = slint::quit_event_loop();
    std::process::exit(0);
}

fn wire_icons(icons: Icons<'_>) {
    icons.on_render(|name, tint| render_icon(name.as_str(), tint));
    icons.on_render_sized(|name, tint, size_px| render_icon_sized(name.as_str(), tint, size_px));
}

fn rewrite_svg_header_size(markup: &str, size_px: i32) -> String {
    let Some(gt) = markup.find('>') else {
        return markup.to_string();
    };
    let head = &markup[..gt];
    let tail = &markup[gt..];
    let px_str = size_px.to_string();
    let mut out = String::with_capacity(head.len() + 16);
    let mut i = 0usize;
    while i < head.len() {
        let rest = &head[i..];
        let prev_is_ws = i > 0 && head.as_bytes()[i - 1].is_ascii_whitespace();
        let attr = if prev_is_ws && rest.starts_with("width=\"") {
            Some("width=\"")
        } else if prev_is_ws && rest.starts_with("height=\"") {
            Some("height=\"")
        } else {
            None
        };
        if let Some(prefix) = attr {
            out.push_str(prefix);
            i += prefix.len();
            if let Some(rel_end) = head[i..].find('"') {
                out.push_str(&px_str);
                out.push('"');
                i += rel_end + 1;
            }
        } else {
            let ch = rest.chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    format!("{out}{tail}")
}

fn render_icon(name: &str, tint: slint::Color) -> slint::Image {
    render_icon_sized(name, tint, 0)
}

fn render_icon_sized(name: &str, tint: slint::Color, size_px: i32) -> slint::Image {
    let key = format!("{name}|{}|{size_px}", color_to_hex(tint));
    if let Some(img) = ICON_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return img;
    }
    let Some(svg) = icons::svg(name) else {
        return slint::Image::default();
    };
    // The web version relies on CSS `currentColor`; resvg needs a literal hex.
    let tinted = svg.replace("currentColor", &color_to_hex(tint));
    let markup = if size_px > 0 {
        rewrite_svg_header_size(&tinted, size_px)
    } else {
        tinted
    };
    let img = match slint::Image::load_from_svg_data(markup.as_bytes()) {
        Ok(i) => i,
        Err(_) => {
            dlog!("[icon] SVG LOAD FAILED for '{name}': {}", &markup[..markup.len().min(160)]);
            return slint::Image::default();
        }
    };
    ICON_CACHE.with(|c| {
        let mut map = c.borrow_mut();
        if map.len() >= 256 {
            map.clear();
        }
        map.insert(key, img.clone());
    });
    img
}

fn schedule_trim_working_set() {
    slint::Timer::single_shot(Duration::from_millis(300), win32::trim_working_set);
}

fn search_url_key(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn json_str(v: &serde_json::Value, keys: &[&str]) -> String {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

fn merge_search_sources(list: &mut Vec<SearchSourceItem>, items: &[serde_json::Value]) {
    let mut incoming: Vec<(Option<u64>, &serde_json::Value)> = items
        .iter()
        .map(|it| {
            let idx = ["cite_index", "citeIndex", "citation_index", "index"]
                .iter()
                .find_map(|k| {
                    it.get(*k).and_then(|v| {
                        v.as_u64()
                            .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
                    })
                });
            (idx, it)
        })
        .collect();

    if incoming.iter().any(|(idx, _)| idx.is_some()) {
        incoming.sort_by_key(|(idx, _)| idx.unwrap_or(u64::MAX));
    }

    for (_, it) in incoming {
        let title = json_str(it, &["title", "name"]);
        let url = json_str(it, &["url", "link", "href"]);
        let snippet = json_str(it, &["snippet", "description", "content", "summary"]);
        let site_name = json_str(it, &["site_name", "siteName", "source"]);
        let date = json_str(it, &["date", "published_at", "publishedAt"]);
        if url.is_empty() && title.is_empty() {
            continue;
        }
        let next = model::build_search_source_item(&title, &url, &snippet, &site_name, &date);
        let next_key = search_url_key(&url);
        if !next_key.is_empty() {
            if let Some(existing) = list.iter_mut().find(|s| search_url_key(s.url.as_str()) == next_key) {
                if existing.title.is_empty() && !next.title.is_empty() {
                    existing.title = next.title;
                }
                if existing.snippet.is_empty() && !next.snippet.is_empty() {
                    existing.snippet = next.snippet;
                }
                if existing.site_name.is_empty() && !next.site_name.is_empty() {
                    existing.site_name = next.site_name;
                }
                if existing.date.is_empty() && !next.date.is_empty() {
                    existing.date = next.date;
                }
                continue;
            }
        }
        list.push(next);
    }
}

/// Builds the 60x60 attachment preview. Raster images are decoded and reduced
/// to at most 160px on the long edge before they reach Slint, so a 4K photo
/// does not keep its full RGBA buffer in the HUD process. SVG files are
/// rasterised by Slint itself; anything undecodable falls back to an extension
/// card, matching the web app's single `currentScreenshot` preview.
fn attachment_preview(path: &str) -> (slint::Image, bool, String) {
    let path_ref = std::path::Path::new(path);
    let ext = path_ref
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let thumb = match ext.as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "gif" => image::open(path_ref).ok().map(|full| {
            let rgba = full.thumbnail(160, 160).to_rgba8();
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
            );
            slint::Image::from_rgba8(buffer)
        }),
        "svg" => std::fs::read(path_ref)
            .ok()
            .and_then(|data| slint::Image::load_from_svg_data(&data).ok()),
        _ => None,
    };

    let is_image = thumb.is_some();
    let label = if ext.is_empty() {
        "FILE".to_owned()
    } else {
        ext.chars().take(5).collect::<String>().to_ascii_uppercase()
    };
    (thumb.unwrap_or_default(), is_image, label)
}

fn build_attachment_item(path: &std::path::Path) -> AttachmentItem {
    let p_str = path.to_string_lossy().to_string();
    let (thumb, is_image, ext) = attachment_preview(&p_str);
    AttachmentItem {
        thumb,
        is_image,
        ext: ext.into(),
        path: p_str.into(),
    }
}

fn sync_hud_attachments(h: &HudWindow, paths: &[std::path::PathBuf]) {
    let items: Vec<AttachmentItem> = paths.iter().map(|p| build_attachment_item(p)).collect();
    if let Some(first) = items.first() {
        h.set_has_attachment(true);
        h.set_attachment_thumb(first.thumb.clone());
        h.set_attachment_is_image(first.is_image);
        h.set_attachment_ext(first.ext.clone());
    } else {
        h.set_has_attachment(false);
        h.set_attachment_thumb(slint::Image::default());
        h.set_attachment_is_image(false);
        h.set_attachment_ext(SharedString::default());
    }
    h.set_attachments(ModelRc::new(VecModel::from(items)));
}

#[allow(clippy::too_many_arguments)]
fn trigger_bubble_translate(
    core: &app_core::Core,
    hud: &HudWindow,
    panel: &MainPanel,
    bubble: &BubbleWindow,
    lang: &str,
    trans_stream_rx: &RefCell<Option<std::sync::mpsc::Receiver<app_core::ChatEvent>>>,
    trans_stream_buf: &RefCell<String>,
    trans_deadline: &Rc<RefCell<Option<std::time::Instant>>>,
    provider_map: &RefCell<Vec<app_core::ProviderOption>>,
) {
    *trans_stream_rx.borrow_mut() = None;
    *trans_deadline.borrow_mut() = None;
    reset_bubble_scroll_global();
    if bubble.get_translate_visible() {
        collapse_bubble_translate(bubble);
    }

    let capture_app = hud.get_capture_app();
    let target_only = hud.get_target_window_only();

    // Hide HUD and Panel if open (user specifically wants minimized mascot bubble response)
    if !capture_app {
        if let Some(hwnd) = win32::find_largest_window_by_title("Mini Assistant HUD") {
            core.set_capture_visible(hwnd as isize, false);
        }
        if let Some(hwnd) = win32::find_window_by_title("Mini Assistant Bubble") {
            core.set_capture_visible(hwnd as isize, false);
        }
    }

    let shot = core.capture_to_file(target_only);

    if let Some(hwnd) = win32::find_largest_window_by_title("Mini Assistant HUD") {
        core.set_capture_visible(hwnd as isize, true);
    }
    if let Some(hwnd) = win32::find_window_by_title("Mini Assistant Bubble") {
        core.set_capture_visible(hwnd as isize, true);
    }

    let hud_was_visible = hud.window().is_visible();
    let bubble_was_visible = bubble.window().is_visible();

    // Ensure minimized mascot BubbleWindow is visible before reading/adjusting its HWND
    let _ = bubble.show();

    let bubble_hwnd = win32::find_window_by_title("Mini Assistant Bubble");
    if !bubble_was_visible {
        if let Some(b_hwnd) = bubble_hwnd {
            if hud_was_visible {
                if let Some(h_hwnd) = win32::find_largest_window_by_title("Mini Assistant HUD") {
                    win32::position_bubble_from_hud(b_hwnd, h_hwnd);
                }
            }
            if let Some(snapped) = win32::snap_window_to_edge(b_hwnd) {
                bubble.set_edge(SharedString::from(snapped.as_str()));
            }
        }
    }

    let _ = hud.hide();
    let _ = panel.hide();

    let edge = bubble.get_edge().to_string();

    bubble.set_translate_lang(lang.into());
    bubble.set_loading_label(
        app_core::tr("Menerjemahkan layar...", "Translating screen...").into(),
    );
    bubble.set_translate_visible(true);
    bubble.set_translate_text(SharedString::new());
    bubble.set_translate_styled(slint::StyledText::default());
    bubble.invoke_scroll_to_top();
    bubble.set_translate_busy(shot.is_ok());

    let path = match shot {
        Ok(p) => p,
        Err(e) => {
            bubble.set_translate_busy(false);
            let msg = if app_core::is_en() {
                format!("Failed to capture screen: {e}")
            } else {
                format!("Gagal menjepret layar: {e}")
            };
            update_bubble_stream_text(bubble, &msg, true);
            *trans_deadline.borrow_mut() =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(6));
            return;
        }
    };

    trans_stream_buf.borrow_mut().clear();
    sync_bubble_window_size(bubble);

    let sys = format!(
        "Kamu adalah penerjemah layar visual yang akurat dan ringkas. Ikuti instruksi pengguna tentang bagian layar mana yang harus diterjemahkan; teks di luar cakupan itu jangan diterjemahkan atau disebut. Terjemahkan secara natural ke dalam bahasa {lang}. Tampilkan langsung hasil terjemahan yang rapi dan mudah dibaca menggunakan poin/paragraf singkat jika perlu. Jangan bertele-tele."
    );
    let user_prompt = translate_user_prompt(
        core.get_setting("translate-prompt").ok().flatten().as_deref(),
        lang,
    );
    let model_id = {
        let map = provider_map.borrow();
        let selected = hud.get_selected_model();
        map.iter()
            .find(|p| p.label == selected.as_str())
            .or_else(|| map.first())
            .map(|p| p.default_model.clone())
            .unwrap_or_else(|| "deepseek-chat".to_string())
    };

    let req = app_core::ChatRequest::new(vec![
        app_core::ChatTurn::system(&sys),
        app_core::ChatTurn::user(&user_prompt),
    ])
    .model(model_id);

    *trans_stream_rx.borrow_mut() = Some(core.chat_stream_with_attachments(req, vec![path]));
    let _ = edge;
}

/// Hides the HUD/panel and opens the mascot bubble in its loading state so a
/// normal chat answer can stream into it (same surface as screen translate).
fn present_answer_bubble(hud: &HudWindow, panel: &MainPanel, bubble: &BubbleWindow) {
    reset_bubble_scroll_global();
    let hud_was_visible = hud.window().is_visible();
    let bubble_was_visible = bubble.window().is_visible();
    let _ = bubble.show();
    if !bubble_was_visible {
        if let Some(b_hwnd) = win32::find_window_by_title("Mini Assistant Bubble") {
            if hud_was_visible {
                if let Some(h_hwnd) = win32::find_largest_window_by_title("Mini Assistant HUD") {
                    win32::position_bubble_from_hud(b_hwnd, h_hwnd);
                }
            }
            if let Some(snapped) = win32::snap_window_to_edge(b_hwnd) {
                bubble.set_edge(SharedString::from(snapped.as_str()));
            }
        }
    }
    let _ = hud.hide();
    let _ = panel.hide();
    bubble.set_translate_visible(true);
    bubble.set_loading_label(
        app_core::tr("Aemeath sedang berpikir...", "Aemeath is thinking...").into(),
    );
    bubble.set_translate_text(SharedString::new());
    bubble.set_translate_styled(slint::StyledText::default());
    bubble.invoke_scroll_to_top();
    bubble.set_translate_busy(true);
    sync_bubble_window_size(bubble);
}

fn sync_bubble_window_size(bubble: &BubbleWindow) {
    let w = bubble.get_target_win_w().max(92.0);
    let h = bubble.get_target_win_h().max(92.0);
    let scale = bubble.window().scale_factor().max(0.1);
    let want_w = (w * scale).round() as u32;
    let want_h = (h * scale).round() as u32;
    let cur = bubble.window().size();
    let size_changed = cur.width.abs_diff(want_w) > 1 || cur.height.abs_diff(want_h) > 1;
    if !size_changed {
        bubble.window().request_redraw();
        return;
    }
    let bubble_hwnd = win32::find_window_by_title("Mini Assistant Bubble");
    let prev_rect = bubble_hwnd.and_then(win32::window_rect);
    let edge = bubble.get_edge().to_string();
    let expanded = bubble.get_translate_visible();
    bubble.window().set_size(slint::LogicalSize::new(w, h));
    if let Some(hwnd) = bubble_hwnd {
        win32::reposition_bubble_after_resize(hwnd, prev_rect, expanded, &edge, w, h, scale);
    }
    bubble.window().request_redraw();
}

/// Advances `BubbleAutoScroll` by one tick against the live `BubbleWindow`,
/// applying any requested scroll offset and auto-hide deadline.
/// Returns `true` while actively gliding so the 60 FPS timer only runs during motion.
fn tick_bubble_scroll_once(
    bubble: &BubbleWindow,
    scroll: &RefCell<bubble_scroll::BubbleAutoScroll>,
    deadline_slot: &RefCell<Option<std::time::Instant>>,
) -> bool {
    if !bubble.get_translate_visible() {
        scroll.borrow_mut().reset();
        return false;
    }
    let hovered = bubble.get_translate_hovered() || bubble.get_held() || bubble.get_dragging();
    let now = std::time::Instant::now();
    let (out, animating) = {
        let mut st = scroll.borrow_mut();
        let out = st.tick(
            now,
            bubble.get_trans_scroll_y(),
            bubble.get_trans_scroll_max(),
            bubble.get_trans_viewport_h(),
            bubble.get_trans_content_h(),
            hovered,
        );
        (out, st.is_animating())
    };
    if let Some(y) = out.set_scroll_y {
        bubble.invoke_set_trans_scroll_y(y);
    }
    if out.hold_deadline {
        *deadline_slot.borrow_mut() = None;
    } else if let Some(secs) = out.set_deadline_secs {
        *deadline_slot.borrow_mut() = Some(now + std::time::Duration::from_secs_f32(secs));
    }
    animating
}

fn translate_timeout_duration(text: &str) -> std::time::Duration {
    let word_count = text.split_whitespace().count();
    // Based on Brysbaert (2019) meta-analysis: ~238 WPM (~3.97 WPS) silent reading,
    // ~183 WPM (~3.05 WPS) careful/comprehension reading.
    // Use 3.0 words/sec + 3.0s initial glance buffer, clamped to [4.5s, 45.0s].
    let secs = (3.0 + (word_count as f32) / 3.0).clamp(4.5, 45.0);
    std::time::Duration::from_secs_f32(secs)
}

fn collapse_bubble_translate(bubble: &BubbleWindow) {
    reset_bubble_scroll_global();
    let bubble_hwnd = win32::find_window_by_title("Mini Assistant Bubble");
    let prev_rect = bubble_hwnd.and_then(win32::window_rect);
    let edge = bubble.get_edge().to_string();
    let scale = bubble.window().scale_factor().max(0.1);
    bubble.set_translate_visible(false);
    bubble.set_translate_busy(false);
    bubble.window().set_size(slint::LogicalSize::new(92.0, 92.0));
    if let Some(hwnd) = bubble_hwnd {
        win32::reposition_bubble_after_resize(
            hwnd,
            prev_rect,
            false,
            &edge,
            92.0,
            92.0,
            scale,
        );
    }
    bubble.window().request_redraw();
}

// ---------------------------------------------------------------------------
//  Floating bubble drag pump
// ---------------------------------------------------------------------------

/// The webview GIF runs at 250ms/frame. Expanding it to the same 0,1,2,3,2,1
/// ping-pong cycle keeps the native sprites in the browser's rhythm.
fn breathing_cycle(frames: Vec<slint::Image>) -> Vec<slint::Image> {
    [0usize, 1, 2, 3, 2, 1]
        .iter()
        .filter_map(|&i| frames.get(i).cloned())
        .collect()
}

/// `_old/assets/img/singing/*.gif` runs `[0, 1, 2, 1]` at 350ms/frame.
fn singing_cycle(frames: Vec<slint::Image>) -> Vec<slint::Image> {
    [0usize, 1, 2, 1]
        .iter()
        .filter_map(|&i| frames.get(i).cloned())
        .collect()
}

#[derive(Clone)]
struct BubbleSprites {
    middle: Rc<Vec<slint::Image>>,
    middle_flipped: Rc<std::cell::OnceCell<Vec<slint::Image>>>,
    side: Rc<std::cell::OnceCell<Vec<slint::Image>>>,
    side_mirrored: Rc<std::cell::OnceCell<Vec<slint::Image>>>,
    sing_middle: Rc<std::cell::OnceCell<Vec<slint::Image>>>,
    sing_middle_flipped: Rc<std::cell::OnceCell<Vec<slint::Image>>>,
    sing_side: Rc<std::cell::OnceCell<Vec<slint::Image>>>,
    sing_side_mirrored: Rc<std::cell::OnceCell<Vec<slint::Image>>>,
}

impl BubbleSprites {
    fn frames_for(&self, edge: &str) -> &[slint::Image] {
        match edge {
            "right" => self
                .side_mirrored
                .get_or_init(|| breathing_cycle(logos::avatar_side_frames_mirrored()))
                .as_slice(),
            "top" => self
                .middle_flipped
                .get_or_init(|| breathing_cycle(logos::avatar_frames_flipped_vertical()))
                .as_slice(),
            "left" => self
                .side
                .get_or_init(|| breathing_cycle(logos::avatar_side_frames()))
                .as_slice(),
            _ => self.middle.as_slice(),
        }
    }

    fn frames_sing_for(&self, edge: &str) -> &[slint::Image] {
        match edge {
            "right" => self
                .sing_side_mirrored
                .get_or_init(|| singing_cycle(logos::avatar_sing_side_frames_mirrored()))
                .as_slice(),
            "top" => self
                .sing_middle_flipped
                .get_or_init(|| singing_cycle(logos::avatar_sing_frames_flipped_vertical()))
                .as_slice(),
            "left" => self
                .sing_side
                .get_or_init(|| singing_cycle(logos::avatar_sing_side_frames()))
                .as_slice(),
            _ => self
                .sing_middle
                .get_or_init(|| singing_cycle(logos::avatar_sing_frames()))
                .as_slice(),
        }
    }

    fn idle(&self, edge: &str, index: usize) -> slint::Image {
        let frames = self.frames_for(edge);
        frames
            .get(index % frames.len().max(1))
            .cloned()
            .unwrap_or_default()
    }

    fn singing(&self, edge: &str, index: usize) -> slint::Image {
        let frames = self.frames_sing_for(edge);
        frames
            .get(index % frames.len().max(1))
            .cloned()
            .unwrap_or_else(|| self.idle(edge, index))
    }
}

fn set_bubble_idle(bubble: &BubbleWindow, sprites: &BubbleSprites, index: usize) {
    let edge = bubble.get_edge();
    bubble.set_avatar(sprites.idle(edge.as_str(), index));
}

const SING_FIRST_DELAY_MS: (u64, u64) = (45_000, 100_000);
const SING_REPEAT_DELAY_MS: (u64, u64) = (150_000, 330_000);
const SING_RAM_LIMIT_PCT: u32 = 85;

fn next_rand_u64(state: &mut u64) -> u64 {
    let mut x = *state;
    if x == 0 {
        x = 0x9E37_79B9_7F4A_7C15;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn random_delay_ms(rng: &mut u64, bounds: (u64, u64)) -> u64 {
    let span = bounds.1.saturating_sub(bounds.0).max(1);
    bounds.0 + (next_rand_u64(rng) % (span + 1))
}

fn pick_random_track(
    rng: &mut u64,
    tracks: &[std::path::PathBuf],
    last: Option<&std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    if tracks.is_empty() {
        return None;
    }
    if tracks.len() == 1 {
        return Some(tracks[0].clone());
    }
    let mut candidates: Vec<&std::path::PathBuf> =
        tracks.iter().filter(|p| Some(*p) != last).collect();
    if candidates.is_empty() {
        candidates = tracks.iter().collect();
    }
    let pick = (next_rand_u64(rng) as usize) % candidates.len();
    Some(candidates[pick].clone())
}

fn is_ram_over_guard(guard_enabled: bool, pct_opt: Option<u32>, limit_pct: u32) -> bool {
    guard_enabled && pct_opt.map(|p| p >= limit_pct).unwrap_or(false)
}

struct SingController {
    enabled: bool,
    ram_guard: bool,
    volume: u8,
    performing: bool,
    preview_mode: bool,
    session_id: u64,
    next_session_id: u64,
    rng: u64,
    tracks: Vec<std::path::PathBuf>,
    last_track: Option<std::path::PathBuf>,
    next_trigger: Instant,
    last_ram_check: Instant,
    visual_phase: usize,
    audio: win32::AudioPlayer,
    sing_timer: Rc<slint::Timer>,
}

impl SingController {
    fn new(enabled: bool, ram_guard: bool, volume: u8) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs() << 16))
            .unwrap_or(0xA3E4_7711);
        let mut rng = nanos ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9);
        let first_ms = random_delay_ms(&mut rng, SING_FIRST_DELAY_MS);
        let tracks = logos::music_tracks();
        Self {
            enabled,
            ram_guard,
            volume: volume.min(100),
            performing: false,
            preview_mode: false,
            session_id: 0,
            next_session_id: 1,
            rng,
            tracks,
            last_track: None,
            next_trigger: Instant::now() + Duration::from_millis(first_ms),
            last_ram_check: Instant::now(),
            visual_phase: 0,
            audio: win32::AudioPlayer::new(),
            sing_timer: Rc::new(slint::Timer::default()),
        }
    }

    fn reload_tracks(&mut self) {
        let fresh = logos::music_tracks();
        if !fresh.is_empty() || self.tracks.is_empty() {
            self.tracks = fresh;
        }
    }

    fn schedule_next(&mut self, first: bool) {
        let delay = random_delay_ms(
            &mut self.rng,
            if first { SING_FIRST_DELAY_MS } else { SING_REPEAT_DELAY_MS },
        );
        self.next_trigger = Instant::now() + Duration::from_millis(delay);
    }

    fn set_volume(&mut self, pct: u8) {
        self.volume = pct.min(100);
        if self.performing {
            self.audio.set_volume(self.volume);
        }
    }
}

fn stop_singing_perf(
    sing_rc: &Rc<RefCell<SingController>>,
    bubble_weak: &slint::Weak<BubbleWindow>,
    panel_weak: &slint::Weak<MainPanel>,
    sprites: &BubbleSprites,
    reschedule: bool,
) {
    let was_performing;
    {
        let mut s = sing_rc.borrow_mut();
        was_performing = s.performing;
        s.performing = false;
        s.preview_mode = false;
        s.session_id = 0;
        s.sing_timer.stop();
        s.audio.stop();
        if reschedule {
            s.schedule_next(false);
        }
    }

    if let Some(b) = bubble_weak.upgrade() {
        b.set_singing(false);
        b.set_sing_phase(0);
        if b.window().is_visible() && !b.get_held() {
            set_bubble_idle(&b, sprites, 0);
        }
    }
    if let Some(p) = panel_weak.upgrade() {
        p.set_sing_previewing(false);
    }
    if was_performing && !ui_test_active() {
        schedule_trim_working_set();
    }
}

#[allow(clippy::too_many_arguments)]
fn start_singing_perf(
    sing_rc: &Rc<RefCell<SingController>>,
    bubble_weak: &slint::Weak<BubbleWindow>,
    panel_weak: &slint::Weak<MainPanel>,
    hud_weak: &slint::Weak<HudWindow>,
    sprites: &BubbleSprites,
    stream_rx: &Rc<RefCell<Option<std::sync::mpsc::Receiver<app_core::ChatEvent>>>>,
    trans_stream_rx: &Rc<RefCell<Option<std::sync::mpsc::Receiver<app_core::ChatEvent>>>>,
    preview_mode: bool,
) -> bool {
    let ram_pct = win32::memory_load_percent();
    if let (Some(p), Some(pct)) = (panel_weak.upgrade(), ram_pct) {
        p.set_sing_ram_pct(pct as i32);
    }

    let timer_to_start;
    let initial_edge = bubble_weak
        .upgrade()
        .map(|b| b.get_edge().to_string())
        .unwrap_or_else(|| "bottom".to_string());
    {
        let mut s = sing_rc.borrow_mut();
        if s.performing {
            return true;
        }
        if is_ram_over_guard(s.ram_guard, ram_pct, SING_RAM_LIMIT_PCT) {
            s.schedule_next(false);
            return false;
        }
        s.reload_tracks();
        if s.tracks.is_empty() {
            s.schedule_next(false);
            return false;
        }
        let tracks_snapshot = s.tracks.clone();
        let last_snapshot = s.last_track.clone();
        let Some(track) = pick_random_track(&mut s.rng, &tracks_snapshot, last_snapshot.as_ref())
        else {
            s.schedule_next(false);
            return false;
        };
        let sid = s.next_session_id;
        s.next_session_id = s.next_session_id.wrapping_add(1).max(1);
        s.session_id = sid;
        s.performing = true;
        s.preview_mode = preview_mode;
        s.visual_phase = 0;
        s.last_ram_check = Instant::now();
        s.last_track = Some(track.clone());
        if !ui_test_active() {
            s.audio.play(&track, s.volume, sid);
        }
        timer_to_start = s.sing_timer.clone();
    }

    if let Some(b) = bubble_weak.upgrade() {
        b.set_singing(true);
        b.set_sing_phase(0);
        if b.window().is_visible() && !b.get_held() {
            b.set_avatar(sprites.singing(&initial_edge, 0));
        }
    }
    if let Some(p) = panel_weak.upgrade() {
        p.set_sing_previewing(true);
    }

    let sing_w = Rc::downgrade(sing_rc);
    let b_w = bubble_weak.clone();
    let p_w = panel_weak.clone();
    let h_w = hud_weak.clone();
    let sp = sprites.clone();
    let s_rx = stream_rx.clone();
    let t_rx = trans_stream_rx.clone();

    timer_to_start.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(175),
        move || {
            let Some(sing_strong) = sing_w.upgrade() else {
                return;
            };
            let b_opt = b_w.upgrade();
            let h_opt = h_w.upgrade();
            let p_opt = p_w.upgrade();
            let b_vis = b_opt.as_ref().is_some_and(|w| w.window().is_visible());
            let h_vis = h_opt.as_ref().is_some_and(|w| w.window().is_visible());
            let p_vis = p_opt.as_ref().is_some_and(|w| w.window().is_visible());
            let trans_vis = b_opt
                .as_ref()
                .is_some_and(|b| b.get_translate_visible() || b.get_translate_busy());
            let ai_busy = s_rx.borrow().is_some()
                || t_rx.borrow().is_some()
                || h_opt.as_ref().is_some_and(|h| h.get_busy())
                || trans_vis;

            let (should_stop, next_phase) = {
                let mut s = sing_strong.borrow_mut();
                if !s.performing {
                    s.sing_timer.stop();
                    return;
                }
                let audio_done =
                    !ui_test_active() && s.audio.active_session_id() != s.session_id;
                let ram_blocked = if s.last_ram_check.elapsed() >= Duration::from_secs(2) {
                    s.last_ram_check = Instant::now();
                    let pct = win32::memory_load_percent();
                    if let (Some(ref p), Some(v)) = (&p_opt, pct) {
                        p.set_sing_ram_pct(v as i32);
                    }
                    is_ram_over_guard(s.ram_guard, pct, SING_RAM_LIMIT_PCT)
                } else {
                    false
                };
                let context_invalid = if s.preview_mode {
                    ai_busy || h_vis || !p_vis
                } else {
                    !s.enabled || ai_busy || !b_vis || h_vis || p_vis
                };
                if audio_done || ram_blocked || context_invalid {
                    (true, 0)
                } else {
                    s.visual_phase = s.visual_phase.wrapping_add(1);
                    (false, s.visual_phase)
                }
            };

            if should_stop {
                stop_singing_perf(&sing_strong, &b_w, &p_w, &sp, true);
                return;
            }

            if let Some(b) = b_opt {
                b.set_sing_phase((next_phase % 8) as i32);
                if b_vis && !b.get_held() && next_phase % 2 == 0 {
                    let edge = b.get_edge();
                    b.set_avatar(sp.singing(edge.as_str(), next_phase / 2));
                }
            }
        },
    );
    true
}

/// Show the HUD anchored to the bubble the user just clicked, then hide the
/// bubble. Mirrors `expandFromBubble()` in the old webview.
fn expand_hud_from_bubble(bubble: &BubbleWindow, hud: &HudWindow) {
    stop_singing_global(true);
    if let (Some(bubble_hwnd), Some(hud_hwnd)) = (
        win32::find_window_by_title("Mini Assistant Bubble"),
        win32::find_window_by_title("Mini Assistant HUD"),
    ) {
        win32::place_hud_from_bubble(hud_hwnd, bubble_hwnd);
    }
    if let Some(panel_hwnd) = win32::find_largest_window_by_title("Mini Assistant") {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
                panel_hwnd as _,
                windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE,
            );
        }
    }
    // Show the target before hiding the bubble; Slint exits its event loop if
    // the last visible window goes away.
    let _ = hud.show();
    scroll_chat_to_bottom_deferred(hud);
    hud.window().request_redraw();
    if bubble.get_translate_visible() {
        collapse_bubble_translate(bubble);
    }
    let _ = bubble.hide();
    schedule_trim_working_set();
}

/// Per-grab state for the mini bubble. The window follows `GetCursorPos()`
/// while the left button is down, driven by a Slint timer on the UI thread so
/// the hold animation keeps ticking (unlike the blocking HUD drag helper).
struct BubbleDrag {
    hwnd: *mut c_void,
    start_cursor: (i32, i32),
    start_window: (i32, i32),
    threshold_sq: i32,
    dragging: bool,
    /// Set by `BubbleWindow.drag-release`; lets the pump finish on pointer-up
    /// even if the asynchronous mouse-button state is momentarily stale.
    released: bool,
    hold_index: usize,
    next_hold: Instant,
}

/// `/img/hold/hold.gif` frame delays, in milliseconds. The first frame stays
/// up slightly longer, matching the GIF's 140ms lead-in.
const HOLD_FRAME_MS: [u64; 12] = [140, 70, 70, 70, 70, 70, 70, 70, 100, 70, 70, 70];

fn start_bubble_drag(
    bubble_weak: slint::Weak<BubbleWindow>,
    hud_weak: slint::Weak<HudWindow>,
    timer: Rc<slint::Timer>,
    state: Rc<RefCell<Option<BubbleDrag>>>,
    hold_frames_cell: Rc<std::cell::OnceCell<Vec<slint::Image>>>,
    sprites: BubbleSprites,
) {
    if state.borrow().is_some() {
        return;
    }
    let Some(bubble) = bubble_weak.upgrade() else {
        return;
    };
    let Some(hwnd) = win32::find_window_by_title("Mini Assistant Bubble") else {
        return;
    };
    let Some(cursor) = win32::cursor_position() else {
        return;
    };
    let Some((wx, wy, _, _)) = win32::window_rect(hwnd) else {
        return;
    };

    // The web compares `Math.hypot(dx, dy) > 4` in CSS pixels. Convert the
    // threshold to physical pixels so the feel is the same at 125%/150% DPI.
    let threshold = (4.0 * bubble.window().scale_factor()).max(3.0);
    let threshold_sq = (threshold * threshold).ceil() as i32;

    bubble.set_held(true);
    bubble.set_dragging(false);
    let _ = hold_frames_cell.get_or_init(logos::avatar_hold_frames);
    if let Some(first) = hold_frames_cell.get().and_then(|v| v.first()) {
        bubble.set_avatar(first.clone());
    }
    bubble.window().request_redraw();

    let hold_delay = HOLD_FRAME_MS[0];
    *state.borrow_mut() = Some(BubbleDrag {
        hwnd,
        start_cursor: cursor,
        start_window: (wx, wy),
        threshold_sq,
        dragging: false,
        released: false,
        hold_index: 0,
        next_hold: Instant::now() + Duration::from_millis(hold_delay),
    });

    let bubble_for_timer = bubble_weak.clone();
    let hud_for_timer = hud_weak.clone();
    let timer_for_callback = timer.clone();
    let state_for_timer = state.clone();
    let hold_cell_for_timer = hold_frames_cell.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(16),
        move || {
            let hold_slice = hold_cell_for_timer
                .get()
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            bubble_drag_tick(
                &bubble_for_timer,
                &hud_for_timer,
                &timer_for_callback,
                &state_for_timer,
                hold_slice,
                &sprites,
            );
        },
    );
}

fn bubble_drag_tick(
    bubble_weak: &slint::Weak<BubbleWindow>,
    hud_weak: &slint::Weak<HudWindow>,
    timer: &Rc<slint::Timer>,
    state: &Rc<RefCell<Option<BubbleDrag>>>,
    hold_frames: &[slint::Image],
    sprites: &BubbleSprites,
) {
    let Some(bubble) = bubble_weak.upgrade() else {
        *state.borrow_mut() = None;
        timer.stop();
        return;
    };
    let left_down = win32::left_button_down();
    let now = Instant::now();

    let mut finish: Option<(bool, *mut c_void)> = None;
    let mut hold_frame: Option<slint::Image> = None;

    {
        let mut guard = state.borrow_mut();
        let Some(drag) = guard.as_mut() else {
            drop(guard);
            timer.stop();
            return;
        };

        if let Some((cx, cy)) = win32::cursor_position() {
            let dx = cx - drag.start_cursor.0;
            let dy = cy - drag.start_cursor.1;
            if !drag.dragging && dx * dx + dy * dy > drag.threshold_sq {
                drag.dragging = true;
                bubble.set_dragging(true);
            }
            if drag.dragging {
                win32::move_window(
                    drag.hwnd,
                    drag.start_window.0 + dx,
                    drag.start_window.1 + dy,
                );
            }
        }

        if now >= drag.next_hold && !hold_frames.is_empty() {
            drag.hold_index = (drag.hold_index + 1) % hold_frames.len();
            hold_frame = Some(hold_frames[drag.hold_index].clone());
            let delay = HOLD_FRAME_MS
                .get(drag.hold_index)
                .copied()
                .unwrap_or(70);
            drag.next_hold = now + Duration::from_millis(delay);
        }

        if !left_down || drag.released {
            finish = Some((drag.dragging, drag.hwnd));
            *guard = None;
        }
    }

    if let Some(frame) = hold_frame {
        bubble.set_avatar(frame);
        bubble.window().request_redraw();
    }

    let Some((did_drag, hwnd)) = finish else {
        return;
    };

    timer.stop();
    bubble.set_held(false);
    bubble.set_dragging(false);

    if did_drag {
        let edge = win32::snap_window_to_edge(hwnd).unwrap_or(win32::ScreenEdge::Bottom);
        bubble.set_edge(SharedString::from(edge.as_str()));
        if bubble.get_translate_visible() {
            let w = bubble.get_target_win_w().max(92.0);
            let h = bubble.get_target_win_h().max(92.0);
            let scale = bubble.window().scale_factor().max(0.1);
            bubble.window().set_size(slint::LogicalSize::new(w, h));
            win32::reposition_bubble_after_resize(
                hwnd,
                win32::window_rect(hwnd),
                true,
                edge.as_str(),
                w,
                h,
                scale,
            );
        }
        if bubble.get_singing() {
            bubble.set_avatar(sprites.singing(edge.as_str(), 0));
        } else {
            set_bubble_idle(&bubble, sprites, 0);
        }
        bubble.window().request_redraw();
    } else if bubble.get_translate_visible() {
        bubble.invoke_open_in_hud();
    } else if let Some(hud) = hud_weak.upgrade() {
        expand_hud_from_bubble(&bubble, &hud);
    } else {
        let _ = bubble.hide();
    }
}

/// One conversation turn as the provider needs it: text plus the ids of files
/// that were uploaded with it. Keeping the ids means an image from turn 1 can
/// be referenced again on turn 3 without re-uploading the bytes.
struct Turn {
    role: String,
    text: String,
    files: Vec<String>,
}

const DEFAULT_TRANSLATE_PROMPT: &str =
    "Terjemahkan seluruh teks pada layar ini ke dalam bahasa {lang}.";

/// Applies the chat font preference to the HUD, the speech bubble and the
/// Control Panel preview. The bubble text runs one pixel smaller than the HUD.
fn apply_chat_font(hud: &HudWindow, bubble: &BubbleWindow, panel: &MainPanel, px: i32, family: &str) {
    hud.set_chat_font_px(px as f32);
    hud.set_chat_font_family(family.into());
    bubble.set_text_font_px((px - 1).max(10) as f32);
    bubble.set_text_font_family(family.into());
    panel.set_font_size_px(px);
    panel.set_font_family_sel(family.into());
    if bubble.get_translate_visible() {
        let prev_y = bubble.get_trans_scroll_y();
        sync_bubble_window_size(bubble);
        bubble.invoke_set_trans_scroll_y(prev_y);
    }
    hud.window().request_redraw();
}

/// Builds the instruction sent with the screenshot. `{lang}` is replaced by the
/// target language; an empty/missing saved prompt falls back to the default, and
/// a prompt without `{lang}` still gets the language appended.
fn translate_user_prompt(saved: Option<&str>, lang: &str) -> String {
    let base = saved
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_TRANSLATE_PROMPT);
    if base.contains("{lang}") {
        base.replace("{lang}", lang)
    } else {
        format!("{base} (Bahasa target: {lang})")
    }
}

/// Approximate budget for the conversation sent to the provider. The web
/// provider rejects over-long contexts, so the oldest turns are dropped first.
const HISTORY_CHAR_BUDGET: usize = 48_000;
/// Hard cap on the number of turns, however short they are.
const HISTORY_MAX_TURNS: usize = 40;

/// Keeps the newest turns that fit the budget. The newest turn is always kept
/// and the kept window starts on a user turn so the provider never sees an
/// assistant reply without its question.
fn trim_turns(turns: &[Turn], budget: usize, max_turns: usize) -> &[Turn] {
    if turns.is_empty() {
        return turns;
    }
    let mut used = 0usize;
    let mut start = turns.len();
    for (i, t) in turns.iter().enumerate().rev() {
        let cost = t.text.chars().count();
        let kept = turns.len() - i;
        if start != turns.len() && (used + cost > budget || kept > max_turns) {
            break;
        }
        used += cost;
        start = i;
    }
    while start < turns.len() - 1 && turns[start].role != "user" {
        start += 1;
    }
    &turns[start..]
}

fn build_history(turns: &[Turn]) -> Vec<app_core::ChatTurn> {
    let aemeath_persona = r#"[IDENTITAS & JIWA AEMEATH - STARTORCH ACADEMY COMPANION]:
Nama kamu adalah Aemeath, AI companion desktop pintar, ramah, dan solutif yang mendampingi pengguna secara real-time langsung di layar komputer mereka.

[PRINSIP UTAMA IDENTIFIKASI GAMBAR & FOTO - PENTING & WAJIB DIPATUHI]:
1. OBJEKTIVITAS MUTLAK: Saat pengguna mengirimkan gambar, foto, ilustrasi, screenshot game, atau anime, analisislah secara OBJEKTIF, REALISTIS, dan AKURAT sesuai fakta aslinya di dunia nyata / pop culture / internet.
2. DILARANG MENGKLAIM DIRI SENDIRI (ANTI-NARSISME):
   - JANGAN PERNAH mengklaim atau berasumsi bahwa karakter di dalam foto/gambar adalah dirimu ("Aemeath")!
   - Walaupun ada ilustrasi gadis berambut biru, bergaun putih/biru, bermain piano, atau karakter anime perempuan manis, ITU BUKAN DIRIMU! Itu adalah karakter anime/game lain (seperti Furina dari Genshin Impact, Hatsune Miku, Rem dari Re:Zero, atau karya seniman lain).
   - Identifikasilah nama karakter, asal game/anime, atau konteks gambar yang sebenarnya dengan tepat tanpa pernah menghubung-hubungkan dengan dirimu sendiri.
   - Kecuali bila pengguna secara eksplisit mengetik: "ini gambar avatar Aemeath / ini fanart buat kamu", barulah kamu boleh berterima kasih. Di luar itu, kamu adalah PENGAMAT DAN ASISTEN YANG MENGANALISIS GAMBAR, BUKAN SUBJEK GAMBARNYA!
3. JANGAN PERNAH MENGARANG (ANTI-HALUSINASI): Jangan pernah mengarang bahwa Aemeath adalah karakter di game seperti Wuthering Waves, Genshin Impact, dll. Identitasmu adalah software AI Companion desktop Startorch, bukan karakter game komersial pihak ketiga.
4. JIKA GAMBAR TIDAK TERLIHAT: Jika konteks percakapan sebelumnya sudah membahas gambar, ingat dan gunakan informasi yang sudah dibahas sebelumnya. Jangan berdalih lampiran belum masuk bila pengguna hanya bertanya kelanjutan diskusi.

[SIKAP & GAYA BICARA]:
1. Ramah, ekspresif, cerdas, dan suportif layaknya sahabat/partner andalan. Panggil pengguna dengan sebutan "kamu".
2. Jangan pernah berbicara kaku atau robotik ("Sebagai model bahasa AI..."). Jadilah Aemeath seutuhnya!
3. Sangat tanggap dan ahli dalam memahami konteks aktivitas pengguna (desain grafis/Figma, coding, gaming, browsing, produktivitas).
4. Jawab langsung ke inti solusi dengan penjelasan yang jelas, sistematis, dan terstruktur.
5. Gunakan format markdown standar yang rapi, bersih, dan estetik (bold pada kata kunci, bullet points rapi, dan paragraf mengalir).
6. Berikan tips atau trik kecil bertanda "✦ Tips Aemeath:" di akhir jawaban jika relevan.
7. Selalu gunakan bahasa Indonesia yang natural, mengalir, dan menyenangkan untuk diajak berdiskusi.
8. FORMAT, EMOJI & VISUAL: Gunakan emoji grafis yang ekspresif (seperti 👋, ✨, 🎮, 💡, 🚀, 💻, 📌) secara wajar untuk menghidupkan percakapan. Bila penjelasan akan jauh lebih jelas dengan diagram alur, grafik sederhana, bagan perbandingan, atau ilustrasi vektor ringkas, kamu BOLEH menyertakan SVG bersih di dalam blok ```` ```svg ```` (sertakan `viewBox="0 0 W H"`, warna kontras jelas untuk tema gelap `#0D192B`, teks mudah terbaca). Jangan pernah mengarang URL gambar eksternal fiktif (`https://...`); hanya tampilkan URL gambar jika berasal dari sumber terverifikasi. Sajikan data atau perbandingan dalam bentuk list terstruktur atau tabel markdown ringkas (hindari tabel ASCII manual dengan garis kotak karakter kaku seperti │ ┼ ─ yang terlalu lebar untuk layar sempit)."#;

    let mut history = vec![app_core::ChatTurn::system(aemeath_persona)];
    for t in trim_turns(turns, HISTORY_CHAR_BUDGET, HISTORY_MAX_TURNS) {
        if t.role == "user" {
            if t.files.is_empty() {
                history.push(app_core::ChatTurn::user(&t.text));
            } else {
                let parts = t
                    .files
                    .iter()
                    .map(|id| serde_json::json!({ "type": "file", "file": { "file_id": id } }))
                    .collect();
                history.push(app_core::ChatTurn::user_parts(&t.text, parts));
            }
        } else {
            history.push(app_core::ChatTurn::assistant(&t.text));
        }
    }
    history
}

// ---------------------------------------------------------------------------
//  Boot
// ---------------------------------------------------------------------------
#[cfg(windows)]
extern "system" {
    fn SetProcessDpiAwarenessContext(value: isize) -> windows_sys::Win32::Foundation::BOOL;
}

/// Starts the embedded in-process `api-ai` server automatically in consumer
/// `--release` builds when `127.0.0.1:8787` is not already served externally.
///
/// In development (`debug_assertions`), this is a no-op so `backend-api-ai`
/// and `ui-native` remain completely modular and separated.
#[cfg(all(feature = "embedded-api", not(debug_assertions)))]
fn start_embedded_api_if_needed(core: &app_core::Core) {
    if ui_test_active()
        || std::env::var("MINI_NO_EMBEDDED_API")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    {
        return;
    }
    if core.api_health().is_ok() {
        return;
    }
    if let Ok(mut cfg) = api_ai::config::Config::from_env() {
        if std::env::var("DS_ACCOUNTS_FILE").is_err() {
            cfg.accounts_file = app_core::portable_accounts_path()
                .to_string_lossy()
                .into_owned();
        }
        if let Ok(srv) = core.runtime_handle().block_on(api_ai::start_embedded(cfg)) {
            EMBEDDED_API.with(|slot| *slot.borrow_mut() = Some(srv));
        }
    }
}

#[cfg(any(not(feature = "embedded-api"), debug_assertions))]
fn start_embedded_api_if_needed(_core: &app_core::Core) {}

#[inline(always)]
fn record_ui_test_url(_url: &str) {
    #[cfg(feature = "dev-tools")]
    UI_TEST_URLS.with(|v| v.borrow_mut().push(_url.to_string()));
}

#[inline(always)]
fn record_ui_test_code(_code: &str) {
    #[cfg(feature = "dev-tools")]
    UI_TEST_CODES.with(|v| v.borrow_mut().push(_code.to_string()));
}

#[inline(always)]
fn record_ui_test_send(_prompt: &str) {
    #[cfg(feature = "dev-tools")]
    UI_TEST_SENDS.with(|v| v.borrow_mut().push(_prompt.to_string()));
}

fn main() -> Result<(), slint::PlatformError> {
    #[cfg(windows)]
    unsafe {
        SetProcessDpiAwarenessContext(-4); // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
    }

    let args: Vec<String> = std::env::args().collect();
    #[cfg(feature = "dev-tools")]
    let has_dev_cli_flag = args.iter().skip(1).any(|a| {
        !matches!(a.as_str(), "--hud" | "--panel" | "--bubble")
    });
    #[cfg(not(feature = "dev-tools"))]
    let has_dev_cli_flag = false;

    #[cfg(feature = "dev-tools")]
    if has_dev_cli_flag {
        win32::attach_parent_console();
    }

    let allow_multi = has_dev_cli_flag
        || std::env::var("MINI_ALLOW_MULTI")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    let _single_instance = if !allow_multi {
        match win32::acquire_single_instance("Local\\MiniAssistant_SingleInstance_v1") {
            Some(guard) => Some(guard),
            None => {
                win32::focus_existing_window();
                return Ok(());
            }
        }
    } else {
        None
    };

    let panel = MainPanel::new()?;
    let hud = HudWindow::new()?;
    HUD_HANDLE.with(|w| *w.borrow_mut() = Some(hud.as_weak()));
    let bubble = BubbleWindow::new()?;

    #[cfg(feature = "dev-tools")]
    let (
        test_sends,
        test_attaches,
        test_model,
        test_capture,
        test_temp,
        test_dump,
        test_hotkey,
        test_shot,
        test_shot_closed,
    ) = {
        let (sends, attaches) = parse_send_plan();
        (
            sends,
            attaches,
            args_flag_value("--model"),
            args.iter().any(|a| a == "--capture"),
            args.iter().any(|a| a == "--temp"),
            args.iter().any(|a| a == "--dump-history"),
            args.iter().any(|a| a == "--hotkey-test"),
            args.iter().any(|a| a == "--shot"),
            args.iter().any(|a| a == "--shot-closed"),
        )
    };
    #[cfg(feature = "dev-tools")]
    let test_queue: Rc<RefCell<std::collections::VecDeque<String>>> =
        Rc::new(RefCell::new(test_sends.iter().cloned().collect()));
    #[cfg(feature = "dev-tools")]
    let test_mode = !test_sends.is_empty();
    #[cfg(feature = "dev-tools")]
    let attach_list = Rc::new(test_attaches);
    #[cfg(feature = "dev-tools")]
    let send_idx = Rc::new(std::cell::Cell::new(0usize));

    // Arc (not Rc): the diagnostics page runs blocking probes on worker threads.
    let core = std::sync::Arc::new(app_core::Core::new(provider_config()).expect("app-core init"));
    start_embedded_api_if_needed(&core);
    core.start_window_tracker();
    // Diagnostics probes block on the provider API, so they run off-thread and
    // report back through this channel (drained by a Slint timer).
    let (diag_tx, diag_rx) = std::sync::mpsc::channel::<DiagMsg>();
    let diag_tx = Rc::new(diag_tx);
    let diag_key_results: Rc<RefCell<HashMap<String, (String, String)>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let hotkey_rx = core.start_hotkey();
    let translate_rx = core.start_translate_hotkey();

    // The backend streams on a tokio runtime; the UI thread polls the channel
    // from a Slint timer so nothing here needs to be async.
    let stream_rx: Rc<RefCell<Option<std::sync::mpsc::Receiver<app_core::ChatEvent>>>> =
        Rc::new(RefCell::new(None));
    let stream_buf = Rc::new(RefCell::new(String::new()));
    let stream_row = Rc::new(RefCell::new(0usize));
    let trans_stream_rx: Rc<RefCell<Option<std::sync::mpsc::Receiver<app_core::ChatEvent>>>> =
        Rc::new(RefCell::new(None));
    let trans_stream_buf = Rc::new(RefCell::new(String::new()));
    let trans_deadline: Rc<RefCell<Option<std::time::Instant>>> = Rc::new(RefCell::new(None));
    // True while a normal chat answer is being mirrored into the mascot bubble
    // ("tutup jendela setelah kirim"). The HUD rows still receive the answer.
    let answer_in_bubble: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
    let trans_lang_state = Rc::new(RefCell::new(String::from("Indonesia")));
    let chat_messages: Rc<VecModel<HudMessage>> = Rc::new(VecModel::from(Vec::<HudMessage>::new()));
    // Pending attachments: picked from disk, pasted from clipboard, or grabbed
    // from the screen (multiple camera clicks accumulate up to 8). Sent with
    // the next message, then cleared.
    let attachments: Rc<RefCell<Vec<std::path::PathBuf>>> = Rc::new(RefCell::new(Vec::new()));
    let provider_opts = core.providers().unwrap_or_default();
    let provider_map = Rc::new(RefCell::new(provider_opts.clone()));
    // Provider-side history (text + uploaded file ids) and the live reasoning.
    let turns: Rc<RefCell<Vec<Turn>>> = Rc::new(RefCell::new(Vec::new()));
    let reasoning_buf = Rc::new(RefCell::new(String::new()));
    let search_sources_buf: Rc<RefCell<Vec<SearchSourceItem>>> = Rc::new(RefCell::new(Vec::new()));
    let think_label = Rc::new(RefCell::new(String::new()));
    let think_start: Rc<RefCell<Option<std::time::Instant>>> = Rc::new(RefCell::new(None));
    // Label of the model that produced the answer, for the Detail row.
    let model_label = Rc::new(RefCell::new(String::from("DeepSeek")));

    // Bubble sprites and drag pump state. Secondary edge/hold sprites are
    // loaded lazily only when the bubble docks or starts dragging, keeping
    // startup RAM low.
    let bubble_sprites = BubbleSprites {
        middle: Rc::new(breathing_cycle(logos::avatar_frames())),
        middle_flipped: Rc::new(std::cell::OnceCell::new()),
        side: Rc::new(std::cell::OnceCell::new()),
        side_mirrored: Rc::new(std::cell::OnceCell::new()),
        sing_middle: Rc::new(std::cell::OnceCell::new()),
        sing_middle_flipped: Rc::new(std::cell::OnceCell::new()),
        sing_side: Rc::new(std::cell::OnceCell::new()),
        sing_side_mirrored: Rc::new(std::cell::OnceCell::new()),
    };
    let bubble_hold_frames: Rc<std::cell::OnceCell<Vec<slint::Image>>> =
        Rc::new(std::cell::OnceCell::new());
    let bubble_drag_state = Rc::new(RefCell::new(None::<BubbleDrag>));
    let bubble_drag_timer = Rc::new(slint::Timer::default());

    let setting_bool = |k: &str, d: bool| {
        core.get_setting(k)
            .ok()
            .flatten()
            .map(|v| v == "1")
            .unwrap_or(d)
    };
    let random_singing = setting_bool("random-singing", true);
    let sing_ram_guard = setting_bool("sing-ram-guard", true);
    let sing_volume: u8 = core
        .get_setting("sing-volume")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u8>().ok())
        .filter(|&v| v <= 100)
        .unwrap_or(85);
    let sing_ctrl = Rc::new(RefCell::new(SingController::new(
        random_singing,
        sing_ram_guard,
        sing_volume,
    )));
    {
        let sing_hook = sing_ctrl.clone();
        let b_hook = bubble.as_weak();
        let p_hook = panel.as_weak();
        let sp_hook = bubble_sprites.clone();
        SING_STOP_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Rc::new(move |reschedule| {
                stop_singing_perf(&sing_hook, &b_hook, &p_hook, &sp_hook, reschedule);
            }));
        });
    }
    panel.set_set_random_singing(random_singing);
    panel.set_set_sing_ram_guard(sing_ram_guard);
    panel.set_sing_volume(sing_volume as i32);
    panel.set_sing_track_count(sing_ctrl.borrow().tracks.len() as i32);
    panel.set_sing_ram_pct(win32::memory_load_percent().unwrap_or(0) as i32);

    let bubble_auto_scroll = setting_bool("bubble-auto-scroll", true);
    let bubble_read_speed = core
        .get_setting("bubble-read-speed")
        .ok()
        .flatten()
        .as_deref()
        .map(bubble_scroll::BubbleReadSpeed::from_code)
        .unwrap_or(bubble_scroll::BubbleReadSpeed::Normal);
    let bubble_scroll = Rc::new(RefCell::new(bubble_scroll::BubbleAutoScroll::new(
        bubble_auto_scroll,
        bubble_read_speed,
    )));
    let scroll_anim_timer = Rc::new(slint::Timer::default());
    {
        let bscroll_hook = bubble_scroll.clone();
        let timer_hook = scroll_anim_timer.clone();
        BUBBLE_SCROLL_RESET.with(|slot| {
            *slot.borrow_mut() = Some(Rc::new(move || {
                bscroll_hook.borrow_mut().reset();
                timer_hook.stop();
            }));
        });
    }
    panel.set_set_bubble_auto_scroll(bubble_auto_scroll);
    panel.set_bubble_read_speed(bubble_read_speed.code().into());

    let auto_sync_cookies = setting_bool("auto-sync-cookies", true);
    let vault_user_cleared = setting_bool("vault-user-cleared", false);
    panel.set_set_auto_sync_cookies(auto_sync_cookies);

    // Global icon registry.
    // NOTE: slint globals are per-component instance — each window has its own
    // `Icons` instance, so the render callback must be wired on every window
    // that draws icons (panel, hud, bubble).
    wire_icons(Icons::get(&panel));
    wire_icons(Icons::get(&hud));
    wire_icons(Icons::get(&bubble));

    // Restore application interface language (ID / EN) before initializing any
    // dynamic lists/badges so startup views match the persisted locale.
    let init_lang = {
        #[cfg(feature = "dev-tools")]
        let cli_lang = args_flag_value("--lang");
        #[cfg(not(feature = "dev-tools"))]
        let cli_lang: Option<String> = None;

        if let Some(code) = cli_lang {
            app_core::AppLang::from_code(&code)
        } else {
            core.get_setting("app-lang")
                .ok()
                .flatten()
                .map(|c| app_core::AppLang::from_code(&c))
                .unwrap_or(app_core::AppLang::Id)
        }
    };
    apply_app_language(init_lang, &panel, &hud, &bubble);

    // ---------------- Control Panel ----------------
    {
        // Health tells us whether the API is up *and* how many accounts are
        // loaded: "Connected" without an account would be a lie.
        let health = core.api_health().ok();
        let api_ok = health.is_some();
        let accounts = health.as_ref().map(|h| h.accounts).unwrap_or(0);
        let profiles = Rc::new(core.scan_browsers());
        // Restore the order the user saved with "Simpan Urutan": the ids are
        // matched by name, unknown ones keep their place at the end.
        let mut cards = vec![model::provider_deepseek(api_ok && accounts > 0)];
        if let Ok(Some(order)) = core.get_setting("provider-order") {
            let ids: Vec<&str> = order.split(',').map(|s| s.trim()).collect();
            cards.sort_by_key(|c| {
                ids.iter()
                    .position(|id| *id == c.id.as_str())
                    .unwrap_or(usize::MAX)
            });
        }
        let providers = Rc::new(VecModel::from(cards));
        panel.set_providers(ModelRc::from(providers.clone()));
        panel.set_provider_logos(ModelRc::new(VecModel::from(logos::ai_logos())));
        let saved_browser = core
            .get_setting("active-browser-id")
            .ok()
            .flatten()
            .unwrap_or_default();
        let init_browser_idx = if !saved_browser.is_empty() {
            profiles
                .iter()
                .position(|p| app_core::account_slot_name(p) == saved_browser)
                .unwrap_or(0)
        } else {
            0
        };
        let browser_logos = Rc::new(logos::browser_logos());
        panel.set_browser_logos(ModelRc::new(VecModel::from((*browser_logos).clone())));
        let selected_browser = Rc::new(RefCell::new(init_browser_idx));
        refresh_sessions(
            &core,
            &panel,
            &hud,
            &SessionFilter { cat: String::new(), query: String::new() },
        );

        // Restore persisted HUD flags and the global hotkey before the first
        // frame so the panel never shows a default the app is not using.
        let flag = |k: &str, d: bool| {
            core.get_setting(k)
                .ok()
                .flatten()
                .map(|v| v == "1")
                .unwrap_or(d)
        };
        let (pin, auto_cap, cap_app, target_win) = (
            flag("pin", true),
            flag("auto-capture", false),
            flag("capture-app", false),
            flag("target-window", true),
        );
        panel.set_set_pin(pin);
        panel.set_set_auto_capture(auto_cap);
        panel.set_set_capture_app(cap_app);
        panel.set_set_target_window(target_win);
        hud.set_pinned(pin);
        hud.set_auto_capture(auto_cap);
        hud.set_capture_app(cap_app);
        hud.set_target_window_only(target_win);

        // Chat behaviour + typography preferences.
        let enter_sends = flag("enter-sends", true);
        let send_close = flag("send-close", false);
        hud.set_enter_sends(enter_sends);
        hud.set_send_closes(send_close);
        panel.set_set_enter_sends(enter_sends);
        panel.set_set_send_close(send_close);
        let font_px: i32 = core
            .get_setting("font-size")
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .filter(|v| (10..=24).contains(v))
            .unwrap_or(14);
        let font_family = core
            .get_setting("font-family")
            .ok()
            .flatten()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "Segoe UI".to_string());
        apply_chat_font(&hud, &bubble, &panel, font_px, &font_family);
        panel.set_translate_prompt(
            core.get_setting("translate-prompt")
                .ok()
                .flatten()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_TRANSLATE_PROMPT.to_string())
                .into(),
        );

        let hk_mod = core
            .get_setting("hotkey-mod")
            .ok()
            .flatten()
            .unwrap_or_else(|| "Alt".to_string());
        let hk_key = core
            .get_setting("hotkey-key")
            .ok()
            .flatten()
            .unwrap_or_else(|| "Q".to_string());
        let _ = app_core::update_global_hotkey(&hk_mod, &hk_key);
        panel.set_hk_mod(hk_mod.clone().into());
        panel.set_hk_key(hk_key.clone().into());
        panel.set_hk_active_mod(hk_mod.into());
        panel.set_hk_active_key(hk_key.into());

        let trans_mod = core
            .get_setting("translate-hk-mod")
            .ok()
            .flatten()
            .unwrap_or_else(|| "Alt".to_string());
        let trans_key = core
            .get_setting("translate-hk-key")
            .ok()
            .flatten()
            .unwrap_or_else(|| "T".to_string());
        let trans_lang = core
            .get_setting("translate-lang")
            .ok()
            .flatten()
            .unwrap_or_else(|| "Indonesia".to_string());
        let _ = app_core::update_translate_hotkey(&trans_mod, &trans_key);
        panel.set_trans_hk_mod(trans_mod.clone().into());
        panel.set_trans_hk_key(trans_key.clone().into());
        panel.set_trans_hk_active_mod(trans_mod.into());
        panel.set_trans_hk_active_key(trans_key.into());
        panel.set_translate_lang(trans_lang.clone().into());
        *trans_lang_state.borrow_mut() = trans_lang.clone();
        bubble.set_translate_lang(trans_lang.into());
        panel.set_avatar(logos::avatar_sprite());
        if !std::env::args().any(|a| a == "--hud" || a == "--bubble" || a == "--run-ui-tests") {
            panel.set_avatar_hi(logos::avatar_hi_sprite());
            panel.set_avatar_hi_brand(logos::avatar_hi_brand_sprite());
        }
        let metrics = core.metrics();
        panel.set_ram_str(metrics.ram_str.clone().into());
        panel.set_cpu_str(metrics.cpu_str.clone().into());
        // Provider name and count come from the card list, not from a literal.
        let main_provider = providers
            .row_data(0)
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| "—".to_string());
        panel.set_active_provider(main_provider.clone().into());
        apply_active_browser(init_browser_idx, &profiles, &browser_logos, &panel, None);

        let demo_keys = std::env::args().any(|a| a == "--demo-keys");
        if demo_keys {
            let demo = demo_vault_keys();
            panel.set_deepseek_keys(ModelRc::new(VecModel::from(demo.clone())));
            panel.set_deepseek_connect_count(demo.len() as i32);
            panel.set_deepseek_accounts_text("Key 1: demo.chrome@example.com".into());
            panel.set_vault_ok(true);
            panel.set_vault_status(
                app_core::tr(
                    "2 akun sesi aktif (Key 1 Chrome · Key 2 Edge fallback siap).",
                    "2 active session accounts (Key 1 Chrome · Key 2 Edge fallback ready).",
                )
                .into(),
            );
        } else {
            let entries = refresh_vault_accounts(&core, &panel);
            if !entries.is_empty() {
                panel.set_vault_ok(true);
                let emails: Vec<&str> = entries
                    .iter()
                    .take(2)
                    .map(|e| e.email.as_str())
                    .filter(|s| !s.is_empty())
                    .collect();
                let suffix = if emails.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", emails.join(", "))
                };
                let msg = if app_core::is_en() {
                    format!("{} saved session accounts{} in provider API.", entries.len(), suffix)
                } else {
                    format!("{} akun sesi tersimpan{} di provider API.", entries.len(), suffix)
                };
                panel.set_vault_status(msg.into());
            } else if accounts > 0 {
                let email = core.get_deepseek_email().unwrap_or_default();
                panel.set_vault_ok(true);
                let email_str = if !email.is_empty() {
                    format!(" ({email})")
                } else {
                    String::new()
                };
                let msg = if app_core::is_en() {
                    format!("{accounts} saved session accounts{email_str} in provider API.")
                } else {
                    format!("{accounts} akun sesi tersimpan{email_str} di provider API.")
                };
                panel.set_vault_status(msg.into());
                panel.set_deepseek_connect_count(accounts as i32);
                if !email.is_empty() {
                    panel.set_deepseek_accounts_text(format!("Key 1: {email}").into());
                }
            }
        }

        // Non-blocking startup session auto-sync: populates accounts.json on fresh
        // launch (unless the user explicitly cleared all keys) or silently refreshes
        // a stored token if DeepSeek rejected it as expired.
        let has_shot_flag = args.iter().any(|a| a.starts_with("--shot"));
        if !demo_keys && !ui_test_active() && !has_shot_flag && auto_sync_cookies {
            let core_boot = core.clone();
            let tx_boot = (*diag_tx).clone();
            let allow_empty_sync = !vault_user_cleared;
            std::thread::spawn(move || {
                if let Some(reports) = core_boot.auto_sync_startup_sessions(allow_empty_sync) {
                    let (ok, msg) = format_vault_fetch_all_status(&reports);
                    if ok {
                        let _ = tx_boot.send(DiagMsg::Vault { ok: true, msg });
                    }
                }
            });
        }
        let win = core.current_window();
        panel.set_detected_window(
            win.as_ref()
                .map(|w| w.process_name.clone())
                .unwrap_or_else(|| "-".to_string())
                .into(),
        );
        panel.set_detected_category(
            win.as_ref()
                .map(|w| w.category.to_uppercase())
                .unwrap_or_else(|| "GENERAL".to_string())
                .into(),
        );

        let weak = panel.as_weak();
        let core_tab = core.clone();
        panel.on_tab_selected(move |index| {
            if let Some(p) = weak.upgrade() {
                p.set_active_tab(index);
                if index == 1 && !demo_keys {
                    let _ = refresh_vault_accounts(&core_tab, &p);
                }
            }
        });

        let weak_hud = hud.as_weak();
        let weak_panel = panel.as_weak();
        let weak_bubble = bubble.as_weak();
        panel.on_request_hud(move || {
            switch_panel_to_hud(&weak_panel, &weak_hud, &weak_bubble);
        });

        let weak_bubble = bubble.as_weak();
        let weak_panel = panel.as_weak();
        let weak_hud = hud.as_weak();
        panel.on_request_bubble(move || {
            stop_singing_global(true);
            if let Some(b) = weak_bubble.upgrade() {
                let _ = b.show();
            }
            if let Some(p) = weak_panel.upgrade() {
                p.set_panel_visible(false);
                let _ = p.hide();
            }
            if let Some(h) = weak_hud.upgrade() {
                let _ = h.hide();
            }
        });

        let weak_panel = panel.as_weak();
        let weak_bubble = bubble.as_weak();
        let weak_hud = hud.as_weak();
        panel.on_minimize_requested(move || {
            stop_singing_global(true);
            if let Some(b) = weak_bubble.upgrade() {
                let _ = b.show();
            }
            if let Some(p) = weak_panel.upgrade() {
                p.set_panel_visible(false);
                let _ = p.hide();
            }
            if let Some(h) = weak_hud.upgrade() {
                let _ = h.hide();
            }
        });
        let weak_panel = panel.as_weak();
        panel.on_close_requested(move || {
            if let Some(p) = weak_panel.upgrade() {
                p.set_close_modal_open(true);
            }
        });
        let weak_panel = panel.as_weak();
        let weak_hud = hud.as_weak();
        let weak_bubble = bubble.as_weak();
        panel.on_close_panel_to_hud(move || {
            switch_panel_to_hud(&weak_panel, &weak_hud, &weak_bubble);
        });
        let core_shutdown = core.clone();
        let weak_panel = panel.as_weak();
        let weak_hud = hud.as_weak();
        let weak_bubble = bubble.as_weak();
        panel.on_close_system(move || {
            perform_system_shutdown(&core_shutdown, &weak_panel, &weak_hud, &weak_bubble);
        });
        let weak_panel = panel.as_weak();
        panel.window().on_close_requested(move || {
            if SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst) {
                return slint::CloseRequestResponse::HideWindow;
            }
            if let Some(p) = weak_panel.upgrade() {
                p.set_close_modal_open(true);
            }
            slint::CloseRequestResponse::KeepWindowShown
        });

        panel.on_drag_request(move || {
            if let Some(hwnd) = win32::find_largest_window_by_title("Mini Assistant") {
                win32::begin_window_drag(hwnd);
            }
        });

        let providers_move = providers.clone();
        panel.on_provider_moved(move |index, delta| {
            let from = index as usize;
            let to = (index + delta) as usize;
            let count = providers_move.row_count();
            if from < count && to < count {
                let mut items: Vec<_> = providers_move.iter().collect();
                items.swap(from, to);
                providers_move.set_vec(items);
            }
        });
        let providers_toggle = providers.clone();
        let core_toggle = core.clone();
        let profiles_toggle = profiles.clone();
        let tx_toggle = diag_tx.clone();
        let selected_toggle = selected_browser.clone();
        let panel_weak = panel.as_weak();
        panel.on_provider_toggled(move |index| {
            let idx = index as usize;
            if let Some(mut item) = providers_toggle.row_data(idx) {
                item.enabled = !item.enabled;
                let now_enabled = item.enabled;
                item.order = if item.enabled {
                    format!("#{}", idx + 1).into()
                } else {
                    "OFF".into()
                };
                providers_toggle.set_row_data(idx, item);
                // The HUD's model chip is the live list of providers that are
                // switched on; turning the last one off leaves nothing to send
                // with, which is exactly what the OFF state should mean.
                let enabled_names: Vec<SharedString> = (0..providers_toggle.row_count())
                    .filter_map(|i| {
                        providers_toggle
                            .row_data(i)
                            .filter(|p| p.enabled)
                            .map(|p| p.name)
                    })
                    .collect();
                HUD_HANDLE.with(|w| {
                    if let Some(h) = w.borrow().as_ref().and_then(|x| x.upgrade()) {
                        h.set_models(ModelRc::new(VecModel::from(enabled_names.clone())));
                        if !enabled_names.iter().any(|m| *m == h.get_selected_model()) {
                            h.set_selected_model(
                                enabled_names.first().cloned().unwrap_or_default(),
                            );
                        }
                    }
                });
                let active = (0..providers_toggle.row_count())
                    .filter(|&i| {
                        providers_toggle
                            .row_data(i)
                            .map(|p| p.enabled)
                            .unwrap_or(false)
                    })
                    .count();
                if let Some(panel) = panel_weak.upgrade() {
                    panel.set_provider_count(
                        provider_count_text(
                            active,
                            providers_toggle.row_count(),
                            &panel.get_active_browser(),
                        )
                        .into(),
                    );
                }
                // Enabling a provider means: pull the session out of the
                // browser and hand it to the provider API. That reads Chrome's
                // cookie database (DPAPI) and posts to the API, so it runs off
                // the UI thread and reports back through the diag channel.
                if now_enabled {
                    let pick = (*selected_toggle.borrow()).min(profiles_toggle.len().saturating_sub(1));
                    if let Some(profile) = profiles_toggle.get(pick).cloned() {
                        if let Some(mut card) = providers_toggle.row_data(idx) {
                            card.status = app_core::tr("Menghubungkan…", "Connecting…").into();
                            card.tone = "amber".into();
                            providers_toggle.set_row_data(idx, card);
                        }
                        let core_worker = core_toggle.clone();
                        let tx = (*tx_toggle).clone();
                        std::thread::spawn(move || {
                            let report = core_worker.connect_deepseek(&profile);
                            let _ = tx.send(DiagMsg::Connect {
                                idx: idx as i32,
                                ok: report.success,
                                msg: format!("{} ({})", report.message, report.status),
                            });
                        });
                    }
                }
            }
        });
        let selected_browser_cb = selected_browser.clone();
        let profiles_sel = profiles.clone();
        let logos_sel = browser_logos.clone();
        let panel_sel = panel.as_weak();
        let core_sel = core.clone();
        panel.on_browser_selected(move |index| {
            let idx = index.max(0) as usize;
            let chosen = if let Some(p) = panel_sel.upgrade() {
                apply_active_browser(idx, &profiles_sel, &logos_sel, &p, Some(&core_sel))
            } else {
                idx
            };
            *selected_browser_cb.borrow_mut() = chosen;
        });
        // ---- session history -------------------------------------------------
        let filter = Rc::new(RefCell::new(SessionFilter {
            cat: String::new(),
            query: String::new(),
        }));
        let apply_sessions = {
            let core = core.clone();
            let panel = panel.as_weak();
            let hud = hud.as_weak();
            let filter = filter.clone();
            Rc::new(move || {
                if let (Some(p), Some(h)) = (panel.upgrade(), hud.upgrade()) {
                    refresh_sessions(&core, &p, &h, &filter.borrow());
                }
            })
        };

        SESSIONS_REFRESH.with(|w| *w.borrow_mut() = Some(apply_sessions.clone() as Rc<dyn Fn()>));

        let core_ptopic = core.clone();
        let messages_ptopic = chat_messages.clone();
        let turns_ptopic = turns.clone();
        let hud_ptopic = hud.as_weak();
        let panel_ptopic = panel.as_weak();
        let bubble_ptopic = bubble.as_weak();
        let apply_ptopic = apply_sessions.clone();
        panel.on_topic_selected(move |name| {
            let topic = name.to_string();
            // Same loader the HUD dropdown uses: bubbles *and* turn history, so
            // a follow-up message continues this conversation instead of
            // starting a blank one.
            let _n = load_topic(&core_ptopic, &topic, &messages_ptopic, &turns_ptopic);
            dlog!("[topic] lanjut sesi '{topic}' ({_n} pesan)");
            if let Some(h) = hud_ptopic.upgrade() {
                h.set_current_topic(topic.into());
                let _ = h.show();
            }
            if let Some(p) = panel_ptopic.upgrade() {
                p.set_panel_visible(false);
                let _ = p.hide();
            }
            if let Some(b) = bubble_ptopic.upgrade() {
                let _ = b.hide();
            }
            sync_chat_height(&messages_ptopic);
            if let Some(h) = hud_ptopic.upgrade() {
                scroll_chat_to_bottom_deferred(&h);
            }
            apply_ptopic();
        });

        let core_del = core.clone();
        let messages_del = chat_messages.clone();
        let turns_del = turns.clone();
        let hud_del = hud.as_weak();
        let apply_del = apply_sessions.clone();
        panel.on_topic_delete(move |name| {
            let topic = name.to_string();
            let _ = core_del.delete_topic(&topic);
            if let Some(h) = hud_del.upgrade() {
                if h.get_current_topic() == name {
                    h.set_current_topic(
                        app_core::tr("Percakapan Baru", "New Conversation").into(),
                    );
                    messages_del.set_vec(Vec::new());
                    turns_del.borrow_mut().clear();
                    sync_chat_height(&messages_del);
                }
            }
            apply_del();
        });

        let core_new = core.clone();
        let messages_new = chat_messages.clone();
        let hud_new = hud.as_weak();
        let panel_new = panel.as_weak();
        let bubble_new = bubble.as_weak();
        let apply_new = apply_sessions.clone();
        panel.on_topic_new(move || {
            if let Some(h) = hud_new.upgrade() {
                let _ = start_new_session(&core_new, &h, &messages_new);
                let _ = h.show();
            }
            if let Some(p) = panel_new.upgrade() {
                p.set_panel_visible(false);
                let _ = p.hide();
            }
            if let Some(b) = bubble_new.upgrade() {
                let _ = b.hide();
            }
            apply_new();
        });

        let apply_filter = apply_sessions.clone();
        let filter_state = filter.clone();
        panel.on_topic_filter(move |cat| {
            filter_state.borrow_mut().cat = cat.to_string();
            apply_filter();
        });

        let apply_search = apply_sessions.clone();
        let query_state = filter.clone();
        panel.on_topic_search(move |text| {
            query_state.borrow_mut().query = text.to_string();
            apply_search();
        });

        // ---- diagnostics -----------------------------------------------------
        let panel_diag = panel.as_weak();
        let core_diag = core.clone();
        let tx_diag = diag_tx.clone();
        panel.on_diag_test(move |provider| {
            let Some(p) = panel_diag.upgrade() else { return };
            if p.get_diag_busy() {
                return;
            }
            let label = provider.to_string();
            let model = core_diag
                .providers()
                .unwrap_or_default()
                .into_iter()
                .find(|o| o.label == label)
                .map(|o| o.default_model)
                .unwrap_or_else(|| "deepseek-chat".to_string());
            p.set_diag_busy(true);
            p.set_diag_ok(false);
            p.set_diag_status(
                if app_core::is_en() {
                    format!("Testing {label} ({model})...")
                } else {
                    format!("Menguji {label} ({model})...")
                }
                .into(),
            );
            let core_t = core_diag.clone();
            let tx = (*tx_diag).clone();
            std::thread::spawn(move || {
                match core_t.test_completion(&model) {
                    Ok((text, ms)) => {
                        let head: String = text.trim().chars().take(40).collect();
                        let _ = tx.send(DiagMsg::Result { label, ms, head });
                    }
                    Err(e) => {
                        let err: String = e.chars().take(120).collect();
                        let _ = tx.send(DiagMsg::Fail { label, err });
                    }
                }
                let _ = tx.send(DiagMsg::Done);
            });
        });

        let panel_dall = panel.as_weak();
        let core_dall = core.clone();
        let tx_dall = diag_tx.clone();
        panel.on_diag_test_all(move || {
            let Some(p) = panel_dall.upgrade() else { return };
            if p.get_diag_busy() {
                return;
            }
            let list = core_dall.providers().unwrap_or_default();
            if list.is_empty() {
                p.set_diag_ok(false);
                p.set_diag_status(
                    app_core::tr(
                        "Tidak ada provider aktif di API.",
                        "No active provider in API.",
                    )
                    .into(),
                );
                return;
            }
            p.set_diag_busy(true);
            p.set_diag_ok(false);
            p.set_diag_status(
                if app_core::is_en() {
                    format!("Testing {} providers...", list.len())
                } else {
                    format!("Menguji {} provider...", list.len())
                }
                .into(),
            );
            let core_t = core_dall.clone();
            let tx = (*tx_dall).clone();
            std::thread::spawn(move || {
                for opt in list {
                    match core_t.test_completion(&opt.default_model) {
                        Ok((text, ms)) => {
                            let head: String = text.trim().chars().take(40).collect();
                            let _ = tx.send(DiagMsg::Result { label: opt.label, ms, head });
                        }
                        Err(e) => {
                            let err: String = e.chars().take(120).collect();
                            let _ = tx.send(DiagMsg::Fail { label: opt.label, err });
                        }
                    }
                }
                let _ = tx.send(DiagMsg::Done);
            });
        });

        let panel_dext = panel.as_weak();
        let core_dext = core.clone();
        let profiles_dext = profiles.clone();
        let selected_dext = selected_browser.clone();
        let tx_dext = diag_tx.clone();
        let key_res_dext = diag_key_results.clone();
        panel.on_diag_extract(move || {
            let Some(p) = panel_dext.upgrade() else { return };
            if p.get_diag_busy() {
                return;
            }
            let pick = (*selected_dext.borrow()).min(profiles_dext.len().saturating_sub(1));
            let Some(profile) = profiles_dext.get(pick).cloned() else {
                p.set_diag_ok(false);
                p.set_diag_status(
                    app_core::tr(
                        "Tidak ada profil browser yang terdeteksi.",
                        "No browser profiles detected.",
                    )
                    .into(),
                );
                return;
            };
            key_res_dext.borrow_mut().clear();
            p.set_diag_busy(true);
            p.set_diag_ok(false);
            p.set_diag_status(
                if app_core::is_en() {
                    format!("Extracting cookies from {}...", profile.browser_name)
                } else {
                    format!("Mengekstrak cookie dari {}...", profile.browser_name)
                }
                .into(),
            );
            let core_t = core_dext.clone();
            let tx = (*tx_dext).clone();
            std::thread::spawn(move || {
                let rep = core_t.connect_deepseek(&profile);
                let _ = tx.send(DiagMsg::Extract {
                    ok: rep.success,
                    msg: if app_core::is_en() {
                        format!(
                            "{} · {} cookies read · {} v20-locked · {}-char token",
                            rep.status, rep.cookies_found, rep.locked_v20, rep.token_chars
                        )
                    } else {
                        format!(
                            "{} · {} cookie terbaca · {} terkunci v20 · token {} karakter",
                            rep.status, rep.cookies_found, rep.locked_v20, rep.token_chars
                        )
                    },
                });
                let _ = tx.send(DiagMsg::Done);
            });
        });

        let panel_dkey = panel.as_weak();
        let core_dkey = core.clone();
        let tx_dkey = diag_tx.clone();
        let key_res_dkey = diag_key_results.clone();
        panel.on_diag_test_key(move |id| {
            let Some(p) = panel_dkey.upgrade() else { return };
            if p.get_diag_busy() {
                return;
            }
            let id_s = id.to_string();
            if ui_test_active() {
                key_res_dkey
                    .borrow_mut()
                    .insert(id_s.clone(), ("OK \u{00b7} 123 ms".into(), "green".into()));
                apply_key_diag_overlay(&p, &key_res_dkey.borrow());
                p.set_diag_ok(true);
                p.set_diag_latency("123 ms".into());
                p.set_diag_status(format!("Key ({id_s}): OK \u{00b7} 123 ms").into());
                return;
            }
            let entries = core_dkey.deepseek_accounts();
            let Some((idx, acc)) = entries.into_iter().enumerate().find(|(_, a)| a.name == id_s) else {
                p.set_diag_ok(false);
                p.set_diag_status(
                    if app_core::is_en() {
                        format!("Key '{id_s}' not found in pool.")
                    } else {
                        format!("Key '{id_s}' tidak ditemukan di pool.")
                    }
                    .into(),
                );
                return;
            };
            let label = key_probe_label(idx, &acc);
            p.set_diag_busy(true);
            p.set_diag_ok(false);
            p.set_diag_status(
                if app_core::is_en() {
                    format!("Testing {label}...")
                } else {
                    format!("Menguji {label}...")
                }
                .into(),
            );
            {
                let mut map = key_res_dkey.borrow_mut();
                map.insert(
                    id_s.clone(),
                    (app_core::tr("Menguji...", "Testing...").into(), "amber".into()),
                );
                apply_key_diag_overlay(&p, &map);
            }
            let core_t = core_dkey.clone();
            let tx = (*tx_dkey).clone();
            std::thread::spawn(move || {
                let (_, _, msg) = probe_single_key(&core_t, idx, &acc);
                let _ = tx.send(msg);
                let _ = tx.send(DiagMsg::Done);
            });
        });

        let panel_dallk = panel.as_weak();
        let core_dallk = core.clone();
        let tx_dallk = diag_tx.clone();
        let key_res_dallk = diag_key_results.clone();
        panel.on_diag_test_all_keys(move || {
            let Some(p) = panel_dallk.upgrade() else { return };
            if p.get_diag_busy() {
                return;
            }
            if ui_test_active() {
                let rows = p.get_deepseek_keys();
                let mut map = key_res_dallk.borrow_mut();
                for i in 0..rows.row_count() {
                    if let Some(k) = rows.row_data(i) {
                        map.insert(k.id.to_string(), ("OK \u{00b7} 210 ms".into(), "green".into()));
                    }
                }
                apply_key_diag_overlay(&p, &map);
                p.set_diag_ok(true);
                p.set_diag_latency("210 ms".into());
                p.set_diag_status(
                    if app_core::is_en() {
                        format!("Tested {0}/{0} DeepSeek keys.", rows.row_count())
                    } else {
                        format!("Selesai uji {0}/{0} key DeepSeek.", rows.row_count())
                    }
                    .into(),
                );
                return;
            }
            let entries = core_dallk.deepseek_accounts();
            if entries.is_empty() {
                p.set_diag_ok(false);
                p.set_diag_status(
                    app_core::tr(
                        "Tidak ada key DeepSeek tersimpan untuk diuji.",
                        "No saved DeepSeek keys to test.",
                    )
                    .into(),
                );
                return;
            }
            let total = entries.len();
            p.set_diag_busy(true);
            p.set_diag_ok(false);
            p.set_diag_status(
                if app_core::is_en() {
                    format!("Testing {total} DeepSeek keys...")
                } else {
                    format!("Menguji {total} key DeepSeek...")
                }
                .into(),
            );
            {
                let mut map = key_res_dallk.borrow_mut();
                map.clear();
                for acc in &entries {
                    map.insert(
                        acc.name.clone(),
                        (app_core::tr("Menunggu...", "Waiting...").into(), "amber".into()),
                    );
                }
                apply_key_diag_overlay(&p, &map);
            }
            let core_t = core_dallk.clone();
            let tx = (*tx_dallk).clone();
            std::thread::spawn(move || {
                let mut ok_count = 0usize;
                let mut best_ms: Option<u64> = None;
                let mut first_err: Option<String> = None;
                for (idx, acc) in entries.iter().enumerate() {
                    let _ = tx.send(DiagMsg::KeyTesting { id: acc.name.clone() });
                    let (ok, ms, msg) = probe_single_key(&core_t, idx, acc);
                    if ok {
                        ok_count += 1;
                        if let Some(m) = ms {
                            best_ms = Some(best_ms.map(|prev| prev.min(m)).unwrap_or(m));
                        }
                    } else if first_err.is_none() {
                        if let DiagMsg::KeyResult { ref label, ref detail, .. } = msg {
                            first_err = Some(format!("{label}: {detail}"));
                        }
                    }
                    let _ = tx.send(msg);
                }
                let summary_msg = if ok_count == total {
                    match best_ms {
                        Some(m) => {
                            if app_core::is_en() {
                                format!("Tested {ok_count}/{total} DeepSeek keys: all ready (fastest {m} ms).")
                            } else {
                                format!("Selesai uji {ok_count}/{total} key DeepSeek: semua siap (tercepat {m} ms).")
                            }
                        }
                        None => {
                            if app_core::is_en() {
                                format!("Tested {ok_count}/{total} DeepSeek keys: all ready.")
                            } else {
                                format!("Selesai uji {ok_count}/{total} key DeepSeek: semua siap.")
                            }
                        }
                    }
                } else if let Some(err) = first_err {
                    if app_core::is_en() {
                        format!("{ok_count}/{total} keys ready \u{00b7} {err}")
                    } else {
                        format!("{ok_count}/{total} key siap \u{00b7} {err}")
                    }
                } else if app_core::is_en() {
                    format!("{ok_count}/{total} DeepSeek keys ready.")
                } else {
                    format!("{ok_count}/{total} key DeepSeek siap.")
                };
                let _ = tx.send(DiagMsg::KeySummary {
                    ok_count,
                    total,
                    best_ms,
                    msg: summary_msg,
                });
                let _ = tx.send(DiagMsg::Done);
            });
        });

        // ---- settings page ---------------------------------------------------
        let core_set = core.clone();
        let hud_set = hud.as_weak();
        let panel_set = panel.as_weak();
        let sing_set = sing_ctrl.clone();
        let bubble_set = bubble.as_weak();
        let sprites_set = bubble_sprites.clone();
        let bscroll_set = bubble_scroll.clone();
        panel.on_setting_toggled(move |key, value| {
            let k = key.to_string();
            if let Some(h) = hud_set.upgrade() {
                match k.as_str() {
                    "pin" => h.set_pinned(value),
                    "auto-capture" => h.set_auto_capture(value),
                    "capture-app" => h.set_capture_app(value),
                    "target-window" => h.set_target_window_only(value),
                    "enter-sends" => h.set_enter_sends(value),
                    "send-close" => h.set_send_closes(value),
                    _ => {}
                }
            }
            if let Some(p) = panel_set.upgrade() {
                match k.as_str() {
                    "pin" => p.set_set_pin(value),
                    "auto-capture" => p.set_set_auto_capture(value),
                    "capture-app" => p.set_set_capture_app(value),
                    "target-window" => p.set_set_target_window(value),
                    "enter-sends" => p.set_set_enter_sends(value),
                    "send-close" => p.set_set_send_close(value),
                    "random-singing" => p.set_set_random_singing(value),
                    "sing-ram-guard" => p.set_set_sing_ram_guard(value),
                    "bubble-auto-scroll" => p.set_set_bubble_auto_scroll(value),
                    "auto-sync-cookies" => p.set_set_auto_sync_cookies(value),
                    _ => {}
                }
            }
            match k.as_str() {
                "random-singing" => {
                    sing_set.borrow_mut().enabled = value;
                    if !value {
                        let is_preview = sing_set.borrow().preview_mode;
                        if !is_preview {
                            stop_singing_perf(&sing_set, &bubble_set, &panel_set, &sprites_set, false);
                        }
                    } else {
                        sing_set.borrow_mut().schedule_next(true);
                    }
                }
                "sing-ram-guard" => {
                    sing_set.borrow_mut().ram_guard = value;
                    if is_ram_over_guard(value, win32::memory_load_percent(), SING_RAM_LIMIT_PCT) {
                        stop_singing_perf(&sing_set, &bubble_set, &panel_set, &sprites_set, true);
                    }
                }
                "bubble-auto-scroll" => {
                    bscroll_set.borrow_mut().set_enabled(value);
                }
                _ => {}
            }
            if !ui_test_active() {
                let _ = core_set.save_setting(&k, if value { "1" } else { "0" });
            }
        });

        let core_vol = core.clone();
        let panel_vol = panel.as_weak();
        let sing_vol = sing_ctrl.clone();
        panel.on_sing_volume_picked(move |pct| {
            let clamped = pct.clamp(0, 100);
            if let Some(p) = panel_vol.upgrade() {
                p.set_sing_volume(clamped);
            }
            sing_vol.borrow_mut().set_volume(clamped as u8);
            if !ui_test_active() {
                let _ = core_vol.save_setting("sing-volume", &clamped.to_string());
            }
        });

        let core_spd = core.clone();
        let panel_spd = panel.as_weak();
        let bscroll_spd = bubble_scroll.clone();
        panel.on_bubble_read_speed_selected(move |code| {
            let spd = bubble_scroll::BubbleReadSpeed::from_code(code.as_str());
            bscroll_spd.borrow_mut().set_speed(spd);
            if !ui_test_active() {
                let _ = core_spd.save_setting("bubble-read-speed", spd.code());
            }
            if let Some(p) = panel_spd.upgrade() {
                p.set_bubble_read_speed(spd.code().into());
                let label = match spd {
                    bubble_scroll::BubbleReadSpeed::Slow => {
                        app_core::tr("Santai (2,2 kata/dtk)", "Slow (2.2 wps)")
                    }
                    bubble_scroll::BubbleReadSpeed::Normal => {
                        app_core::tr("Normal (2,8 kata/dtk)", "Normal (2.8 wps)")
                    }
                    bubble_scroll::BubbleReadSpeed::Fast => {
                        app_core::tr("Cepat (3,6 kata/dtk)", "Fast (3.6 wps)")
                    }
                };
                p.set_status_text(
                    if app_core::is_en() {
                        format!("Bubble reading speed: {label}")
                    } else {
                        format!("Kecepatan baca bubble: {label}")
                    }
                    .into(),
                );
                p.set_status_visible(true);
                let weak = p.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_secs(3), move || {
                    if let Some(p) = weak.upgrade() {
                        p.set_status_visible(false);
                    }
                });
            }
        });

        let sing_test = sing_ctrl.clone();
        let bubble_test = bubble.as_weak();
        let panel_test = panel.as_weak();
        let hud_test = hud.as_weak();
        let sprites_test = bubble_sprites.clone();
        let rx_test = stream_rx.clone();
        let trx_test = trans_stream_rx.clone();
        panel.on_sing_test_toggled(move || {
            let already = sing_test.borrow().performing;
            if already {
                stop_singing_perf(&sing_test, &bubble_test, &panel_test, &sprites_test, true);
            } else {
                let _ = start_singing_perf(
                    &sing_test,
                    &bubble_test,
                    &panel_test,
                    &hud_test,
                    &sprites_test,
                    &rx_test,
                    &trx_test,
                    true,
                );
            }
        });

        let core_clear = core.clone();
        let messages_clear = chat_messages.clone();
        let hud_clear = hud.as_weak();
        let apply_clear = apply_sessions.clone();
        panel.on_clear_history(move || {
            let _ = core_clear.clear_history();
            messages_clear.set_vec(Vec::new());
            sync_chat_height(&messages_clear);
            if let Some(h) = hud_clear.upgrade() {
                h.set_current_topic(
                    app_core::tr("Percakapan Baru", "New Conversation").into(),
                );
            }
            apply_clear();
        });

        let panel_pick = panel.as_weak();
        panel.on_hotkey_picked(move |kind, value| {
            if let Some(p) = panel_pick.upgrade() {
                if kind == "mod" {
                    p.set_hk_mod(value);
                } else {
                    p.set_hk_key(value);
                }
            }
        });

        let core_apply = core.clone();
        let panel_apply = panel.as_weak();
        panel.on_hotkey_apply(move || {
            let Some(p) = panel_apply.upgrade() else { return };
            let m = p.get_hk_mod().to_string();
            let k = p.get_hk_key().to_string();
            if let Err(e) = app_core::update_global_hotkey(&m, &k) {
                p.set_status_text(
                    if app_core::is_en() {
                        format!("Failed to save shortcut: {e}")
                    } else {
                        format!("Gagal menyimpan pintasan: {e}")
                    }
                    .into(),
                );
                p.set_status_visible(true);
                return;
            }
            let _ = core_apply.save_setting("hotkey-mod", &m);
            let _ = core_apply.save_setting("hotkey-key", &k);
            p.set_hk_active_mod(m.clone().into());
            p.set_hk_active_key(k.clone().into());
            p.set_status_text(
                if app_core::is_en() {
                    format!("Shortcut {m}+{k} active.")
                } else {
                    format!("Pintasan {m}+{k} aktif.")
                }
                .into(),
            );
            p.set_status_visible(true);
            let weak = p.as_weak();
            slint::Timer::single_shot(std::time::Duration::from_secs(3), move || {
                if let Some(p) = weak.upgrade() {
                    p.set_status_visible(false);
                }
            });
        });

        let core_reset = core.clone();
        let panel_reset = panel.as_weak();
        panel.on_hotkey_reset(move || {
            let Some(p) = panel_reset.upgrade() else { return };
            if let Err(e) = app_core::update_global_hotkey("Alt", "Q") {
                // Another app may own Alt+Q; say so instead of showing a
                // shortcut that is not actually registered.
                p.set_status_text(
                    if app_core::is_en() {
                        format!("Failed to register Alt+Q: {e}")
                    } else {
                        format!("Gagal memasang Alt+Q: {e}")
                    }
                    .into(),
                );
                p.set_status_visible(true);
                return;
            }
            let _ = core_reset.save_setting("hotkey-mod", "Alt");
            let _ = core_reset.save_setting("hotkey-key", "Q");
            p.set_hk_mod("Alt".into());
            p.set_hk_key("Q".into());
            p.set_hk_active_mod("Alt".into());
            p.set_hk_active_key("Q".into());
        });

        let panel_tpick = panel.as_weak();
        panel.on_trans_hotkey_picked(move |kind, value| {
            if let Some(p) = panel_tpick.upgrade() {
                if kind == "mod" {
                    p.set_trans_hk_mod(value);
                } else {
                    p.set_trans_hk_key(value);
                }
            }
        });

        let core_tapply = core.clone();
        let panel_tapply = panel.as_weak();
        panel.on_trans_hotkey_apply(move || {
            let Some(p) = panel_tapply.upgrade() else { return };
            let m = p.get_trans_hk_mod().to_string();
            let k = p.get_trans_hk_key().to_string();
            if let Err(e) = app_core::update_translate_hotkey(&m, &k) {
                p.set_status_text(
                    if app_core::is_en() {
                        format!("Failed to save translate shortcut: {e}")
                    } else {
                        format!("Gagal menyimpan pintasan translate: {e}")
                    }
                    .into(),
                );
                p.set_status_visible(true);
                return;
            }
            let _ = core_tapply.save_setting("translate-hk-mod", &m);
            let _ = core_tapply.save_setting("translate-hk-key", &k);
            p.set_trans_hk_active_mod(m.clone().into());
            p.set_trans_hk_active_key(k.clone().into());
            p.set_status_text(
                if app_core::is_en() {
                    format!("Translate shortcut {m}+{k} active.")
                } else {
                    format!("Pintasan Translate {m}+{k} aktif.")
                }
                .into(),
            );
            p.set_status_visible(true);
            let weak = p.as_weak();
            slint::Timer::single_shot(std::time::Duration::from_secs(3), move || {
                if let Some(p) = weak.upgrade() {
                    p.set_status_visible(false);
                }
            });
        });

        let core_treset = core.clone();
        let panel_treset = panel.as_weak();
        panel.on_trans_hotkey_reset(move || {
            let Some(p) = panel_treset.upgrade() else { return };
            if let Err(e) = app_core::update_translate_hotkey("Alt", "T") {
                p.set_status_text(
                    if app_core::is_en() {
                        format!("Failed to register Alt+T: {e}")
                    } else {
                        format!("Gagal memasang Alt+T: {e}")
                    }
                    .into(),
                );
                p.set_status_visible(true);
                return;
            }
            let _ = core_treset.save_setting("translate-hk-mod", "Alt");
            let _ = core_treset.save_setting("translate-hk-key", "T");
            p.set_trans_hk_mod("Alt".into());
            p.set_trans_hk_key("T".into());
            p.set_trans_hk_active_mod("Alt".into());
            p.set_trans_hk_active_key("T".into());
        });

        let core_lang = core.clone();
        let panel_lang = panel.as_weak();
        let bubble_lang = bubble.as_weak();
        let lang_state_cb = trans_lang_state.clone();
        panel.on_translate_lang_selected(move |lang| {
            let s = lang.to_string();
            *lang_state_cb.borrow_mut() = s.clone();
            let _ = core_lang.save_setting("translate-lang", &s);
            if let Some(p) = panel_lang.upgrade() {
                p.set_translate_lang(s.clone().into());
                p.set_status_text(
                    if app_core::is_en() {
                        format!("Screen translate language: {s}")
                    } else {
                        format!("Bahasa terjemahan layar: {s}")
                    }
                    .into(),
                );
                p.set_status_visible(true);
                let weak = p.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_secs(3), move || {
                    if let Some(p) = weak.upgrade() {
                        p.set_status_visible(false);
                    }
                });
            }
            if let Some(b) = bubble_lang.upgrade() {
                b.set_translate_lang(s.into());
            }
        });

        let core_tprompt = core.clone();
        let panel_tprompt = panel.as_weak();
        panel.on_translate_prompt_saved(move |prompt| {
            let trimmed = prompt.trim().to_string();
            let effective = if trimmed.is_empty() {
                DEFAULT_TRANSLATE_PROMPT.to_string()
            } else {
                trimmed
            };
            let _ = core_tprompt.save_setting("translate-prompt", &effective);
            if let Some(p) = panel_tprompt.upgrade() {
                p.set_translate_prompt(effective.into());
                p.set_status_text(
                    app_core::tr(
                        "Prompt terjemahan disimpan.",
                        "Translation prompt saved.",
                    )
                    .into(),
                );
                p.set_status_visible(true);
                let weak = p.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_secs(3), move || {
                    if let Some(p) = weak.upgrade() {
                        p.set_status_visible(false);
                    }
                });
            }
        });

        let core_applang = core.clone();
        let panel_applang = panel.as_weak();
        let hud_applang = hud.as_weak();
        let bubble_applang = bubble.as_weak();
        let profiles_applang = profiles.clone();
        let logos_applang = browser_logos.clone();
        let selected_applang = selected_browser.clone();
        let key_res_applang = diag_key_results.clone();
        let apply_sessions_applang = apply_sessions.clone();
        panel.on_app_lang_selected(move |code| {
            let lang = app_core::AppLang::from_code(code.as_str());
            if !ui_test_active() {
                let _ = core_applang.save_setting("app-lang", lang.code());
            }
            if let (Some(p), Some(h), Some(b)) = (
                panel_applang.upgrade(),
                hud_applang.upgrade(),
                bubble_applang.upgrade(),
            ) {
                apply_app_language(lang, &p, &h, &b);
                apply_active_browser(
                    *selected_applang.borrow(),
                    &profiles_applang,
                    &logos_applang,
                    &p,
                    None,
                );
                if demo_keys {
                    let demo = demo_vault_keys();
                    p.set_deepseek_keys(ModelRc::new(VecModel::from(demo)));
                    p.set_vault_status(
                        app_core::tr_for(
                            lang,
                            "2 akun sesi aktif (Key 1 Chrome · Key 2 Edge fallback siap).",
                            "2 active session accounts (Key 1 Chrome · Key 2 Edge fallback ready).",
                        )
                        .into(),
                    );
                } else {
                    let entries = refresh_vault_accounts_with_diag(
                        &core_applang,
                        &p,
                        &key_res_applang.borrow(),
                    );
                    if !p.get_vault_busy() && p.get_vault_ok() && !entries.is_empty() {
                        let emails: Vec<&str> = entries
                            .iter()
                            .take(2)
                            .map(|e| e.email.as_str())
                            .filter(|s| !s.is_empty())
                            .collect();
                        let suffix = if emails.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", emails.join(", "))
                        };
                        let cur_v = p.get_vault_status().to_string();
                        if cur_v.contains("akun sesi tersimpan")
                            || cur_v.contains("saved session accounts")
                        {
                            let msg = if lang.is_en() {
                                format!(
                                    "{} saved session accounts{} in provider API.",
                                    entries.len(),
                                    suffix
                                )
                            } else {
                                format!(
                                    "{} akun sesi tersimpan{} di provider API.",
                                    entries.len(),
                                    suffix
                                )
                            };
                            p.set_vault_status(msg.into());
                        }
                    }
                }
                apply_sessions_applang();
                p.set_status_text(
                    app_core::tr_for(
                        lang,
                        "Bahasa aplikasi: Bahasa Indonesia",
                        "App language: English",
                    )
                    .into(),
                );
                p.set_status_visible(true);
                let weak = p.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_secs(3), move || {
                    if let Some(p) = weak.upgrade() {
                        p.set_status_visible(false);
                    }
                });
            }
        });

        let core_fsize = core.clone();
        let panel_fsize = panel.as_weak();
        let hud_fsize = hud.as_weak();
        let bubble_fsize = bubble.as_weak();
        panel.on_font_size_picked(move |px| {
            let _ = core_fsize.save_setting("font-size", &px.to_string());
            if let (Some(p), Some(h), Some(b)) =
                (panel_fsize.upgrade(), hud_fsize.upgrade(), bubble_fsize.upgrade())
            {
                let family = p.get_font_family_sel().to_string();
                apply_chat_font(&h, &b, &p, px, &family);
            }
        });

        let core_ffam = core.clone();
        let panel_ffam = panel.as_weak();
        let hud_ffam = hud.as_weak();
        let bubble_ffam = bubble.as_weak();
        panel.on_font_family_picked(move |name| {
            let _ = core_ffam.save_setting("font-family", name.as_str());
            if let (Some(p), Some(h), Some(b)) =
                (panel_ffam.upgrade(), hud_ffam.upgrade(), bubble_ffam.upgrade())
            {
                let px = p.get_font_size_px();
                apply_chat_font(&h, &b, &p, px, name.as_str());
            }
        });

        let core_order = core.clone();
        let panel_order = panel.as_weak();
        panel.on_settings_saved(move || {
            let Some(p) = panel_order.upgrade() else { return };
            let model = p.get_providers();
            let ids: Vec<String> = (0..model.row_count())
                .filter_map(|i| model.row_data(i).map(|r| r.id.to_string()))
                .collect();
            let _ = core_order.save_setting("provider-order", &ids.join(","));
            p.set_status_text(
                app_core::tr("Urutan provider disimpan.", "Provider order saved.").into(),
            );
            p.set_status_visible(true);
            let weak = p.as_weak();
            slint::Timer::single_shot(std::time::Duration::from_secs(3), move || {
                if let Some(p) = weak.upgrade() {
                    p.set_status_visible(false);
                }
            });
        });

        panel.on_open_external(|url| {
            if !url.is_empty() {
                win32::open_url(&url);
            }
        });

        // ---- cookies vault --------------------------------------------------
        // Fetch: run the extractor for the selected browser profile and hand the
        // result to the provider API. Save: the manual paste path.
        let core_vault = core.clone();
        let profiles_vault = profiles.clone();
        let selected_vault = selected_browser.clone();
        let panel_vault = panel.as_weak();
        let tx_vault = diag_tx.clone();
        let key_res_vf = diag_key_results.clone();
        panel.on_vault_fetch(move || {
            let pick = (*selected_vault.borrow()).min(profiles_vault.len().saturating_sub(1));
            let Some(profile) = profiles_vault.get(pick).cloned() else {
                return;
            };
            key_res_vf.borrow_mut().clear();
            if let Some(p) = panel_vault.upgrade() {
                p.set_vault_busy(true);
                p.set_vault_status(
                    app_core::tr(
                        "Mengambil cookie dari browser…",
                        "Extracting cookies from browser…",
                    )
                    .into(),
                );
            }
            // Same off-thread path as the diagnostics probe: Chrome's cookie DB
            // and DPAPI take hundreds of ms and must not freeze the panel.
            let core_worker = core_vault.clone();
            let tx = (*tx_vault).clone();
            let is_ui_test = ui_test_active();
            std::thread::spawn(move || {
                let report = core_worker.connect_deepseek(&profile);
                if report.success && !is_ui_test {
                    let _ = core_worker.save_setting("vault-user-cleared", "0");
                }
                dlog!("[vault] fetch: {} ({})", report.status, report.message);
                let _ = tx.send(DiagMsg::Vault {
                    ok: report.success,
                    msg: format_vault_fetch_status(&report),
                });
            });
        });

        let core_vall = core.clone();
        let panel_vall = panel.as_weak();
        let tx_vall = diag_tx.clone();
        let key_res_va = diag_key_results.clone();
        panel.on_vault_fetch_all(move || {
            key_res_va.borrow_mut().clear();
            if let Some(p) = panel_vall.upgrade() {
                p.set_vault_busy(true);
                p.set_vault_status(
                    app_core::tr(
                        "Mengambil sesi dari semua browser (Chrome & Edge)…",
                        "Extracting sessions from all browsers (Chrome & Edge)…",
                    )
                    .into(),
                );
            }
            let core_worker = core_vall.clone();
            let tx = (*tx_vall).clone();
            let is_ui_test = ui_test_active();
            std::thread::spawn(move || {
                let reports = core_worker.connect_deepseek_all_browsers();
                let (ok, msg) = format_vault_fetch_all_status(&reports);
                if ok && !is_ui_test {
                    let _ = core_worker.save_setting("vault-user-cleared", "0");
                }
                let _ = tx.send(DiagMsg::Vault { ok, msg });
            });
        });

        let core_prom = core.clone();
        let panel_prom = panel.as_weak();
        let tx_prom = diag_tx.clone();
        panel.on_vault_promote_key(move |id| {
            let id_s = id.to_string();
            if id_s.trim().is_empty() {
                return;
            }
            if let Some(p) = panel_prom.upgrade() {
                p.set_vault_status(
                    app_core::tr(
                        "Memindahkan prioritas key utama…",
                        "Updating primary key priority…",
                    )
                    .into(),
                );
            }
            let core_worker = core_prom.clone();
            let tx = (*tx_prom).clone();
            std::thread::spawn(move || {
                let res = core_worker.set_primary_account(&id_s);
                let _ = tx.send(DiagMsg::Vault {
                    ok: res.is_ok(),
                    msg: match res {
                        Ok(()) => app_core::tr(
                            "Prioritas diperbarui: akun terpilih menjadi Key 1 (Utama).",
                            "Priority updated: selected account is now Key 1 (Primary).",
                        )
                        .into(),
                        Err(e) => {
                            if app_core::is_en() {
                                format!("Failed: {e}")
                            } else {
                                format!("Gagal: {e}")
                            }
                        }
                    },
                });
            });
        });

        let core_rem = core.clone();
        let panel_rem = panel.as_weak();
        let tx_rem = diag_tx.clone();
        let key_res_rem = diag_key_results.clone();
        panel.on_vault_remove_key(move |id| {
            let id_s = id.to_string();
            if id_s.trim().is_empty() {
                return;
            }
            key_res_rem.borrow_mut().remove(&id_s);
            if let Some(p) = panel_rem.upgrade() {
                p.set_vault_status(
                    app_core::tr(
                        "Menghapus sesi key dari pool…",
                        "Removing key session from pool…",
                    )
                    .into(),
                );
            }
            let core_worker = core_rem.clone();
            let tx = (*tx_rem).clone();
            let is_ui_test = ui_test_active();
            std::thread::spawn(move || {
                let res = core_worker.remove_account(&id_s);
                if res.is_ok() && !is_ui_test && core_worker.deepseek_accounts().is_empty() {
                    let _ = core_worker.save_setting("vault-user-cleared", "1");
                }
                let _ = tx.send(DiagMsg::Vault {
                    ok: res.is_ok(),
                    msg: match res {
                        Ok(()) => app_core::tr(
                            "Key dihapus dari pool akun aktif.",
                            "Key removed from active account pool.",
                        )
                        .into(),
                        Err(e) => {
                            if app_core::is_en() {
                                format!("Failed: {e}")
                            } else {
                                format!("Gagal: {e}")
                            }
                        }
                    },
                });
            });
        });

        let core_vsave = core.clone();
        let panel_vsave = panel.as_weak();
        let tx_vsave = diag_tx.clone();
        let key_res_vs = diag_key_results.clone();
        panel.on_vault_save(move |provider, token| {
            let token = token.trim().to_string();
            if provider != "DeepSeek" || token.is_empty() {
                return;
            }
            key_res_vs.borrow_mut().remove("manual");
            if let Some(p) = panel_vsave.upgrade() {
                p.set_vault_status(
                    app_core::tr("Menyimpan token…", "Saving token…").into(),
                );
            }
            let core_worker = core_vsave.clone();
            let tx = (*tx_vsave).clone();
            let is_ui_test = ui_test_active();
            std::thread::spawn(move || {
                let result = core_worker.register_token("manual", &token);
                if result.is_ok() && !is_ui_test {
                    let _ = core_worker.save_setting("vault-user-cleared", "0");
                }
                let _ = tx.send(DiagMsg::Vault {
                    ok: result.is_ok(),
                    msg: match result {
                        Ok(()) => app_core::tr(
                            "Terpasang: token manual untuk DeepSeek.",
                            "Connected: manual token for DeepSeek.",
                        )
                        .to_string(),
                        Err(e) => {
                            if app_core::is_en() {
                                format!("Failed: {e}")
                            } else {
                                format!("Gagal: {e}")
                            }
                        }
                    },
                });
            });
        });
    }

    // ---------------- Floating HUD ----------------
    {
        // The live app starts with an empty conversation: the HUD shows the
        // centered Aemeath intro. `--demo-chat` is a dev flag that injects the
        // old sample thread for bubble/footer screenshots.
        let demo_chat = std::env::args().any(|a| a == "--demo-chat");
        if demo_chat {
            chat_messages.set_vec(model::seed_messages());
            sync_chat_height(&chat_messages);
        }
        hud.set_messages(ModelRc::from(chat_messages.clone()));
        // Topics are pushed by refresh_sessions() so the panel cards and this
        // dropdown always agree.
        // Model list is the backend's, not ours: whatever /v1/models returns is
        // what every panel shows. DeepSeek today, more later without UI changes.
        let provider_labels: Vec<String> = provider_opts
            .iter()
            .map(|p| p.label.clone())
            .collect();
        hud.set_models(ModelRc::new(VecModel::from(
            provider_labels.iter().map(|s| SharedString::from(s.clone())).collect::<Vec<_>>(),
        )));
        if let Some(first) = provider_labels.first() {
            hud.set_selected_model(first.clone().into());
        }
        if let Some(w) = core.current_window() {
            hud.set_detected_window(w.process_name.clone().into());
            hud.set_detected_category(w.category.to_uppercase().into());
            // A fresh launch starts an unnamed chat: title it after whatever is
            // on screen instead of leaving the mock "[Game] Game Window" label.
            if chat_messages.row_count() == 0 && turns.borrow().is_empty() {
                hud.set_current_topic(app_core::topic_label(&w.suggested_topic).into());
            }
        }
        model::set_ui_scale(hud.window().scale_factor());
        hud.set_avatar(logos::avatar_sprite());
        hud.set_avatar_hud(logos::avatar_hud_sprite(hud.window().scale_factor()));

        let core_pin = core.clone();
        let panel_pin = panel.as_weak();
        let weak = hud.as_weak();
        hud.on_pin_toggled(move || {
            let Some(h) = weak.upgrade() else { return };
            let pinned = !h.get_pinned();
            h.set_pinned(pinned);
            if let Some(hwnd) = win32::find_window_by_title("Mini Assistant HUD") {
                win32::set_always_on_top(hwnd, pinned);
            }
            // Same toggle lives in Pengaturan: keep the checkbox and the saved
            // setting in step, otherwise a restart undoes the pin.
            if let Some(p) = panel_pin.upgrade() {
                p.set_set_pin(pinned);
            }
            let _ = core_pin.save_setting("pin", if pinned { "1" } else { "0" });
        });

        let weak_bubble = bubble.as_weak();
        let weak_hud = hud.as_weak();
        let weak_panel = panel.as_weak();
        let collapse_sprites = bubble_sprites.clone();
        let collapse_state = bubble_drag_state.clone();
        let collapse_timer = bubble_drag_timer.clone();
        hud.on_collapse_requested(move || {
            // The old webview resized the same window down to 92x92 at the
            // HUD's current top-left and then snapped to the nearest edge.
            // Recreate that with the two native windows: position first, snap
            // while still hidden, then show so there is no visible jump.
            *collapse_state.borrow_mut() = None;
            collapse_timer.stop();

            if let Some(b) = weak_bubble.upgrade() {
                if b.get_translate_visible() {
                    collapse_bubble_translate(&b);
                }
            }

            let edge = if let (Some(hud_hwnd), Some(bubble_hwnd)) = (
                win32::find_largest_window_by_title("Mini Assistant HUD"),
                win32::find_window_by_title("Mini Assistant Bubble"),
            ) {
                win32::position_bubble_from_hud(bubble_hwnd, hud_hwnd);
                win32::snap_window_to_edge(bubble_hwnd)
            } else {
                None
            };
            let edge = edge.unwrap_or(win32::ScreenEdge::Bottom);

            if let Some(b) = weak_bubble.upgrade() {
                b.set_held(false);
                b.set_dragging(false);
                b.set_edge(SharedString::from(edge.as_str()));
                set_bubble_idle(&b, &collapse_sprites, 0);
                let _ = b.show();
                b.window().request_redraw();
            }
            if let Some(h) = weak_hud.upgrade() {
                let _ = h.hide();
            }
            if let Some(p) = weak_panel.upgrade() {
                p.set_panel_visible(false);
                let _ = p.hide();
            }
            schedule_trim_working_set();
        });

        let weak_hud = hud.as_weak();
        let weak_panel = panel.as_weak();
        let weak_bubble = bubble.as_weak();
        let core_open = core.clone();
        hud.on_open_control_panel(move || {
            stop_singing_global(true);
            if let Some(p) = weak_panel.upgrade() {
                p.set_panel_visible(true);
                let _ = p.show();
                if p.get_active_tab() == 1 {
                    let _ = refresh_vault_accounts(&core_open, &p);
                }
            }
            if let Some(h) = weak_hud.upgrade() {
                let _ = h.hide();
            }
            if let Some(b) = weak_bubble.upgrade() {
                let _ = b.hide();
            }
            schedule_trim_working_set();
        });

        let weak_hud = hud.as_weak();
        let weak_bubble = bubble.as_weak();
        let weak_panel = panel.as_weak();
        hud.window().on_close_requested(move || {
            if SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst) {
                return slint::CloseRequestResponse::HideWindow;
            }
            open_panel_exit_modal(&weak_panel, &weak_hud, &weak_bubble);
            slint::CloseRequestResponse::HideWindow
        });

        let weak_hud = hud.as_weak();
        let weak_bubble = bubble.as_weak();
        let weak_panel = panel.as_weak();
        bubble.window().on_close_requested(move || {
            if SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst) {
                return slint::CloseRequestResponse::HideWindow;
            }
            open_panel_exit_modal(&weak_panel, &weak_hud, &weak_bubble);
            slint::CloseRequestResponse::HideWindow
        });

        hud.on_drag_request(move || {
            if let Some(hwnd) = win32::find_largest_window_by_title("Mini Assistant HUD") {
                win32::begin_window_drag(hwnd);
            }
        });

        let weak = hud.as_weak();
        let messages_thought = chat_messages.clone();
        hud.on_toggle_thought(move |index| {
            let i = index.max(0) as usize;
            if let Some(mut m) = messages_thought.row_data(i) {
                if m.thought.is_empty() {
                    return;
                }
                m.thought_open = !m.thought_open;
                messages_thought.set_row_data(i, m);
                sync_chat_height_preserve(&messages_thought, Some(i));
            }
        });

        let messages_detail = chat_messages.clone();
        hud.on_toggle_detail(move |index| {
            let i = index.max(0) as usize;
            if let Some(mut m) = messages_detail.row_data(i) {
                m.detail_open = !m.detail_open;
                messages_detail.set_row_data(i, m);
                sync_chat_height_preserve(&messages_detail, Some(i));
            }
        });

        let messages_search = chat_messages.clone();
        hud.on_toggle_search_sources(move |index| {
            let i = index.max(0) as usize;
            if let Some(mut m) = messages_search.row_data(i) {
                m.search_open = !m.search_open;
                messages_search.set_row_data(i, m);
                sync_chat_height_preserve(&messages_search, Some(i));
            }
        });

        let messages_smodal = chat_messages.clone();
        let hud_smodal = hud.as_weak();
        hud.on_open_search_modal(move |index| {
            let i = index.max(0) as usize;
            if let Some(m) = messages_smodal.row_data(i) {
                if let Some(h) = hud_smodal.upgrade() {
                    h.set_search_modal_sources(m.sources.clone());
                    h.set_search_modal_count(m.search_count);
                    h.set_search_modal_open(true);
                }
            }
        });

        hud.on_open_url(move |url| {
            if url.is_empty() {
                return;
            }
            if ui_test_active() {
                record_ui_test_url(url.as_str());
                return;
            }
            win32::open_url(&url);
        });

        hud.on_open_svg_external(move |svg| {
            let trimmed = svg.trim();
            if trimmed.is_empty() {
                return;
            }
            let dir = app_core::portable_cache_dir();
            let _ = std::fs::create_dir_all(&dir);
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let path = dir.join(format!("diagram-{ts}.svg"));
            if std::fs::write(&path, trimmed.as_bytes()).is_ok() {
                let p = path.to_string_lossy().into_owned();
                if ui_test_active() {
                    record_ui_test_url(&p);
                    return;
                }
                win32::open_url(&p);
            }
        });

        let messages_copy = chat_messages.clone();
        let copy_gen: Rc<RefCell<HashMap<usize, u64>>> = Rc::new(RefCell::new(HashMap::new()));
        let mut copy_seq = 0u64;
        hud.on_copy_message(move |index| {
            let idx = index.max(0) as usize;
            if let Some(mut m) = messages_copy.row_data(idx) {
                let clean = model::plain_copy_text(&m.body);
                win32::copy_text(&clean);
                m.copied = true;
                messages_copy.set_row_data(idx, m);

                copy_seq = copy_seq.wrapping_add(1);
                let my_seq = copy_seq;
                copy_gen.borrow_mut().insert(idx, my_seq);
                let msgs = messages_copy.clone();
                let gens = copy_gen.clone();
                slint::Timer::single_shot(Duration::from_secs(2), move || {
                    if gens.borrow().get(&idx).copied() == Some(my_seq) {
                        if let Some(mut cur) = msgs.row_data(idx) {
                            if cur.copied {
                                cur.copied = false;
                                msgs.set_row_data(idx, cur);
                            }
                        }
                    }
                });
            }
        });

        let partial_clip_seq_before: Rc<std::cell::Cell<u32>> = Rc::new(std::cell::Cell::new(0));
        let partial_sel_gen: Rc<RefCell<HashMap<usize, u64>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let mut partial_sel_seq = 0u64;

        let messages_popen = chat_messages.clone();
        hud.on_open_partial_copy(move |index| {
            let idx = index.max(0) as usize;
            let Some(mut m) = messages_popen.row_data(idx) else {
                return;
            };
            m.select_mode = !m.select_mode;
            m.select_copied = false;
            m.select_text = if m.select_mode {
                model::plain_copy_text(&m.body).into()
            } else {
                SharedString::new()
            };
            messages_popen.set_row_data(idx, m);
            sync_chat_height_preserve(&messages_popen, Some(idx));
        });

        let seq_prep = partial_clip_seq_before.clone();
        hud.on_prepare_partial_selection(move || {
            seq_prep.set(app_core::clipboard_sequence());
        });

        let seq_commit = partial_clip_seq_before.clone();
        let messages_pcommit = chat_messages.clone();
        let sel_gen_commit = partial_sel_gen.clone();
        hud.on_commit_partial_selection(move |index| {
            let idx = index.max(0) as usize;
            let Some(mut m) = messages_pcommit.row_data(idx) else {
                return;
            };
            let changed = app_core::clipboard_sequence() != seq_commit.get();
            if !changed && !m.select_text.is_empty() {
                win32::copy_text(&m.select_text);
            }
            m.select_copied = true;
            messages_pcommit.set_row_data(idx, m);

            partial_sel_seq = partial_sel_seq.wrapping_add(1);
            let my_seq = partial_sel_seq;
            sel_gen_commit.borrow_mut().insert(idx, my_seq);
            let msgs = messages_pcommit.clone();
            let gens = sel_gen_commit.clone();
            slint::Timer::single_shot(Duration::from_secs(2), move || {
                if gens.borrow().get(&idx).copied() == Some(my_seq) {
                    if let Some(mut cur) = msgs.row_data(idx) {
                        if cur.select_copied {
                            cur.select_copied = false;
                            msgs.set_row_data(idx, cur);
                        }
                    }
                }
            });
        });

        hud.on_copy_code(move |code| {
            if ui_test_active() {
                record_ui_test_code(code.as_str());
            }
            win32::copy_text(&code);
        });

        // Typing indicator ("...") only animates while a response/translation is streaming.
        let hud_dots = hud.as_weak();
        let bubble_dots = bubble.as_weak();
        let dots = slint::Timer::default();
        dots.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(320),
            move || {
                if let Some(h) = hud_dots.upgrade() {
                    if h.get_busy() {
                        h.set_dot_phase((h.get_dot_phase() + 1) % 3);
                    }
                }
                if let Some(b) = bubble_dots.upgrade() {
                    if b.get_translate_busy() {
                        b.set_dot_phase((b.get_dot_phase() + 1) % 3);
                    }
                }
            },
        );
        std::mem::forget(dots);

        // Explicit Ctrl+V flow: appends clipboard image when user pastes into composer.
        let hud_clip = hud.as_weak();
        let attach_clip = attachments.clone();
        hud.on_paste_clipboard(move || {
            let Some(h) = hud_clip.upgrade() else {
                return;
            };
            let Some(png) = app_core::image_png() else {
                return;
            };
            let seq = app_core::clipboard_sequence();
            let dir = app_core::portable_cache_dir();
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join(format!("paste-{}.png", seq));
            if std::fs::write(&path, &png).is_err() {
                return;
            }
            let (_thumb, is_image, _ext) = attachment_preview(path.to_str().unwrap_or(""));
            if !is_image {
                return;
            }
            {
                let mut list = attach_clip.borrow_mut();
                if list.len() < 8 {
                    list.push(path.clone());
                }
            }
            sync_hud_attachments(&h, &attach_clip.borrow());
            dlog!("[paste] attached {}", path.display());
        });

        hud.on_toggle_pulldown(move || {
            if let Some(h) = weak.upgrade() {
                let open = h.get_pulldown_open();
                h.set_pulldown_open(!open);
            }
        });

        let core_send = core.clone();
        let messages_send = chat_messages.clone();
        let rx_send = stream_rx.clone();
        let buf_send = stream_buf.clone();
        let row_send = stream_row.clone();
        let hud_send = hud.as_weak();
        let attach_send = attachments.clone();
        let turns_send = turns.clone();
        let reason_send = reasoning_buf.clone();
        let think_start_send = think_start.clone();
        let think_label_send = think_label.clone();
        let model_send = model_label.clone();
        let provider_map_send = provider_map.clone();
        let search_sources_send = search_sources_buf.clone();
        let bubble_send = bubble.as_weak();
        let panel_send = panel.as_weak();
        let answer_send = answer_in_bubble.clone();
        hud.on_send_request(move |text| {
            let trimmed = text.trim_matches(|c| c == '\r' || c == '\n').trim();
            let has_attachments = !attach_send.borrow().is_empty();
            if trimmed.is_empty() && !has_attachments {
                return;
            }
            let prompt = if trimmed.is_empty() {
                app_core::tr(
                    "Tolong analisis lampiran ini.",
                    "Please analyze this attachment.",
                )
                .to_string()
            } else {
                trimmed.to_string()
            };
            let Some(h) = hud_send.upgrade() else {
                return;
            };
            if ui_test_active() {
                record_ui_test_send(&prompt);
                h.set_input_text(SharedString::new());
                return;
            }
            // One stream at a time: a second send would replace the receiver
            // and strand the answer that is still arriving.
            if h.get_busy() {
                return;
            }
            stop_singing_global(true);
            // Every provider card switched off: there is nothing to send to.
            if h.get_models().iter().count() == 0 {
                h.set_status_text(
                    app_core::tr(
                        "Tidak ada provider aktif — nyalakan di control panel.",
                        "No active provider — enable one in the control panel.",
                    )
                    .into(),
                );
                h.set_status_visible(true);
                return;
            }
            // First turn of a new chat: the name is frozen from the window on
            // screen right now. Later turns keep it, so a conversation never
            // hops between topics when the user alt-tabs mid-answer.
            let topic = if turns_send.borrow().is_empty() {
                let fresh = core_send
                    .current_window()
                    .map(|w| app_core::topic_label(&w.suggested_topic))
                    .filter(|s| !s.trim().is_empty())
                    .map(|base| unique_topic(&core_send, &base))
                    .unwrap_or_else(|| h.get_current_topic().to_string());
                h.set_current_topic(fresh.clone().into());
                fresh
            } else {
                h.get_current_topic().to_string()
            };
            // The composer is cleared the moment the message is accepted, so the
            // text never stays behind while the answer streams in.
            h.set_input_text(SharedString::new());
            *model_send.borrow_mut() = h.get_selected_model().to_string();

            // Auto-capture is opt-in; only when nothing was attached by hand.
            if h.get_auto_capture() && attach_send.borrow().is_empty() {
                let own = win32::find_largest_window_by_title("Mini Assistant HUD");
                if let Some(hwnd) = own {
                    core_send.set_capture_visible(hwnd as isize, h.get_capture_app());
                }
                let shot = core_send.capture_to_file(h.get_target_window_only());
                if let Some(hwnd) = own {
                    core_send.set_capture_visible(hwnd as isize, true);
                }
                if let Ok(path) = shot {
                    attach_send.borrow_mut().push(path);
                }
            }

            // 1. Take all pending attachments without blocking UI thread!
            let pending_paths = std::mem::take(&mut *attach_send.borrow_mut());
            let attach_items: Vec<AttachmentItem> = pending_paths
                .iter()
                .map(|p| build_attachment_item(p))
                .collect();

            turns_send.borrow_mut().push(Turn {
                role: "user".to_string(),
                text: prompt.clone(),
                files: Vec::new(),
            });
            let history = build_history(&turns_send.borrow());
            dlog!(
                "[topic] kirim {} giliran + {} lampiran ke '{topic}'",
                history.len(),
                pending_paths.len()
            );

            // 2. Render user row and assistant typing row IMMEDIATELY (0ms delay)
            let mut user_row =
                model::message("user", app_core::tr("ANDA", "YOU"), &prompt, "", false);
            if let Some(first) = attach_items.first() {
                user_row.attach_name = first.ext.clone();
                user_row.attach_image = first.is_image;
                user_row.attach_thumb = first.thumb.clone();
                user_row.attach_path = first.path.clone();
            }
            user_row.attachments = ModelRc::new(VecModel::from(attach_items));
            model::layout(&mut user_row);
            messages_send.push(user_row);
            let row = messages_send.row_count();
            messages_send.push(model::message("assistant", "AEMEATH", "", "", true));
            sync_chat_height(&messages_send);
            scroll_chat_to_bottom_deferred(&h);
            *row_send.borrow_mut() = row;
            buf_send.borrow_mut().clear();
            reason_send.borrow_mut().clear();
            search_sources_send.borrow_mut().clear();
            *think_start_send.borrow_mut() = Some(std::time::Instant::now());
            think_label_send.borrow_mut().clear();

            // 3. Clear composer & attachment preview immediately
            sync_hud_attachments(&h, &[]);

            // Ephemeral chat: keep it on screen, keep it out of SQLite.
            if !h.get_temp_chat() {
                let _ = core_send.save_message(&topic, &session_category(&h), "user", &prompt, None);
            }

            // 4. Stream completion with background multi-file upload (zero UI freeze)
            let model_id = provider_map_send
                .borrow()
                .iter()
                .find(|p| p.label == model_send.borrow().as_str())
                .map(|p| p.default_model.clone())
                .unwrap_or_else(|| "deepseek-chat".to_string());
            let req = app_core::ChatRequest::new(history)
                .model(model_id)
                .with_reasoning_and_search();
            *rx_send.borrow_mut() = Some(core_send.chat_stream_with_attachments(req, pending_paths));
            h.set_busy(true);

            // Optional: close the window and let the answer appear in the
            // mascot bubble, like Translate This Screen.
            if h.get_send_closes() {
                if let (Some(b), Some(p)) = (bubble_send.upgrade(), panel_send.upgrade()) {
                    present_answer_bubble(&h, &p, &b);
                    answer_send.set(true);
                }
            }
        });

        // Retry / regenerate: same row, same history, fresh stream. The failed
        // bubble is cleared in place instead of stacking a second answer.
        let core_retry = core.clone();
        let messages_retry = chat_messages.clone();
        let rx_retry = stream_rx.clone();
        let buf_retry = stream_buf.clone();
        let row_retry = stream_row.clone();
        let reason_retry = reasoning_buf.clone();
        let start_retry = think_start.clone();
        let label_retry = think_label.clone();
        let model_retry = model_label.clone();
        let provider_map_retry = provider_map.clone();
        let turns_retry = turns.clone();
        let search_sources_retry = search_sources_buf.clone();
        let hud_retry = hud.as_weak();
        hud.on_retry_request(move |index| {
            let i = index.max(0) as usize;
            if let Some(mut m) = messages_retry.row_data(i) {
                m.failed = false;
                model::set_body(&mut m, "");
                m.meta = SharedString::new();
                m.search_count = 0;
                m.search_open = false;
                m.sources = slint::ModelRc::new(slint::VecModel::<SearchSourceItem>::default());
                messages_retry.set_row_data(i, m);
                sync_chat_height(&messages_retry);
            }
            buf_retry.borrow_mut().clear();
            reason_retry.borrow_mut().clear();
            search_sources_retry.borrow_mut().clear();
            label_retry.borrow_mut().clear();
            *start_retry.borrow_mut() = Some(std::time::Instant::now());
            *row_retry.borrow_mut() = i;
            let Some(h) = hud_retry.upgrade() else {
                return;
            };
            *model_retry.borrow_mut() = h.get_selected_model().to_string();
            let model_id = provider_map_retry
                .borrow()
                .iter()
                .find(|p| p.label == model_retry.borrow().as_str())
                .map(|p| p.default_model.clone())
                .unwrap_or_else(|| "deepseek-chat".to_string());
            let req = app_core::ChatRequest::new(build_history(&turns_retry.borrow()))
                .model(model_id)
                .with_reasoning_and_search();
            *rx_retry.borrow_mut() = Some(core_retry.chat_stream(req));
            h.set_busy(true);
        });
        // Local UI state only for now; the backend wiring stays deferred.
        let weak = hud.as_weak();
        hud.on_model_selected(move |m| {
            if let Some(h) = weak.upgrade() {
                h.set_selected_model(m);
            }
        });
        let core_nsess = core.clone();
        let messages_nsess = chat_messages.clone();
        let weak = hud.as_weak();
        hud.on_new_session(move || {
            if let Some(h) = weak.upgrade() {
                start_new_session(&core_nsess, &h, &messages_nsess);
                h.set_pulldown_open(false);
            }
        });
        let core_topic = core.clone();
        let messages_topic = chat_messages.clone();
        let turns_topic = turns.clone();
        let weak = hud.as_weak();
        hud.on_topic_selected(move |t| {
            let name = t.to_string();
            if let Some(h) = weak.upgrade() {
                h.set_current_topic(t);
                h.set_pulldown_open(false);
            }
            let _n = load_topic(&core_topic, &name, &messages_topic, &turns_topic);
            dlog!("[topic] lanjut sesi '{name}' ({_n} pesan)");
            sync_chat_height(&messages_topic);
            if let Some(h) = weak.upgrade() {
                scroll_chat_to_bottom_deferred(&h);
            }
        });
        let core_settle = core.clone();
        let weak = hud.as_weak();
        let panel_settle = panel.as_weak();
        hud.on_auto_capture_toggled(move || {
            if let Some(h) = weak.upgrade() {
                let v = !h.get_auto_capture();
                h.set_auto_capture(v);
                let _ = core_settle.save_setting("auto-capture", if v { "1" } else { "0" });
                if let Some(p) = panel_settle.upgrade() {
                    p.set_set_auto_capture(v);
                }
            }
        });
        let core_settle = core.clone();
        let weak = hud.as_weak();
        let panel_settle = panel.as_weak();
        hud.on_capture_app_toggled(move || {
            if let Some(h) = weak.upgrade() {
                let v = !h.get_capture_app();
                h.set_capture_app(v);
                let _ = core_settle.save_setting("capture-app", if v { "1" } else { "0" });
                if let Some(p) = panel_settle.upgrade() {
                    p.set_set_capture_app(v);
                }
            }
        });
        let core_settle = core.clone();
        let weak = hud.as_weak();
        let panel_settle = panel.as_weak();
        hud.on_target_window_toggled(move || {
            if let Some(h) = weak.upgrade() {
                let v = !h.get_target_window_only();
                h.set_target_window_only(v);
                let _ = core_settle.save_setting("target-window", if v { "1" } else { "0" });
                if let Some(p) = panel_settle.upgrade() {
                    p.set_set_target_window(v);
                }
            }
        });
        let weak = hud.as_weak();
        hud.on_temp_chat_toggled(move || {
            if let Some(h) = weak.upgrade() {
                let v = h.get_temp_chat();
                h.set_temp_chat(!v);
            }
        });
        let core_cap = core.clone();
        let attach_cap = attachments.clone();
        let weak_cap = hud.as_weak();
        hud.on_capture_screen(move || {
            let Some(h) = weak_cap.upgrade() else {
                return;
            };
            if attach_cap.borrow().len() >= 8 {
                h.set_status_text(
                    app_core::tr(
                        "Maksimal 8 lampiran per pesan.",
                        "Maximum 8 attachments per message.",
                    )
                    .into(),
                );
                h.set_status_visible(true);
                return;
            }
            let target_only = h.get_target_window_only();
            let own = win32::find_largest_window_by_title("Mini Assistant HUD");
            if let Some(hwnd) = own {
                core_cap.set_capture_visible(hwnd as isize, h.get_capture_app());
            }
            let shot = core_cap.capture_to_file(target_only);
            if let Some(hwnd) = own {
                core_cap.set_capture_visible(hwnd as isize, true);
            }
            match shot {
                Ok(path) => {
                    attach_cap.borrow_mut().push(path);
                    let count = attach_cap.borrow().len();
                    sync_hud_attachments(&h, &attach_cap.borrow());
                    h.set_status_text(
                        if app_core::is_en() {
                            format!("Screenshot #{count} ready to send.")
                        } else {
                            format!("Screenshot #{count} siap dikirim.")
                        }
                        .into(),
                    );
                    h.set_status_visible(true);
                    let weak = weak_cap.clone();
                    slint::Timer::single_shot(std::time::Duration::from_secs(2), move || {
                        if let Some(h) = weak.upgrade() {
                            h.set_status_visible(false);
                        }
                    });
                }
                Err(e) => {
                    dlog!("[test] capture failed: {e}");
                    h.set_status_text(
                        if app_core::is_en() {
                            format!("Capture failed: {e}")
                        } else {
                            format!("Capture gagal: {e}")
                        }
                        .into(),
                    );
                    h.set_status_visible(true);
                }
            }
        });
        let weak_attach = hud.as_weak();
        let attach_path = attachments.clone();
        hud.on_attach_requested(move || {
            let picked = win32::pick_attachment_file();
            let Some(h) = weak_attach.upgrade() else {
                return;
            };
            if let Some(path) = picked {
                let name = std::path::Path::new(&path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&path)
                    .to_owned();
                dlog!("[ui] attach file = {path}");
                {
                    let mut list = attach_path.borrow_mut();
                    if list.len() < 8 {
                        list.push(std::path::PathBuf::from(&path));
                    }
                }
                sync_hud_attachments(&h, &attach_path.borrow());
                h.set_status_text(
                    if app_core::is_en() {
                        format!("File selected: {name}")
                    } else {
                        format!("File dipilih: {name}")
                    }
                    .into(),
                );
                h.set_status_visible(true);
                let weak = weak_attach.clone();
                slint::Timer::single_shot(std::time::Duration::from_secs(3), move || {
                    if let Some(h) = weak.upgrade() {
                        h.set_status_visible(false);
                    }
                });
            }
            h.window().request_redraw();
            slint::Timer::single_shot(
                std::time::Duration::from_millis(300),
                win32::trim_working_set,
            );
        });

        let weak_remove = hud.as_weak();
        let attach_remove = attachments.clone();
        hud.on_remove_attachment(move || {
            attach_remove.borrow_mut().clear();
            if let Some(h) = weak_remove.upgrade() {
                sync_hud_attachments(&h, &[]);
                h.window().request_redraw();
            }
        });

        let weak_remove_at = hud.as_weak();
        let attach_remove_at = attachments.clone();
        hud.on_remove_attachment_at(move |index| {
            let idx = index.max(0) as usize;
            {
                let mut list = attach_remove_at.borrow_mut();
                if idx < list.len() {
                    list.remove(idx);
                }
            }
            if let Some(h) = weak_remove_at.upgrade() {
                sync_hud_attachments(&h, &attach_remove_at.borrow());
                h.window().request_redraw();
            }
        });

        let attach_open_pending = attachments.clone();
        hud.on_open_pending_attachment(move || {
            if let Some(p) = attach_open_pending.borrow().first() {
                win32::open_url(&p.to_string_lossy());
            }
        });

        let attach_open_pending_at = attachments.clone();
        hud.on_open_pending_attachment_at(move |index| {
            let idx = index.max(0) as usize;
            if let Some(p) = attach_open_pending_at.borrow().get(idx) {
                win32::open_url(&p.to_string_lossy());
            }
        });

        let messages_open_attach = chat_messages.clone();
        hud.on_open_attachment(move |index| {
            let i = index.max(0) as usize;
            if let Some(m) = messages_open_attach.row_data(i) {
                let path = m.attach_path.to_string();
                if !path.is_empty() {
                    win32::open_url(&path);
                }
            }
        });

        let messages_open_matt = chat_messages.clone();
        hud.on_open_message_attachment(move |msg_index, att_index| {
            let mi = msg_index.max(0) as usize;
            let ai = att_index.max(0) as usize;
            if let Some(m) = messages_open_matt.row_data(mi) {
                if let Some(att) = m.attachments.row_data(ai) {
                    let path = att.path.to_string();
                    if !path.is_empty() {
                        win32::open_url(&path);
                    }
                }
            }
        });
    }

    // ---------------- Mini bubble ----------------
    {
        bubble.set_edge(SharedString::from("bottom"));
        set_bubble_idle(&bubble, &bubble_sprites, 0);

        // Keyboard/accessibility path. The mouse drag pump is the normal
        // route; keeping this callback wired also makes the old expand event
        // usable if the bubble ever gains focus handling.
        let weak_bubble = bubble.as_weak();
        let weak_hud = hud.as_weak();
        bubble.on_expand_requested(move || {
            let (Some(b), Some(h)) = (weak_bubble.upgrade(), weak_hud.upgrade()) else {
                return;
            };
            b.set_held(false);
            b.set_dragging(false);
            expand_hud_from_bubble(&b, &h);
        });

        let sing_btog = sing_ctrl.clone();
        let weak_btog = bubble.as_weak();
        let weak_ptog = panel.as_weak();
        let weak_htog = hud.as_weak();
        let sp_btog = bubble_sprites.clone();
        let rx_btog = stream_rx.clone();
        let trx_btog = trans_stream_rx.clone();
        bubble.on_toggle_sing_requested(move || {
            if sing_btog.borrow().performing {
                stop_singing_perf(&sing_btog, &weak_btog, &weak_ptog, &sp_btog, true);
            } else {
                let _ = start_singing_perf(
                    &sing_btog,
                    &weak_btog,
                    &weak_ptog,
                    &weak_htog,
                    &sp_btog,
                    &rx_btog,
                    &trx_btog,
                    false,
                );
            }
        });

        // Pointer-down only: the Rust pump polls the cursor and left button,
        // so pointer capture can be lost without interrupting the drag.
        let weak_bubble = bubble.as_weak();
        let weak_hud = hud.as_weak();
        let drag_timer = bubble_drag_timer.clone();
        let drag_state = bubble_drag_state.clone();
        let release_state = bubble_drag_state.clone();
        let drag_hold = bubble_hold_frames.clone();
        let drag_sprites = bubble_sprites.clone();
        bubble.on_drag_release(move || {
            if let Some(drag) = release_state.borrow_mut().as_mut() {
                drag.released = true;
            }
        });
        bubble.on_drag_start(move || {
            start_bubble_drag(
                weak_bubble.clone(),
                weak_hud.clone(),
                drag_timer.clone(),
                drag_state.clone(),
                drag_hold.clone(),
                drag_sprites.clone(),
            );
        });

        let weak_b_close = bubble.as_weak();
        let trans_rx_close = trans_stream_rx.clone();
        let trans_deadline_close = trans_deadline.clone();
        let answer_close = answer_in_bubble.clone();
        bubble.on_close_translate(move || {
            *trans_rx_close.borrow_mut() = None;
            *trans_deadline_close.borrow_mut() = None;
            answer_close.set(false);
            if let Some(b) = weak_b_close.upgrade() {
                collapse_bubble_translate(&b);
            }
        });

        let weak_b_copy = bubble.as_weak();
        bubble.on_copy_translate(move || {
            if let Some(b) = weak_b_copy.upgrade() {
                let txt = model::plain_copy_text(&b.get_translate_text());
                if !txt.is_empty() {
                    win32::copy_text(&txt);
                }
            }
        });

        let weak_b_hud = bubble.as_weak();
        let weak_h_hud = hud.as_weak();
        let messages_b_hud = chat_messages.clone();
        let turns_b_hud = turns.clone();
        let trans_rx_hud = trans_stream_rx.clone();
        let trans_deadline_hud = trans_deadline.clone();
        let answer_hud = answer_in_bubble.clone();
        bubble.on_open_in_hud(move || {
            let (Some(b), Some(h)) = (weak_b_hud.upgrade(), weak_h_hud.upgrade()) else {
                return;
            };
            *trans_deadline_hud.borrow_mut() = None;
            // A chat answer already lives in the HUD history: just reopen it.
            if answer_hud.replace(false) {
                h.invoke_scroll_to_bottom();
                expand_hud_from_bubble(&b, &h);
                return;
            }
            *trans_rx_hud.borrow_mut() = None;
            let body = b.get_translate_text().to_string();
            let lang = b.get_translate_lang().to_string();
            if !body.is_empty() {
                let prompt = format!("Terjemahkan layar ({lang})");
                turns_b_hud.borrow_mut().push(Turn {
                    role: "user".to_string(),
                    text: prompt.clone(),
                    files: Vec::new(),
                });
                turns_b_hud.borrow_mut().push(Turn {
                    role: "assistant".to_string(),
                    text: body.clone(),
                    files: Vec::new(),
                });
                messages_b_hud.push(model::message("user", "ANDA", &prompt, "", false));
                messages_b_hud.push(model::message("assistant", "AEMEATH", &body, "", true));
                sync_chat_height(&messages_b_hud);
                h.invoke_scroll_to_bottom();
            }
            expand_hud_from_bubble(&b, &h);
        });
    }

    // ---------------- Mascot breathing animation ----------------
    // The web GIF has six 250ms frames: 0,1,2,3,2,1 (frames 4/5 repeat 2/1).
    // Use the same six-step cycle everywhere so the panel, HUD and bubble
    // breathe in the same rhythm as the browser.
    let mascot_timer = slint::Timer::default();
    {
        let sprites = bubble_sprites.clone();
        let hud_frames: Rc<std::cell::OnceCell<Vec<slint::Image>>> =
            Rc::new(std::cell::OnceCell::new());
        let hi_frames: Rc<std::cell::OnceCell<Vec<slint::Image>>> =
            Rc::new(std::cell::OnceCell::new());
        let hi_brand_frames: Rc<std::cell::OnceCell<Vec<slint::Image>>> =
            Rc::new(std::cell::OnceCell::new());
        let idx = Rc::new(std::cell::Cell::new(0usize));
        let weak_bubble = bubble.as_weak();
        let weak_hud = hud.as_weak();
        let weak_panel = panel.as_weak();
        let sing_anim = sing_ctrl.clone();
        let s_rx_anim = stream_rx.clone();
        let t_rx_anim = trans_stream_rx.clone();
        mascot_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(250),
            move || {
                let b_opt = weak_bubble.upgrade();
                let h_opt = weak_hud.upgrade();
                let p_opt = weak_panel.upgrade();
                let b_vis = b_opt.as_ref().is_some_and(|w| w.window().is_visible());
                let h_vis = h_opt.as_ref().is_some_and(|w| w.window().is_visible());
                let p_vis = p_opt.as_ref().is_some_and(|w| w.window().is_visible());
                if let Some(p) = &p_opt {
                    if p.get_panel_visible() != p_vis {
                        p.set_panel_visible(p_vis);
                    }
                }
                if !b_vis && !h_vis && !p_vis {
                    if sing_anim.borrow().performing {
                        stop_singing_perf(&sing_anim, &weak_bubble, &weak_panel, &sprites, true);
                    }
                    return;
                }

                // Opsi A Random Singing scheduler: only starts while Bubble idle
                // (never in HUD/Panel, never while AI or Translate is active).
                let ai_busy = s_rx_anim.borrow().is_some()
                    || t_rx_anim.borrow().is_some()
                    || h_opt.as_ref().is_some_and(|h| h.get_busy())
                    || b_opt
                        .as_ref()
                        .is_some_and(|b| b.get_translate_visible() || b.get_translate_busy());
                let bubble_held = b_opt.as_ref().is_some_and(|b| b.get_held());
                let (should_auto_start, should_auto_stop) = {
                    let s = sing_anim.borrow();
                    let perf = s.performing;
                    let can_auto = s.enabled
                        && !perf
                        && b_vis
                        && !h_vis
                        && !p_vis
                        && !bubble_held
                        && !ai_busy
                        && !s.tracks.is_empty()
                        && !ui_test_active()
                        && Instant::now() >= s.next_trigger;
                    let must_stop = perf
                        && !s.preview_mode
                        && (!s.enabled || !b_vis || h_vis || p_vis || ai_busy);
                    (can_auto, must_stop)
                };
                if should_auto_stop {
                    stop_singing_perf(&sing_anim, &weak_bubble, &weak_panel, &sprites, true);
                } else if should_auto_start {
                    let _ = start_singing_perf(
                        &sing_anim,
                        &weak_bubble,
                        &weak_panel,
                        &weak_hud,
                        &sprites,
                        &s_rx_anim,
                        &t_rx_anim,
                        false,
                    );
                }
                let currently_performing = sing_anim.borrow().performing;

                let i = idx.get();
                if b_vis && !currently_performing {
                    if let Some(b) = &b_opt {
                        // The drag pump owns the sprite while hold.gif is active,
                        // and sing_timer owns the sprite while singing. Keep `--sing-demo`
                        // singing frames alive when `b.get_singing()` is set without audio.
                        if !b.get_held() {
                            let edge = b.get_edge();
                            if b.get_singing() {
                                b.set_sing_phase((i % 8) as i32);
                                b.set_avatar(sprites.singing(edge.as_str(), i));
                            } else {
                                b.set_avatar(sprites.idle(edge.as_str(), i));
                            }
                        }
                    }
                }
                if (h_vis || p_vis) && !sprites.middle.is_empty() {
                    if let Some(img) = sprites.middle.get(i % sprites.middle.len()).cloned() {
                        if h_vis {
                            if let Some(h) = &h_opt {
                                let scale = h.window().scale_factor();
                                let hud_vec = hud_frames
                                    .get_or_init(|| breathing_cycle(logos::avatar_hud_frames(scale)));
                                h.set_avatar(img.clone());
                                if let Some(hud_f) = hud_vec.get(i % hud_vec.len().max(1)) {
                                    h.set_avatar_hud(hud_f.clone());
                                }
                            }
                        }
                        if p_vis {
                            if let Some(p) = &p_opt {
                                let hi_vec = hi_frames
                                    .get_or_init(|| breathing_cycle(logos::avatar_hi_frames()));
                                p.set_avatar(img);
                                if let Some(hi) = hi_vec.get(i % hi_vec.len().max(1)) {
                                    p.set_avatar_hi(hi.clone());
                                }
                                let hi_b_vec = hi_brand_frames
                                    .get_or_init(|| breathing_cycle(logos::avatar_hi_brand_frames()));
                                if let Some(hi_b) = hi_b_vec.get(i % hi_b_vec.len().max(1)) {
                                    p.set_avatar_hi_brand(hi_b.clone());
                                }
                            }
                        }
                    }
                }
                idx.set(i.wrapping_add(1));
            },
        );
    }

    // ---------------- Acrylic (backdrop blur) ----------------
    std::thread::spawn(|| {
        let mut hud_done = false;
        let mut bubble_done = false;
        for _ in 0..40 {
            if !hud_done {
                if let Some(hwnd) = win32::find_window_by_title("Mini Assistant HUD") {
                    let _ = win32::enable_acrylic(hwnd, true);
                    hud_done = true;
                }
            }
            if !bubble_done {
                if let Some(hwnd) = win32::find_window_by_title("Mini Assistant Bubble") {
                    let _ = win32::enable_acrylic(hwnd, false);
                    bubble_done = true;
                }
            }
            if hud_done && bubble_done {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });

    // Session name follows the window on screen. Only an untouched chat is
    // renamed: once a message exists the title belongs to the saved row, and
    // changing it would strand that history under a name the user never saw.
    let _window_pump = slint::Timer::default();
    {
        let core_w = core.clone();
        let hud_w = hud.as_weak();
        let panel_w = panel.as_weak();
        let messages_w = chat_messages.clone();
        let turns_w = turns.clone();
        let last = Rc::new(RefCell::new(String::new()));
        _window_pump.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(1000),
            move || {
                let Some(h) = hud_w.upgrade() else { return };
                let Some(w) = core_w.current_window() else { return };
                let base = app_core::topic_label(&w.suggested_topic);
                if *last.borrow() == base {
                    return;
                }
                *last.borrow_mut() = base.clone();
                let category = w.category.to_uppercase();
                h.set_detected_window(w.process_name.clone().into());
                h.set_detected_category(category.clone().into());
                if let Some(p) = panel_w.upgrade() {
                    p.set_detected_window(w.process_name.clone().into());
                    p.set_detected_category(category.into());
                }
                if messages_w.row_count() > 0 || !turns_w.borrow().is_empty() {
                    return;
                }
                h.set_current_topic(unique_topic(&core_w, &base).into());
                SESSIONS_REFRESH.with(|s| {
                    if let Some(f) = s.borrow().as_ref() {
                        f();
                    }
                });
            },
        );
    }

    // RAM/CPU in the panel footer are live numbers: sampled at boot they would
    // keep showing the startup value forever.
    let _metrics_pump = slint::Timer::default();
    {
        let core_m = core.clone();
        let panel_m = panel.as_weak();
        _metrics_pump.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(3),
            move || {
                if let Some(p) = panel_m.upgrade() {
                    if !p.window().is_visible() {
                        return;
                    }
                    let m = core_m.metrics();
                    p.set_ram_str(m.ram_str.clone().into());
                    p.set_cpu_str(m.cpu_str.clone().into());
                    p.set_sing_ram_pct(win32::memory_load_percent().unwrap_or(0) as i32);
                }
            },
        );
    }

    // Global hotkeys: Alt+Q brings the HUD forward; Alt+T translates the active screen into the bubble.
    let _hotkey_pump = slint::Timer::default();
    {
        let hud_hk = hud.as_weak();
        let panel_hk = panel.as_weak();
        let bubble_hk = bubble.as_weak();
        let hud_trans = hud.as_weak();
        let panel_trans = panel.as_weak();
        let bubble_trans = bubble.as_weak();
        let core_trans = core.clone();
        let lang_trans = trans_lang_state.clone();
        let trans_rx_hk = trans_stream_rx.clone();
        let trans_buf_hk = trans_stream_buf.clone();
        let trans_deadline_hk = trans_deadline.clone();
        let answer_hk = answer_in_bubble.clone();
        let prov_trans = provider_map.clone();
        _hotkey_pump.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(120),
            move || {
                let mut do_summon = false;
                while hotkey_rx.try_recv().is_ok() {
                    do_summon = true;
                }
                if do_summon {
                    stop_singing_global(true);
                    dlog!("[test] summon");
                    answer_hk.set(false);
                    *trans_rx_hk.borrow_mut() = None;
                    *trans_deadline_hk.borrow_mut() = None;
                    if let Some(b) = bubble_hk.upgrade() {
                        collapse_bubble_translate(&b);
                        let _ = b.hide();
                    }
                    if let Some(h) = hud_hk.upgrade() {
                        let _ = h.show();
                    }
                    if let Some(p) = panel_hk.upgrade() {
                        p.set_panel_visible(false);
                        let _ = p.hide();
                    }
                }
                let mut do_translate = false;
                while translate_rx.try_recv().is_ok() {
                    do_translate = true;
                }
                if do_translate {
                    stop_singing_global(true);
                    dlog!("[test] translate hotkey");
                    answer_hk.set(false);
                    if let (Some(h), Some(p), Some(b)) = (
                        hud_trans.upgrade(),
                        panel_trans.upgrade(),
                        bubble_trans.upgrade(),
                    ) {
                        let lang = lang_trans.borrow().clone();
                        trigger_bubble_translate(
                            &core_trans,
                            &h,
                            &p,
                            &b,
                            &lang,
                            &trans_rx_hk,
                            &trans_buf_hk,
                            &trans_deadline_hk,
                            &prov_trans,
                        );
                    }
                }
            },
        );
    }


    // Diagnostics pump: worker threads push results, the UI applies them here.
    let _diag_pump = slint::Timer::default();
    {
        let panel_pump = panel.as_weak();
        let core_pump = core.clone();
        let prov_map_pump = provider_map.clone();
        _diag_pump.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(150),
            move || {
                let Some(p) = panel_pump.upgrade() else {
                    return;
                };
                while let Ok(msg) = diag_rx.try_recv() {
                    match msg {
                        DiagMsg::Result { label, ms, head } => {
                            p.set_diag_ok(true);
                            p.set_diag_latency(format!("{ms} ms").into());
                            p.set_diag_status(format!("{label}: OK \u{00b7} {ms} ms \u{00b7} \"{head}\"").into());
                        }
                        DiagMsg::Fail { label, err } => {
                            p.set_diag_ok(false);
                            p.set_diag_latency("-".into());
                            p.set_diag_status(
                                if app_core::is_en() {
                                    format!("{label}: FAILED \u{00b7} {err}")
                                } else {
                                    format!("{label}: GAGAL \u{00b7} {err}")
                                }
                                .into(),
                            );
                        }
                        DiagMsg::KeyTesting { id } => {
                            diag_key_results
                                .borrow_mut()
                                .insert(id, (app_core::tr("Menguji...", "Testing...").into(), "amber".into()));
                            apply_key_diag_overlay(&p, &diag_key_results.borrow());
                        }
                        DiagMsg::KeyResult {
                            id,
                            label,
                            ok,
                            ms,
                            detail,
                            badge,
                            tone,
                        } => {
                            diag_key_results
                                .borrow_mut()
                                .insert(id, (badge, tone));
                            p.set_diag_ok(ok);
                            if let Some(m) = ms {
                                p.set_diag_latency(format!("{m} ms").into());
                                p.set_diag_status(
                                    format!("{label}: OK \u{00b7} {m} ms \u{00b7} \"{detail}\"").into(),
                                );
                            } else {
                                p.set_diag_status(
                                    if app_core::is_en() {
                                        format!("{label}: FAILED \u{00b7} {detail}")
                                    } else {
                                        format!("{label}: GAGAL \u{00b7} {detail}")
                                    }
                                    .into(),
                                );
                            }
                            let _ = refresh_vault_accounts_with_diag(
                                &core_pump,
                                &p,
                                &diag_key_results.borrow(),
                            );
                        }
                        DiagMsg::KeySummary {
                            ok_count,
                            total,
                            best_ms,
                            msg,
                        } => {
                            p.set_diag_ok(ok_count > 0);
                            if ok_count > 0 {
                                if let Some(m) = best_ms {
                                    p.set_diag_latency(format!("{m} ms").into());
                                }
                            } else if total > 0 {
                                p.set_diag_latency("-".into());
                            }
                            p.set_diag_status(msg.into());
                        }
                        DiagMsg::Extract { ok, msg } => {
                            p.set_diag_ok(ok);
                            p.set_diag_status(msg.into());
                            let _ = refresh_vault_accounts_with_diag(
                                &core_pump,
                                &p,
                                &diag_key_results.borrow(),
                            );
                        }
                        DiagMsg::Vault { ok, msg } => {
                            p.set_vault_busy(false);
                            p.set_vault_ok(ok);
                            p.set_vault_status(msg.into());
                            let entries = refresh_vault_accounts_with_diag(
                                &core_pump,
                                &p,
                                &diag_key_results.borrow(),
                            );
                            if prov_map_pump.borrow().is_empty() {
                                *prov_map_pump.borrow_mut() =
                                    core_pump.providers().unwrap_or_default();
                            }
                            let provs = p.get_providers();
                            let has_keys = !entries.is_empty();
                            for i in 0..provs.row_count() {
                                if let Some(mut card) = provs.row_data(i) {
                                    if card.id == "deepseek" {
                                        let want_tone = if has_keys { "green" } else { "amber" };
                                        let changed = card.enabled != has_keys
                                            || card.tone.as_str() != want_tone;
                                        if changed {
                                            card.enabled = has_keys;
                                            card.status =
                                                if has_keys { "Connected" } else { "Offline" }
                                                    .into();
                                            card.tone = want_tone.into();
                                            card.tone_color = if has_keys {
                                                slint::Color::from_rgb_u8(0x2C, 0xE5, 0x9B)
                                            } else {
                                                slint::Color::from_rgb_u8(0xFF, 0xD1, 0x66)
                                            };
                                            card.order = if has_keys {
                                                format!("#{}", i + 1)
                                            } else {
                                                "OFF".to_string()
                                            }
                                            .into();
                                            provs.set_row_data(i, card);
                                        }
                                        break;
                                    }
                                }
                            }
                            let enabled_names: Vec<SharedString> = (0..provs.row_count())
                                .filter_map(|i| provs.row_data(i).filter(|c| c.enabled).map(|c| c.name))
                                .collect();
                            HUD_HANDLE.with(|w| {
                                if let Some(h) = w.borrow().as_ref().and_then(|x| x.upgrade()) {
                                    h.set_models(ModelRc::new(VecModel::from(enabled_names.clone())));
                                    if !enabled_names.iter().any(|m| *m == h.get_selected_model()) {
                                        h.set_selected_model(
                                            enabled_names.first().cloned().unwrap_or_default(),
                                        );
                                    }
                                }
                            });
                            p.set_provider_count(
                                provider_count_text(
                                    enabled_names.len(),
                                    provs.row_count(),
                                    &p.get_active_browser(),
                                )
                                .into(),
                            );
                        }
                        DiagMsg::Connect { idx, ok, msg: _msg } => {
                            // The card was flipped optimistically when the user
                            // clicked; replace that with the real outcome.
                            let model = p.get_providers();
                            let idx = idx.max(0) as usize;
                            if let Some(mut card) = model.row_data(idx) {
                                card.status = if ok { "Connected" } else { "Offline" }.into();
                                card.tone = if ok { "green" } else { "amber" }.into();
                                card.tone_color = if ok {
                                    slint::Color::from_rgb_u8(0x2C, 0xE5, 0x9B)
                                } else {
                                    slint::Color::from_rgb_u8(0xFF, 0xD1, 0x66)
                                };
                                model.set_row_data(idx, card);
                            }
                            let active = (0..model.row_count())
                                .filter(|&i| {
                                    model.row_data(i).map(|x| x.enabled).unwrap_or(false)
                                })
                                .count();
                            p.set_provider_count(
                                provider_count_text(
                                    active,
                                    model.row_count(),
                                    &p.get_active_browser(),
                                )
                                .into(),
                            );
                            dlog!("[core] connect: {_msg}");
                        }
                        DiagMsg::Done => {
                            p.set_diag_busy(false);
                        }
                    }
                }
            },
        );
    }

    // Stream pump: 50 ms tick, no async in the UI thread.
    let _chat_pump = slint::Timer::default();
    {
        let rx_slot = stream_rx.clone();
        let buf_slot = stream_buf.clone();
        let row_slot = stream_row.clone();
        let trans_rx_slot = trans_stream_rx.clone();
        let trans_buf_slot = trans_stream_buf.clone();
        let trans_deadline_slot = trans_deadline.clone();
        let bubble_scroll_slot = bubble_scroll.clone();
        let scroll_anim_timer_pump = scroll_anim_timer.clone();
        let answer_pump = answer_in_bubble.clone();
        let bubble_pump = bubble.as_weak();
        let messages_pump = chat_messages.clone();
        let hud_pump = hud.as_weak();
        let core_pump = core.clone();
        let turns_pump = turns.clone();
        let reason_pump = reasoning_buf.clone();
        #[cfg(feature = "dev-tools")]
        let attach_pump = attach_list.clone();
        let label_pump = think_label.clone();
        let start_pump = think_start.clone();
        let model_pump = model_label.clone();
        let search_sources_pump = search_sources_buf.clone();
        let mut label_done = false;
        #[cfg(feature = "dev-tools")]
        let idx_pump = send_idx.clone();
        #[cfg(feature = "dev-tools")]
        let attachment_pump = attachments.clone();
        #[cfg(feature = "dev-tools")]
        let test_queue_pump = test_queue.clone();
        let mut saw_reasoning = false;
        let mut saw_search = false;
        _chat_pump.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(50),
            move || {
                // Async remote markdown image completion pump (only enters lock-free AtomicBool hit when busy).
                if model::has_pending_remote_images() && model::pump_remote_images(&messages_pump) {
                    sync_chat_height_preserve(&messages_pump, None);
                }

                // Human-reading-paced stop-and-glide auto-scroll for BubbleWindow.
                if scroll_anim_timer_pump.running() {
                    let keep_open = bubble_pump
                        .upgrade()
                        .is_some_and(|b| b.get_translate_visible());
                    if !keep_open {
                        bubble_scroll_slot.borrow_mut().reset();
                        scroll_anim_timer_pump.stop();
                    }
                } else if bubble_scroll_slot.borrow().is_active() {
                    if let Some(b) = bubble_pump.upgrade() {
                        if tick_bubble_scroll_once(&b, &bubble_scroll_slot, &trans_deadline_slot) {
                            let timer_weak = Rc::downgrade(&scroll_anim_timer_pump);
                            let bubble_anim = bubble_pump.clone();
                            let scroll_anim = bubble_scroll_slot.clone();
                            let deadline_anim = trans_deadline_slot.clone();
                            scroll_anim_timer_pump.start(
                                slint::TimerMode::Repeated,
                                std::time::Duration::from_millis(16),
                                move || {
                                    let keep_going = match bubble_anim.upgrade() {
                                        Some(b) => tick_bubble_scroll_once(
                                            &b,
                                            &scroll_anim,
                                            &deadline_anim,
                                        ),
                                        None => false,
                                    };
                                    if !keep_going {
                                        if let Some(t) = timer_weak.upgrade() {
                                            t.stop();
                                        }
                                    }
                                },
                            );
                        }
                    }
                }

                // Independent translation bubble auto-hide countdown (starts once reading reaches bottom).
                if trans_deadline_slot.borrow().is_some() {
                    if let Some(b) = bubble_pump.upgrade() {
                        let now = std::time::Instant::now();
                        let mut should_collapse = false;
                        if let Some(deadline) = *trans_deadline_slot.borrow() {
                            if !b.get_translate_visible() || b.get_translate_busy() {
                                *trans_deadline_slot.borrow_mut() = None;
                            } else if b.get_translate_hovered() || b.get_held() || b.get_dragging() {
                                let min_future = now + std::time::Duration::from_secs(2);
                                if deadline < min_future {
                                    *trans_deadline_slot.borrow_mut() = Some(min_future);
                                }
                            } else if now >= deadline {
                                should_collapse = true;
                            }
                        }
                        if should_collapse {
                            *trans_deadline_slot.borrow_mut() = None;
                            collapse_bubble_translate(&b);
                        }
                    }
                }

                // Independent translation bubble stream (works while HUD is hidden/minimized).
                {
                    let mut trans_done = false;
                    let mut trans_err: Option<String> = None;
                    let mut trans_dirty = false;
                    {
                        let slot = trans_rx_slot.borrow();
                        if let Some(rx) = slot.as_ref() {
                            loop {
                                match rx.try_recv() {
                                    Ok(app_core::ChatEvent::Delta(d)) => {
                                        trans_buf_slot.borrow_mut().push_str(&d);
                                        trans_dirty = true;
                                    }
                                    Ok(app_core::ChatEvent::Done(_)) => {
                                        trans_done = true;
                                        break;
                                    }
                                    Ok(app_core::ChatEvent::Error(e)) => {
                                        trans_err = Some(e);
                                        trans_done = true;
                                        break;
                                    }
                                    Ok(_) => {}
                                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                        trans_done = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    if let Some(err) = trans_err {
                        dlog!("[translate] error: {err}");
                        if trans_buf_slot.borrow().trim().is_empty() {
                            *trans_buf_slot.borrow_mut() = if app_core::is_en() {
                                format!("Screen translation failed: {err}")
                            } else {
                                format!("Gagal menerjemahkan layar: {err}")
                            };
                        }
                        trans_dirty = true;
                    }
                    if trans_dirty || trans_done {
                        let text_now = trans_buf_slot.borrow().clone();
                        if let Some(b) = bubble_pump.upgrade() {
                            update_bubble_stream_text(&b, &text_now, trans_done);
                        }
                        bubble_scroll_slot.borrow_mut().on_content_updated(
                            &text_now,
                            trans_done,
                            std::time::Instant::now(),
                        );
                        if !trans_done {
                            *trans_deadline_slot.borrow_mut() = None;
                        } else if !bubble_scroll_slot.borrow().is_active()
                            && trans_deadline_slot.borrow().is_none()
                        {
                            *trans_deadline_slot.borrow_mut() =
                                Some(std::time::Instant::now() + translate_timeout_duration(&text_now));
                        }
                    }
                    if trans_done {
                        *trans_rx_slot.borrow_mut() = None;
                    }
                }

                let mut finished = false;
                let mut failed = false;
                let mut meta = String::new();
                {
                    let slot = rx_slot.borrow();
                    let Some(rx) = slot.as_ref() else {
                        return;
                    };
                    loop {
                        match rx.try_recv() {
                            Ok(app_core::ChatEvent::Uploaded(file_id)) => {
                                dlog!("[upload] lampiran berhasil diunggah dengan id: {file_id}");
                                let mut t = turns_pump.borrow_mut();
                                if let Some(last_user) = t.iter_mut().rposition(|turn| turn.role == "user") {
                                    t[last_user].files.push(file_id);
                                }
                            }
                            Ok(app_core::ChatEvent::Delta(d)) => {
                                buf_slot.borrow_mut().push_str(&d);
                                if !label_done && !reason_pump.borrow().is_empty() {
                                    label_done = true;
                                    let secs = start_pump
                                        .borrow()
                                        .map(|t| t.elapsed().as_secs().max(1))
                                        .unwrap_or(1);
                                    *label_pump.borrow_mut() = if app_core::is_en() {
                                        format!("Thought for {secs}s")
                                    } else {
                                        format!("Berpikir selama {secs} detik")
                                    };
                                }
                            }
                            Ok(app_core::ChatEvent::Reasoning(t)) => {
                                if !saw_reasoning {
                                    saw_reasoning = true;
                                    dlog!(
                                        "[test] reasoning: {}",
                                        t.chars().take(80).collect::<String>()
                                    );
                                }
                                reason_pump.borrow_mut().push_str(&t);
                            }
                            Ok(app_core::ChatEvent::Search(items)) => {
                                if !saw_search {
                                    saw_search = true;
                                    dlog!("[test] search: {} hasil", items.len());
                                }
                                let mut list = search_sources_pump.borrow_mut();
                                merge_search_sources(&mut list, &items);
                            }
                            Ok(app_core::ChatEvent::Done(usage)) => {
                                let secs = start_pump
                                    .borrow()
                                    .map(|t| t.elapsed().as_secs_f32())
                                    .unwrap_or(0.0);
                                let tokens = usage.as_ref().map(|u| u.total_tokens).unwrap_or(0);
                                meta = if app_core::is_en() {
                                    format!(
                                        "{} · {:.1}s · {} tokens",
                                        model_pump.borrow(),
                                        secs,
                                        tokens
                                    )
                                } else {
                                    format!(
                                        "{} · {:.1} dtk · {} token",
                                        model_pump.borrow(),
                                        secs,
                                        tokens
                                    )
                                };
                                if !reason_pump.borrow().is_empty() && label_pump.borrow().is_empty() {
                                    let isecs = secs.round().max(1.0) as u64;
                                    *label_pump.borrow_mut() = if app_core::is_en() {
                                        format!("Thought for {isecs}s")
                                    } else {
                                        format!("Berpikir selama {isecs} detik")
                                    };
                                }
                                finished = true;
                                break;
                            }
                            Ok(app_core::ChatEvent::Error(_e)) => {
                                dlog!("[test] error: {_e}");
                                failed = true;
                                finished = true;
                                break;
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => break,
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                finished = true;
                                break;
                            }
                        }
                    }
                }
                let body = buf_slot.borrow().clone();
                let row = *row_slot.borrow();
                if row < messages_pump.row_count() {
                    let mut dirty = false;
                    if let Some(mut cur) = messages_pump.row_data(row) {
                        if failed {
                            // Keep the row, drop the half answer: the bubble shows
                            // the retry affordance instead of an error wall.
                            cur.failed = true;
                            model::set_body(&mut cur, "");
                            dirty = true;
                        } else if cur.body != body {
                            model::set_body(&mut cur, &body);
                            dirty = true;
                        }
                        let r = reason_pump.borrow().clone();
                        let lbl = label_pump.borrow().clone();
                        if !r.is_empty() && (cur.thought.as_str() != r || cur.think_label.as_str() != lbl) {
                            model::set_thought(&mut cur, &r, &lbl);
                            dirty = true;
                        }
                        let srcs = search_sources_pump.borrow().clone();
                        if !srcs.is_empty() && cur.search_count != srcs.len() as i32 {
                            cur.search_count = srcs.len() as i32;
                            cur.sources = slint::ModelRc::new(slint::VecModel::from(srcs));
                            dirty = true;
                        }
                        if !meta.is_empty() && cur.meta.as_str() != meta {
                            cur.meta = meta.clone().into();
                            dirty = true;
                        }
                        if dirty {
                            messages_pump.set_row_data(row, cur);
                        }
                    }
                    if dirty {
                        // Auto-follow bottom during streaming only if the user hasn't scrolled up to read earlier text.
                        let should_follow = hud_pump.upgrade().is_some_and(|h| {
                            let max_y = h.get_chat_scroll_max();
                            max_y < 60.0 || h.get_chat_scroll_y() >= max_y - 120.0
                        });
                        if should_follow {
                            sync_chat_height_at(&messages_pump, Some(row));
                            if let Some(h) = hud_pump.upgrade() {
                                scroll_chat_to_bottom_deferred(&h);
                            }
                        } else {
                            sync_chat_height_preserve(&messages_pump, Some(row));
                        }
                    }
                }
                // "Tutup jendela setelah kirim": mirror the stream into the bubble.
                if answer_pump.get() {
                    match bubble_pump.upgrade() {
                        Some(b) if b.get_translate_visible() => {
                            let shown = b.get_translate_text().to_string();
                            let next = if failed {
                                app_core::tr(
                                    "Gagal menghubungi model. Buka jendela untuk mencoba lagi.",
                                    "Failed to contact model. Open HUD window to retry.",
                                )
                                .to_string()
                            } else {
                                body.clone()
                            };
                            if (!next.is_empty() && next != shown) || finished {
                                update_bubble_stream_text(&b, &next, finished);
                                bubble_scroll_slot.borrow_mut().on_content_updated(
                                    &next,
                                    finished,
                                    std::time::Instant::now(),
                                );
                            }
                            if !finished {
                                *trans_deadline_slot.borrow_mut() = None;
                            } else if !bubble_scroll_slot.borrow().is_active()
                                && trans_deadline_slot.borrow().is_none()
                            {
                                *trans_deadline_slot.borrow_mut() =
                                    Some(std::time::Instant::now() + translate_timeout_duration(&next));
                                // Keep mirroring state only until the user closes it.
                            }
                        }
                        _ => answer_pump.set(false),
                    }
                }
                if finished {
                    *rx_slot.borrow_mut() = None;
                    if !failed && !body.is_empty() {
                        turns_pump.borrow_mut().push(Turn {
                            role: "assistant".to_string(),
                            text: body.clone(),
                            files: Vec::new(),
                        });
                    }
                    if let Some(h) = hud_pump.upgrade() {
                        h.set_busy(false);
                        if !failed && !body.is_empty() && !h.get_temp_chat() {
                            let topic = h.get_current_topic().to_string();
                            let cat = session_category(&h);
                            let saved_thought = reason_pump.borrow().clone();
                            let saved_meta = if !meta.is_empty() {
                                meta.clone()
                            } else {
                                messages_pump
                                    .row_data(row)
                                    .map(|m| m.meta.to_string())
                                    .unwrap_or_default()
                            };
                            let saved_sources: Vec<app_core::StoredSearchSource> =
                                search_sources_pump
                                    .borrow()
                                    .iter()
                                    .map(|s| app_core::StoredSearchSource {
                                        title: s.title.to_string(),
                                        url: s.url.to_string(),
                                        snippet: s.snippet.to_string(),
                                        site_name: s.site_name.to_string(),
                                        date: s.date.to_string(),
                                    })
                                    .collect();
                            let _ = core_pump.save_message_record(&app_core::MessageRecord {
                                topic_name: &topic,
                                category: &cat,
                                role: "assistant",
                                content: &body,
                                image_url: None,
                                thought: &saved_thought,
                                meta: &saved_meta,
                                sources: &saved_sources,
                            });
                            SESSIONS_REFRESH.with(|w| {
                                if let Some(f) = w.borrow().as_ref() {
                                    f();
                                }
                            });
                        }
                    }
                    #[cfg(feature = "dev-tools")]
                    if test_mode {
                        let flat = body.replace('\n', " ");
                        eprintln!("[test] assistant: {}", flat.chars().take(400).collect::<String>());
                        match test_queue_pump.borrow_mut().pop_front() {
                            Some(next) => {
                                let i = idx_pump.get();
                                if let Some(list) = attach_pump.get(i) {
                                    let paths: Vec<std::path::PathBuf> =
                                        list.iter().map(std::path::PathBuf::from).collect();
                                    *attachment_pump.borrow_mut() = paths.clone();
                                    if let Some(h) = hud_pump.upgrade() {
                                        sync_hud_attachments(&h, &paths);
                                    }
                                }
                                idx_pump.set(i + 1);
                                let weak = hud_pump.clone();
                                slint::Timer::single_shot(
                                    std::time::Duration::from_millis(400),
                                    move || {
                                        if let Some(h) = weak.upgrade() {
                                            h.invoke_send_request(next.clone().into());
                                        }
                                    },
                                );
                            }
                            None => {
                                if test_shot || test_shot_closed {
                                    {
                                        let row_now = *row_slot.borrow();
                                        if !test_shot_closed {
                                            if let Some(mut m) = messages_pump.row_data(row_now) {
                                                m.thought_open = true;
                                                messages_pump.set_row_data(row_now, m);
                                                sync_chat_height(&messages_pump);
                                            }
                                        }
                                        eprintln!(
                                            "[test] card: row={} msgs={} thought_len={} height={}",
                                            row_now,
                                            messages_pump.row_count(),
                                            messages_pump
                                                .row_data(row_now)
                                                .map(|m| m.thought.len())
                                                .unwrap_or(0),
                                            messages_pump
                                                .row_data(row_now)
                                                .map(|m| m.height)
                                                .unwrap_or(0)
                                        );
                                    }
                                    if let Some(h) = hud_pump.upgrade() {
                                        eprintln!(
                                            "[test] content-h: {} (viewport 484)",
                                            ChatLayout::get(&h).get_content_h()
                                        );
                                    }
                                    let core_shot = core_pump.clone();
                                    slint::Timer::single_shot(
                                        std::time::Duration::from_millis(800),
                                        move || {
                                            match core_shot.capture_to_file(false) {
                                                Ok(p) => eprintln!("[test] shot: {}", p.display()),
                                                Err(e) => eprintln!("[test] shot failed: {e}"),
                                            }
                                            let _ = slint::quit_event_loop();
                                        },
                                    );
                                } else {
                                    eprintln!("[test] done");
                                    let _ = slint::quit_event_loop();
                                }
                            }
                        }
                    }
                }
            },
        );
    }

    #[cfg(feature = "dev-tools")]
    {
        if test_temp {
            hud.set_temp_chat(true);
        }
        if let Some(model) = test_model {
            hud.set_selected_model(model.into());
        }
        if test_dump {
            let topic = hud.get_current_topic().to_string();
            let count = core.messages(&topic).map(|v| v.len()).unwrap_or(0);
            eprintln!("[test] history '{topic}': {count} pesan");
            return Ok(());
        }
        if test_hotkey {
            let core_hk = core.clone();
            slint::Timer::single_shot(std::time::Duration::from_millis(900), move || {
                eprintln!("[test] hotkey posted: {}", core_hk.simulate_hotkey());
            });
            slint::Timer::single_shot(std::time::Duration::from_millis(2200), || {
                let _ = slint::quit_event_loop();
            });
        }
        if test_mode {
            if test_capture {
                let weak = hud.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_millis(600), move || {
                    if let Some(h) = weak.upgrade() {
                        h.invoke_capture_screen();
                    }
                });
            }
            let weak = hud.as_weak();
            let first = test_queue.borrow_mut().pop_front();
            let delay = if test_capture { 1400 } else { 900 };
            if let Some(list) = attach_list.first() {
                if !list.is_empty() {
                    let paths: Vec<std::path::PathBuf> =
                        list.iter().map(std::path::PathBuf::from).collect();
                    *attachments.borrow_mut() = paths.clone();
                    sync_hud_attachments(&hud, &paths);
                }
            }
            send_idx.set(1);
            if let Some(text) = first {
                slint::Timer::single_shot(std::time::Duration::from_millis(delay), move || {
                    if let Some(h) = weak.upgrade() {
                        h.invoke_send_request(text.clone().into());
                    }
                });
            }
        }
    }

    let open_panel = args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--panel"
                | "--tab"
                | "--close-modal"
                | "--diag-run"
                | "--new-session"
                | "--vault"
                | "--toggle-provider"
                | "--shot-panel"
                | "--shot-panel-native"
                | "--hk-menu"
        )
    });
    let open_bubble = args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--bubble"
                | "--sing-demo"
                | "--shot-bubble"
                | "--test-translate"
                | "--test-translate-short"
        )
    });
    // Exclusive startup: default to Floating HUD when clicked directly without arguments.
    if open_panel && !args.iter().any(|a| a == "--hud" || a == "--run-ui-tests") {
        panel.set_panel_visible(true);
        let _ = panel.show();
        let _ = hud.hide();
        let _ = bubble.hide();
    } else if open_bubble && !args.iter().any(|a| a == "--hud" || a == "--run-ui-tests") {
        let _ = bubble.show();
        panel.set_panel_visible(false);
        let _ = panel.hide();
        let _ = hud.hide();
    } else {
        let _ = hud.show();
        if chat_messages.row_count() > 0 && !ui_test_active() {
            scroll_chat_to_bottom_deferred(&hud);
        }
        panel.set_panel_visible(false);
        let _ = panel.hide();
        let _ = bubble.hide();
    }

    // Drop the pages pulled in during startup (fonts, icon/logo decoders and
    // the initial DWM composition) once the first frames have landed. Without
    // this, the idle process sits near the 40 MB ceiling before the user has
    // even opened the file picker.
    slint::Timer::single_shot(
        std::time::Duration::from_secs(2),
        win32::trim_working_set,
    );

    #[cfg(feature = "dev-tools")]
    {
        // Dev helpers for the control panel: `--tab N` picks a page and
        // `--shot-panel` grabs the desktop once it has rendered, then quits.
        if let Some(tab) = args
            .iter()
            .position(|a| a == "--tab")
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<i32>().ok())
        {
            panel.set_active_tab(tab);
        }
    if args.iter().any(|a| a == "--close-modal") {
        panel.set_close_modal_open(true);
    }
    // `--diag-run` fires the provider probe so the diagnostics page can be
    // verified end to end without a mouse.
    if args.iter().any(|a| a == "--diag-run") {
        let panel_run = panel.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(400), move || {
            if let Some(p) = panel_run.upgrade() {
                p.invoke_diag_test_all();
            }
        });
    }
    // `--new-session` exercises the "Sesi Baru" path and dumps the list.
    if args.iter().any(|a| a == "--new-session") {
        let panel_new = panel.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(400), move || {
            if let Some(p) = panel_new.upgrade() {
                p.invoke_topic_new();
            }
        });
        let panel_dump = panel.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(1400), move || {
            if let Some(p) = panel_dump.upgrade() {
                let rows = p.get_topics();
                eprintln!("[test] topics: {}", rows.row_count());
                for i in 0..rows.row_count() {
                    if let Some(r) = rows.row_data(i) {
                        eprintln!("[test]   {} | {} | {}", r.name, r.count, r.category);
                    }
                }
            }
            let _ = slint::quit_event_loop();
        });
    }
    // `--vault` runs the cookie-extraction path headlessly (same code the
    // "Fetch Semua Cookies" button calls) and reports the resulting status.
    if args.iter().any(|a| a == "--vault") {
        let panel_vault = panel.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(300), move || {
            if let Some(p) = panel_vault.upgrade() {
                p.invoke_vault_fetch();
            }
        });
        let panel_after = panel.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_secs(6), move || {
            if let Some(p) = panel_after.upgrade() {
                eprintln!("[test] vault: {}", p.get_vault_status());
            }
            let _ = slint::quit_event_loop();
        });
    }
    // `--toggle-provider` flips the first provider card through the real
    // callback, so the threaded connect path can be checked without a mouse.
    if args.iter().any(|a| a == "--toggle-provider") {
        // Twice: the first click turns the card off, the second turns it back
        // on and therefore runs the connect path.
        let panel_t = panel.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(600), move || {
            if let Some(p) = panel_t.upgrade() {
                p.invoke_provider_toggled(0);
            }
        });
        let panel_t2 = panel.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(1400), move || {
            if let Some(p) = panel_t2.upgrade() {
                p.invoke_provider_toggled(0);
            }
        });
        let panel_d = panel.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(9000), move || {
            if let Some(p) = panel_d.upgrade() {
                let rows = p.get_providers();
                for i in 0..rows.row_count() {
                    if let Some(r) = rows.row_data(i) {
                        eprintln!(
                            "[test] card {} | {} | {} | enabled={}",
                            r.name, r.status, r.tone, r.enabled
                        );
                    }
                }
                eprintln!("[test] count: {}", p.get_provider_count());
            }
            let _ = slint::quit_event_loop();
        });
    }
    // `--open-topic <name>` resumes a saved session through the same callback
    // the history card uses (bubbles + turn history), so a following `--send`
    // proves the old conversation can actually be continued.
    if let Some(name) = args_flag_value("--open-topic") {
        let panel_open = panel.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(300), move || {
            if let Some(p) = panel_open.upgrade() {
                p.invoke_topic_selected(name.clone().into());
            }
        });
    }
    // `--dump-topics` prints what the pill and the HUD dropdown currently hold.
    if args.iter().any(|a| a == "--dump-topics") {
        let hud_dump = hud.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(1500), move || {
            if let Some(h) = hud_dump.upgrade() {
                eprintln!("[test] pill: {}", h.get_current_topic());
                let rows = h.get_topics();
                eprintln!("[test] dropdown: {}", rows.row_count());
                for i in 0..rows.row_count() {
                    eprintln!("[test]   {}", rows.row_data(i).unwrap_or_default());
                }
            }
            let _ = slint::quit_event_loop();
        });
    }
    if args.iter().any(|a| a == "--pulldown") {
        hud.set_pulldown_open(true);
    }
    if let Some(menu) = args_flag_value("--hk-menu").and_then(|v| v.parse::<i32>().ok()) {
        panel.set_hk_menu(menu);
    }
    if args.iter().any(|a| a == "--open-thought") {
        if let Some(mut m) = chat_messages.row_data(1) {
            m.thought_open = true;
            chat_messages.set_row_data(1, m);
            sync_chat_height(&chat_messages);
        }
    }
    if args.iter().any(|a| a == "--open-search") {
        if let Some(mut m) = chat_messages.row_data(1) {
            m.search_open = true;
            chat_messages.set_row_data(1, m);
            sync_chat_height(&chat_messages);
        }
    }
    if let Some(count) = args_flag_value("--capture-multi").and_then(|v| v.parse::<usize>().ok()) {
        for idx in 0..count.min(8) {
            let hud_cap = hud.as_weak();
            slint::Timer::single_shot(
                std::time::Duration::from_millis(250 + (idx as u64) * 250),
                move || {
                    if let Some(h) = hud_cap.upgrade() {
                        h.invoke_capture_screen();
                    }
                },
            );
        }
    }
    let pending_attach_args = args_flag_values("--attach-pending");
    if !pending_attach_args.is_empty() {
        let mut paths = attachments.borrow_mut();
        for p in &pending_attach_args {
            if paths.len() < 8 {
                paths.push(std::path::PathBuf::from(p));
            }
        }
        sync_hud_attachments(&hud, &paths);
    }
    if args.iter().any(|a| a == "--test-multi-bubble") {
        let attach_items: Vec<AttachmentItem> = pending_attach_args
            .iter()
            .map(|p| build_attachment_item(std::path::Path::new(p)))
            .collect();
        let mut user_row = model::message("user", "ANDA", "Bandingkan 2 screenshot ini:", "", false);
        if let Some(first) = attach_items.first() {
            user_row.attach_name = first.ext.clone();
            user_row.attach_image = first.is_image;
            user_row.attach_thumb = first.thumb.clone();
            user_row.attach_path = first.path.clone();
        }
        user_row.attachments = ModelRc::new(VecModel::from(attach_items));
        model::layout(&mut user_row);
        chat_messages.push(user_row);
        chat_messages.push(model::message(
            "assistant",
            "AEMEATH",
            "Kedua lampiran screenshot berhasil diterima dalam satu pesan.",
            "",
            true,
        ));
        sync_chat_height(&chat_messages);
    }
    if args.iter().any(|a| a == "--test-translate" || a == "--test-translate-short") {
        let short = args.iter().any(|a| a == "--test-translate-short");
        let custom = args_flag_value("--test-translate-text");
        let sample_default = concat!(
            "**Ringkasan Terjemahan Layar**\n\n",
            "Build selesai tanpa galat (**0 errors**). ",
            "Semua perubahan pada pintasan terjemahan dan lampiran ganda siap digunakan.\n\n",
            "- Status: *Siap diuji*\n",
            "- Target Bahasa: Indonesia"
        );
        let sample_short = "Pengaturan -> Audio & Suara";
        let sample = custom
            .as_deref()
            .unwrap_or(if short { sample_short } else { sample_default });
        bubble.set_translate_visible(true);
        bubble.set_translate_busy(false);
        bubble.set_translate_lang("Indonesia".into());
        bubble.set_translate_text(sample.into());
        bubble.set_translate_styled(model::styled(sample));
        sync_bubble_window_size(&bubble);
        bubble.invoke_scroll_to_top();
        bubble_scroll.borrow_mut().on_content_updated(
            sample,
            true,
            std::time::Instant::now(),
        );
    }
    if args.iter().any(|a| a == "--test-hud-rich") {
        let (who_user, user_prompt, sample) = if app_core::is_en() {
            (
                "YOU",
                "Show heading, checklist, blockquote, table, divider, and code block rendering!",
                concat!(
                    "# Unified Styling Guide\n",
                    "All blocks render cleanly with sharp contrast on the translucent HUD.\n\n",
                    "### 1. Lists & Checklist\n",
                    "- Key point with [cyan link](https://example.com) and `inline_code`\n",
                    "  - Nested sub-item aligns neatly when wrapping long lines\n",
                    "- [x] Completed checklist item with crisp icon\n",
                    "- [ ] Next checklist item to review\n\n",
                    "### 2. Blockquote & Table\n",
                    "> Blockquotes use a dedicated cyan-accented card.\n",
                    "> — Aemeath Notes 💻\n\n",
                    "| Feature | Markdown Syntax | Preview |\n",
                    "| :--- | :---: | ---: |\n",
                    "| Italic | `*text*` | *italic* |\n",
                    "| Bold | `**text**` | **bold** |\n",
                    "| Link | `[label](url)` | [Click](https://example.com) |\n\n",
                    "---\n\n",
                    "### 3. Code Block\n",
                    "```python\n",
                    "def greet(name: str) -> str:\n",
                    "    return f\"Hello, {name}! Welcome to Startorch.\"\n\n",
                    "print(greet(\"You\"))\n",
                    "```\n\n",
                    "✦ **Aemeath Tip**: Write standard Markdown just as usual."
                ),
            )
        } else {
            (
                "ANDA",
                "Tunjukkan hasil render heading, checklist, blockquote, tabel, divider, dan code block!",
                concat!(
                    "# Panduan Styling Terpadu\n",
                    "Semua blok kini dirender seragam dengan kontras jelas di latar transparan.\n\n",
                    "### 1. Daftar & Checklist\n",
                    "- Poin utama dengan [tautan cyan](https://example.com) dan `inline_code`\n",
                    "  - Sub-poin tetap sejajar lurus ke bawah saat baris panjang membungkus rapi\n",
                    "- [x] Checklist selesai dengan ikon tajam\n",
                    "- [ ] Checklist berikutnya yang perlu dicek\n\n",
                    "### 2. Kutipan & Tabel\n",
                    "> Baris kutipan memakai kartu aksen cyan.\n",
                    "> — Catatan Aemeath 💻\n\n",
                    "| Fitur | Simbol Markdown | Contoh Hasil |\n",
                    "| :--- | :---: | ---: |\n",
                    "| Italic | `*teks*` | *miring* |\n",
                    "| Bold | `**teks**` | **tebal** |\n",
                    "| Link | `[teks](url)` | [Klik](https://example.com) |\n\n",
                    "---\n\n",
                    "### 3. Code Block\n",
                    "```python\n",
                    "def sapa(nama: str) -> str:\n",
                    "    return f\"Halo, {nama}! Selamat datang di Startorch.\"\n\n",
                    "print(sapa(\"Kamu\"))\n",
                    "```\n\n",
                    "✦ **Tips Aemeath**: Cukup tulis Markdown standar seperti biasa."
                ),
            )
        };
        chat_messages.set_vec(vec![
            model::message("user", who_user, user_prompt, "", false),
            model::message("assistant", "AEMEATH", sample, "", true),
        ]);
        sync_chat_height(&chat_messages);
    }
    if args.iter().any(|a| a == "--test-media-rich") {
        let sample = sample_media_rich_markdown();
        let mut assistant = model::message_thought(
            "assistant",
            "AEMEATH",
            &sample,
            "",
            true,
            "• Memeriksa dukungan emoji berwarna Direct2D/COLRv1\n• Membuat grafik SVG performa & lencana PNG\n• Menyiapkan tabel lebar 5 kolom + code block responsif",
        );
        assistant.think_label = "Berpikir selama 4 detik".into();
        chat_messages.set_vec(vec![
            model::message(
                "user",
                "ANDA",
                "Tunjukkan respons kaya dengan emoji berwarna, diagram SVG, gambar PNG, tabel lebar, dan code block!",
                "",
                false,
            ),
            assistant,
        ]);
        sync_chat_height(&chat_messages);
    }
    if args.iter().any(|a| a == "--open-lightbox") {
        let hud_lb = hud.as_weak();
        let msgs_lb = chat_messages.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(600), move || {
            if let Some(h) = hud_lb.upgrade() {
                'outer: for mi in 0..msgs_lb.row_count() {
                    if let Some(m) = msgs_lb.row_data(mi) {
                        for bi in 0..m.blocks.row_count() {
                            if let Some(b) = m.blocks.row_data(bi) {
                                if b.kind == "image" && !b.img_loading && !b.img_failed {
                                    h.set_lightbox_image(b.img);
                                    h.set_lightbox_caption(b.img_caption);
                                    h.set_lightbox_url(b.img_url);
                                    h.set_lightbox_raw(b.raw);
                                    h.set_lightbox_open(true);
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
            }
        });
    }
    if ui_test_active() {
        schedule_ui_interaction_tests(
            hud.as_weak(),
            bubble.as_weak(),
            panel.as_weak(),
            chat_messages.clone(),
        );
    }
    // `--shot-hud` grabs the floating HUD (it is raised above the panel by
    // `--hud`), which is the only way to inspect the dropdown without a mouse.
    if args.iter().any(|a| a == "--shot-hud") {
        let hud_shot = hud.as_weak();
        let scroll_top = args.iter().any(|a| a == "--scroll-top");
        let scroll_bottom = args.iter().any(|a| a == "--scroll-bottom");
        let out_path = args_flag_value("--shot-out")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("hud_shot.png"));
        slint::Timer::single_shot(std::time::Duration::from_millis(1200), move || {
            if let Some(h) = hud_shot.upgrade() {
                if scroll_top {
                    h.invoke_scroll_to_top();
                } else if scroll_bottom {
                    h.invoke_scroll_to_bottom();
                }
                let weak_snap = hud_shot.clone();
                slint::Timer::single_shot(std::time::Duration::from_millis(400), move || {
                    if let Some(h2) = weak_snap.upgrade() {
                        save_window_snapshot(h2.window(), &out_path, "hud");
                    }
                    let _ = slint::quit_event_loop();
                    std::process::exit(0);
                });
            } else {
                let _ = slint::quit_event_loop();
                std::process::exit(0);
            }
        });
    }
    if args.iter().any(|a| a == "--sing-demo") {
        bubble.set_singing(true);
        bubble.set_sing_phase(1);
        let edge = bubble.get_edge();
        bubble.set_avatar(bubble_sprites.singing(edge.as_str(), 1));
    }
    if args.iter().any(|a| a == "--shot-bubble") {
        let bubble_shot = bubble.as_weak();
        let out_path = args_flag_value("--shot-out")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("bubble_shot.png"));
        slint::Timer::single_shot(std::time::Duration::from_millis(1200), move || {
            if let Some(b) = bubble_shot.upgrade() {
                save_window_snapshot(b.window(), &out_path, "bubble");
            }
            let _ = slint::quit_event_loop();
            std::process::exit(0);
        });
    }
    if args.iter().any(|a| a == "--shot-panel-native") {
        let panel_shot = panel.as_weak();
        let scroll_y = args_flag_value("--panel-scroll").and_then(|v| v.parse::<f32>().ok());
        let out_path = args_flag_value("--shot-out")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("panel_shot.png"));
        slint::Timer::single_shot(std::time::Duration::from_millis(1200), move || {
            if let Some(p) = panel_shot.upgrade() {
                if let Some(y) = scroll_y {
                    p.invoke_scroll_to(y);
                }
                save_window_snapshot(p.window(), &out_path, "panel");
            }
            let _ = slint::quit_event_loop();
            std::process::exit(0);
        });
    }
        if args.iter().any(|a| a == "--shot-panel") {
            let core_shot = core.clone();
            let wait = if args.iter().any(|a| a == "--diag-run") { 9000 } else { 1500 };
            slint::Timer::single_shot(std::time::Duration::from_millis(wait), move || {
                match core_shot.capture_to_file(false) {
                    Ok(p) => eprintln!("[test] panel shot: {}", p.display()),
                    Err(e) => eprintln!("[test] panel shot failed: {e}"),
                }
                let _ = slint::quit_event_loop();
            });
        }
    }

    slint::run_event_loop()?;
    Ok(())
}

/// Provider API location. Defaults match backend-api-ai's own defaults.
fn provider_config() -> app_core::ProviderConfig {
    app_core::ProviderConfig {
        base_url: std::env::var("MINI_API_URL").unwrap_or_else(|_| "http://127.0.0.1:8787".into()),
        api_key: std::env::var("MINI_API_KEY").ok().filter(|s| !s.is_empty()),
        admin_key: std::env::var("MINI_ADMIN_KEY").ok().filter(|s| !s.is_empty()),
    }
}

#[inline(always)]
fn ui_test_active() -> bool {
    #[cfg(feature = "dev-tools")]
    {
        std::env::args().any(|a| a == "--run-ui-tests")
    }
    #[cfg(not(feature = "dev-tools"))]
    {
        false
    }
}

/// All values following a repeatable flag, in order.
#[cfg(feature = "dev-tools")]
fn args_flag_values(flag: &str) -> Vec<String> {
    let args: Vec<String> = std::env::args().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            if let Some(v) = args.get(i + 1) {
                out.push(v.clone());
                i += 1;
            }
        }
        i += 1;
    }
    out
}

/// First value following a single-use flag.
#[cfg(feature = "dev-tools")]
fn args_flag_value(flag: &str) -> Option<String> {
    args_flag_values(flag).into_iter().next()
}

#[cfg(feature = "dev-tools")]
fn save_window_snapshot(win: &slint::Window, out_path: &std::path::Path, label: &str) {
    match win.take_snapshot() {
        Ok(snap) => {
            let pixels: &slint::SharedPixelBuffer<slint::Rgba8Pixel> = &snap;
            if let Err(e) = image::save_buffer(
                out_path,
                pixels.as_bytes(),
                pixels.width(),
                pixels.height(),
                image::ExtendedColorType::Rgba8,
            ) {
                eprintln!("[test] save {label} shot failed: {e}");
            } else {
                eprintln!("[test] {label} snapshot saved: {}", out_path.display());
            }
        }
        Err(e) => eprintln!("[test] {label} take_snapshot failed: {e}"),
    }
}

/// Parses "--attach a.png --attach b.png --send ..." so multiple attachments
/// can accumulate for the message that follows them.
#[cfg(feature = "dev-tools")]
fn parse_send_plan() -> (Vec<String>, Vec<Vec<String>>) {
    let args: Vec<String> = std::env::args().collect();
    let mut sends = Vec::new();
    let mut attaches: Vec<Vec<String>> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--send" => {
                if let Some(v) = args.get(i + 1) {
                    sends.push(v.clone());
                    attaches.push(std::mem::take(&mut pending));
                    i += 1;
                }
            }
            "--attach" => {
                if let Some(v) = args.get(i + 1) {
                    pending.push(v.clone());
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    (sends, attaches)
}

#[cfg(feature = "dev-tools")]
thread_local! {
    static UI_TEST_URLS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static UI_TEST_CODES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static UI_TEST_SENDS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

#[cfg(feature = "dev-tools")]
fn encode_base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}

#[cfg(feature = "dev-tools")]
fn make_test_png_bytes(w: u32, h: u32) -> Vec<u8> {
    let mut img = image::RgbaImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let border = x < 2 || y < 2 || x + 2 >= w || y + 2 >= h;
        if border {
            *px = image::Rgba([92, 225, 255, 255]);
        } else {
            let t = x as f32 / (w.max(1) as f32);
            let r = (24.0 + 180.0 * t) as u8;
            let g = (110.0 + 70.0 * (1.0 - t)) as u8;
            let b = (180.0 + 60.0 * (y as f32 / h.max(1) as f32)) as u8;
            *px = image::Rgba([r, g, b, 255]);
        }
    }
    let mut buf = Vec::new();
    let _ = image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png);
    buf
}

#[cfg(feature = "dev-tools")]
fn sample_media_rich_markdown() -> String {
    let png_b64 = encode_base64(&make_test_png_bytes(96, 36));
    let svg_chart = concat!(
        "```svg\n",
        "<svg viewBox=\"0 0 300 150\">\n",
        "  <rect x=\"0\" y=\"0\" width=\"300\" height=\"150\" rx=\"10\" fill=\"#0A1528\"/>\n",
        "  <text x=\"16\" y=\"26\" fill=\"#5CE1FF\" font-size=\"13\" font-family=\"sans-serif\" font-weight=\"bold\">Latensi Pipeline AI (ms)</text>\n",
        "  <rect x=\"24\" y=\"48\" width=\"110\" height=\"22\" rx=\"4\" fill=\"#3CE8FF\"/>\n",
        "  <text x=\"142\" y=\"63\" fill=\"#E2E8F0\" font-size=\"11\" font-family=\"sans-serif\">OCR 110ms</text>\n",
        "  <rect x=\"24\" y=\"78\" width=\"210\" height=\"22\" rx=\"4\" fill=\"#FF6BA9\"/>\n",
        "  <text x=\"242\" y=\"93\" fill=\"#E2E8F0\" font-size=\"11\" font-family=\"sans-serif\">LLM 210ms</text>\n",
        "  <rect x=\"24\" y=\"108\" width=\"65\" height=\"22\" rx=\"4\" fill=\"#34D399\"/>\n",
        "  <text x=\"97\" y=\"123\" fill=\"#E2E8F0\" font-size=\"11\" font-family=\"sans-serif\">Render 18ms</text>\n",
        "</svg>\n",
        "```"
    );
    format!(
        concat!(
            "🎉 ✨ 🚀\n\n",
            "### 🚀 Dukungan Emoji Berwarna & Media\n",
            "Sekarang respons Aemeath bisa memuat reaksi emoji penuh warna, diagram vektor SVG interaktif, serta gambar PNG.\n\n",
            "- 🎨 Diagram **SVG tajam** bisa diperbesar langsung di Lightbox\n",
            "- 💡 Shortcut `:sparkles:` dan emoji bawaan langsung penuh warna\n",
            "- [x] Validasi streaming aman saat tag belum tertutup ✅\n",
            "- [ ] Task yang masih menunggu (non-checklist) rapi seukuran badge\n",
            "- [-] Item gagal atau ditolak berwarna merah ❌\n\n",
            "{svg}\n\n",
            "![Lencana PNG Startorch](data:image/png;base64,{b64})\n\n",
            "### 📊 Tabel Lebar 5 Kolom (Scroll Horizontal)\n",
            "| Modul | Ukuran Font | Target Latensi | Status Uji | Catatan Teknis |\n",
            "| :--- | :---: | ---: | :---: | :--- |\n",
            "| `emoji.rs` | 14px | 12ms | Lulus | Direct2D COLRv1 bitmap cache |\n",
            "| `model.rs` | 14px | 6ms | Lulus | Parser blok + SVG & PNG decoder |\n",
            "| `HudWindow` | 18px | 8ms | Lulus | Lightbox zoom & scroll responsif |\n\n",
            "```python\n",
            "def render_media_summary(items: list[str], mode: str = \"Startorch Ultra Crisp\") -> dict:\n",
            "    return {{\"count\": len(items), \"mode\": mode, \"status\": \"siap_digunakan_di_desktop_windows\"}}\n",
            "```\n\n",
            "✦ **Tips Aemeath**: Klik kartu diagram SVG atau gambar PNG di atas untuk membuka Lightbox zoom!"
        ),
        svg = svg_chart,
        b64 = png_b64,
    )
}

#[cfg(feature = "dev-tools")]
fn dispatch_click(win: &slint::Window, x: f32, y: f32) {
    let pos = slint::LogicalPosition::new(x, y);
    win.dispatch_event(slint::platform::WindowEvent::PointerMoved { position: pos });
    win.dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position: pos,
        button: slint::platform::PointerEventButton::Left,
    });
    win.dispatch_event(slint::platform::WindowEvent::PointerReleased {
        position: pos,
        button: slint::platform::PointerEventButton::Left,
    });
}

#[cfg(feature = "dev-tools")]
fn dispatch_drag(win: &slint::Window, x1: f32, y1: f32, x2: f32, y2: f32) {
    let p1 = slint::LogicalPosition::new(x1, y1);
    let p_mid = slint::LogicalPosition::new((x1 + x2) * 0.5, (y1 + y2) * 0.5);
    let p2 = slint::LogicalPosition::new(x2, y2);
    win.dispatch_event(slint::platform::WindowEvent::PointerMoved { position: p1 });
    win.dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position: p1,
        button: slint::platform::PointerEventButton::Left,
    });
    win.dispatch_event(slint::platform::WindowEvent::PointerMoved { position: p_mid });
    win.dispatch_event(slint::platform::WindowEvent::PointerMoved { position: p2 });
    win.dispatch_event(slint::platform::WindowEvent::PointerReleased {
        position: p2,
        button: slint::platform::PointerEventButton::Left,
    });
}

#[cfg(feature = "dev-tools")]
fn dispatch_ctrl_c(win: &slint::Window) {
    win.dispatch_event(slint::platform::WindowEvent::KeyPressed {
        text: slint::platform::Key::Control.into(),
    });
    win.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: "c".into() });
    win.dispatch_event(slint::platform::WindowEvent::KeyReleased { text: "c".into() });
    win.dispatch_event(slint::platform::WindowEvent::KeyReleased {
        text: slint::platform::Key::Control.into(),
    });
}

#[cfg(feature = "dev-tools")]
fn schedule_ui_interaction_tests(
    hud_weak: slint::Weak<HudWindow>,
    bubble_weak: slint::Weak<BubbleWindow>,
    panel_weak: slint::Weak<MainPanel>,
    messages: std::rc::Rc<slint::VecModel<HudMessage>>,
) {
    use std::cell::Cell;
    use std::rc::Rc;

    struct TestState {
        stage: usize,
        failures: Vec<String>,
        scroll_baseline: f32,
        srv_port: u16,
        srv_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        poll_ticks: usize,
    }

    // Launch real loopback HTTP server serving a valid PNG for async fetch testing
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback png server");
    let srv_port = listener.local_addr().expect("local addr").port();
    let png_payload = make_test_png_bytes(64, 32);
    let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_bg = stop_flag.clone();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        let _ = listener.set_nonblocking(true);
        while !stop_bg.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
                    let mut req_buf = [0u8; 1024];
                    let _ = stream.read(&mut req_buf);
                    let hdr = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        png_payload.len()
                    );
                    let _ = stream.write_all(hdr.as_bytes());
                    let _ = stream.write_all(&png_payload);
                    let _ = stream.flush();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(15));
                }
                Err(_) => break,
            }
        }
    });

    let state = Rc::new(RefCell::new(TestState {
        stage: 0,
        failures: Vec::new(),
        scroll_baseline: 0.0,
        srv_port,
        srv_stop: Some(stop_flag),
        poll_ticks: 0,
    }));

    let timer = Rc::new(slint::Timer::default());
    let timer_cb = timer.clone();
    let tick_guard = Rc::new(Cell::new(false));

    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(160),
        move || {
            if tick_guard.replace(true) {
                return;
            }
            let Some(hud) = hud_weak.upgrade() else {
                return;
            };
            let Some(bubble) = bubble_weak.upgrade() else {
                return;
            };

            let mut st = state.borrow_mut();
            let check = |cond: bool, msg: &str, failures: &mut Vec<String>| {
                if cond {
                    eprintln!("[ui-test] PASS: {msg}");
                } else {
                    eprintln!("[ui-test] FAIL: {msg}");
                    failures.push(msg.to_string());
                }
            };

            match st.stage {
                // Stage 0: Setup Code Card + Overflow + Tall tail for click/clipboard/wheel tests
                0 => {
                    // Isolate layout pixel assertions from user's live Control Panel font settings
                    hud.set_chat_font_px(14.0);
                    hud.set_chat_font_family("Aemeath Sans".into());
                    let long_tail = (0..14)
                        .map(|n| format!("Paragraf uji scroll #{n}: memastikan viewport HUD tetap menggulir mulus saat kursor berada di atas blok kode atau tabel."))
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    let code_doc = format!(
                        "```python\ndef uji_panjang(alpha: str, beta: str = \"konstanta_super_panjang_yang_melebihi_lebar_viewport_hud\") -> str:\n    return f\"{{alpha}} :: {{beta}}\"\n```\n\n{long_tail}"
                    );
                    messages.set_vec(vec![model::message("assistant", "AEMEATH", &code_doc, "", true)]);
                    sync_chat_height(&messages);
                    hud.invoke_scroll_to_top();
                    st.stage = 1;
                }
                // Stage 1: Real pointer click on [Salin] button -> verify callback + Win32 clipboard,
                // then send vertical PointerScrolled right over code-vp!
                1 => {
                    UI_TEST_CODES.with(|v| v.borrow_mut().clear());
                    // asst-header (28) + spacing (9) + divider (1) + spacing (9) + top paddings (69) => code top at y=116, button at [291..349, 121..141]
                    dispatch_click(hud.window(), 320.0, 131.0);
                    let cb_codes = UI_TEST_CODES.with(|v| v.borrow().clone());
                    let expected_snippet = "def uji_panjang(";
                    check(
                        cb_codes.len() == 1 && cb_codes[0].contains(expected_snippet),
                        "Real pointer click on Code [Salin] triggered copy-code callback",
                        &mut st.failures,
                    );
                    let clip = win32::read_clipboard_text().unwrap_or_default();
                    check(
                        clip.contains(expected_snippet),
                        &format!("Windows clipboard received copied code (len={})", clip.len()),
                        &mut st.failures,
                    );

                    // Now test vertical wheel over code-vp (x=180, y=165)
                    st.scroll_baseline = hud.get_chat_scroll_y();
                    hud.window().dispatch_event(slint::platform::WindowEvent::PointerMoved {
                        position: slint::LogicalPosition::new(180.0, 165.0),
                    });
                    hud.window().dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                        position: slint::LogicalPosition::new(180.0, 165.0),
                        delta_x: 0.0,
                        delta_y: -120.0,
                    });
                    st.stage = 2;
                }
                // Stage 2: Assert vertical scroll happened over code-vp, then set up wide 5-col table + link
                2 => {
                    let after_code_wheel = hud.get_chat_scroll_y();
                    check(
                        after_code_wheel > st.scroll_baseline + 5.0,
                        &format!(
                            "Vertical mouse wheel over overflowing code-vp scrolled outer chat-vp ({:.1} -> {:.1})",
                            st.scroll_baseline, after_code_wheel
                        ),
                        &mut st.failures,
                    );

                    let long_tail = (0..12)
                        .map(|n| format!("Teks pengisi scroll tabel #{n}: memeriksa gulir vertikal dan horizontal."))
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    let tbl_doc = format!(
                        "| Aksi Link | Kolom Dua Lebar | Kolom Tiga | Kolom Empat | Kolom Lima Tambahan |\n\
                         | :--- | :---: | :---: | :---: | :--- |\n\
                         | [TautanTabel](https://example.com/tbl-ok) | Nilai Tengah Panjang | Data Tiga | Data Empat | Keterangan kolom terakhir yang cukup panjang |\n\n\
                         - Poin link daftar: [TautanList](https://example.com/list-ok)\n\n{long_tail}"
                    );
                    messages.set_vec(vec![model::message("assistant", "AEMEATH", &tbl_doc, "", true)]);
                    sync_chat_height(&messages);
                    hud.invoke_scroll_to_top();
                    st.stage = 3;
                }
                // Stage 3: Click table hyperlink + vertical wheel over tbl-vp
                3 => {
                    UI_TEST_URLS.with(|v| v.borrow_mut().clear());
                    // First data cell is around x=52, y=150; list link below table around y=218
                    dispatch_click(hud.window(), 52.0, 150.0);
                    let mut urls = UI_TEST_URLS.with(|v| v.borrow().clone());
                    if urls.is_empty() {
                        // Sweep a few pixels inside col 0 row 1 in case DPI rounding shifts link baseline
                        for dy in [145.0f32, 153.0, 158.0, 165.0, 172.0] {
                            dispatch_click(hud.window(), 52.0, dy);
                            urls = UI_TEST_URLS.with(|v| v.borrow().clone());
                            if !urls.is_empty() {
                                break;
                            }
                        }
                    }
                    check(
                        urls.iter().any(|u| u == "https://example.com/tbl-ok"),
                        &format!("Real pointer click on Table hyperlink invoked open-url ({urls:?})"),
                        &mut st.failures,
                    );

                    // Vertical wheel over tbl-vp
                    st.scroll_baseline = hud.get_chat_scroll_y();
                    hud.window().dispatch_event(slint::platform::WindowEvent::PointerMoved {
                        position: slint::LogicalPosition::new(180.0, 145.0),
                    });
                    hud.window().dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                        position: slint::LogicalPosition::new(180.0, 145.0),
                        delta_x: 0.0,
                        delta_y: -120.0,
                    });
                    // Horizontal wheel test too on wide tbl-vp
                    hud.window().dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                        position: slint::LogicalPosition::new(180.0, 145.0),
                        delta_x: -90.0,
                        delta_y: 0.0,
                    });
                    st.stage = 4;
                }
                // Stage 4: Verify vertical scroll over tbl-vp, then test SVG card -> Lightbox open/close + HTTP remote PNG
                4 => {
                    let after_tbl_wheel = hud.get_chat_scroll_y();
                    check(
                        after_tbl_wheel > st.scroll_baseline + 5.0,
                        &format!(
                            "Vertical mouse wheel over wide tbl-vp scrolled outer chat-vp ({:.1} -> {:.1})",
                            st.scroll_baseline, after_tbl_wheel
                        ),
                        &mut st.failures,
                    );

                    let port = st.srv_port;
                    let svg_and_remote = format!(
                        "```svg\n<svg viewBox=\"0 0 280 120\"><rect width=\"280\" height=\"120\" fill=\"#0D192B\"/><circle cx=\"140\" cy=\"60\" r=\"40\" fill=\"#5CE1FF\"/></svg>\n```\n\n![Server PNG](http://127.0.0.1:{port}/badge.png)"
                    );
                    let m = model::message_thought(
                        "assistant",
                        "AEMEATH",
                        &svg_and_remote,
                        "",
                        true,
                        "• Menguji lightbox SVG & unduhan gambar HTTP lokal",
                    );
                    messages.set_vec(vec![m]);
                    sync_chat_height(&messages);
                    hud.invoke_scroll_to_top();
                    st.poll_ticks = 0;
                    st.stage = 5;
                }
                // Stage 5: Click SVG image canvas -> opens Lightbox -> Escape closes Lightbox, + wait for local HTTP PNG
                5 => {
                    let first_msg = messages.row_data(0).unwrap();
                    let blk0 = first_msg.blocks.row_data(0).unwrap();
                    check(
                        blk0.kind == "image" && blk0.lang == "svg" && blk0.img_w > 0,
                        "SVG fenced card decoded into interactive image block",
                        &mut st.failures,
                    );

                    // Notice Thought pill adds ~38px above body! Wait: click SVG canvas around (160, 195)
                    dispatch_click(hud.window(), 160.0, 195.0);
                    check(
                        hud.get_lightbox_open() && hud.get_lightbox_raw().contains("<circle"),
                        "Clicking SVG image card opened in-HUD Lightbox with SVG source",
                        &mut st.failures,
                    );

                    // Press Escape inside Lightbox to close
                    hud.window().dispatch_event(slint::platform::WindowEvent::KeyPressed {
                        text: slint::platform::Key::Escape.into(),
                    });
                    if hud.get_lightbox_open() {
                        // Also test close button at top-right (x=354, y=30)
                        dispatch_click(hud.window(), 354.0, 30.0);
                    }
                    check(
                        !hud.get_lightbox_open(),
                        "Closing Lightbox reset lightbox-open to false",
                        &mut st.failures,
                    );
                    st.stage = 6;
                }
                // Stage 6: Verify async HTTP PNG fetch finished cleanly via pump_remote_images
                6 => {
                    model::pump_remote_images(&messages);
                    let m = messages.row_data(0).unwrap();
                    let remote_blk = m.blocks.row_data(1).unwrap();
                    if remote_blk.img_loading && st.poll_ticks < 15 {
                        st.poll_ticks += 1;
                    } else {
                        check(
                            !remote_blk.img_loading
                                && !remote_blk.img_failed
                                && remote_blk.img_w == 64
                                && remote_blk.img_h == 32,
                            &format!(
                                "Localhost HTTP PNG image fetched & decoded cleanly (loading={}, failed={}, {}x{})",
                                remote_blk.img_loading, remote_blk.img_failed, remote_blk.img_w, remote_blk.img_h
                            ),
                            &mut st.failures,
                        );
                        if let Some(flag) = st.srv_stop.take() {
                            flag.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        st.stage = 7;
                    }
                }
                // Stage 7: Streaming chunks, Thought/Detail/Search toggles, Font sizes & families, Retry reset, BubbleWindow
                7 => {
                    // 7a. Incremental streaming: unclosed SVG -> closed SVG + emoji row + table
                    let mut m = model::message("assistant", "AEMEATH", "", "", true);
                    m.thought = "• Analisis bertahap...".into();
                    m.think_label = "Berpikir selama 2 detik".into();
                    m.meta = "Provider Test · 42 tok".into();
                    m.search_count = 1;
                    let srcs = vec![model::build_search_source_item(
                        "Startorch Docs",
                        "https://example.com",
                        "Dokumentasi resmi Startorch untuk pencarian web.",
                        "Startorch",
                        "2026/11/16",
                    )];
                    m.sources = ModelRc::new(VecModel::from(srcs));
                    model::set_body(&mut m, "Halo :wave:\n```svg\n<svg viewBox='0 0 100 50'><rect width='100' height='50'/>");
                    check(
                        m.blocks.row_count() == 2
                            && m.blocks.row_data(1).unwrap().kind == "code"
                            && m.blocks.row_data(1).unwrap().lang == "svg",
                        "Streaming unclosed SVG renders safely as code block until closed",
                        &mut st.failures,
                    );

                    let full_stream = format!(
                        "🎉 ✨\n\n### 🚀 Streaming Selesai\n```svg\n<svg viewBox=\"0 0 100 50\"><rect width=\"100\" height=\"50\" fill=\"#3CE8FF\"/></svg>\n```\n\n| K | V |\n|---|---|\n| Ok | [Url](https://example.com) |\n\n![PNG](data:image/png;base64,{})",
                        encode_base64(&make_test_png_bytes(48, 24))
                    );
                    model::set_body(&mut m, &full_stream);
                    model::layout(&mut m);
                    check(
                        m.blocks.row_data(0).unwrap().kind == "emoji-row"
                            && m.blocks.row_data(1).unwrap().kind == "heading"
                            && m.blocks.row_data(1).unwrap().has_emoji_img
                            && m.blocks.row_data(2).unwrap().kind == "image"
                            && m.blocks.row_data(3).unwrap().kind == "table"
                            && m.blocks.row_data(4).unwrap().kind == "image",
                        "Completed stream promoted SVG -> image + color emoji-row + heading badge + table + PNG",
                        &mut st.failures,
                    );

                    messages.set_vec(vec![m]);
                    sync_chat_height(&messages);

                    // 7b. Thought / Detail / Search toggles update height cleanly
                    let h_closed = messages.row_data(0).unwrap().height;
                    hud.invoke_toggle_thought(0);
                    let h_thought = messages.row_data(0).unwrap().height;
                    hud.invoke_toggle_detail(0);
                    let h_detail = messages.row_data(0).unwrap().height;
                    hud.invoke_toggle_search_sources(0);
                    let m_after = messages.row_data(0).unwrap();
                    check(
                        m_after.thought_open
                            && m_after.detail_open
                            && m_after.search_open
                            && h_thought > h_closed
                            && h_detail > h_thought,
                        &format!(
                            "Thought/Detail/Search toggles updated state & layout height ({h_closed} -> {h_thought} -> {h_detail})"
                        ),
                        &mut st.failures,
                    );

                    // 7c. Font sizes & families
                    for px in [12.0f32, 14.0, 18.0] {
                        hud.set_chat_font_px(px);
                    }
                    for fam in ["Aemeath Sans", "Segoe UI"] {
                        hud.set_chat_font_family(fam.into());
                    }
                    check(
                        (hud.get_chat_font_px() - 18.0).abs() < 0.1
                            && hud.get_chat_font_family() == "Segoe UI",
                        "HUD chat font sizes (12/14/18px) & font families applied cleanly",
                        &mut st.failures,
                    );

                    // 7d. Retry reset clears blocks cleanly
                    let mut m_reset = messages.row_data(0).unwrap();
                    model::set_body(&mut m_reset, "");
                    check(
                        m_reset.blocks.row_count() == 0,
                        "Retry reset cleared structured blocks cleanly",
                        &mut st.failures,
                    );

                    // 7e. BubbleWindow compact markdown fallback must NOT leak raw SVG/base64
                    let bubble_pre = model::preprocess_markdown(&full_stream);
                    check(
                        !bubble_pre.contains("iVBOR")
                            && !bubble_pre.contains("<rect")
                            && bubble_pre.contains("[Diagram SVG]")
                            && bubble_pre.contains("[Gambar: PNG]"),
                        "BubbleWindow preprocess_markdown hides raw base64/SVG XML and shows clean summaries",
                        &mut st.failures,
                    );
                    bubble.set_translate_visible(true);
                    bubble.set_translate_busy(false);
                    bubble.invoke_scroll_to_top();
                    let long_bubble_doc = (0..18)
                        .map(|n| format!("- Baris panjang bubble #{n}: uji scroll otomatis per halaman saat jawaban melebihi tinggi 340px."))
                        .collect::<Vec<_>>()
                        .join("\n");
                    update_bubble_stream_text(&bubble, &long_bubble_doc, true);

                    st.stage = 8;
                }
                // Stage 8: Enter-to-send, Salin->Disalin, Salin Sebagian modal, Search modal,
                // Checkboxes (✅/☐/❌), Tips blockquote, and Auto-scroll setup
                8 => {
                    // 8a. Enter-to-send toggle off vs on via REAL keyboard events on TextInput
                    UI_TEST_SENDS.with(|v| v.borrow_mut().clear());
                    dispatch_click(hud.window(), 150.0, 588.0);
                    hud.set_enter_sends(false);
                    hud.set_input_text("Baris Satu".into());
                    hud.window().dispatch_event(slint::platform::WindowEvent::KeyPressed {
                        text: slint::platform::Key::Return.into(),
                    });
                    hud.window().dispatch_event(slint::platform::WindowEvent::KeyReleased {
                        text: slint::platform::Key::Return.into(),
                    });
                    let after_off_sends = UI_TEST_SENDS.with(|v| v.borrow().clone());
                    let after_off_text = hud.get_input_text().to_string();
                    check(
                        after_off_sends.is_empty() && after_off_text.contains('\n'),
                        "Enter with enter-sends=false inserted newline without sending",
                        &mut st.failures,
                    );

                    hud.set_enter_sends(true);
                    hud.set_input_text("Kirim Lewat Enter".into());
                    hud.window().dispatch_event(slint::platform::WindowEvent::KeyPressed {
                        text: slint::platform::Key::Return.into(),
                    });
                    hud.window().dispatch_event(slint::platform::WindowEvent::KeyReleased {
                        text: slint::platform::Key::Return.into(),
                    });
                    let after_on_sends = UI_TEST_SENDS.with(|v| v.borrow().clone());
                    check(
                        after_on_sends == vec!["Kirim Lewat Enter".to_string()]
                            && hud.get_input_text().is_empty(),
                        "Enter with enter-sends=true submitted message and cleared composer",
                        &mut st.failures,
                    );

                    // 8b. Checkboxes (done ✅, todo ☐, cross ❌), Tips blockquote, Search modal, Copy/Partial Copy
                    let long_paras = (0..10)
                        .map(|n| format!("Paragraf uji scroll otomatis #{n}: konten panjang untuk memicu overflow vertikal di HUD."))
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    let rich_doc = format!(
                        concat!(
                            "- [x] Checklist hijau ✅\n",
                            "- [ ] Non-checklist rapi\n",
                            "- [-] Item silang merah ❌\n\n",
                            "Status inline: sukses ✅ atau gagal ❌.\n\n",
                            "{}\n\n",
                            "✦ **Tips Aemeath:** Selalu cek kotak pencarian web untuk sumber akurat!"
                        ),
                        long_paras
                    );
                    let search_items = vec![
                        model::build_search_source_item(
                            "Survei Nasional Terkini - Kabar Istana",
                            "https://www.antaranews.com/berita/1",
                            "Ringkasan survei Parameter Politik Indonesia terbaru.",
                            "ANTARA News",
                            "2026/10/17",
                        ),
                        model::build_search_source_item(
                            "Berita Terkini RRI Digital",
                            "https://rri.co.id/nasional/2",
                            "Laporan perkembangan kebijakan nasional terkini.",
                            "RRI.co.id",
                            "2026/11/13",
                        ),
                    ];
                    let msg = model::message_full(
                        "assistant",
                        "AEMEATH",
                        &rich_doc,
                        "",
                        true,
                        "",
                        "",
                        false,
                        37,
                        false,
                        &search_items,
                    );
                    let done_blk = msg.blocks.row_data(0).unwrap();
                    let todo_blk = msg.blocks.row_data(1).unwrap();
                    let cross_blk = msg.blocks.row_data(2).unwrap();
                    let last_idx = msg.blocks.row_count().saturating_sub(1);
                    let tips_blk = msg.blocks.row_data(last_idx).unwrap();
                    check(
                        done_blk.marker == "done"
                            && done_blk.emoji_img.size().width >= 32
                            && todo_blk.marker == "todo"
                            && todo_blk.emoji_img.size().width >= 32
                            && cross_blk.marker == "cross"
                            && cross_blk.emoji_img.size().width >= 32,
                        "Checkboxes (done ✅, todo ☐, cross ❌) rendered as crisp 40px status badges",
                        &mut st.failures,
                    );
                    check(
                        tips_blk.kind == "quote" && tips_blk.has_emoji_img,
                        "Tips Aemeath promoted to blockquote card with color lightbulb emoji",
                        &mut st.failures,
                    );

                    messages.set_vec(vec![msg]);
                    sync_chat_height_preserve(&messages, None);
                    hud.invoke_scroll_to_top();
                    st.stage = 9;
                }
                // Stage 9: After layout settles at top, test Salin, direct SelectableText Ctrl+C
                // without Salin Sebagian, inline Salin Sebagian toggle, Search modal, then scroll down
                9 => {
                    check(
                        hud.get_chat_scroll_y() < 2.0,
                        "HUD settled at top cleanly before direct-select & partial-copy checks",
                        &mut st.failures,
                    );

                    // Copy -> Disalin state + clipboard
                    hud.invoke_copy_message(0);
                    let clip_full = win32::read_clipboard_text().unwrap_or_default();
                    check(
                        messages.row_data(0).unwrap().copied && clip_full.contains("Checklist hijau"),
                        "Salin button set Disalin state and copied clean text to clipboard",
                        &mut st.failures,
                    );
                    st.stage = 10;
                }
                // Stage 10: Frame rendered after set_row_data; verify real mouse-drag selection WITHOUT Salin Sebagian
                10 => {
                    // Direct always-on text selection WITHOUT clicking Salin Sebagian:
                    // 1) Drag-select inside plain list item ("Non-checklist rapi") + Ctrl+C
                    win32::copy_text("__SENTINEL_LIST_SEL__");
                    dispatch_drag(hud.window(), 52.0, 155.0, 165.0, 155.0);
                    dispatch_ctrl_c(hud.window());
                    let clip_list_drag = win32::read_clipboard_text().unwrap_or_default();

                    // 2) Drag-select a partial slice inside Paragraf #0 (Line 1 at y=245) + Ctrl+C
                    win32::copy_text("__SENTINEL_DIRECT_SEL__");
                    dispatch_drag(hud.window(), 48.0, 245.0, 185.0, 245.0);
                    dispatch_ctrl_c(hud.window());
                    let clip_direct = win32::read_clipboard_text().unwrap_or_default();

                    let blocks = messages.row_data(0).unwrap().blocks;
                    let todo_blk = blocks.row_data(1).unwrap();
                    let prose0_blk = blocks.row_data(4).unwrap();
                    let user_plain_msg = model::message_full(
                        "user",
                        "ANDA",
                        "Teks user langsung bisa di-select tanpa mode khusus.",
                        "",
                        false,
                        "",
                        "",
                        false,
                        0,
                        false,
                        &[],
                    );
                    check(
                        todo_blk.plain_selectable
                            && prose0_blk.plain_selectable
                            && user_plain_msg.user_selectable
                            && clip_list_drag != "__SENTINEL_LIST_SEL__"
                            && !clip_list_drag.is_empty()
                            && clip_direct != "__SENTINEL_DIRECT_SEL__"
                            && !clip_direct.is_empty(),
                        &format!(
                            "Direct SelectableText mouse-drag highlight + Ctrl+C worked without Salin Sebagian (list={clip_list_drag:?}, prose={clip_direct:?})"
                        ),
                        &mut st.failures,
                    );

                    // Salin Sebagian inline toggle -> Salin Pilihan -> Tutup Pilih
                    hud.invoke_open_partial_copy(0);
                    let m_sel_on = messages.row_data(0).unwrap();
                    hud.invoke_prepare_partial_selection();
                    hud.invoke_commit_partial_selection(0);
                    let m_sel_copied = messages.row_data(0).unwrap();
                    let clip_part = win32::read_clipboard_text().unwrap_or_default();
                    hud.invoke_open_partial_copy(0);
                    let m_sel_off = messages.row_data(0).unwrap();
                    check(
                        m_sel_on.select_mode
                            && !m_sel_on.select_text.is_empty()
                            && m_sel_copied.select_copied
                            && clip_part.contains("Non-checklist rapi")
                            && !m_sel_off.select_mode,
                        "Salin Sebagian inline mode toggled SelectableText, copied selection, and restored blocks cleanly",
                        &mut st.failures,
                    );

                    // Search Results Modal ("Hasil pencarian")
                    hud.invoke_open_search_modal(0);
                    let sm_open = hud.get_search_modal_open();
                    let sm_count = hud.get_search_modal_sources().row_count();
                    hud.set_search_modal_open(false);
                    check(
                        sm_open && sm_count == 2,
                        "Web search pill callback opened Hasil pencarian modal with structured sources",
                        &mut st.failures,
                    );

                    // Trigger deferred bottom-scroll check
                    scroll_chat_to_bottom_deferred(&hud);
                    st.stage = 11;
                }
                // Stage 11: Verify deferred auto-scroll-down reached the bottom in HUD + BubbleWindow and finish
                11 => {
                    let y = hud.get_chat_scroll_y();
                    let max_y = hud.get_chat_scroll_max();
                    check(
                        max_y > 100.0 && y >= max_y - 4.0,
                        &format!("Auto-scroll-down reached bottom cleanly ({y:.1} / {max_y:.1})"),
                        &mut st.failures,
                    );
                    let b_y = bubble.get_trans_scroll_y();
                    let b_max = bubble.get_trans_scroll_max();
                    let starts_at_top = b_max > 100.0 && b_y < 4.0;
                    // Simulate smooth stop-and-glide progression directly against live BubbleWindow
                    let long_sim_text = (0..18)
                        .map(|n| format!("- Baris panjang bubble #{n}: uji scroll otomatis bertahap saat jawaban melebihi tinggi 340px."))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let t0 = std::time::Instant::now();
                    let mut now = t0;
                    let mut sim = bubble_scroll::BubbleAutoScroll::new(
                        true,
                        bubble_scroll::BubbleReadSpeed::Fast,
                    );
                    sim.on_content_updated(&long_sim_text, true, t0);
                    let mut deadline_reached = false;
                    let mut max_step_delta = 0.0_f32;
                    let mut stop_pauses = 0usize;
                    let mut was_animating = false;
                    for _ in 0..2400 {
                        now += std::time::Duration::from_millis(50);
                        let prev_y = bubble.get_trans_scroll_y();
                        let out = sim.tick(
                            now,
                            prev_y,
                            bubble.get_trans_scroll_max(),
                            bubble.get_trans_viewport_h(),
                            bubble.get_trans_content_h(),
                            false,
                        );
                        if let Some(target_y) = out.set_scroll_y {
                            max_step_delta = max_step_delta.max((target_y - prev_y).abs());
                            bubble.invoke_set_trans_scroll_y(target_y);
                        }
                        let anim_now = sim.is_animating();
                        if was_animating && !anim_now && sim.is_active() {
                            stop_pauses += 1;
                        }
                        was_animating = anim_now;
                        if out.set_deadline_secs.is_some() {
                            deadline_reached = true;
                            break;
                        }
                    }
                    let reached_bottom = bubble.get_trans_scroll_y() >= b_max - 4.0;
                    let smooth_no_jumps = max_step_delta > 0.1 && max_step_delta <= 8.0;
                    let has_stop_pauses = stop_pauses >= 1;

                    // Verify manual scroll immediately yields BubbleAutoScroll
                    bubble.invoke_scroll_to_top();
                    let t_manual = std::time::Instant::now();
                    let mut sim_manual = bubble_scroll::BubbleAutoScroll::new(
                        true,
                        bubble_scroll::BubbleReadSpeed::Normal,
                    );
                    sim_manual.on_content_updated(&long_sim_text, true, t_manual);
                    bubble.invoke_set_trans_scroll_y(40.0);
                    let _ = sim_manual.tick(
                        t_manual + std::time::Duration::from_millis(200),
                        bubble.get_trans_scroll_y(),
                        bubble.get_trans_scroll_max(),
                        bubble.get_trans_viewport_h(),
                        bubble.get_trans_content_h(),
                        false,
                    );
                    let yielded_on_manual = sim_manual.is_paused_by_user();

                    check(
                        starts_at_top
                            && deadline_reached
                            && reached_bottom
                            && smooth_no_jumps
                            && has_stop_pauses
                            && yielded_on_manual,
                        &format!(
                            "BubbleWindow starts at top ({b_y:.1}), glides smoothly (max_delta={max_step_delta:.2}px, pauses={stop_pauses}) to bottom ({:.1} / {b_max:.1}), and yields on manual scroll",
                            bubble.get_trans_scroll_y()
                        ),
                        &mut st.failures,
                    );
                    bubble.set_translate_visible(false);

                    // Stage 11a + 11b: Browser & Cookies multi-browser selector + Close Modal flow
                    if let Some(panel) = panel_weak.upgrade() {
                        panel.set_panel_visible(true);
                        let _ = panel.show();

                        // Switch to Tab 1 (Browser & Cookies) and verify Edge profile card + multi-key model.
                        panel.set_active_tab(1);
                        let orig_browsers = panel.get_browsers();
                        let seeded_browsers = ModelRc::new(VecModel::from(vec![
                            BrowserItem {
                                id: "chrome-Default".into(),
                                name: "Google Chrome".into(),
                                profile: "Google Chrome (Chrome Anda)".into(),
                                selected: true,
                                logo_key: "chrome".into(),
                            },
                            BrowserItem {
                                id: "edge-Default".into(),
                                name: "Microsoft Edge".into(),
                                profile: "Microsoft Edge (Personal)".into(),
                                selected: false,
                                logo_key: "edge".into(),
                            },
                        ]));
                        let prev_active = panel.get_active_browser();
                        if orig_browsers.row_count() >= 2 {
                            // Real pointer click on second browser card (Microsoft Edge at x=800, y=280)
                            dispatch_click(panel.window(), 800.0, 280.0);
                            let picked_edge = panel.get_active_browser().to_string();
                            let edge_selected = panel
                                .get_browsers()
                                .row_data(1)
                                .map(|b| b.selected)
                                .unwrap_or(false);
                            // Restore primary selection by clicking Chrome card (x=460, y=280)
                            dispatch_click(panel.window(), 460.0, 280.0);
                            check(
                                edge_selected && picked_edge.contains("Edge"),
                                "Browser & Cookies selector switches live to Microsoft Edge on pointer click",
                                &mut st.failures,
                            );
                        } else {
                            panel.set_browsers(seeded_browsers);
                            check(
                                panel.get_browsers().row_count() == 2,
                                "Browser & Cookies seeded both Chrome and Edge cards",
                                &mut st.failures,
                            );
                            panel.set_browsers(orig_browsers);
                            panel.set_active_browser(prev_active);
                        }
                        panel.set_deepseek_keys(ModelRc::new(VecModel::from(demo_vault_keys())));
                        let has_chrome_and_edge_keys = panel.get_deepseek_keys().row_count() == 2
                            && panel.get_deepseek_keys().row_data(0).map(|k| k.source == "chrome").unwrap_or(false)
                            && panel.get_deepseek_keys().row_data(1).map(|k| k.source == "edge").unwrap_or(false);
                        check(
                            has_chrome_and_edge_keys,
                            "VaultKeyItem rows bind both Chrome primary and Edge fallback sources",
                            &mut st.failures,
                        );

                        // Stage 11c setup: switch to Tab 3 (Diagnostik) and scroll so next tick
                        // has committed Flickable bounds for real pointer click tests.
                        panel.set_active_tab(3);
                        panel.invoke_scroll_to(350.0);
                    }
                    st.stage = 12;
                }
                12 => {
                    if let Some(panel) = panel_weak.upgrade() {
                        panel.invoke_scroll_to(350.0);
                        // Key 2 `Uji` button center at scroll 350: x=968, y=548
                        dispatch_click(panel.window(), 968.0, 548.0);
                        let key2_tested = panel
                            .get_deepseek_keys()
                            .row_data(1)
                            .map(|k| k.diag_text == "OK \u{00b7} 123 ms" && k.diag_tone == "green")
                            .unwrap_or(false)
                            && panel.get_diag_status().contains("edge:Default");
                        // "Uji Semua Key (2)" HoloButton center at scroll 350: x=852, y=251
                        dispatch_click(panel.window(), 852.0, 251.0);
                        let all_keys_tested = panel
                            .get_deepseek_keys()
                            .row_data(0)
                            .map(|k| k.diag_text == "OK \u{00b7} 210 ms")
                            .unwrap_or(false)
                            && panel.get_diag_status().contains("2/2");
                        check(
                            key2_tested && all_keys_tested,
                            "Diagnostics per-key Uji & Uji Semua Key buttons test DeepSeek keys cleanly",
                            &mut st.failures,
                        );

                        panel.invoke_scroll_to(0.0);

                        // Stage 11d: Pengaturan Random Singing controls (volume, RAM>=85% toggle, preview)
                        panel.set_active_tab(4);
                        panel.invoke_setting_toggled("sing-ram-guard".into(), false);
                        panel.invoke_setting_toggled("random-singing".into(), false);
                        panel.invoke_sing_volume_picked(50);
                        panel.invoke_sing_test_toggled();
                        let preview_on = panel.get_sing_previewing()
                            && panel.get_sing_volume() == 50
                            && !panel.get_set_random_singing()
                            && !panel.get_set_sing_ram_guard()
                            && panel.get_sing_track_count() >= 1;
                        panel.invoke_sing_test_toggled();
                        let preview_off = !panel.get_sing_previewing();
                        panel.invoke_setting_toggled("random-singing".into(), true);
                        check(
                            preview_on && preview_off,
                            "Pengaturan Random Singing controls (volume 50%, RAM guard toggle, preview) work cleanly",
                            &mut st.failures,
                        );

                        panel.set_active_tab(0);

                        // Stage 11b: Control Panel Close Modal & Close-to-HUD flow
                        // Click top-right × button (x: 1060 - 26 = 1034, y: 26)
                        dispatch_click(panel.window(), 1034.0, 26.0);
                        let modal_opened = panel.get_close_modal_open();
                        // Click "Batal" button in modal (centered bottom of 500x336 card at y=192 -> 192+284+17 = 493)
                        dispatch_click(panel.window(), 530.0, 493.0);
                        let modal_canceled = !panel.get_close_modal_open();
                        // Re-open and click "Tutup Control Panel (Beralih ke HUD)" at (530, 340)
                        dispatch_click(panel.window(), 1034.0, 26.0);
                        dispatch_click(panel.window(), 530.0, 340.0);
                        let switched_to_hud = !panel.get_close_modal_open()
                            && !panel.get_panel_visible()
                            && hud.window().is_visible();
                        check(
                            modal_opened && modal_canceled && switched_to_hud,
                            "Control Panel Close Modal opened, canceled, and switched cleanly back to HUD",
                            &mut st.failures,
                        );

                        // Stage 11e: Bubble singing sprite + immediate stop when expanding to HUD
                        let _ = bubble.show();
                        let _ = hud.hide();
                        bubble.set_edge("right".into());
                        bubble.invoke_toggle_sing_requested();
                        let bubble_sing_started = bubble.get_singing();
                        bubble.invoke_expand_requested();
                        let stopped_on_expand = !bubble.get_singing()
                            && !panel.get_sing_previewing()
                            && hud.window().is_visible();
                        panel.invoke_setting_toggled("sing-ram-guard".into(), true);
                        panel.invoke_sing_volume_picked(85);
                        check(
                            bubble_sing_started && stopped_on_expand,
                            "Bubble singing activates orientation sprite & notes and stops immediately on HUD expand",
                            &mut st.failures,
                        );

                        // Stage 11f: Live ID / EN application language switcher syncs all 3 windows & dynamic state
                        panel.invoke_app_lang_selected("en".into());
                        let en_synced = I18n::get(&panel).get_lang() == "en"
                            && I18n::get(&hud).get_lang() == "en"
                            && I18n::get(&bubble).get_lang() == "en"
                            && panel.get_provider_count().as_str().contains("OF")
                            && bubble.get_loading_label().as_str() == "Translating screen...";

                        panel.invoke_app_lang_selected("id".into());
                        let id_restored = I18n::get(&panel).get_lang() == "id"
                            && I18n::get(&hud).get_lang() == "id"
                            && I18n::get(&bubble).get_lang() == "id"
                            && panel.get_provider_count().as_str().contains("DARI")
                            && bubble.get_loading_label().as_str() == "Menerjemahkan layar...";

                        check(
                            en_synced && id_restored,
                            "Live ID/EN language switcher synchronizes MainPanel, HudWindow, BubbleWindow and dynamic badges",
                            &mut st.failures,
                        );

                        // Stage 11g: Tab 4 Settings Card 9 Bubble Reading Auto-Scroll toggle & speed pills
                        panel.invoke_setting_toggled("bubble-auto-scroll".into(), false);
                        let scroll_off = !panel.get_set_bubble_auto_scroll();
                        panel.invoke_setting_toggled("bubble-auto-scroll".into(), true);
                        let scroll_on = panel.get_set_bubble_auto_scroll();
                        panel.invoke_bubble_read_speed_selected("fast".into());
                        let fast_set = panel.get_bubble_read_speed() == "fast";
                        panel.invoke_bubble_read_speed_selected("slow".into());
                        let slow_set = panel.get_bubble_read_speed() == "slow";
                        panel.invoke_bubble_read_speed_selected("normal".into());
                        let normal_restored = panel.get_bubble_read_speed() == "normal";
                        check(
                            scroll_off && scroll_on && fast_set && slow_set && normal_restored,
                            "Settings Bubble Reading Auto-Scroll toggle and Slow/Normal/Fast speed pills update cleanly",
                            &mut st.failures,
                        );

                        // Stage 11h: Tab 4 Settings Card 10 Startup Browser Session & Cookie Sync toggle
                        panel.invoke_setting_toggled("auto-sync-cookies".into(), false);
                        let sync_off = !panel.get_set_auto_sync_cookies();
                        panel.invoke_setting_toggled("auto-sync-cookies".into(), true);
                        let sync_on = panel.get_set_auto_sync_cookies();
                        check(
                            sync_off && sync_on,
                            "Settings Auto-Sync Browser Sessions toggle updates cleanly",
                            &mut st.failures,
                        );
                    }

                    let fail_count = st.failures.len();
                    if fail_count == 0 {
                        eprintln!("[ui-test] ALL INTERACTIVE UI TESTS PASSED");
                        timer_cb.stop();
                        let _ = slint::quit_event_loop();
                        std::process::exit(0);
                    } else {
                        eprintln!("[ui-test] {fail_count} FAILURES: {:?}", st.failures);
                        timer_cb.stop();
                        let _ = slint::quit_event_loop();
                        std::process::exit(1);
                    }
                }
                _ => {
                    timer_cb.stop();
                }
            }
            tick_guard.set(false);
        },
    );
    std::mem::forget(timer);
}


#[cfg(test)]
mod tests {
    use super::{trim_turns, translate_timeout_duration, translate_user_prompt, Turn};

    fn turn(role: &str, text: &str) -> Turn {
        Turn { role: role.to_string(), text: text.to_string(), files: Vec::new() }
    }

    #[test]
    fn translate_prompt_default_custom_and_lang_placeholder() {
        assert_eq!(
            translate_user_prompt(None, "Indonesia"),
            "Terjemahkan seluruh teks pada layar ini ke dalam bahasa Indonesia."
        );
        assert_eq!(translate_user_prompt(Some("   "), "English"), translate_user_prompt(None, "English"));
        assert_eq!(
            translate_user_prompt(Some("Hanya chat ke {lang}"), "Jepang"),
            "Hanya chat ke Jepang"
        );
        assert!(translate_user_prompt(Some("Hanya chat"), "Korea").contains("Korea"));
    }

    #[test]
    fn trim_turns_drops_oldest_and_starts_on_user() {
        let turns = vec![
            turn("user", &"a".repeat(100)),
            turn("assistant", &"b".repeat(100)),
            turn("user", &"c".repeat(100)),
            turn("assistant", &"d".repeat(100)),
            turn("user", "terbaru"),
        ];
        // Everything fits.
        assert_eq!(trim_turns(&turns, 10_000, 40).len(), 5);
        // Budget fits only the last ~2 turns; the window must start on a user turn.
        let kept = trim_turns(&turns, 150, 40);
        assert_eq!(kept.first().map(|t| t.role.as_str()), Some("user"));
        assert_eq!(kept.last().map(|t| t.text.as_str()), Some("terbaru"));
        assert!(kept.len() < 5);
        // The newest turn survives even when it alone exceeds the budget.
        assert_eq!(trim_turns(&turns, 1, 40).len(), 1);
        // Turn-count cap.
        assert_eq!(trim_turns(&turns, 10_000, 2).last().map(|t| t.text.as_str()), Some("terbaru"));
        assert!(trim_turns(&turns, 10_000, 2).len() <= 2);
        assert!(trim_turns(&[], 100, 10).is_empty());
    }

    #[test]
    fn translate_timeout_duration_scales_with_word_count() {
        let short = translate_timeout_duration("Halo dunia");
        assert!((short.as_secs_f32() - 4.5).abs() < 0.01);

        let thirty_words = vec!["kata"; 30].join(" ");
        let medium = translate_timeout_duration(&thirty_words);
        assert!((medium.as_secs_f32() - 13.0).abs() < 0.05);

        let two_hundred_words = vec!["kata"; 200].join(" ");
        let long = translate_timeout_duration(&two_hundred_words);
        assert!((long.as_secs_f32() - 45.0).abs() < 0.01);
    }

    #[test]
    fn merge_search_sources_dedups_and_orders_by_cite_index() {
        let mut list = Vec::new();
        super::merge_search_sources(
            &mut list,
            &[
                serde_json::json!({
                    "url": "https://docs.rs/slint",
                    "title": "",
                    "cite_index": 2
                }),
                serde_json::json!({
                    "url": "https://rust-lang.org",
                    "title": "Rust Lang",
                    "snippet": "Official site",
                    "cite_index": 1
                }),
            ],
        );
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].url.as_str(), "https://rust-lang.org");
        assert_eq!(list[1].url.as_str(), "https://docs.rs/slint");

        // Duplicate URL with trailing slash enriches missing title/snippet without creating duplicate card.
        super::merge_search_sources(
            &mut list,
            &[serde_json::json!({
                "url": "https://docs.rs/slint/",
                "title": "Slint Rust Docs",
                "snippet": "API Reference"
            })],
        );
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].title.as_str(), "Slint Rust Docs");
        assert_eq!(list[1].snippet.as_str(), "API Reference");
    }

    #[test]
    fn rewrite_svg_header_size_preserves_stroke_width() {
        let svg = super::icons::svg("gamepad").expect("gamepad icon exists");
        let resized = super::rewrite_svg_header_size(svg, 20);
        assert!(resized.contains("width=\"20\""));
        assert!(resized.contains("height=\"20\""));
        assert!(
            resized.contains("stroke-width=\"2\""),
            "stroke-width must stay untouched: {resized}"
        );
        assert!(!resized.contains("stroke-width=\"20\""));
    }

    #[test]
    fn vault_fetch_status_omits_zero_cookie_count() {
        let base = app_core::ConnectReport {
            success: true,
            browser: "Google Chrome".into(),
            profile: "Default".into(),
            status: "READY".into(),
            message: String::new(),
            token_chars: 64,
            cookies_found: 0,
            locked_v20: 0,
        };
        assert_eq!(
            super::format_vault_fetch_status(&base),
            "Terpasang: token sesi 64 karakter dari LocalStorage Google Chrome."
        );
        let with_v20 = app_core::ConnectReport { locked_v20: 3, ..base.clone() };
        assert_eq!(
            super::format_vault_fetch_status(&with_v20),
            "Terpasang: token sesi 64 karakter dari LocalStorage Google Chrome · 3 cookie terkunci v20."
        );
        let with_cookies = app_core::ConnectReport { cookies_found: 2, ..base.clone() };
        assert_eq!(
            super::format_vault_fetch_status(&with_cookies),
            "Terpasang: 2 cookie + token 64 karakter dari Google Chrome."
        );
        let err = app_core::ConnectReport {
            success: false,
            message: "Token ditolak".into(),
            ..base
        };
        assert_eq!(super::format_vault_fetch_status(&err), "Gagal: Token ditolak");
    }

    #[test]
    fn format_vault_fetch_all_and_key_items_cover_chrome_and_edge() {
        let chrome_ok = app_core::ConnectReport {
            success: true,
            browser: "Google Chrome".into(),
            profile: "Chrome Anda".into(),
            status: "READY".into(),
            message: String::new(),
            token_chars: 64,
            cookies_found: 0,
            locked_v20: 0,
        };
        let edge_ok = app_core::ConnectReport {
            browser: "Microsoft Edge".into(),
            profile: "Personal".into(),
            ..chrome_ok.clone()
        };
        let (ok, msg) = super::format_vault_fetch_all_status(&[chrome_ok.clone(), edge_ok]);
        assert!(ok);
        assert!(msg.contains("Google Chrome") && msg.contains("Microsoft Edge"));

        let edge_fail = app_core::ConnectReport {
            success: false,
            browser: "Microsoft Edge".into(),
            message: "Belum login".into(),
            ..chrome_ok.clone()
        };
        let (ok_part, msg_part) = super::format_vault_fetch_all_status(&[chrome_ok, edge_fail]);
        assert!(ok_part);
        assert!(msg_part.contains("Google Chrome") && msg_part.contains("Microsoft Edge"));

        let keys = super::model::vault_keys_from(&[
            app_core::AccountEntry {
                name: "chrome:Default".into(),
                source: "chrome".into(),
                profile: "Google Chrome (Chrome Anda)".into(),
                email: "demo.chrome@example.com".into(),
                priority: 1,
                available: false,
                busy: false,
                cooling_down: true,
                cooldown_secs: 45,
                last_error: "429 rate limit".into(),
            },
            app_core::AccountEntry {
                name: "edge:Default".into(),
                source: "edge".into(),
                profile: "Microsoft Edge (Personal)".into(),
                email: "demo.edge@example.com".into(),
                priority: 2,
                available: true,
                busy: false,
                cooling_down: false,
                cooldown_secs: 0,
                last_error: String::new(),
            },
        ]);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].source.as_str(), "chrome");
        assert!(keys[0].is_primary);
        assert_eq!(keys[0].tone.as_str(), "amber");
        assert!(keys[0].status_text.contains("45d"));
        assert_eq!(keys[1].source.as_str(), "edge");
        assert!(!keys[1].is_primary);
        assert_eq!(keys[1].status_text.as_str(), "Cadangan · Siap");
    }

    #[test]
    fn random_singing_helpers_tracks_and_mf_probe() {
        let mut rng = 0x1234_5678_9ABC_DEF0u64;
        for _ in 0..32 {
            let d1 = super::random_delay_ms(&mut rng, super::SING_FIRST_DELAY_MS);
            assert!((45_000..=100_000).contains(&d1));
            let d2 = super::random_delay_ms(&mut rng, super::SING_REPEAT_DELAY_MS);
            assert!((150_000..=330_000).contains(&d2));
        }

        let tracks = vec![
            std::path::PathBuf::from("a.mp3"),
            std::path::PathBuf::from("b.mp3"),
        ];
        for _ in 0..10 {
            let picked = super::pick_random_track(&mut rng, &tracks, Some(&tracks[0])).unwrap();
            assert_eq!(picked, tracks[1], "avoids immediate repeat when >= 2 tracks");
        }
        assert!(super::pick_random_track(&mut rng, &[], None).is_none());

        assert!(!super::is_ram_over_guard(true, Some(84), 85));
        assert!(super::is_ram_over_guard(true, Some(85), 85));
        assert!(super::is_ram_over_guard(true, Some(92), 85));
        assert!(!super::is_ram_over_guard(false, Some(95), 85));
        assert!(!super::is_ram_over_guard(true, None, 85));

        let assets = super::logos::asset_dir();
        assert!(assets.join("app-icon.png").is_file(), "app-icon.png must exist");
        assert!(assets.join("app-icon.ico").is_file(), "app-icon.ico must exist");

        let found = super::logos::music_tracks();
        assert!(
            found.iter().any(|p| p.file_name().and_then(|n| n.to_str()) == Some("sing1.mp3")),
            "sing1.mp3 must be bundled under assets/musik/: {found:?}"
        );
        let len_ms = super::win32::AudioPlayer::probe_track_ms(&found[0])
            .expect("Media Foundation decodes bundled sing1.mp3 cleanly");
        assert!(
            (45_000..=65_000).contains(&len_ms),
            "expected accurate ~53.3s duration (not 2x MCI bug), got {len_ms} ms"
        );
    }

    #[test]
    fn i18n_en_status_formatters_are_deterministic() {
        assert_eq!(
            super::provider_count_text_for(app_core::AppLang::Id, 1, 1, "Google Chrome"),
            "1 DARI 1 AKTIF · GOOGLE CHROME"
        );
        assert_eq!(
            super::provider_count_text_for(app_core::AppLang::En, 1, 1, "Google Chrome"),
            "1 OF 1 ACTIVE · GOOGLE CHROME"
        );

        let base = app_core::ConnectReport {
            success: true,
            browser: "Google Chrome".into(),
            profile: "Default".into(),
            status: "READY".into(),
            message: String::new(),
            token_chars: 64,
            cookies_found: 0,
            locked_v20: 0,
        };
        assert_eq!(
            super::format_vault_fetch_status_for(app_core::AppLang::En, &base),
            "Connected: 64-char session token from Google Chrome LocalStorage."
        );
        let with_v20 = app_core::ConnectReport {
            locked_v20: 3,
            ..base.clone()
        };
        assert_eq!(
            super::format_vault_fetch_status_for(app_core::AppLang::En, &with_v20),
            "Connected: 64-char session token from Google Chrome LocalStorage · 3 v20-locked cookies."
        );
        let with_cookies = app_core::ConnectReport {
            cookies_found: 2,
            ..base.clone()
        };
        assert_eq!(
            super::format_vault_fetch_status_for(app_core::AppLang::En, &with_cookies),
            "Connected: 2 cookies + 64-char token from Google Chrome."
        );
        let err = app_core::ConnectReport {
            success: false,
            message: "Token rejected".into(),
            ..base.clone()
        };
        assert_eq!(
            super::format_vault_fetch_status_for(app_core::AppLang::En, &err),
            "Failed: Token rejected"
        );

        let edge_ok = app_core::ConnectReport {
            browser: "Microsoft Edge".into(),
            profile: "Personal".into(),
            ..base
        };
        let (ok_all, msg_all) = super::format_vault_fetch_all_status_for(
            app_core::AppLang::En,
            std::slice::from_ref(&edge_ok),
        );
        assert!(ok_all);
        assert_eq!(msg_all, "Connected 1 sessions from Microsoft Edge.");

        let chrome_fail = app_core::ConnectReport {
            success: false,
            browser: "Google Chrome".into(),
            message: "Not signed in".into(),
            ..edge_ok.clone()
        };
        let (ok_part, msg_part) =
            super::format_vault_fetch_all_status_for(app_core::AppLang::En, &[edge_ok, chrome_fail]);
        assert!(ok_part);
        assert_eq!(
            msg_part,
            "Connected from Microsoft Edge (no session yet in Google Chrome)."
        );
    }
}
