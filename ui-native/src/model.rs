//! View-model helpers for the Slint UI.
//!
//! Everything here turns `app_core` values into the structs generated from
//! `ui/model.slint`. No invented rows: an empty database renders as empty.

use crate::{
    AttachmentItem, BrowserItem, EmojiGlyphItem, HudMessage, MessageBlockItem, ProviderItem,
    SearchSourceItem, TableCellItem, TableRowItem, TopicCategory, TopicItem, VaultKeyItem,
};
use slint::{Model, SharedString};
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::Hasher;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;

static UI_SCALE_PCT: AtomicU32 = AtomicU32::new(100);

/// Records the HUD display scale factor so rasterized Direct2D color emojis
/// match physical destination pixels 1:1 under Slint's software renderer.
pub fn set_ui_scale(scale: f32) {
    let clamped = (scale.clamp(1.0, 3.0) * 100.0).round() as u32;
    UI_SCALE_PCT.store(clamped, Ordering::Relaxed);
}

fn scaled_px(logical_px: u32) -> u32 {
    let pct = UI_SCALE_PCT.load(Ordering::Relaxed).clamp(100, 300);
    ((logical_px as u64 * pct as u64 + 50) / 100).max(8) as u32
}

/// First alphanumeric character of a topic name, upper-cased, for the card tile.
fn initial_of(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "•".to_string())
}

/// ISO timestamps are unreadable in a card; keep date + HH:MM.
fn pretty_when(created_at: &str) -> String {
    let date = created_at.split('T').next().unwrap_or(created_at);
    let time = created_at
        .split('T')
        .nth(1)
        .and_then(|t| t.get(0..5))
        .unwrap_or("");
    if time.is_empty() {
        date.to_string()
    } else {
        format!("{date} · {time}")
    }
}

/// Preprocesses markdown before passing to Slint's `StyledText::from_markdown`.
///
/// Features:
fn parse_citation_tag(s: &str) -> Option<(usize, String)> {
    for (prefix, one_based) in [("[reference:", false), ("[citation:", true)] {
        if let Some(rest) = s.strip_prefix(prefix) {
            if let Some(close) = rest.find(']') {
                if let Ok(n) = rest[..close].trim().parse::<usize>() {
                    let display = if one_based { n.max(1) } else { n + 1 };
                    return Some((prefix.len() + close + 1, format!("[{display}]")));
                }
            }
        }
    }
    None
}

/// 1. Global citation cleaning: `[reference:N]` -> `[N+1]` and `[citation:N]` -> `[N]`.
/// 2. Clean blockquotes: `▎ Quote` without wrapping in `*...*` so inline formatting survives.
/// 3. Headings: converted to clean bold lines `**Title**`.
/// 4. Tables: aligned monospace columns with borders `col1 │ col2`.
/// 5. Code blocks: clean formatted monospace blocks with frame divider.
/// 6. Preserves real emojis (no stripping or substituting with kaomojis).
/// 7. Auto-balances asterisks so bold markdown never leaks raw asterisks.
pub fn preprocess_markdown(body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }

    // 1. Clean global citation tags like [reference:0] -> [1], [citation:7] -> [7]
    let mut text = body.replace("\r\n", "\n");
    for (prefix, one_based) in [("[reference:", false), ("[citation:", true)] {
        let mut cursor = 0;
        while let Some(rel) = text[cursor..].find(prefix) {
            let pos = cursor + rel;
            if let Some(end_pos) = text[pos..].find(']') {
                let num_str = &text[pos + prefix.len()..pos + end_pos];
                if let Ok(n) = num_str.trim().parse::<usize>() {
                    let display = if one_based { n.max(1) } else { n + 1 };
                    let repl = format!("[{display}]");
                    let repl_len = repl.len();
                    text.replace_range(pos..pos + end_pos + 1, &repl);
                    cursor = pos + repl_len;
                } else {
                    text.replace_range(pos..pos + end_pos + 1, "");
                    cursor = pos;
                }
            } else {
                break;
            }
        }
    }

    // Clean stray internal tokens and unsupported HTML blocks
    text = text.replace("DEEP_SEARCH", "");
    text = text.replace("FINISHED", "");
    text = text.replace("<center>", "").replace("</center>", "");
    text = text.replace("<p>", "").replace("</p>", "\n\n");
    text = text.replace("<br>", "\n").replace("<br/>", "\n").replace("<br />", "\n");

    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    let mut in_code_block = false;
    let mut skip_svg_fence = false;

    let push_blank_line = |out: &mut Vec<String>| {
        if let Some(last) = out.last() {
            if !last.is_empty() {
                out.push(String::new());
            }
        }
    };

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        // 1. Fenced code block (```lang ... ```)
        if trimmed.starts_with("```") {
            if !in_code_block {
                push_blank_line(&mut out);
                in_code_block = true;
                let lang = trimmed.trim_start_matches('`').trim();
                let lower_lang = lang.to_ascii_lowercase();
                if lower_lang == "svg" || (lower_lang == "xml" && lines.get(i + 1).is_some_and(|l| l.contains("<svg"))) {
                    skip_svg_fence = true;
                    out.push("🖼 **[Diagram SVG]**".to_string());
                } else {
                    skip_svg_fence = false;
                    let lang_label = if lang.is_empty() { "CODE" } else { lang };
                    out.push(format!("**[{}]**", lang_label.to_uppercase()));
                }
            } else {
                in_code_block = false;
                skip_svg_fence = false;
                push_blank_line(&mut out);
            }
            i += 1;
            continue;
        }

        if in_code_block {
            if skip_svg_fence {
                i += 1;
                continue;
            }
            if trimmed.is_empty() {
                out.push("\u{00A0}".to_string());
            } else {
                // Escape markdown characters so code never leaks bold/italic/inline-code spans
                let escaped = line
                    .replace('\\', "\\\\")
                    .replace('`', "\\`")
                    .replace('*', "\\*")
                    .replace('_', "\\_")
                    .replace('~', "\\~");
                out.push(format!("`{}`", escaped));
            }
            i += 1;
            continue;
        }

        // Collapse standalone or inline markdown images before leaking base64 URLs
        let cleaned_imgs = replace_markdown_images_summary(line);
        let expanded_line = crate::emoji::expand_shortcodes(&cleaned_imgs);
        let line = expanded_line.as_str();
        let trimmed = line.trim();

        // 2. Blank lines -> standard clean paragraph break (no \u{00A0} noise)
        if trimmed.is_empty() {
            push_blank_line(&mut out);
            i += 1;
            continue;
        }

        // 3. Horizontal rules (---, ***, ___)
        if (trimmed.starts_with("---") || trimmed.starts_with("***") || trimmed.starts_with("___"))
            && trimmed.chars().all(|c| matches!(c, '-' | '*' | '_' | ' '))
            && trimmed.len() >= 3
        {
            push_blank_line(&mut out);
            out.push("· · ·".to_string());
            push_blank_line(&mut out);
            i += 1;
            continue;
        }

        // 4. Headings (#, ##, ###, ####, etc.)
        if trimmed.starts_with('#') {
            let heading_text = trimmed.trim_start_matches('#').trim();
            if !heading_text.is_empty() {
                let clean = heading_text.trim_start_matches("**").trim_end_matches("**");
                push_blank_line(&mut out);
                out.push(format!("**{clean}**"));
                push_blank_line(&mut out);
                i += 1;
                continue;
            }
        }

        // 5. Block quotes (> quote)
        if let Some(rest) = trimmed.strip_prefix('>') {
            let quote_text = rest.trim().replace('✦', "◆");
            out.push(format!("│ *{quote_text}*"));
            i += 1;
            continue;
        }

        // 6. Markdown tables (| col1 | col2 | or col1 | col2 | col3)
        let is_table_candidate = trimmed.contains('|')
            && i + 1 < lines.len()
            && {
                let next_trim = lines[i + 1].trim();
                next_trim.contains('|')
                    && next_trim.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
                    && next_trim.contains('-')
            };

        if is_table_candidate {
            push_blank_line(&mut out);
            let mut table_lines = Vec::new();
            while i < lines.len() && lines[i].trim().contains('|') {
                table_lines.push(lines[i].trim());
                i += 1;
            }

            if table_lines.len() >= 2 {
                let mut rows: Vec<Vec<String>> = Vec::new();
                for tl in &table_lines {
                    let is_sep = tl.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '));
                    if is_sep {
                        continue;
                    }
                    let cells: Vec<String> = tl
                        .trim_matches('|')
                        .split('|')
                        .map(|c| c.trim().to_string())
                        .collect();
                    rows.push(cells);
                }

                if !rows.is_empty() {
                    let data_rows = &rows[1..];
                    let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
                    let mut col_widths = vec![0usize; col_count];
                    for r in &rows {
                        for (c_idx, cell) in r.iter().enumerate() {
                            let clean_w = cell.replace(['`', '*'], "").chars().count();
                            col_widths[c_idx] = col_widths[c_idx].max(clean_w);
                        }
                    }
                    let total_width: usize = col_widths.iter().sum::<usize>() + (col_count.saturating_sub(1) * 3);

                    if total_width <= 26 && rows.len() <= 5 {
                        // Compact aligned table for small numeric or key tables
                        for (r_idx, r) in rows.iter().enumerate() {
                            let mut line_buf = String::new();
                            for (c_idx, cell) in r.iter().enumerate() {
                                let width = col_widths.get(c_idx).copied().unwrap_or(cell.len());
                                let clean_w = cell.replace(['`', '*'], "").chars().count();
                                line_buf.push_str(cell);
                                if clean_w < width {
                                    line_buf.push_str(&" ".repeat(width - clean_w));
                                }
                                if c_idx + 1 < r.len() {
                                    line_buf.push_str(" │ ");
                                }
                            }
                            out.push(format!("`{line_buf}`"));
                            if r_idx == 0 && rows.len() > 1 {
                                let mut div_buf = String::new();
                                for (c_idx, &width) in col_widths.iter().enumerate() {
                                    div_buf.push_str(&"─".repeat(width.max(1)));
                                    if c_idx + 1 < col_widths.len() {
                                        div_buf.push_str("─┼─");
                                    }
                                }
                                out.push(format!("`{div_buf}`"));
                            }
                        }
                    } else {
                        // Clean readable cards for mobile / HUD width
                        for r in data_rows {
                            if r.len() == 1 {
                                out.push(format!("• {}", r[0]));
                            } else if r.len() == 2 {
                                out.push(format!("• **{}**: {}", r[0], r[1]));
                            } else if r.len() >= 3 {
                                let key = &r[0];
                                let rest = r[1..].join(" — ");
                                out.push(format!("• **{}**: {}", key, rest));
                            }
                        }
                    }
                    push_blank_line(&mut out);
                    continue;
                }
            }
        }

        // 7. Clean list items to neat bullets (no gaps between items!)
        let is_unordered = trimmed.starts_with("- ") || trimmed.starts_with("* ");
        let is_ordered = trimmed.len() > 2 && trimmed.as_bytes()[0].is_ascii_digit() && trimmed.contains(". ");
        if is_unordered || is_ordered {
            if is_unordered {
                let rest = &trimmed[2..];
                out.push(format!("• {rest}"));
            } else {
                out.push(trimmed.to_string());
            }
            i += 1;
            continue;
        }

        // Regular line
        out.push(line.to_string());
        i += 1;
    }

    let mut joined = out.join("\n");

    // 8. Auto-balance bold asterisks so unclosed "**" never leak raw asterisks
    let star_count = joined.matches("**").count();
    if !star_count.is_multiple_of(2) {
        joined.push_str("**");
    }

    joined
}

/// Formats text for copying to clipboard: clean and readable without raw markdown asterisks or code ticks.
pub fn plain_copy_text(raw: &str) -> String {
    let preprocessed = preprocess_markdown(raw);
    preprocessed.replace("**", "").replace(['*', '`'], "")
}

pub fn styled(body: &str) -> slint::StyledText {
    let cleaned = preprocess_markdown(body);
    match slint::StyledText::from_markdown(&cleaned) {
        Ok(t) => t,
        Err(_e) => {
            #[cfg(debug_assertions)]
            eprintln!("[styled] error after preprocess: {_e}");
            let plain = cleaned.replace("**", "").replace('*', "");
            slint::StyledText::from_plain_text(&plain)
        }
    }
}

/// Derives display site metadata (`site_name`, `badge_text`, `badge_color`) and
/// normalized fields for one web search citation card and stacked pill badge.
pub fn build_search_source_item(
    title: &str,
    url: &str,
    snippet: &str,
    site_name: &str,
    date: &str,
) -> SearchSourceItem {
    let raw_url = url.trim();
    let without_scheme = raw_url
        .strip_prefix("https://")
        .or_else(|| raw_url.strip_prefix("http://"))
        .unwrap_or(raw_url);
    let host_raw = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    let host = host_raw.strip_prefix("www.").unwrap_or(host_raw);
    let host_lower = host.to_ascii_lowercase();

    let (known_site, badge_text, rgb): (&str, String, (u8, u8, u8)) =
        if host_lower.contains("antaranews") {
            ("ANTARA News", "A".into(), (200, 30, 43))
        } else if host_lower.contains("rri.co") {
            ("RRI.co.id", "R".into(), (29, 78, 216))
        } else if host_lower.contains("republika.") {
            ("Republika.co", "R".into(), (220, 38, 38))
        } else if host_lower.contains("metrotvnews") {
            ("MetroTVNews.com", "M".into(), (2, 132, 199))
        } else if host_lower.contains("kompas.") {
            ("Kompas.com", "K".into(), (234, 88, 12))
        } else if host_lower.contains("detik.") {
            ("detikcom", "d".into(), (30, 64, 175))
        } else if host_lower.contains("yahoo.") {
            ("Yahoo Finance", "Y".into(), (124, 58, 237))
        } else if host_lower.contains("bloomberg.") {
            ("Bloomberg", "B".into(), (51, 65, 85))
        } else if host_lower.contains("techcrunch.") {
            ("TechCrunch", "TC".into(), (5, 150, 105))
        } else if host_lower.contains("brewindolphin") {
            ("Brewin Dolphin", "BD".into(), (15, 118, 110))
        } else {
            let derived = if !host.is_empty() { host } else { "Web" };
            let ch = derived
                .chars()
                .find(|c| c.is_alphanumeric())
                .map(|c| c.to_ascii_uppercase().to_string())
                .unwrap_or_else(|| "W".to_string());
            let palette: [(u8, u8, u8); 6] = [
                (2, 132, 199),
                (16, 185, 129),
                (245, 158, 11),
                (220, 38, 38),
                (139, 92, 246),
                (236, 72, 153),
            ];
            let hash = host_lower.bytes().fold(0usize, |acc, b| acc.wrapping_mul(31).wrapping_add(b as usize));
            (derived, ch, palette[hash % palette.len()])
        };

    let resolved_site = if !site_name.trim().is_empty() {
        site_name.trim().to_string()
    } else {
        known_site.to_string()
    };

    SearchSourceItem {
        title: title.trim().into(),
        url: raw_url.into(),
        snippet: snippet.trim().into(),
        site_name: resolved_site.into(),
        date: date.trim().into(),
        badge_text: badge_text.into(),
        badge_color: slint::Color::from_rgb_u8(rgb.0, rgb.1, rgb.2),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn message_full(
    role: &str,
    provider: &str,
    body: &str,
    badge: &str,
    markdown: bool,
    thought: &str,
    think_label: &str,
    thought_open: bool,
    search_count: i32,
    search_open: bool,
    sources: &[SearchSourceItem],
) -> HudMessage {
    let source_items: Vec<SearchSourceItem> = sources.to_vec();

    let (blocks, fallback_styled) = build_blocks_and_fallback_styled(body, markdown);
    let mut m = HudMessage {
        role: role.into(),
        provider: provider.into(),
        body: body.into(),
        badge: badge.into(),
        markdown,
        styled: fallback_styled,
        thought: thought.into(),
        thought_open,
        think_label: think_label.into(),
        offset_y: 0,
        height: 0,
        body_y: 0,
        body_h: 0,
        thought_h: 0,
        bubble_w: 0,
        meta: "DeepSeek · 3.2 dtk · 340 token".into(),
        detail_open: false,
        failed: false,
        attach_thumb: slint::Image::default(),
        attach_name: SharedString::new(),
        attach_image: false,
        attach_path: SharedString::new(),
        attachments: slint::ModelRc::new(slint::VecModel::from(Vec::<AttachmentItem>::new())),
        search_count,
        search_open,
        copied: false,
        user_selectable: !markdown || !has_rich_inline_syntax(&normalize_inline_md(body)),
        select_mode: false,
        select_text: SharedString::new(),
        select_copied: false,
        sources: slint::ModelRc::new(slint::VecModel::from(source_items)),
        blocks,
    };
    layout(&mut m);
    m
}

pub fn seed_messages() -> Vec<HudMessage> {
    vec![
        message("user", "ANDA", "kamu bisa akses internet kan?", "", false),
        message_full(
            "assistant",
            "DEEPSEEK",
            "Bisa, kok! ( •̀ᴗ•́ )و\nAku Aemeath bisa bantu kamu mengakses informasi dari internet lewat fitur pencarian yang terintegrasi di aplikasi kita.\n\nBerikut beberapa kapabilitas utama pencarian web:\n- **Pencarian Terkini**: Menemukan berita terbaru dan dokumentasi teknis secara real-time.\n- **Rangkuman Sumber**: Mengekstrak poin penting langsung dari berbagai sumber tepercaya dengan sitasi jelas.\n- **Verifikasi Fakta**: Membandingkan data dari beragam halaman web untuk akurasi tinggi.",
            "",
            true,
            "• Memeriksa ketersediaan koneksi internet\n• Mengakses API search Google / DuckDuckGo\n• Mengekstrak informasi dari halaman terverifikasi",
            "Berpikir selama 3 detik",
            false,
            37,
            false,
            &[
                build_search_source_item(
                    "Survei PPI: Dedi Mulyadi teratas dalam simulasi elektabilitas cawapres - Kabar Istana",
                    "https://www.antaranews.com/berita/survei-ppi",
                    "Jakarta (ANTARA) - Hasil survei terbaru Parameter Politik Indonesia (PPI) menunjukkan dinamika kandidat nasional terkini.",
                    "ANTARA News",
                    "2026/10/17",
                ),
                build_search_source_item(
                    "Rangkuman Berita Nasional & Kebijakan Digital Terkini - RRI.co.id",
                    "https://rri.co.id/nasional/kebijakan-digital",
                    "Sejumlah pemangku kebijakan membahas percepatan infrastruktur AI dan layanan publik real-time di tingkat nasional.",
                    "RRI.co.id",
                    "2026/11/13",
                ),
                build_search_source_item(
                    "Perkembangan Riset Komputasi Modern & Model Bahasa Indonesia",
                    "https://www.republika.co.id/berita/riset-komputasi-modern",
                    "Riset kolaboratif membuka peluang pemanfaatan model bahasa ringan dan pencarian web terstruktur.",
                    "Republika.co",
                    "2026/11/13",
                ),
                build_search_source_item(
                    "Tren Adopsi Asisten Desktop Berbasis AI di Kalangan Developer",
                    "https://www.metrotvnews.com/teknologi/asisten-desktop-ai",
                    "Penggunaan asisten produktivitas layar penuh kian meningkat karena integrasi pencarian cepat dan ringkasan konteks.",
                    "MetroTVNews.com",
                    "2026/11/13",
                ),
                build_search_source_item(
                    "Analisis Bubble AI dan Tren Komputasi Modern",
                    "https://techcrunch.com/analysis-ai-bubble",
                    "Evaluasi arsitektur AI lokal vs cloud serta efisiensi penggunaan memori aplikasi klien.",
                    "TechCrunch",
                    "2026/11/12",
                ),
            ],
        ),
        message("user", "ANDA", "Tolong tunjukkan keanekaragaman fitur formatting yang bisa kamu pakai di chat kita.", "", false),
        message(
            "assistant",
            "DEEPSEEK",
            "Halo juga! (≧◡≦) 👋 Siap, aku tunjukkan keanekaragaman fitur formatting yang bisa kamu pakai di chat kita. Anggap ini semacam *cheat sheet* interaktif ya, biar kamu tahu apa saja yang bisa dirender dengan cantik di layar desktopmu~ ✨\n\n### 1. Teks Dasar\n\n• *Italic* → untuk penekanan halus atau istilah asing\n• **Bold** → untuk kata kunci penting\n• ***Bold + Italic*** → untuk penekanan super kuat\n• ~~Strikethrough~~ → untuk koreksi atau coretan\n• `inline code` → untuk nama variabel, shortcut, atau file\n• Status cepat: selesai ✅ atau belum lolos ❌\n\n### 2. Heading Berjenjang\n\nLevel 3 — Sub-bab\nLevel 4 — Detail kecil\nLevel 5 — Catatan mini\n\n### 3. Checklist & Alur Kerja\n\n- [x] Emoji render berwarna tajam\n- [x] Tabel responsif rapi\n- [ ] Task yang masih menunggu\n- [-] Item yang gagal / ditolak\n\n1. Langkah satu: Analisis struktur input\n2. Langkah dua: Format preprocessing ke Slint\n3. Langkah tiga: Layout fleksibel tanpa clipping\n\n### 4. Blockquote\n\n> \"Kode yang baik adalah kode yang bisa dibaca oleh manusia, bukan hanya oleh mesin.\"\n> — Pepatah Developer 💻\n\n### 5. Tabel Perbandingan\n\nFitur | Simbol Markdown | Contoh Hasil\n---|---|---\nItalic | `*teks*` | *miring*\nBold | `**teks**` | **tebal**\nCode | `` `teks` `` | `monospace`\nLink | `[teks](url)` | [Klik Disini](https://example.com)\n\n### 6. Code Block\n\n```python\ndef sapa(nama: str) -> str:\n    \"\"\"Fungsi sapa sederhana\"\"\"\n    return f\"Halo, {nama}! Selamat datang di Startorch.\"\n\nprint(sapa(\"Kamu\"))\n```\n\n---\n\n> 💡 **Tips Aemeath:** Untuk chat harian, kamu cukup pakai bold untuk poin penting, italic untuk nada, dan tabel untuk perbandingan data. Kombinasi heading, checklist, tabel, dan blockquote bikin jawaban terasa seperti dokumen profesional! 🎉",
            "",
            true,
        ),
    ]
}

/// Builds one chat bubble from live data.
pub fn message(role: &str, provider: &str, body: &str, badge: &str, markdown: bool) -> HudMessage {
    message_thought(role, provider, body, badge, markdown, "")
}

/// Same, with the reasoning text shown behind the "Detail" dropdown.
pub fn message_thought(
    role: &str,
    provider: &str,
    body: &str,
    badge: &str,
    markdown: bool,
    thought: &str,
) -> HudMessage {
    let (blocks, fallback_styled) = build_blocks_and_fallback_styled(body, markdown);
    let mut m = HudMessage {
        role: role.into(),
        provider: provider.into(),
        body: body.into(),
        badge: badge.into(),
        markdown,
        styled: fallback_styled,
        thought: thought.into(),
        thought_open: false,
        think_label: SharedString::new(),
        offset_y: 0,
        height: 0,
        body_y: 0,
        body_h: 0,
        thought_h: 0,
        bubble_w: 0,
        meta: SharedString::new(),
        detail_open: false,
        failed: false,
        attach_thumb: slint::Image::default(),
        attach_name: SharedString::new(),
        attach_image: false,
        attach_path: SharedString::new(),
        attachments: slint::ModelRc::new(slint::VecModel::<AttachmentItem>::default()),
        search_count: 0,
        search_open: false,
        copied: false,
        user_selectable: !markdown || !has_rich_inline_syntax(&normalize_inline_md(body)),
        select_mode: false,
        select_text: SharedString::new(),
        select_copied: false,
        sources: slint::ModelRc::new(slint::VecModel::<SearchSourceItem>::default()),
        blocks,
    };
    layout(&mut m);
    m
}

/// Rewrites the message body, keeping `styled` and structured `blocks` in sync
/// during streaming updates, resets, or retries. Skips building the unused
/// fallback `styled` tree whenever rich `blocks` handle rendering.
pub fn set_body(m: &mut HudMessage, body: &str) {
    m.body = body.into();
    let (blocks, fallback_styled) = build_blocks_and_fallback_styled(body, m.markdown);
    m.styled = fallback_styled;
    m.blocks = blocks;
    m.user_selectable = !m.markdown || !has_rich_inline_syntax(&normalize_inline_md(body));
    if m.select_mode {
        m.select_text = plain_copy_text(body).into();
    }
}

fn build_blocks_and_fallback_styled(
    body: &str,
    markdown: bool,
) -> (slint::ModelRc<MessageBlockItem>, slint::StyledText) {
    let blocks = blocks_model(body, markdown);
    let fallback = if blocks.row_count() == 0 && !body.trim().is_empty() {
        styled(body)
    } else {
        slint::StyledText::default()
    };
    (blocks, fallback)
}

fn blocks_model(body: &str, markdown: bool) -> slint::ModelRc<MessageBlockItem> {
    if !markdown || body.trim().is_empty() {
        return slint::ModelRc::new(slint::VecModel::<MessageBlockItem>::default());
    }
    slint::ModelRc::new(slint::VecModel::from(parse_message_blocks(body)))
}

fn empty_cells_model() -> slint::ModelRc<TableCellItem> {
    slint::ModelRc::new(slint::VecModel::<TableCellItem>::default())
}

fn empty_rows_model() -> slint::ModelRc<TableRowItem> {
    slint::ModelRc::new(slint::VecModel::<TableRowItem>::default())
}

fn empty_emojis_model() -> slint::ModelRc<EmojiGlyphItem> {
    slint::ModelRc::new(slint::VecModel::<EmojiGlyphItem>::default())
}

fn default_block(kind: &str, gap_top: i32) -> MessageBlockItem {
    MessageBlockItem {
        kind: kind.into(),
        styled: slint::StyledText::default(),
        raw: SharedString::new(),
        plain_selectable: false,
        level: 0,
        marker: SharedString::new(),
        indent: 0,
        gap_top,
        lang: SharedString::new(),
        headers: empty_cells_model(),
        rows: empty_rows_model(),
        table_w: 0,
        emoji_img: slint::Image::default(),
        has_emoji_img: false,
        emojis: empty_emojis_model(),
        img: slint::Image::default(),
        img_w: 0,
        img_h: 0,
        img_url: SharedString::new(),
        img_caption: SharedString::new(),
        img_loading: false,
        img_failed: false,
    }
}

/// Scales `(w, h)` proportionally to fit inside `(max_w, max_h)` while keeping natural aspect.
pub fn fit_image_box(w: i32, h: i32, max_w: i32, max_h: i32) -> (i32, i32) {
    if w <= 0 || h <= 0 {
        return (max_w.min(280), 120.min(max_h));
    }
    let scale = ((max_w as f32) / (w as f32))
        .min((max_h as f32) / (h as f32))
        .min(1.0);
    let disp_w = ((w as f32) * scale).round().max(48.0) as i32;
    let disp_h = ((h as f32) * scale).round().max(32.0) as i32;
    (disp_w, disp_h)
}

const MAX_IMAGE_CACHE_ENTRIES: usize = 48;

#[derive(Clone)]
struct CachedDecodedImage {
    img: slint::Image,
    w: i32,
    h: i32,
    raw_svg: Option<String>,
}

static HAS_PENDING_REMOTE_IMAGES: AtomicBool = AtomicBool::new(false);

fn image_cache_key(tag: u8, data: &[u8]) -> (u64, u64, usize) {
    let mut h1 = DefaultHasher::new();
    h1.write_u8(tag);
    h1.write(data);
    let mut h2 = 0xcbf29ce484222325u64 ^ (tag as u64);
    for &b in data {
        h2 ^= b as u64;
        h2 = h2.wrapping_mul(0x100000001b3);
    }
    (h1.finish(), h2, data.len())
}

#[derive(Clone)]
enum CachedImage {
    Loading,
    Ready { img: slint::Image, w: i32, h: i32 },
    Failed(String),
}

type ImageFetchPayload = (String, Result<(Vec<u8>, Option<String>), String>);

struct ImageFetchBus {
    cache: HashMap<String, CachedImage>,
    decoded_cache: HashMap<(u64, u64, usize), CachedDecodedImage>,
    in_flight: HashSet<String>,
    tx: mpsc::Sender<ImageFetchPayload>,
    rx: mpsc::Receiver<ImageFetchPayload>,
}

impl ImageFetchBus {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            cache: HashMap::new(),
            decoded_cache: HashMap::new(),
            in_flight: HashSet::new(),
            tx,
            rx,
        }
    }

    fn insert_decoded(&mut self, key: (u64, u64, usize), entry: CachedDecodedImage) {
        if self.decoded_cache.len() >= MAX_IMAGE_CACHE_ENTRIES && !self.decoded_cache.contains_key(&key) {
            self.decoded_cache.clear();
        }
        self.decoded_cache.insert(key, entry);
    }

    fn insert_remote(&mut self, url: String, entry: CachedImage) {
        if self.cache.len() >= MAX_IMAGE_CACHE_ENTRIES && !self.cache.contains_key(&url) {
            self.cache.retain(|_, v| matches!(v, CachedImage::Loading));
            if self.cache.len() >= MAX_IMAGE_CACHE_ENTRIES {
                self.cache.clear();
            }
        }
        self.cache.insert(url, entry);
    }
}

thread_local! {
    static IMAGE_BUS: RefCell<ImageFetchBus> = RefCell::new(ImageFetchBus::new());
}

enum ResolvedImage {
    Ready {
        img: slint::Image,
        w: i32,
        h: i32,
        raw_svg: Option<String>,
    },
    Loading,
    Failed(String),
}

fn ensure_svg_xmlns(svg: &str) -> String {
    let lower = svg.to_ascii_lowercase();
    if let Some(open_pos) = lower.find("<svg") {
        let after = &svg[open_pos..];
        let tag_end = after.find('>').unwrap_or(after.len());
        let tag_slice = &lower[open_pos..open_pos + tag_end];
        if !tag_slice.contains("xmlns=") {
            let mut out = String::with_capacity(svg.len() + 36);
            out.push_str(&svg[..open_pos + 4]);
            out.push_str(r#" xmlns="http://www.w3.org/2000/svg""#);
            out.push_str(&svg[open_pos + 4..]);
            return out;
        }
    }
    svg.to_string()
}

fn extract_xml_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let pos = lower.find(&needle)?;
    let rest = tag[pos + needle.len()..].trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let inner = &rest[1..];
    let end = inner.find(quote)?;
    Some(&inner[..end])
}

fn parse_svg_num(s: &str) -> Option<f32> {
    let s = s.trim();
    if s.ends_with('%') {
        return None;
    }
    let clean: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    let v = clean.parse::<f32>().ok()?;
    if v > 0.0 {
        Some(v)
    } else {
        None
    }
}

fn parse_svg_dimensions(svg: &str) -> (i32, i32) {
    let lower = svg.to_ascii_lowercase();
    if let Some(start) = lower.find("<svg") {
        let rest = &svg[start..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..end];

        if let (Some(w_str), Some(h_str)) =
            (extract_xml_attr(tag, "width"), extract_xml_attr(tag, "height"))
        {
            if let (Some(w), Some(h)) = (parse_svg_num(w_str), parse_svg_num(h_str)) {
                return (w.round().clamp(48.0, 2000.0) as i32, h.round().clamp(48.0, 2000.0) as i32);
            }
        }
        if let Some(vb) = extract_xml_attr(tag, "viewbox") {
            let nums: Vec<f32> = vb
                .split(|c: char| c.is_whitespace() || c == ',')
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse::<f32>().ok())
                .collect();
            if nums.len() >= 4 && nums[2] > 0.0 && nums[3] > 0.0 {
                return (
                    nums[2].round().clamp(48.0, 2000.0) as i32,
                    nums[3].round().clamp(48.0, 2000.0) as i32,
                );
            }
        }
    }
    (320, 200)
}

fn decode_svg_image(raw_svg: &str) -> Option<(slint::Image, i32, i32, String)> {
    let trimmed = raw_svg.trim();
    if trimmed.is_empty() {
        return None;
    }
    let key = image_cache_key(b'S', trimmed.as_bytes());
    let cached = IMAGE_BUS.with(|bus| {
        bus.borrow().decoded_cache.get(&key).and_then(|e| {
            e.raw_svg
                .as_ref()
                .map(|svg| (e.img.clone(), e.w, e.h, svg.clone()))
        })
    });
    if let Some(hit) = cached {
        return Some(hit);
    }

    let lower = trimmed.to_ascii_lowercase();
    if !lower.contains("<svg") || !lower.contains("</svg>") {
        return None;
    }
    let normalized = ensure_svg_xmlns(trimmed);
    let img = slint::Image::load_from_svg_data(normalized.as_bytes()).ok()?;
    let sz = img.size();
    let (w, h) = if sz.width > 0 && sz.height > 0 {
        (sz.width as i32, sz.height as i32)
    } else {
        parse_svg_dimensions(&normalized)
    };
    let w = w.max(48);
    let h = h.max(32);
    IMAGE_BUS.with(|bus| {
        bus.borrow_mut().insert_decoded(
            key,
            CachedDecodedImage {
                img: img.clone(),
                w,
                h,
                raw_svg: Some(normalized.clone()),
            },
        );
    });
    Some((img, w, h, normalized))
}

fn decode_raster_image_bytes(bytes: &[u8]) -> Option<(slint::Image, i32, i32, Option<String>)> {
    if bytes.is_empty() {
        return None;
    }
    // Check if payload is SVG XML (even with leading BOM/whitespace/<?xml)
    let head = bytes.get(..bytes.len().min(512)).unwrap_or(bytes);
    if let Ok(head_str) = std::str::from_utf8(head) {
        let hl = head_str.to_ascii_lowercase();
        if hl.contains("<svg") {
            if let Ok(full_str) = std::str::from_utf8(bytes) {
                if let Some((img, w, h, norm)) = decode_svg_image(full_str) {
                    return Some((img, w, h, Some(norm)));
                }
            }
        }
    }

    let dyn_img = image::load_from_memory(bytes).ok()?;
    let rgba = dyn_img.to_rgba8();
    let w = rgba.width();
    let h = rgba.height();
    if w == 0 || h == 0 {
        return None;
    }
    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
    Some((slint::Image::from_rgba8(buf), w as i32, h as i32, None))
}

fn percent_decode_utf8(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push(((hi << 4) | lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn resolve_local_path(src: &str) -> Option<std::path::PathBuf> {
    let trimmed = src.trim();
    if let Some(rest) = trimmed.strip_prefix("file://") {
        // Handle file:///C:/... (Windows) and file://C:/... or file:///...
        let cleaned = rest.strip_prefix('/').unwrap_or(rest);
        let decoded = percent_decode_utf8(cleaned);
        #[cfg(windows)]
        let p = std::path::PathBuf::from(decoded.replace('/', "\\"));
        #[cfg(not(windows))]
        let p = std::path::PathBuf::from(decoded);
        return if p.exists() { Some(p) } else { None };
    }
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("data:")
    {
        return None;
    }
    let p = Path::new(trimmed);
    if p.exists() && p.is_file() {
        Some(p.to_path_buf())
    } else {
        None
    }
}

fn resolve_image_source(src: &str) -> ResolvedImage {
    let src = src.trim();
    if src.is_empty() {
        return ResolvedImage::Failed(
            app_core::tr("URL gambar kosong", "Image URL is empty").into(),
        );
    }

    // 1. data: URI
    if let Some(rest) = src.strip_prefix("data:") {
        let key = image_cache_key(b'D', src.as_bytes());
        if let Some(hit) = IMAGE_BUS.with(|bus| bus.borrow().decoded_cache.get(&key).cloned()) {
            return ResolvedImage::Ready {
                img: hit.img,
                w: hit.w,
                h: hit.h,
                raw_svg: hit.raw_svg,
            };
        }
        if let Some((meta, payload)) = rest.split_once(',') {
            let meta_lower = meta.to_ascii_lowercase();
            let raw_bytes = if meta_lower.ends_with(";base64") {
                match app_core::decode_base64_bytes(payload) {
                    Ok(b) => b,
                    Err(e) => {
                        return ResolvedImage::Failed(
                            app_core::tr(
                                &format!("Base64 tidak valid: {e}"),
                                &format!("Invalid Base64: {e}"),
                            )
                            .into(),
                        );
                    }
                }
            } else {
                percent_decode_utf8(payload).into_bytes()
            };
            return match decode_raster_image_bytes(&raw_bytes) {
                Some((img, w, h, raw_svg)) => {
                    IMAGE_BUS.with(|bus| {
                        bus.borrow_mut().insert_decoded(
                            key,
                            CachedDecodedImage {
                                img: img.clone(),
                                w,
                                h,
                                raw_svg: raw_svg.clone(),
                            },
                        );
                    });
                    ResolvedImage::Ready {
                        img,
                        w,
                        h,
                        raw_svg,
                    }
                }
                None => ResolvedImage::Failed(
                    app_core::tr("Gagal decode data gambar", "Failed to decode image data").into(),
                ),
            };
        }
        return ResolvedImage::Failed(
            app_core::tr("Format data URI tidak valid", "Invalid data URI format").into(),
        );
    }

    // 2. Local filesystem / file:// URI
    if let Some(path) = resolve_local_path(src) {
        let meta_sig = std::fs::metadata(&path).ok().map(|m| {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let mut sig = Vec::with_capacity(src.len() + 24);
            sig.extend_from_slice(src.as_bytes());
            sig.extend_from_slice(&m.len().to_le_bytes());
            sig.extend_from_slice(&mtime.to_le_bytes());
            image_cache_key(b'F', &sig)
        });
        if let Some(key) = meta_sig {
            if let Some(hit) = IMAGE_BUS.with(|bus| bus.borrow().decoded_cache.get(&key).cloned()) {
                return ResolvedImage::Ready {
                    img: hit.img,
                    w: hit.w,
                    h: hit.h,
                    raw_svg: hit.raw_svg,
                };
            }
        }
        return match std::fs::read(&path) {
            Ok(bytes) => match decode_raster_image_bytes(&bytes) {
                Some((img, w, h, raw_svg)) => {
                    if let Some(key) = meta_sig {
                        IMAGE_BUS.with(|bus| {
                            bus.borrow_mut().insert_decoded(
                                key,
                                CachedDecodedImage {
                                    img: img.clone(),
                                    w,
                                    h,
                                    raw_svg: raw_svg.clone(),
                                },
                            );
                        });
                    }
                    ResolvedImage::Ready {
                        img,
                        w,
                        h,
                        raw_svg,
                    }
                }
                None => ResolvedImage::Failed(
                    app_core::tr(
                        "Format file gambar tidak didukung",
                        "Unsupported image file format",
                    )
                    .into(),
                ),
            },
            Err(e) => ResolvedImage::Failed(
                app_core::tr(
                    &format!("Gagal membaca file: {e}"),
                    &format!("Failed to read file: {e}"),
                )
                .into(),
            ),
        };
    }

    // 3. Remote HTTP / HTTPS URL (cached + async background fetch)
    if src.starts_with("http://") || src.starts_with("https://") {
        let mut status = None;
        IMAGE_BUS.with(|bus| {
            let mut b = bus.borrow_mut();
            if let Some(cached) = b.cache.get(src) {
                status = Some(cached.clone());
                return;
            }
            if !b.in_flight.contains(src) {
                b.in_flight.insert(src.to_string());
                HAS_PENDING_REMOTE_IMAGES.store(true, Ordering::Relaxed);
                b.insert_remote(src.to_string(), CachedImage::Loading);
                let tx = b.tx.clone();
                let url = src.to_string();
                std::thread::spawn(move || {
                    let res = app_core::fetch_image_bytes(&url);
                    let _ = tx.send((url, res));
                });
            }
            status = Some(CachedImage::Loading);
        });
        return match status.unwrap_or(CachedImage::Loading) {
            CachedImage::Loading => ResolvedImage::Loading,
            CachedImage::Ready { img, w, h } => ResolvedImage::Ready {
                img,
                w,
                h,
                raw_svg: None,
            },
            CachedImage::Failed(e) => ResolvedImage::Failed(e),
        };
    }

    ResolvedImage::Failed(
        app_core::tr(
            "Sumber gambar tidak valid atau file tidak ditemukan",
            "Invalid image source or file not found",
        )
        .into(),
    )
}

/// Returns `true` while any remote markdown image is still downloading.
pub fn has_pending_remote_images() -> bool {
    HAS_PENDING_REMOTE_IMAGES.load(Ordering::Relaxed)
}

/// Drains completed remote image fetches, caches decoded `slint::Image`s, and updates
/// any chat messages currently showing a loading spinner for those URLs.
/// Returns `true` if any message changed.
pub fn pump_remote_images(model: &slint::VecModel<HudMessage>) -> bool {
    let mut completed = Vec::new();
    IMAGE_BUS.with(|bus| {
        let mut b = bus.borrow_mut();
        while let Ok(item) = b.rx.try_recv() {
            b.in_flight.remove(&item.0);
            completed.push(item);
        }
        HAS_PENDING_REMOTE_IMAGES.store(!b.in_flight.is_empty(), Ordering::Relaxed);
    });

    if completed.is_empty() {
        return false;
    }

    let mut updated_urls = HashSet::with_capacity(completed.len());
    let mut decoded_entries = Vec::with_capacity(completed.len());
    for (url, res) in completed {
        let entry = match res {
            Ok((bytes, _ct)) => match decode_raster_image_bytes(&bytes) {
                Some((img, w, h, _svg)) => CachedImage::Ready { img, w, h },
                None => CachedImage::Failed(
                    app_core::tr(
                        "Format gambar tidak didukung",
                        "Unsupported image format",
                    )
                    .into(),
                ),
            },
            Err(e) => CachedImage::Failed(e),
        };
        updated_urls.insert(url.clone());
        decoded_entries.push((url, entry));
    }

    IMAGE_BUS.with(|bus| {
        let mut b = bus.borrow_mut();
        for (url, entry) in decoded_entries {
            b.insert_remote(url, entry);
        }
    });

    if updated_urls.is_empty() {
        return false;
    }

    let mut any_changed = false;
    for i in 0..model.row_count() {
        let Some(mut msg) = model.row_data(i) else {
            continue;
        };
        let mut needs_rebuild = false;
        for b_idx in 0..msg.blocks.row_count() {
            if let Some(blk) = msg.blocks.row_data(b_idx) {
                if blk.kind == "image"
                    && blk.img_loading
                    && updated_urls.contains(blk.img_url.as_str())
                {
                    needs_rebuild = true;
                    break;
                }
            }
        }
        if needs_rebuild {
            msg.blocks = blocks_model(&msg.body, msg.markdown);
            model.set_row_data(i, msg);
            any_changed = true;
        }
    }
    if any_changed {
        relayout_dirty(model, None);
    }
    any_changed
}

/// Parses `![alt](src)` (with optional optional title `"..."`) or `[![alt](src)](link_url)`
/// starting at index 0 of `s`. Returns `(consumed_bytes, alt, img_src, click_url)`.
fn parse_markdown_image_at(s: &str) -> Option<(usize, String, String, String)> {
    let mut wrapped_link = false;
    let img_start = if s.starts_with("[![") {
        wrapped_link = true;
        1usize
    } else if s.starts_with("![") {
        0usize
    } else {
        return None;
    };

    let after_bang_bracket = &s[img_start + 2..];
    let alt_end_rel = after_bang_bracket.find(']')?;
    let alt = after_bang_bracket[..alt_end_rel].trim().to_string();
    let after_alt = &after_bang_bracket[alt_end_rel + 1..];
    if !after_alt.starts_with('(') {
        return None;
    }

    let (img_dest_raw, paren_len) = parse_balanced_parens(after_alt)?;
    let img_src = strip_markdown_link_title(&img_dest_raw);
    if img_src.is_empty() {
        return None;
    }

    let mut total_consumed = img_start + 2 + alt_end_rel + 1 + paren_len;
    let mut click_url = img_src.clone();

    if wrapped_link {
        let after_badge = &s[total_consumed..];
        if after_badge.starts_with("](") {
            if let Some((outer_dest, outer_paren_len)) = parse_balanced_parens(&after_badge[1..]) {
                let cleaned_outer = strip_markdown_link_title(&outer_dest);
                if !cleaned_outer.is_empty() {
                    click_url = cleaned_outer;
                }
                total_consumed += 1 + outer_paren_len;
            } else {
                return None;
            }
        } else {
            return None;
        }
    }

    Some((total_consumed, alt, img_src, click_url))
}

fn parse_balanced_parens(s: &str) -> Option<(String, usize)> {
    if !s.starts_with('(') {
        return None;
    }
    let mut depth = 0usize;
    for (idx, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some((s[1..idx].trim().to_string(), idx + 1));
                }
            }
            '\n' | '\r' => return None,
            _ => {}
        }
    }
    None
}

fn strip_markdown_link_title(dest: &str) -> String {
    let trimmed = dest.trim();
    if trimmed.starts_with('<') && trimmed.ends_with('>') && trimmed.len() >= 2 {
        return trimmed[1..trimmed.len() - 1].trim().to_string();
    }
    // Data URIs contain no unescaped spaces and never titles
    if trimmed.starts_with("data:") {
        return trimmed.to_string();
    }
    // Strip trailing ` "title"` or ` 'title'` if present
    if let Some(ws) = trimmed.find(char::is_whitespace) {
        let tail = trimmed[ws..].trim();
        if (tail.starts_with('"') && tail.ends_with('"'))
            || (tail.starts_with('\'') && tail.ends_with('\''))
        {
            return trimmed[..ws].trim().to_string();
        }
    }
    trimmed.to_string()
}

/// Replaces `![alt](src)` outside code spans with short `[Gambar: alt]` labels for compact view.
fn replace_markdown_images_summary(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut i = 0usize;
    let mut in_code = false;
    let bytes = line.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'`' {
            in_code = !in_code;
            out.push('`');
            i += 1;
            continue;
        }
        if !in_code && (line[i..].starts_with("![") || line[i..].starts_with("[![")) {
            if let Some((consumed, alt, _src, _url)) = parse_markdown_image_at(&line[i..]) {
                if alt.is_empty() {
                    out.push_str("🖼 **[Gambar]**");
                } else {
                    out.push_str(&format!("🖼 **[Gambar: {alt}]**"));
                }
                i += consumed;
                continue;
            }
        }
        let ch = line[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Replaces citations `[reference:N]` -> `[N+1]`, expands emoji `:shortcodes:`, and
/// swaps glyphs absent from `AemeathSans` (`✦`, `◦`, `▪`, `▸`, `▎`) outside inline code spans.
fn normalize_inline_md(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut in_code = false;
    let mut code_ticks = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'`' {
            let mut ticks = 0;
            while i + ticks < bytes.len() && bytes[i + ticks] == b'`' {
                ticks += 1;
            }
            for _ in 0..ticks {
                out.push('`');
            }
            if !in_code {
                in_code = true;
                code_ticks = ticks;
            } else if ticks == code_ticks {
                in_code = false;
                code_ticks = 0;
            }
            i += ticks;
            continue;
        }

        if !in_code {
            // Citations: [reference:N] -> [N+1], [citation:N] -> [N]
            if let Some((len, label)) = parse_citation_tag(&input[i..]) {
                out.push_str(&label);
                i += len;
                continue;
            }

            // Emoji :shortcode: outside inline code
            if bytes[i] == b':' {
                if let Some(end_rel) = input[i + 1..].find(':') {
                    let cand = &input[i..i + 1 + end_rel + 1];
                    if end_rel > 0 && end_rel <= 28 && !cand.contains(char::is_whitespace) {
                        let expanded = crate::emoji::expand_shortcodes(cand);
                        if expanded != cand {
                            match expanded.as_str() {
                                "✅" | "✔" => out.push_str("<font color=\"#22C55E\">✅</font>"),
                                "❌" | "❎" => out.push_str("<font color=\"#EF4444\">❌</font>"),
                                _ => out.push_str(&expanded),
                            }
                            i += cand.len();
                            continue;
                        }
                    }
                }
            }

            // Case-insensitive <br>, <br/>, <br /> inside table cells/prose
            if bytes[i] == b'<' {
                let tail = &input[i..];
                let lower4 = tail.get(..4).map(|s| s.to_ascii_lowercase());
                let lower5 = tail.get(..5).map(|s| s.to_ascii_lowercase());
                let lower6 = tail.get(..6).map(|s| s.to_ascii_lowercase());
                if lower4.as_deref() == Some("<br>") {
                    out.push_str("  \n");
                    i += 4;
                    continue;
                }
                if lower5.as_deref() == Some("<br/>") {
                    out.push_str("  \n");
                    i += 5;
                    continue;
                }
                if lower6.as_deref() == Some("<br />") {
                    out.push_str("  \n");
                    i += 6;
                    continue;
                }
            }
        }

        let ch = input[i..].chars().next().unwrap();
        if !in_code {
            match ch {
                '✅' | '☑' | '✔' => out.push_str("<font color=\"#22C55E\">✅</font>"),
                '❌' | '❎' | '✖' | '✘' | '☒' => out.push_str("<font color=\"#EF4444\">❌</font>"),
                '✦' => out.push('◆'),
                '◦' => out.push('○'),
                '▪' => out.push('■'),
                '▸' => out.push('›'),
                '▎' => out.push('│'),
                _ => out.push(ch),
            }
        } else {
            out.push(ch);
        }
        i += ch.len_utf8();
    }
    out
}

/// Escapes unknown `<tag>` sequences outside inline code so `StyledText::from_markdown`
/// preserves generic types (`Vec<T>`) or comparisons instead of rejecting HTML tags.
fn escape_unsupported_html_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    let mut chars = input.char_indices().peekable();
    let mut in_code = false;
    while let Some((idx, ch)) = chars.next() {
        if ch == '`' {
            in_code = !in_code;
            out.push(ch);
            continue;
        }
        if !in_code && ch == '<' {
            let rest = &input[idx..];
            let lower = rest.get(..12).unwrap_or(rest).to_ascii_lowercase();
            let allowed = lower.starts_with("<u>")
                || lower.starts_with("</u>")
                || lower.starts_with("</font>")
                || lower.starts_with("<font ");
            if !allowed {
                if let Some(&(_, next_ch)) = chars.peek() {
                    if next_ch.is_ascii_alphabetic() || next_ch == '/' {
                        out.push_str("\\<");
                        continue;
                    }
                }
            }
        }
        out.push(ch);
    }
    out
}

fn has_rich_inline_syntax(s: &str) -> bool {
    s.contains('*')
        || s.contains('`')
        || s.contains("~~")
        || s.contains("](")
        || s.contains("<font")
        || s.contains("</font>")
        || s.contains("<u>")
        || s.contains("</u>")
}

fn strip_heading_bold_markers(norm: &str) -> String {
    let mut clean = norm.trim().to_string();
    if clean.starts_with("**") && clean.ends_with("**") && clean.len() > 4 {
        clean = clean[2..clean.len() - 2].trim().to_string();
    }
    clean.replace("**", "")
}

/// Populates `styled`, `raw`, and `plain_selectable` on a text block. Blocks
/// with no mixed inline markdown (bold/italic/code/links/color tags) render in
/// a read-only `TextInput` so users can highlight & copy them without toggling mode.
fn apply_block_text(blk: &mut MessageBlockItem, md: &str, bold_uniform: bool) {
    let (cleaned_md, inline_clusters) = crate::emoji::extract_inline_emojis(md);
    let inline_px = scaled_px(18);
    let emoji_items: Vec<EmojiGlyphItem> = inline_clusters
        .into_iter()
        .filter_map(|cluster| {
            crate::emoji::render_color_emoji(&cluster, inline_px).map(|img| EmojiGlyphItem {
                img,
                text: cluster.into(),
            })
        })
        .collect();
    blk.emojis = slint::ModelRc::new(slint::VecModel::from(emoji_items));

    let md_ref = cleaned_md.as_str();
    let norm = normalize_inline_md(md_ref);
    if bold_uniform {
        blk.styled = styled_bold_inline(md_ref);
        let collapsed = strip_heading_bold_markers(&norm);
        let plain = collapsed.trim().to_string();
        if !has_rich_inline_syntax(&plain) {
            blk.plain_selectable = true;
            blk.raw = plain.replace("  \n", "\n").into();
        } else {
            blk.plain_selectable = false;
            blk.raw = norm.into();
        }
    } else {
        blk.styled = styled_inline(md_ref);
        if !has_rich_inline_syntax(&norm) {
            blk.plain_selectable = true;
            blk.raw = norm.replace("  \n", "\n").into();
        } else {
            blk.plain_selectable = false;
            blk.raw = norm.into();
        }
    }
}

fn styled_inline(md: &str) -> slint::StyledText {
    let norm = normalize_inline_md(md);
    if let Ok(st) = slint::StyledText::from_markdown(&norm) {
        return st;
    }
    let escaped = escape_unsupported_html_tags(&norm);
    slint::StyledText::from_markdown(&escaped)
        .unwrap_or_else(|_| slint::StyledText::from_plain_text(&norm))
}

fn styled_bold_inline(md: &str) -> slint::StyledText {
    let norm = normalize_inline_md(md);
    let mut clean = norm.trim().to_string();
    if clean.starts_with("**") && clean.ends_with("**") && clean.len() > 4 {
        clean = clean[2..clean.len() - 2].trim().to_string();
    }
    // Strip interior ** pairs inside a heading/table-header so outer **bold** never cancels out.
    let collapsed = clean.replace("**", "");
    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        return slint::StyledText::default();
    }
    let wrapped = format!("**{trimmed}**");
    styled_inline(&wrapped)
}

fn compute_gap_top(prev: Option<&str>, curr: &str, curr_indent: i32) -> i32 {
    let Some(p) = prev else {
        return 0;
    };
    match (p, curr) {
        (_, "divider") | ("divider", _) => 8,
        (_, "heading") => 11,
        ("heading", _) => 4,
        ("list", "list") => {
            if curr_indent > 0 {
                2
            } else {
                4
            }
        }
        ("list", _) => 8,
        (_, "list") => 6,
        (_, "code")
        | ("code", _)
        | (_, "table")
        | ("table", _)
        | (_, "quote")
        | ("quote", _)
        | (_, "image")
        | ("image", _) => 8,
        (_, "emoji-row") | ("emoji-row", _) => 6,
        _ => 7,
    }
}

fn build_image_block(src: &str, caption: &str, click_url: &str, prev_kind: Option<&str>) -> MessageBlockItem {
    let mut blk = default_block("image", compute_gap_top(prev_kind, "image", 0));
    blk.img_url = if click_url.is_empty() {
        src.into()
    } else {
        click_url.into()
    };
    blk.img_caption = normalize_inline_md(caption).into();
    match resolve_image_source(src) {
        ResolvedImage::Ready {
            img,
            w,
            h,
            raw_svg,
        } => {
            blk.img = img;
            blk.img_w = w;
            blk.img_h = h;
            if let Some(svg) = raw_svg {
                blk.raw = svg.into();
                blk.lang = "svg".into();
            }
        }
        ResolvedImage::Loading => {
            blk.img_loading = true;
            blk.img_w = 240;
            blk.img_h = 84;
        }
        ResolvedImage::Failed(err) => {
            blk.img_failed = true;
            blk.raw = err.into();
        }
    }
    blk
}

fn build_svg_block(raw_svg: &str, caption: &str, prev_kind: Option<&str>) -> Option<MessageBlockItem> {
    let (img, w, h, norm) = decode_svg_image(raw_svg)?;
    let mut blk = default_block("image", compute_gap_top(prev_kind, "image", 0));
    blk.img = img;
    blk.img_w = w;
    blk.img_h = h;
    blk.raw = norm.into();
    blk.lang = "svg".into();
    blk.img_caption = caption.into();
    Some(blk)
}

fn split_leading_emoji_with_bold(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim_start();
    if let Some((em, rest)) = crate::emoji::split_leading_emoji(trimmed) {
        return Some((em, rest.trim_start().to_string()));
    }
    for wrapper in ["**", "__"] {
        if let Some(inner) = trimmed.strip_prefix(wrapper) {
            if let Some((em, rest)) = crate::emoji::split_leading_emoji(inner) {
                let rest_trimmed = rest.trim_start();
                if !rest_trimmed.is_empty() && !rest_trimmed.starts_with(wrapper) {
                    return Some((em, format!("{wrapper}{rest_trimmed}")));
                }
            }
        }
    }
    None
}

fn apply_leading_emoji_badge(blk: &mut MessageBlockItem, raw_md: &str, px: u32, bold: bool) {
    let expanded = crate::emoji::expand_shortcodes(raw_md.trim_start());
    if let Some((em, rest_trimmed)) = split_leading_emoji_with_bold(&expanded) {
        if !rest_trimmed.is_empty() {
            if let Some(img) = crate::emoji::render_color_emoji(&em, px) {
                blk.emoji_img = img;
                blk.has_emoji_img = true;
                apply_block_text(blk, &rest_trimmed, bold);
            }
        }
    }
}

fn try_parse_emoji_row(trimmed: &str, prev_kind: Option<&str>) -> Option<MessageBlockItem> {
    if trimmed.is_empty() || trimmed.len() > 96 {
        return None;
    }
    let expanded = crate::emoji::expand_shortcodes(trimmed);
    let clusters = crate::emoji::extract_pure_emoji_row(&expanded)?;
    let px = scaled_px(34);
    let mut items = Vec::with_capacity(clusters.len());
    for cluster in clusters {
        let img = crate::emoji::render_color_emoji(&cluster, px)?;
        items.push(EmojiGlyphItem {
            img,
            text: cluster.into(),
        });
    }
    let mut blk = default_block("emoji-row", compute_gap_top(prev_kind, "emoji-row", 0));
    blk.emojis = slint::ModelRc::new(slint::VecModel::from(items));
    blk.raw = expanded.into();
    Some(blk)
}

fn push_prose_and_inline_images(
    blocks: &mut Vec<MessageBlockItem>,
    prev_kind: &mut Option<&'static str>,
    para_md: &str,
) {
    let bytes = para_md.as_bytes();
    let mut i = 0usize;
    let mut chunk_start = 0usize;
    let mut in_code = false;

    while i < bytes.len() {
        if bytes[i] == b'`' {
            in_code = !in_code;
            i += 1;
            continue;
        }
        if !in_code && (para_md[i..].starts_with("![") || para_md[i..].starts_with("[![")) {
            if let Some((consumed, alt, img_src, click_url)) = parse_markdown_image_at(&para_md[i..])
            {
                let before = para_md[chunk_start..i].trim();
                if !before.is_empty() {
                    let gap = compute_gap_top(*prev_kind, "prose", 0);
                    let mut p_blk = default_block("prose", gap);
                    apply_block_text(&mut p_blk, before, false);
                    blocks.push(p_blk);
                    *prev_kind = Some("prose");
                }
                let img_blk = build_image_block(&img_src, &alt, &click_url, *prev_kind);
                blocks.push(img_blk);
                *prev_kind = Some("image");
                i += consumed;
                chunk_start = i;
                continue;
            }
        }
        let ch = para_md[i..].chars().next().unwrap();
        i += ch.len_utf8();
    }

    let tail = para_md[chunk_start..].trim();
    if !tail.is_empty() {
        let gap = compute_gap_top(*prev_kind, "prose", 0);
        let mut p_blk = default_block("prose", gap);
        apply_block_text(&mut p_blk, tail, false);
        blocks.push(p_blk);
        *prev_kind = Some("prose");
    }
}

fn parse_fence_open(trimmed: &str) -> Option<(char, usize, String)> {
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|&c| c == first).count();
    if len < 3 {
        return None;
    }
    let after = trimmed[len..].trim();
    if first == '`' && after.contains('`') {
        return None;
    }
    let raw_lang = after
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '+' && c != '#' && c != '-' && c != '_')
        .to_ascii_lowercase();
    let lang = match raw_lang.as_str() {
        "" => "code".to_string(),
        "py" | "python3" => "python".to_string(),
        "js" => "javascript".to_string(),
        "ts" => "typescript".to_string(),
        "rs" => "rust".to_string(),
        "sh" | "zsh" | "shell" => "bash".to_string(),
        "yml" => "yaml".to_string(),
        other => other.to_string(),
    };
    Some((first, len, lang))
}

fn is_fence_close(trimmed: &str, fence_char: char, min_len: usize) -> bool {
    let count = trimmed.chars().take_while(|&c| c == fence_char).count();
    count >= min_len && trimmed[count..].trim().is_empty()
}

fn is_hr_line(trimmed: &str) -> bool {
    let stripped: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    if stripped.len() < 3 {
        return false;
    }
    let first = stripped.chars().next().unwrap();
    matches!(first, '-' | '*' | '_') && stripped.chars().all(|c| c == first)
}

fn parse_heading_line(trimmed: &str) -> Option<(i32, String)> {
    let level = trimmed.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &trimmed[level..];
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let clean = rest
        .trim()
        .trim_end_matches('#')
        .trim();
    if clean.is_empty() {
        return None;
    }
    Some((level as i32, clean.to_string()))
}

fn line_has_table_pipe(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let mut in_code = false;
    let mut escaped = false;
    for ch in trimmed.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '`' {
            in_code = !in_code;
            continue;
        }
        if !in_code && ch == '|' {
            return true;
        }
    }
    false
}

fn split_table_cells(line: &str) -> Vec<String> {
    let mut s = line.trim();
    if let Some(rest) = s.strip_prefix('|') {
        s = rest;
    }
    if s.ends_with('|') && !s.ends_with("\\|") {
        s = &s[..s.len() - 1];
    }

    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut in_code = false;
    let mut escaped = false;
    for ch in s.chars() {
        if escaped {
            if ch != '|' {
                cur.push('\\');
            }
            cur.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '`' {
            in_code = !in_code;
            cur.push(ch);
            continue;
        }
        if ch == '|' && !in_code {
            cells.push(cur.trim().to_string());
            cur.clear();
            continue;
        }
        cur.push(ch);
    }
    if escaped {
        cur.push('\\');
    }
    cells.push(cur.trim().to_string());
    cells
}

fn parse_table_separator(line: &str) -> Option<Vec<&'static str>> {
    let trimmed = line.trim();
    if !trimmed.contains('|') && !trimmed.contains('-') {
        return None;
    }
    let has_outer_pipe = trimmed.starts_with('|') && trimmed.ends_with('|');
    let parts = split_table_cells(trimmed);
    if parts.len() < 2 && !has_outer_pipe {
        return None;
    }
    let mut aligns = Vec::with_capacity(parts.len());
    for part in parts {
        let p = part.trim();
        if p.is_empty() {
            return None;
        }
        if !p.chars().all(|c| c == '-' || c == ':') {
            return None;
        }
        let dashes = p.chars().filter(|&c| c == '-').count();
        if dashes < 1 {
            return None;
        }
        let left_colon = p.starts_with(':');
        let right_colon = p.ends_with(':');
        let align = match (left_colon, right_colon) {
            (true, true) => "center",
            (false, true) => "right",
            _ => "left",
        };
        aligns.push(align);
    }
    Some(aligns)
}

fn display_cell_line_len(raw: &str) -> usize {
    let norm = normalize_inline_md(raw);
    let mut best = 0usize;
    for seg in norm.split('\n') {
        let mut plain = String::with_capacity(seg.len());
        let chars: Vec<char> = seg.chars().collect();
        let mut i = 0usize;
        while i < chars.len() {
            // [label](url) -> label
            if chars[i] == '[' {
                if let Some(close_rel) = chars[i + 1..].iter().position(|&c| c == ']') {
                    let close_idx = i + 1 + close_rel;
                    if close_idx + 1 < chars.len() && chars[close_idx + 1] == '(' {
                        if let Some(paren_rel) =
                            chars[close_idx + 2..].iter().position(|&c| c == ')')
                        {
                            for &c in &chars[i + 1..close_idx] {
                                if c != '*' && c != '`' && c != '~' {
                                    plain.push(c);
                                }
                            }
                            i = close_idx + 2 + paren_rel + 1;
                            continue;
                        }
                    }
                }
            }
            let c = chars[i];
            if c == '<' {
                let tail: String = chars[i..].iter().collect();
                let lower = tail.to_ascii_lowercase();
                if lower.starts_with("<u>") || lower.starts_with("</u>") {
                    i += if lower.starts_with("<u>") { 3 } else { 4 };
                    continue;
                }
            }
            if !matches!(c, '*' | '`') {
                plain.push(c);
            }
            i += 1;
        }
        let w: usize = plain
            .trim()
            .chars()
            .map(|c| if c as u32 > 0x2E80 { 2 } else { 1 })
            .sum();
        best = best.max(w);
    }
    best
}

fn compute_table_col_widths(headers: &[String], rows: &[Vec<String>]) -> Vec<i32> {
    let n = headers.len().max(1);
    let mut raw_w = vec![56i32; n];

    for (col, slot) in raw_w.iter_mut().enumerate().take(n) {
        let h_chars = headers.get(col).map(|s| display_cell_line_len(s)).unwrap_or(1);
        let mut max_px = ((h_chars as f32) * 7.6).ceil() as i32 + 22;
        for row in rows {
            if let Some(cell) = row.get(col) {
                let c_chars = display_cell_line_len(cell);
                let cell_px = ((c_chars as f32) * 7.0).ceil() as i32 + 22;
                max_px = max_px.max(cell_px);
            }
        }
        let floor = if h_chars <= 3
            && rows
                .iter()
                .all(|r| r.get(col).map(|s| display_cell_line_len(s)).unwrap_or(0) <= 4)
        {
            44
        } else {
            58
        };
        *slot = max_px.clamp(floor, 215);
    }

    let div_borders = ((n as i32) - 1).max(0);
    let avail = 330 - div_borders;
    let sum: i32 = raw_w.iter().sum();

    if sum < avail {
        let extra = avail - sum;
        let total = sum.max(1);
        let mut distributed = 0i32;
        for (col, slot) in raw_w.iter_mut().enumerate().take(n) {
            let add = if col + 1 == n {
                extra - distributed
            } else {
                (extra * *slot) / total
            };
            *slot += add;
            distributed += add;
        }
    } else if n <= 3 && sum <= ((avail as f32) * 1.32) as i32 {
        let mut scaled = vec![0i32; n];
        let mut all_readable = true;
        let mut used = 0i32;
        for col in 0..n {
            let w = if col + 1 == n {
                avail - used
            } else {
                ((raw_w[col] * avail) / sum.max(1)).max(60)
            };
            if w < 60 {
                all_readable = false;
                break;
            }
            scaled[col] = w;
            used += w;
        }
        if all_readable {
            return scaled;
        }
    }

    raw_w
}

fn parse_list_line(line: &str) -> Option<(i32, String, String)> {
    let mut spaces = 0usize;
    let mut byte_idx = 0usize;
    for ch in line.chars() {
        match ch {
            ' ' => {
                spaces += 1;
                byte_idx += 1;
            }
            '\t' => {
                spaces += 2;
                byte_idx += 1;
            }
            _ => break,
        }
    }
    let indent = ((spaces / 2) as i32).min(3);
    let rest = &line[byte_idx..];

    // Task & status items (- [ ], - [x], - [-], - ✅, - ☐, - ❌, etc.)
    for (prefix, marker) in [
        ("- [ ] ", "todo"),
        ("* [ ] ", "todo"),
        ("+ [ ] ", "todo"),
        ("- ☐ ", "todo"),
        ("* ☐ ", "todo"),
        ("+ ☐ ", "todo"),
        ("- ⬜ ", "todo"),
        ("* ⬜ ", "todo"),
        ("☐ ", "todo"),
        ("⬜ ", "todo"),
        ("- [x] ", "done"),
        ("- [X] ", "done"),
        ("* [x] ", "done"),
        ("* [X] ", "done"),
        ("+ [x] ", "done"),
        ("+ [X] ", "done"),
        ("- ✅ ", "done"),
        ("* ✅ ", "done"),
        ("+ ✅ ", "done"),
        ("- ☑ ", "done"),
        ("* ☑ ", "done"),
        ("- ✔ ", "done"),
        ("✅ ", "done"),
        ("☑ ", "done"),
        ("- [-] ", "cross"),
        ("* [-] ", "cross"),
        ("+ [-] ", "cross"),
        ("- [~] ", "cross"),
        ("* [~] ", "cross"),
        ("- ❌ ", "cross"),
        ("* ❌ ", "cross"),
        ("+ ❌ ", "cross"),
        ("- ❎ ", "cross"),
        ("* ❎ ", "cross"),
        ("- ✖ ", "cross"),
        ("* ✖ ", "cross"),
        ("❌ ", "cross"),
        ("❎ ", "cross"),
    ] {
        if let Some(body) = rest.strip_prefix(prefix) {
            return Some((indent, marker.to_string(), body.trim_start().to_string()));
        }
    }

    // Unordered items
    for prefix in ["- ", "* ", "+ ", "• "] {
        if let Some(body) = rest.strip_prefix(prefix) {
            let marker = match indent {
                0 => "•",
                1 => "○",
                _ => "›",
            };
            return Some((indent, marker.to_string(), body.trim_start().to_string()));
        }
    }

    // Ordered items (1..999 followed by ". " or ") ")
    let digits_len = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if (1..=3).contains(&digits_len) {
        let after_num = &rest[digits_len..];
        if after_num.starts_with(". ") || after_num.starts_with(") ") {
            let num = &rest[..digits_len];
            let body = &after_num[2..];
            return Some((indent, format!("{num}."), body.trim_start().to_string()));
        }
    }

    None
}

fn try_parse_pure_image_line(trimmed: &str) -> Option<(String, String, String)> {
    let (consumed, alt, img_src, click_url) = parse_markdown_image_at(trimmed)?;
    if trimmed[consumed..].trim().is_empty() {
        Some((alt, img_src, click_url))
    } else {
        None
    }
}

fn try_promote_tips_blockquote(line: &str) -> Option<String> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('>') {
        return None;
    }
    for pat in [
        "✦ **Tips",
        "✦ Tips",
        "◆ **Tips",
        "◆ Tips",
        "💡 **Tips",
        "💡 Tips",
        "**Tips Aemeath**",
        "**Tips Aemeath:**",
        "**Tips:**",
        "**Tips ",
        "Tips Aemeath:",
        "Tips Aemeath :",
    ] {
        if let Some(after) = t.strip_prefix(pat) {
            let rest = match pat {
                "✦ **Tips" | "◆ **Tips" | "💡 **Tips" => format!("**Tips{after}"),
                "✦ Tips" | "◆ Tips" | "💡 Tips" => format!("**Tips**{after}"),
                _ => t.to_string(),
            };
            return Some(format!("💡 {rest}"));
        }
    }
    None
}

fn starts_block_boundary(lines: &[&str], idx: usize) -> bool {
    let line = lines[idx];
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    if parse_fence_open(trimmed).is_some()
        || is_hr_line(trimmed)
        || parse_heading_line(trimmed).is_some()
        || trimmed.starts_with('>')
        || try_promote_tips_blockquote(trimmed).is_some()
        || parse_list_line(line).is_some()
        || try_parse_pure_image_line(trimmed).is_some()
        || trimmed.to_ascii_lowercase().starts_with("<svg")
    {
        return true;
    }
    if trimmed.len() <= 96
        && crate::emoji::extract_pure_emoji_row(&crate::emoji::expand_shortcodes(trimmed)).is_some()
    {
        return true;
    }
    if idx + 1 < lines.len()
        && line_has_table_pipe(line)
        && parse_table_separator(lines[idx + 1]).is_some()
    {
        return true;
    }
    false
}

fn strip_one_count_banner<'a>(s: &'a str, verbs: &[&str], nouns: &[&str]) -> Option<&'a str> {
    for verb in verbs {
        if let Some(after_verb) = s.strip_prefix(verb) {
            let digits_len = after_verb.bytes().take_while(|b| b.is_ascii_digit()).count();
            if digits_len > 0 {
                let after_num = &after_verb[digits_len..];
                for noun in nouns {
                    if let Some(after_noun) = after_num.strip_prefix(noun) {
                        let trimmed = after_noun
                            .trim_start_matches("...")
                            .trim_start_matches('…');
                        return Some(trimmed);
                    }
                }
            }
        }
    }
    None
}

fn strip_banner_prefix(s: &str) -> Option<&str> {
    for pat in [
        "Searching for images...",
        "Searching for images…",
        "Searching the web...",
        "Searching the web…",
        "Searching...",
        "Searching…",
        "Relevant images found",
        "Relevant results found",
    ] {
        if let Some(rest) = s.strip_prefix(pat) {
            return Some(rest);
        }
    }
    for bare in ["Searching for images", "Searching the web"] {
        if let Some(rest) = s.strip_prefix(bare) {
            if rest.is_empty()
                || rest.starts_with("Relevant")
                || rest.starts_with("Searching")
                || rest.starts_with("Found ")
                || rest.starts_with("Reading ")
                || rest.starts_with(|c: char| c.is_ascii_whitespace())
            {
                return Some(rest);
            }
        }
    }
    strip_one_count_banner(
        s,
        &["Found ", "Reading "],
        &[
            " web pages",
            " web page",
            " results",
            " result",
            " images",
            " image",
            " pages",
            " page",
        ],
    )
}

fn strip_leaked_search_banners(body: &str) -> String {
    let norm = body.replace("\r\n", "\n");
    let mut out = String::with_capacity(norm.len());
    let mut in_fence = false;
    let mut fence_ch = '\0';
    let mut fence_len = 0usize;

    for (idx, line) in norm.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let t = line.trim_start();
        if in_fence {
            if is_fence_close(t.trim(), fence_ch, fence_len) {
                in_fence = false;
            }
            out.push_str(line);
            continue;
        }
        if let Some((fc, fl, _)) = parse_fence_open(t.trim()) {
            in_fence = true;
            fence_ch = fc;
            fence_len = fl;
            out.push_str(line);
            continue;
        }

        let mut cur = t;
        let mut stripped_any = false;
        while let Some(rest) = strip_banner_prefix(cur) {
            stripped_any = true;
            cur = rest.trim_start();
        }
        out.push_str(if stripped_any { cur } else { line });
    }
    out
}

/// Parses Markdown into structured UI blocks for crisp native rendering in `HudWindow`.
pub fn parse_message_blocks(body: &str) -> Vec<MessageBlockItem> {
    let norm_crlf = strip_leaked_search_banners(body);
    let lines: Vec<&str> = norm_crlf.lines().collect();
    let mut blocks = Vec::new();
    let mut prev_kind: Option<&'static str> = None;
    let mut i = 0usize;

    while i < lines.len() {
        let raw_line = lines[i];
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed == "\u{a0}" {
            i += 1;
            continue;
        }

        // 1. Fenced code block (closed or unclosed while streaming); promotes closed valid SVG fences to image blocks
        if let Some((fence_char, fence_len, lang)) = parse_fence_open(trimmed) {
            i += 1;
            let mut closed = false;
            let mut code_lines = Vec::new();
            while i < lines.len() {
                if is_fence_close(lines[i].trim(), fence_char, fence_len) {
                    closed = true;
                    i += 1;
                    break;
                }
                code_lines.push(lines[i].replace('\t', "    "));
                i += 1;
            }
            while code_lines.last().is_some_and(|l| l.trim().is_empty()) {
                code_lines.pop();
            }
            let raw_code = code_lines.join("\n");
            let is_svg_fence =
                lang == "svg" || (lang == "xml" && raw_code.to_ascii_lowercase().contains("<svg"));
            if closed && is_svg_fence {
                if let Some(svg_blk) = build_svg_block(&raw_code, "Diagram SVG", prev_kind) {
                    blocks.push(svg_blk);
                    prev_kind = Some("image");
                    continue;
                }
            }
            let gap = compute_gap_top(prev_kind, "code", 0);
            let mut blk = default_block("code", gap);
            blk.raw = raw_code.into();
            blk.plain_selectable = true;
            blk.lang = lang.into();
            blocks.push(blk);
            prev_kind = Some("code");
            continue;
        }

        // 1b. Standalone raw inline <svg ...>...</svg> block (when complete)
        if trimmed.to_ascii_lowercase().starts_with("<svg") {
            let end_idx = lines
                .iter()
                .enumerate()
                .skip(i)
                .find(|(_, line)| line.to_ascii_lowercase().contains("</svg>"))
                .map(|(j, _)| j);
            if let Some(end_j) = end_idx {
                let raw_svg = lines[i..=end_j].join("\n");
                if let Some(svg_blk) = build_svg_block(&raw_svg, "Diagram SVG", prev_kind) {
                    blocks.push(svg_blk);
                    prev_kind = Some("image");
                    i = end_j + 1;
                    continue;
                }
            }
        }

        // 2. Horizontal divider
        if is_hr_line(trimmed) {
            let gap = compute_gap_top(prev_kind, "divider", 0);
            blocks.push(default_block("divider", gap));
            prev_kind = Some("divider");
            i += 1;
            continue;
        }

        // 3. ATx Heading (# .. ######) with optional leading color emoji badge
        if let Some((level, heading_text)) = parse_heading_line(trimmed) {
            let gap = compute_gap_top(prev_kind, "heading", 0);
            let mut blk = default_block("heading", gap);
            apply_block_text(&mut blk, &heading_text, true);
            blk.level = level;
            apply_leading_emoji_badge(&mut blk, &heading_text, scaled_px(19), true);
            blocks.push(blk);
            prev_kind = Some("heading");
            i += 1;
            continue;
        }

        // 4. Markdown Table (header + separator + data rows)
        if i + 1 < lines.len() && line_has_table_pipe(raw_line) {
            if let Some(mut aligns) = parse_table_separator(lines[i + 1]) {
                let mut hdr_cells = split_table_cells(raw_line);
                let col_count = hdr_cells.len().max(aligns.len()).max(1);
                while hdr_cells.len() < col_count {
                    hdr_cells.push(String::new());
                }
                hdr_cells.truncate(col_count);
                while aligns.len() < col_count {
                    aligns.push("left");
                }
                aligns.truncate(col_count);

                i += 2;
                let mut data_rows: Vec<Vec<String>> = Vec::new();
                while i < lines.len() {
                    let row_trimmed = lines[i].trim();
                    if row_trimmed.is_empty()
                        || parse_fence_open(row_trimmed).is_some()
                        || is_hr_line(row_trimmed)
                        || parse_heading_line(row_trimmed).is_some()
                        || row_trimmed.starts_with('>')
                        || !line_has_table_pipe(lines[i])
                    {
                        break;
                    }
                    let mut cells = split_table_cells(lines[i]);
                    while cells.len() < col_count {
                        cells.push(String::new());
                    }
                    cells.truncate(col_count);
                    data_rows.push(cells);
                    i += 1;
                }

                let col_widths = compute_table_col_widths(&hdr_cells, &data_rows);
                let div_borders = ((col_count as i32) - 1).max(0);
                let table_w = col_widths.iter().sum::<i32>() + div_borders;

                let hdr_items: Vec<TableCellItem> = hdr_cells
                    .iter()
                    .enumerate()
                    .map(|(c, txt)| TableCellItem {
                        styled: styled_bold_inline(txt),
                        width_px: col_widths[c],
                        align: aligns[c].into(),
                        has_text: !txt.trim().is_empty(),
                    })
                    .collect();

                let row_items: Vec<TableRowItem> = data_rows
                    .iter()
                    .map(|row| TableRowItem {
                        cells: slint::ModelRc::new(slint::VecModel::from(
                            row.iter()
                                .enumerate()
                                .map(|(c, txt)| TableCellItem {
                                    styled: styled_inline(txt),
                                    width_px: col_widths[c],
                                    align: aligns[c].into(),
                                    has_text: !txt.trim().is_empty(),
                                })
                                .collect::<Vec<_>>(),
                        )),
                    })
                    .collect();

                let gap = compute_gap_top(prev_kind, "table", 0);
                let mut blk = default_block("table", gap);
                blk.headers = slint::ModelRc::new(slint::VecModel::from(hdr_items));
                blk.rows = slint::ModelRc::new(slint::VecModel::from(row_items));
                blk.table_w = table_w;
                blocks.push(blk);
                prev_kind = Some("table");
                continue;
            }
        }

        // 5. Blockquote (> ...)
        if trimmed.starts_with('>') {
            let mut quote_paras: Vec<String> = Vec::new();
            let mut cur_para: Vec<String> = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim_start();
                if !t.starts_with('>') {
                    break;
                }
                let inner = t.trim_start_matches('>').trim();
                if inner.is_empty() {
                    if !cur_para.is_empty() {
                        quote_paras.push(cur_para.join("  \n"));
                        cur_para.clear();
                    }
                } else {
                    cur_para.push(inner.to_string());
                }
                i += 1;
            }
            if !cur_para.is_empty() {
                quote_paras.push(cur_para.join("  \n"));
            }
            let quote_md = quote_paras.join("\n\n");
            let gap = compute_gap_top(prev_kind, "quote", 0);
            let mut blk = default_block("quote", gap);
            apply_block_text(&mut blk, &quote_md, false);
            apply_leading_emoji_badge(&mut blk, &quote_md, scaled_px(17), false);
            blocks.push(blk);
            prev_kind = Some("quote");
            continue;
        }

        // 5b. Auto-promote "✦ Tips Aemeath:" / "💡 **Tips:**" lines into a blockquote card with color emoji
        if let Some(first_tips) = try_promote_tips_blockquote(trimmed) {
            let mut tip_lines = vec![first_tips];
            i += 1;
            while i < lines.len() {
                if starts_block_boundary(&lines, i) {
                    break;
                }
                tip_lines.push(lines[i].trim().to_string());
                i += 1;
            }
            let quote_md = tip_lines.join("  \n");
            let gap = compute_gap_top(prev_kind, "quote", 0);
            let mut blk = default_block("quote", gap);
            apply_block_text(&mut blk, &quote_md, false);
            apply_leading_emoji_badge(&mut blk, &quote_md, scaled_px(17), false);
            blocks.push(blk);
            prev_kind = Some("quote");
            continue;
        }

        // 6. List item (unordered, ordered, task) with optional leading color emoji badge
        if let Some((indent, marker, first_text)) = parse_list_line(raw_line) {
            let mut item_lines = vec![first_text];
            i += 1;
            while i < lines.len() {
                if starts_block_boundary(&lines, i) {
                    break;
                }
                let next_trimmed = lines[i].trim();
                if next_trimmed.is_empty() {
                    break;
                }
                // Continuation line indented under the list item
                if lines[i].starts_with("  ") || lines[i].starts_with('\t') {
                    item_lines.push(next_trimmed.to_string());
                    i += 1;
                } else {
                    break;
                }
            }
            let item_md = item_lines.join(" ");
            let gap = compute_gap_top(prev_kind, "list", indent);
            let mut blk = default_block("list", gap);
            apply_block_text(&mut blk, &item_md, false);
            blk.marker = marker.clone().into();
            blk.indent = indent;
            if matches!(marker.as_str(), "done" | "todo" | "cross") {
                blk.emoji_img = crate::emoji::render_status_badge(&marker, scaled_px(16).max(32));
            } else {
                apply_leading_emoji_badge(&mut blk, &item_md, scaled_px(16), false);
            }
            blocks.push(blk);
            prev_kind = Some("list");
            continue;
        }

        // 7a. Standalone markdown image ![alt](src) or badge [![alt](src)](href)
        if let Some((alt, img_src, click_url)) = try_parse_pure_image_line(trimmed) {
            let blk = build_image_block(&img_src, &alt, &click_url, prev_kind);
            blocks.push(blk);
            prev_kind = Some("image");
            i += 1;
            continue;
        }

        // 7b. Standalone expressive color emoji reaction row (1..=6 emojis)
        if let Some(em_blk) = try_parse_emoji_row(trimmed, prev_kind) {
            blocks.push(em_blk);
            prev_kind = Some("emoji-row");
            i += 1;
            continue;
        }

        // 8. Prose paragraph (splitting inline ![alt](src) images into dedicated image blocks)
        let mut para_lines = vec![trimmed.to_string()];
        i += 1;
        while i < lines.len() {
            if starts_block_boundary(&lines, i) {
                break;
            }
            para_lines.push(lines[i].trim().to_string());
            i += 1;
        }
        let para_md = para_lines.join("  \n");
        push_prose_and_inline_images(&mut blocks, &mut prev_kind, &para_md);
    }

    blocks
}

/// Rewrites the reasoning text of one bubble; the caller re-runs `layout`
/// (or `relayout_dirty`) so the bubble can grow around it.
pub fn set_thought(m: &mut HudMessage, thought: &str, label: &str) {
    m.thought = thought.into();
    m.think_label = label.into();
}

/// Extracts clean, individually copyable blocks (paragraphs, list items,
/// quotes, headings, code blocks) from a chat message for the partial-copy modal.
#[cfg(test)]
use crate::PartialCopyItem;
#[cfg(test)]
pub fn partial_copy_segments(body: &str, markdown: bool) -> Vec<PartialCopyItem> {
    partial_copy_segments_for(app_core::current_lang(), body, markdown)
}

#[cfg(test)]
pub fn partial_copy_segments_for(
    lang: app_core::AppLang,
    body: &str,
    markdown: bool,
) -> Vec<PartialCopyItem> {
    if body.trim().is_empty() {
        return Vec::new();
    }
    if !markdown {
        let text_lbl = app_core::tr_for(lang, "Teks", "Text");
        return body
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| PartialCopyItem {
                kind: text_lbl.into(),
                text: l.into(),
                copied: false,
            })
            .collect();
    }

    let mut out = Vec::new();
    for blk in parse_message_blocks(body) {
        let (kind_label, text) = match blk.kind.as_str() {
            "heading" => (app_core::tr_for(lang, "Judul", "Heading").to_string(), blk.raw.to_string()),
            "prose" => (app_core::tr_for(lang, "Paragraf", "Paragraph").to_string(), blk.raw.to_string()),
            "quote" => (app_core::tr_for(lang, "Kutipan", "Quote").to_string(), blk.raw.to_string()),
            "list" => {
                let prefix = match blk.marker.as_str() {
                    "done" => "✅ ",
                    "todo" => "☐ ",
                    "cross" => "❌ ",
                    m if !m.is_empty() => "",
                    _ => "• ",
                };
                let point_lbl = app_core::tr_for(lang, "Poin", "Item").to_string();
                if prefix.is_empty() && !blk.marker.is_empty() {
                    (point_lbl, format!("{} {}", blk.marker, blk.raw))
                } else {
                    (point_lbl, format!("{prefix}{}", blk.raw))
                }
            }
            "code" => {
                let lbl = if blk.lang.is_empty() || blk.lang == "code" {
                    app_core::tr_for(lang, "Kode", "Code").to_string()
                } else {
                    app_core::tr_for(
                        lang,
                        &format!("Kode ({})", blk.lang),
                        &format!("Code ({})", blk.lang),
                    )
                    .to_string()
                };
                out.push(PartialCopyItem {
                    kind: lbl.into(),
                    text: blk.raw,
                    copied: false,
                });
                continue;
            }
            "image" => {
                if blk.lang == "svg" {
                    ("SVG".to_string(), blk.raw.to_string())
                } else if !blk.img_url.is_empty() {
                    (app_core::tr_for(lang, "Gambar", "Image").to_string(), blk.img_url.to_string())
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        let clean = text
            .replace("**", "")
            .replace("~~", "")
            .replace("<font color=\"#22C55E\">", "")
            .replace("<font color=\"#EF4444\">", "")
            .replace("</font>", "")
            .trim()
            .to_string();
        if !clean.is_empty() {
            out.push(PartialCopyItem {
                kind: kind_label.into(),
                text: clean.into(),
                copied: false,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
//  Bubble geometry
// ---------------------------------------------------------------------------

const ACTIONS_H: i32 = 24;
const META_H: i32 = 26;
const MIN_BODY_H: i32 = 22;
const THOUGHT_MAX_H: i32 = 168;
const THOUGHT_LINE_H: i32 = 18;
const THOUGHT_PAD_Y: i32 = 6;
const BODY_LINE_H: i32 = 22;

fn wrapped_lines(text: &str, width: i32, font: f32, advance: f32) -> i32 {
    let per_line = ((width as f32) / (font * advance)).floor().max(8.0) as usize;
    let mut lines = 0;
    for raw in text.lines() {
        let n = raw
            .chars()
            .filter(|c| !matches!(c, '*' | '_' | '`' | '#' | '>'))
            .count();
        lines += if n == 0 { 1 } else { n.div_ceil(per_line) };
    }
    lines.max(1) as i32
}

fn measure_markdown(text: &str, avail: i32, font: f32, advance: f32) -> (i32, i32) {
    let char_w = font * advance;
    let per_line = ((avail as f32) / char_w).floor().max(8.0) as usize;
    let plain = |l: &str| {
        l.chars()
            .filter(|c| !matches!(c, '*' | '_' | '`' | '#' | '>'))
            .count()
    };
    let is_item = |l: &str| {
        let t = l.trim_start();
        t.starts_with("- ")
            || t.starts_with("* ")
            || t.starts_with("• ")
            || (t.len() > 2 && t.as_bytes()[0].is_ascii_digit() && t.contains(". "))
    };

    let mut lines = 0.0f32;
    let mut longest = 0usize;
    let mut para = String::new();
    let mut prev_blank = false;

    let flush = |para: &mut String, lines: &mut f32, longest: &mut usize| {
        if para.is_empty() {
            return;
        }
        let words: Vec<&str> = para.split_whitespace().collect();
        if words.is_empty() {
            para.clear();
            return;
        }
        let mut cur_line_len = 0usize;
        let mut wrapped = 1usize;
        for w in words {
            let w_len = plain(w);
            if cur_line_len == 0 {
                cur_line_len = w_len;
            } else if cur_line_len + 1 + w_len <= per_line {
                cur_line_len += 1 + w_len;
            } else {
                wrapped += 1;
                cur_line_len = w_len;
            }
            *longest = (*longest).max(cur_line_len.min(per_line));
        }
        *lines += wrapped as f32;
        para.clear();
    };

    for raw in text.lines() {
        let t = raw.trim();
        if t.is_empty() || t == "\u{a0}" {
            if !prev_blank {
                flush(&mut para, &mut lines, &mut longest);
                lines += 0.8;
                prev_blank = true;
            }
            continue;
        }
        prev_blank = false;
        if is_item(t) {
            flush(&mut para, &mut lines, &mut longest);
            let words: Vec<&str> = t.split_whitespace().collect();
            let mut cur_line_len = 0usize;
            let mut wrapped = 1usize;
            for w in words {
                let w_len = plain(w);
                if cur_line_len == 0 {
                    cur_line_len = w_len;
                } else if cur_line_len + 1 + w_len <= per_line {
                    cur_line_len += 1 + w_len;
                } else {
                    wrapped += 1;
                    cur_line_len = w_len;
                }
                longest = longest.max(cur_line_len.min(per_line));
            }
            lines += wrapped as f32;
            continue;
        }
        if !para.is_empty() {
            para.push(' ');
        }
        para.push_str(t);
    }
    flush(&mut para, &mut lines, &mut longest);

    let width = ((longest.max(1) as f32) * char_w + 10.0)
        .min(avail as f32)
        .max(64.0)
        .round() as i32;
    (width, lines.ceil().max(1.0) as i32)
}

pub fn layout(m: &mut HudMessage) {
    if m.role == "user" {
        m.thought_h = 0;
        let attach_count = m
            .attachments
            .row_count()
            .max(if m.attach_name.is_empty() { 0 } else { 1 });
        let rows = if attach_count == 0 {
            0
        } else {
            attach_count.div_ceil(4) as i32
        };
        let attach = rows * 68;
        let cols_in_row = attach_count.min(4) as i32;
        let processed = preprocess_markdown(&m.body);
        let (text_w, lines) = measure_markdown(&processed, 290, 14.0, 0.46);
        m.body_y = 10 + attach;
        m.body_h = (lines * BODY_LINE_H).max(MIN_BODY_H) + 6;
        let min_attach_w = if cols_in_row > 0 {
            (cols_in_row * 66 + 18).max(120)
        } else {
            50
        };
        m.bubble_w = (text_w + 24).max(min_attach_w).min(310);
        m.height = m.body_y + m.body_h + 12;
        return;
    }

    // Assistant bubble
    let mut est_body_h = 0i32;
    let block_count = m.blocks.row_count();
    if m.select_mode {
        let (_text_w, body_lines) = measure_markdown(&m.select_text, 310, 14.0, 0.48);
        est_body_h = (body_lines * BODY_LINE_H).max(MIN_BODY_H) + 52;
    } else if block_count > 0 {
        for i in 0..block_count {
            if let Some(blk) = m.blocks.row_data(i) {
                est_body_h += blk.gap_top;
                est_body_h += match blk.kind.as_str() {
                    "divider" => 8,
                    "code" => {
                        let lines = blk.raw.lines().count().max(1) as i32;
                        29 + lines * 18 + 18
                    }
                    "table" => 34 + (blk.rows.row_count() as i32) * 30,
                    "quote" => {
                        let pad = if blk.has_emoji_img { 24 } else { 0 };
                        let lines = wrapped_lines(&blk.raw, (290 - pad).max(160), 13.5, 0.48);
                        lines * 20 + 18
                    }
                    "heading" => {
                        let pad = if blk.has_emoji_img { 26 } else { 0 };
                        let avail = (310 - pad).max(120);
                        wrapped_lines(&blk.raw, avail, 15.5, 0.50) * 23 + 4
                    }
                    "list" => {
                        let marker_pad =
                            if matches!(blk.marker.as_str(), "done" | "todo" | "cross") {
                                6
                            } else {
                                0
                            };
                        let pad = if blk.has_emoji_img { 22 } else { 0 };
                        let avail = (306 - blk.indent * 14 - pad - marker_pad).max(120);
                        wrapped_lines(&blk.raw, avail, 14.0, 0.48) * BODY_LINE_H
                    }
                    "emoji-row" => 40,
                    "image" => {
                        if blk.img_loading {
                            58
                        } else if blk.img_failed {
                            52
                        } else {
                            let (_dw, dh) = fit_image_box(blk.img_w, blk.img_h, 326, 220);
                            dh + if blk.img_caption.is_empty() { 18 } else { 42 }
                        }
                    }
                    _ => wrapped_lines(&blk.raw, 324, 14.0, 0.48) * BODY_LINE_H,
                };
                if blk.kind != "emoji-row" && blk.emojis.row_count() > 0 {
                    est_body_h += 24;
                }
            }
        }
    } else {
        let processed = preprocess_markdown(&m.body);
        let (_text_w, body_lines) = measure_markdown(&processed, 328, 14.5, 0.48);
        est_body_h = (body_lines * BODY_LINE_H).max(MIN_BODY_H) + 14;
    }

    let has_thought = !m.thought.is_empty();
    m.bubble_w = 356;
    let mut y = 52;
    m.thought_h = 0;
    if has_thought {
        if m.thought_open {
            let max_lines = (THOUGHT_MAX_H - THOUGHT_PAD_Y * 2) / THOUGHT_LINE_H;
            let panel_w = 316;
            let lines = wrapped_lines(&m.thought, panel_w, 10.0, 0.62).min(max_lines);
            m.thought_h = lines * THOUGHT_LINE_H + THOUGHT_PAD_Y * 2;
            y = 52 + 29 + m.thought_h + 10;
        } else {
            y = 52 + 28 + 10;
        }
    }

    m.body_y = y;
    m.body_h = est_body_h.max(MIN_BODY_H);
    let detail_h = if m.detail_open { 8 + META_H } else { 0 };
    let actions_h = if m.body.is_empty() { 0 } else { 14 + ACTIONS_H };
    m.height = m.body_y + m.body_h + actions_h + detail_h + 16;
}

pub fn relayout_dirty(model: &slint::VecModel<HudMessage>, dirty: Option<usize>) -> i32 {
    let mut y = 12;
    for i in 0..model.row_count() {
        let Some(mut m) = model.row_data(i) else {
            continue;
        };
        let before = (m.offset_y, m.height, m.body_y, m.body_h, m.thought_h);
        if dirty.is_none_or(|d| d == i) {
            layout(&mut m);
        }
        m.offset_y = y;
        y += m.height + 14;
        if before != (m.offset_y, m.height, m.body_y, m.body_h, m.thought_h) {
            model.set_row_data(i, m);
        }
    }
    y
}

pub fn browser_logo_index(logo_key: &str) -> usize {
    match logo_key.trim().to_ascii_lowercase().as_str() {
        "edge" => 1,
        "brave" => 2,
        "firefox" => 3,
        _ => 0,
    }
}

pub fn browsers_from_with_active(
    list: &[app_core::BrowserProfileInfo],
    active_idx: usize,
) -> Vec<BrowserItem> {
    let clamped = active_idx.min(list.len().saturating_sub(1));
    list.iter()
        .enumerate()
        .map(|(i, b)| BrowserItem {
            id: format!("{}-{}", b.browser_type, b.profile_id).into(),
            name: b.browser_name.clone().into(),
            profile: b.profile_name.clone().into(),
            selected: i == clamped,
            logo_key: b.browser_type.clone().into(),
        })
        .collect()
}

pub fn vault_keys_from(accounts: &[app_core::AccountEntry]) -> Vec<VaultKeyItem> {
    vault_keys_from_for(app_core::current_lang(), accounts)
}

pub fn vault_keys_from_for(
    lang: app_core::AppLang,
    accounts: &[app_core::AccountEntry],
) -> Vec<VaultKeyItem> {
    accounts
        .iter()
        .enumerate()
        .map(|(idx, acc)| {
            let is_primary = idx == 0;
            let (status_text, tone) = if acc.busy {
                (app_core::tr_for(lang, "Sibuk", "Busy").to_string(), "cyan".to_string())
            } else if acc.cooling_down {
                let err_l = acc.last_error.to_ascii_lowercase();
                if err_l.contains("40003") || err_l.contains("invalid token") {
                    (
                        app_core::tr_for(
                            lang,
                            &format!("Token mati ({}d)", acc.cooldown_secs),
                            &format!("Token expired ({}s)", acc.cooldown_secs),
                        )
                        .to_string(),
                        "red".to_string(),
                    )
                } else {
                    (
                        app_core::tr_for(
                            lang,
                            &format!("Limit ({}d)", acc.cooldown_secs),
                            &format!("Rate limit ({}s)", acc.cooldown_secs),
                        )
                        .to_string(),
                        "amber".to_string(),
                    )
                }
            } else if is_primary {
                (
                    app_core::tr_for(lang, "Utama · Siap", "Primary · Ready").to_string(),
                    "green".to_string(),
                )
            } else {
                (
                    app_core::tr_for(lang, "Cadangan · Siap", "Backup · Ready").to_string(),
                    "cyan".to_string(),
                )
            };
            VaultKeyItem {
                id: acc.name.clone().into(),
                badge: format!("Key {}", idx + 1).into(),
                email: acc.email.clone().into(),
                source: acc.source.to_ascii_lowercase().into(),
                profile: acc.profile.clone().into(),
                status_text: status_text.into(),
                tone: tone.into(),
                is_primary,
                diag_text: "".into(),
                diag_tone: "".into(),
            }
        })
        .collect()
}

pub fn format_key_test_badge(res: &app_core::AccountTestResult) -> (String, String) {
    format_key_test_badge_for(app_core::current_lang(), res)
}

pub fn format_key_test_badge_for(
    lang: app_core::AppLang,
    res: &app_core::AccountTestResult,
) -> (String, String) {
    if res.ok {
        let ms = res.latency_ms.unwrap_or(0);
        return (format!("OK · {ms} ms"), "green".to_string());
    }
    let lower = res
        .error
        .as_deref()
        .unwrap_or("gagal")
        .to_ascii_lowercase();
    if ["40003", "40002", "401", "invalid token", "expired"]
        .iter()
        .any(|p| lower.contains(p))
    {
        (
            app_core::tr_for(lang, "Token mati", "Expired token").to_string(),
            "red".to_string(),
        )
    } else if ["429", "rate limit", "quota", "exceeded", "too many"]
        .iter()
        .any(|p| lower.contains(p))
    {
        (
            app_core::tr_for(lang, "Limit kuota", "Rate limited").to_string(),
            "amber".to_string(),
        )
    } else if lower.contains("timeout") || lower.contains("timed out") {
        ("Timeout".to_string(), "amber".to_string())
    } else {
        (
            app_core::tr_for(lang, "Gagal uji", "Test failed").to_string(),
            "red".to_string(),
        )
    }
}

pub fn apply_key_diag_results(
    items: &mut [VaultKeyItem],
    map: &std::collections::HashMap<String, (String, String)>,
) {
    for item in items.iter_mut() {
        if let Some((text, tone)) = map.get(item.id.as_str()) {
            item.diag_text = text.clone().into();
            item.diag_tone = tone.clone().into();
        }
    }
}

pub fn topics_from(
    list: &[app_core::TopicItem],
    active: Option<&str>,
    count: &dyn Fn(&str) -> usize,
) -> Vec<TopicItem> {
    topics_from_for(app_core::current_lang(), list, active, count)
}

pub fn topics_from_for(
    lang: app_core::AppLang,
    list: &[app_core::TopicItem],
    active: Option<&str>,
    count: &dyn Fn(&str) -> usize,
) -> Vec<TopicItem> {
    if list.is_empty() {
        return Vec::new();
    }
    let active = active.unwrap_or_else(|| list[0].name.as_str());
    list.iter()
        .map(|t| {
            let n = count(&t.name);
            let count_text = match lang {
                app_core::AppLang::Id => format!("{n} pesan"),
                app_core::AppLang::En => {
                    if n == 1 {
                        "1 message".to_string()
                    } else {
                        format!("{n} messages")
                    }
                }
            };
            TopicItem {
                name: t.name.clone().into(),
                active: t.name == active,
                count: count_text.into(),
                category: t.category.to_uppercase().into(),
                when: pretty_when(&t.created_at).into(),
                initial: initial_of(&t.name).into(),
            }
        })
        .collect()
}

pub fn pending_topic(name: &str, category: &str) -> TopicItem {
    pending_topic_for(app_core::current_lang(), name, category)
}

pub fn pending_topic_for(lang: app_core::AppLang, name: &str, category: &str) -> TopicItem {
    let cat = category.trim();
    TopicItem {
        name: name.into(),
        active: true,
        count: app_core::tr_for(lang, "0 pesan", "0 messages").into(),
        category: if cat.is_empty() {
            "GENERAL".into()
        } else {
            cat.to_uppercase().into()
        },
        when: app_core::tr_for(lang, "Sekarang", "Just now").into(),
        initial: initial_of(name).into(),
    }
}

pub fn topic_categories(list: &[TopicItem], filter: &str) -> Vec<TopicCategory> {
    topic_categories_for(app_core::current_lang(), list, filter)
}

pub fn topic_categories_for(
    lang: app_core::AppLang,
    list: &[TopicItem],
    filter: &str,
) -> Vec<TopicCategory> {
    let all_label = app_core::tr_for(lang, "Semua", "All");
    let mut out = vec![TopicCategory {
        name: all_label.into(),
        count: list.len().to_string().into(),
        active: filter.is_empty() || filter == "Semua" || filter == "All",
    }];
    let mut seen: Vec<String> = Vec::new();
    for t in list {
        let cat = t.category.to_string();
        if seen.contains(&cat) {
            continue;
        }
        seen.push(cat.clone());
        out.push(TopicCategory {
            name: cat.clone().into(),
            count: list
                .iter()
                .filter(|x| x.category == cat)
                .count()
                .to_string()
                .into(),
            active: filter == cat,
        });
    }
    out
}

pub fn provider_deepseek(connected: bool) -> ProviderItem {
    ProviderItem {
        id: "deepseek".into(),
        name: "DeepSeek".into(),
        status: if connected { "Connected" } else { "Offline" }.into(),
        tone: if connected { "green" } else { "amber" }.into(),
        tone_color: if connected {
            slint::Color::from_rgb_u8(0x2C, 0xE5, 0x9B)
        } else {
            slint::Color::from_rgb_u8(0xFF, 0xD1, 0x66)
        },
        order: "#1".into(),
        tag: String::new().into(),
        enabled: connected,
        logo_key: "deepseek".into(),
        logo_index: 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_formatting_features() {
        let strike = "This is ~~strikethrough~~ text";
        println!("STRIKE: {:?}", slint::StyledText::from_markdown(strike));

        let u_tag = "This is <u>underline</u> text";
        println!("U_TAG: {:?}", slint::StyledText::from_markdown(u_tag));

        let em_strong = "This is ***bold italic*** and **bold** and *italic*";
        println!("EM_STRONG: {:?}", slint::StyledText::from_markdown(em_strong));

        let code = "This is `inline code` text";
        println!("CODE: {:?}", slint::StyledText::from_markdown(code));

        let block = "```python\ndef hello():\n    return 42\n```";
        println!("CODE BLOCK: {:?}", slint::StyledText::from_markdown(block));

        let quote = "> This is a blockquote\n> With multiple lines";
        println!("QUOTE: {:?}", slint::StyledText::from_markdown(quote));

        let headings = "# H1\n## H2\n### H3";
        println!("HEADINGS: {:?}", slint::StyledText::from_markdown(headings));

        let table = "| Header 1 | Header 2 |\n| --- | --- |\n| Cell 1 | Cell 2 |";
        println!("TABLE: {:?}", slint::StyledText::from_markdown(table));

        let emojis = "Hello 👋, welcome ✨! Let's code 💻 and solve 💡 issues 🚀.";
        println!("EMOJIS: {:?}", slint::StyledText::from_markdown(emojis));
    }

    #[test]
    fn test_paragraph_spacing() {
        let text1 = "Para 1\n\nPara 2";
        let res1 = slint::StyledText::from_markdown(text1).unwrap();
        println!("RES1: {:?}", res1);

        let text2 = "Para 1\n\n\u{00A0}\n\nPara 2";
        let res2 = slint::StyledText::from_markdown(text2).unwrap();
        println!("RES2: {:?}", res2);
    }

    #[test]
    fn test_markdown_features() {
        let full_sample = r#"### Fitur dan Cara Penggunaan

Berikut adalah cara mengatasi masalah sudut kustom:
1. Masuk ke **Vector Edit Mode**.
2. Gunakan shortcut `Ctrl + A` untuk memilih semua node.

> Perhatikan bahwa beberapa versi Figma membutuhkan plugin tambahan.

---

| Alat | Tipe Objek | Status |
| :--- | :--- | :--- |
| Pen Tool | Vektor Jalur | Fleksibel |
| Rectangle | Objek Primitif | Didukung Penuh |

```rust
fn main() {
    println!("Hello World");
}
```

Semoga tips ini membantu!"#;

        let processed = preprocess_markdown(full_sample);
        println!("PROCESSED:\n{}\n", processed);
        let res = slint::StyledText::from_markdown(&processed);
        assert!(res.is_ok(), "from_markdown failed: {:?}", res.err());
    }

    #[test]
    fn test_image_3_sample() {
        let sample = r#"- Tutup Antigravity sepenuhnya. Pastikan tidak ada proses yang berjalan di Task Manager.
- Klik kanan pada shortcut Antigravity di desktop atau Start Menu, lalu pilih Properties.
- Buka tab Shortcut.
- Di kolom Target, tambahkan flags berikut di bagian paling akhir (setelah tanda kutip penutup)
```bash
--disable-gpu --disable-gpu-sandbox --no-sandbox
--disable-features=GpuProcessHighPriorityPerWindow
```
- Klik Apply > OK, lalu coba buka Antigravity lagi melalui shortcut tersebut[reference:3].
> Catatan Keamanan: Penggunaan flag `--no-sandbox` memang sedikit menurunkan lapisan keamanan Chromium. Ini adalah workaround sementara yang efektif, namun sebaiknya gunakan ini hanya untuk sementara hingga ada update resmi dari Google yang memperbaiki masalah ini[reference:3].
2. Hapus Cache & Log yang Korup
Jika cara pertama belum berhasil, cache atau log yang rusak akibat update bisa jadi penyebabnya. Menghapusnya aman dan tidak akan menghapus proyek atau pengaturanmu.
- Pastikan Antigravity dalam keadaan tertutup"#;

        let processed = preprocess_markdown(sample);
        println!("PROCESSED IMAGE 3:\n{}\n", processed);

        // Verification 1: All [reference:3] must be replaced with [4]
        assert!(!processed.contains("[reference:"), "No [reference:...] should leak!");
        assert!(processed.contains("[4]"), "Should convert reference:3 to [4]");

        // Verification 2: Blockquote should not have pipes around --no-sandbox
        assert!(!processed.contains("|--no-sandbox|"), "No ugly pipes around inline code");
        assert!(processed.contains("`--no-sandbox`"), "Inline code preserved");

        // Verification 3: Code block formatted compactly without ASCII box frames
        assert!(processed.contains("**[BASH]**"), "Header for code block");
        assert!(processed.contains("--disable-gpu"), "Code block command preserved");
        assert!(!processed.contains("───────"), "No ugly dashed separator frame");

        let res = slint::StyledText::from_markdown(&processed);
        assert!(res.is_ok(), "from_markdown failed: {:?}", res.err());

        // Structured block verification
        let blocks = parse_message_blocks(sample);
        assert!(blocks.iter().any(|b| b.kind == "code" && b.lang == "bash" && b.raw.contains("--disable-gpu")));
        assert!(blocks.iter().any(|b| b.kind == "quote" && b.raw.contains("[4]")));
        assert!(blocks.iter().any(|b| b.kind == "list" && b.marker == "2."));
    }

    #[test]
    fn test_rich_response_formatting_and_spacing() {
        let sample = r#"Halo! Berikut adalah panduan lengkap optimasi:

### 🚀 Rekomendasi Utama
Kami merekomendasikan beberapa langkah penting:
1. Pastikan **hardware acceleration** aktif di pengaturan sistem.
2. Gunakan shortcut `Ctrl + Shift + Esc` untuk memantau performa.
3. Fitur <u>Auto-Sync</u> dan ~~legacy mode~~ sebaiknya disesuaikan.

> Catatan penting: Pengaturan ini aman dan *tidak akan* merusak file konfigurasimu[reference:0].

---

| Opsi | Rekomendasi | Dampak |
| :--- | :---: | ---: |
| VSync | On | Menghilangkan tearing |
| Anti-Aliasing | FXAA | Hemat VRAM |

```bash
# Periksa status driver
dxdiag /t %TEMP%\dxdiag.txt
```

Semoga informasi ini bermanfaat! Hubungi kami jika ada pertanyaan lebih lanjut."#;

        let processed = preprocess_markdown(sample);
        println!("=== PROCESSED RICH RESPONSE ===\n{}\n================================", processed);

        // 1. Check citations & no long ASCII dashes in BubbleWindow fallback
        assert!(processed.contains("[1]"), "reference:0 -> [1]");
        assert!(!processed.contains("[reference:"), "No raw reference tag leaks");
        assert!(!processed.contains("────"), "No wide ASCII horizontal dashes");

        // 2. Check structured blocks for HudWindow
        let blocks = parse_message_blocks(sample);
        assert_eq!(blocks[0].kind, "prose");
        assert_eq!(blocks[0].gap_top, 0);

        let heading = blocks.iter().find(|b| b.kind == "heading").expect("heading block");
        assert_eq!(heading.level, 3);
        assert_eq!(heading.gap_top, 11);

        let list_items: Vec<_> = blocks.iter().filter(|b| b.kind == "list").collect();
        assert_eq!(list_items.len(), 3);
        assert_eq!(list_items[0].marker, "1.");
        assert_eq!(list_items[0].gap_top, 6);
        assert_eq!(list_items[1].gap_top, 4);

        let quote = blocks.iter().find(|b| b.kind == "quote").expect("quote block");
        assert!(quote.raw.contains("[1]"));
        assert!(!quote.raw.contains("▎"));

        assert!(blocks.iter().any(|b| b.kind == "divider"), "Real UI divider block");

        let table = blocks.iter().find(|b| b.kind == "table").expect("table block");
        assert_eq!(table.headers.row_count(), 3);
        assert_eq!(table.rows.row_count(), 2);
        assert_eq!(table.headers.row_data(0).unwrap().align, "left");
        assert_eq!(table.headers.row_data(1).unwrap().align, "center");
        assert_eq!(table.headers.row_data(2).unwrap().align, "right");

        let code = blocks.iter().find(|b| b.kind == "code").expect("code block");
        assert_eq!(code.lang, "bash");
        assert!(code.raw.contains("# Periksa status driver"));
        assert!(code.raw.contains("dxdiag /t %TEMP%\\dxdiag.txt"));

        // 3. Check StyledText parses without errors
        let res = slint::StyledText::from_markdown(&processed);
        assert!(res.is_ok(), "StyledText::from_markdown failed: {:?}", res.err());

        // 4. Check layout measurement & blocks attached to message
        let mut m = message("assistant", "DEEPSEEK", sample, "", true);
        assert!(m.blocks.row_count() > 5, "Structured blocks built on HudMessage");
        layout(&mut m);
        assert!(m.height > 200, "Bubble height properly computed");
    }

    #[test]
    fn test_structured_blocks_streaming_wide_table_and_glyphs() {
        // 1. Unclosed streaming code fence & literal JS backticks preserved verbatim
        let streaming = "Penjelasan singkat\n```javascript\nconst s = `Halo ${name}`;\nconsole.log(s);";
        let blocks = parse_message_blocks(streaming);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1].kind, "code");
        assert_eq!(blocks[1].lang, "javascript");
        assert!(blocks[1].raw.contains("const s = `Halo ${name}`;"));
        assert!(blocks[1].raw.contains("console.log(s);"));

        // 2. Large (>5 rows, >26 char) wide table + escaped pipe + backtick pipe stays a REAL table
        let wide_tbl = concat!(
            "| Model | Context | Speed | Notes | Status |\n",
            "| :--- | :---: | ---: | :--- | :---: |\n",
            "| deepseek-chat | 128K | 62 tok/s | Mendukung `a | b` & JSON | Active |\n",
            "| deepseek-reasoner | 128K | 38 tok/s | Multi-step chain-of-thought | Active |\n",
            "| claude-sonnet | 200K | 55 tok/s | Coding & analisis panjang | Standby |\n",
            "| gpt-4o | 128K | 70 tok/s | Multimodal visual cepat | Standby |\n",
            "| gemini-pro | 1M | 85 tok/s | Konteks ekstra panjang | Standby |\n",
            "| qwen-max | 64K | 60 tok/s | Optimasi multibahasa Asia | Standby |\n",
        );
        let t_blocks = parse_message_blocks(wide_tbl);
        assert_eq!(t_blocks.len(), 1);
        assert_eq!(t_blocks[0].kind, "table");
        assert_eq!(t_blocks[0].headers.row_count(), 5);
        assert_eq!(t_blocks[0].rows.row_count(), 6);
        assert!(t_blocks[0].table_w > 330, "Wide 5-col table scrolls instead of squishing");

        // 3. Task lists (done, todo, cross), nested lists, Tips blockquote promotion, unknown glyphs (✦ -> ◆), and Vec<String>
        let list_and_misc = "- [x] Selesai buat tabel\n  - Sub poin satu\n- [ ] Cek responsif\n- [-] Item silang merah\n\n✦ **Tips Aemeath**: Gunakan tipe `Vec<String>` atau parameter <T> aman.\n\n✦ Catatan dengan ikon pembuka.";
        let l_blocks = parse_message_blocks(list_and_misc);
        assert_eq!(l_blocks[0].marker, "done");
        assert!(l_blocks[0].emoji_img.size().width >= 32);
        assert_eq!(l_blocks[1].marker, "○");
        assert_eq!(l_blocks[1].indent, 1);
        assert_eq!(l_blocks[1].gap_top, 2);
        assert_eq!(l_blocks[2].marker, "todo");
        assert!(l_blocks[2].emoji_img.size().width >= 32);
        assert_eq!(l_blocks[3].marker, "cross");
        assert!(l_blocks[3].emoji_img.size().width >= 32);
        assert_eq!(l_blocks[4].kind, "quote");
        assert!(l_blocks[4].has_emoji_img, "Tips Aemeath should promote to quote with color lightbulb");
        assert!(l_blocks[5].raw.starts_with("◆ "));
    }

    #[test]
    fn test_emojis_preserved() {
        let sample = "Halo juga! 👋 Aku **Aemeath** ✨ siap membantu di layar komputer! 🎮🛠️";
        let processed = preprocess_markdown(sample);
        println!("EMOJI TEST:\n{}\n", processed);
        assert!(processed.contains("👋"), "Preserve waving hand emoji");
        assert!(processed.contains("✨"), "Preserve sparkles emoji");
        assert!(processed.contains("🎮"), "Preserve gamepad emoji");
        assert!(processed.contains("🛠️"), "Preserve tools emoji");
        let res = slint::StyledText::from_markdown(&processed);
        assert!(res.is_ok(), "from_markdown failed: {:?}", res.err());
    }

    fn make_png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([60, 232, 255, 255]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("encode png");
        buf
    }

    #[test]
    fn test_svg_fenced_raw_and_streaming_fallback() {
        // 1. Streaming unclosed SVG stays code block until closed
        let unclosed = "Penjelasan:\n```svg\n<svg viewBox=\"0 0 120 60\"><rect width=\"120\" height=\"60\"/></svg>";
        let b1 = parse_message_blocks(unclosed);
        assert_eq!(b1[1].kind, "code");
        assert_eq!(b1[1].lang, "svg");

        // 2. Closed SVG without xmlns gets xmlns auto-injected and decodes into image block
        let closed = "```svg\n<svg viewBox=\"0 0 180 90\"><circle cx=\"90\" cy=\"45\" r=\"30\" fill=\"#3CE8FF\"/></svg>\n```";
        let b2 = parse_message_blocks(closed);
        assert_eq!(b2.len(), 1);
        assert_eq!(b2[0].kind, "image");
        assert_eq!(b2[0].lang, "svg");
        assert!(b2[0].raw.contains("xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(!b2[0].img_failed);
        assert_eq!((b2[0].img_w, b2[0].img_h), (180, 90));

        // 3. Broken SVG falls back safely to code block instead of crashing
        let broken = "```svg\n<svg><not_closed\n```";
        let b3 = parse_message_blocks(broken);
        assert_eq!(b3[0].kind, "code");

        // 4. Standalone raw <svg>...</svg> without fences
        let raw_inline = "Lihat:\n<svg viewBox=\"0 0 100 50\"><rect width=\"100\" height=\"50\" fill=\"red\"/></svg>\nSelesai.";
        let b4 = parse_message_blocks(raw_inline);
        assert_eq!(b4.len(), 3);
        assert_eq!(b4[1].kind, "image");
        assert_eq!(b4[1].lang, "svg");
    }

    #[test]
    fn test_markdown_images_data_local_and_inline_split() {
        let png = make_png_bytes(40, 20);
        let b64 = {
            const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::new();
            for c in png.chunks(3) {
                let n = ((c[0] as u32) << 16)
                    | ((c.get(1).copied().unwrap_or(0) as u32) << 8)
                    | (c.get(2).copied().unwrap_or(0) as u32);
                out.push(TABLE[((n >> 18) & 63) as usize] as char);
                out.push(TABLE[((n >> 12) & 63) as usize] as char);
                out.push(if c.len() > 1 { TABLE[((n >> 6) & 63) as usize] as char } else { '=' });
                out.push(if c.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
            }
            out
        };

        // Inline image split inside prose + inline code preserved
        let md = format!(
            "Contoh `![kode_bukan_gambar](x.png)` tetap kode. Cek ![Lencana Base64](data:image/png;base64,{b64}) ya!"
        );
        let blks = parse_message_blocks(&md);
        assert_eq!(blks.len(), 3);
        assert_eq!(blks[0].kind, "prose");
        assert!(blks[0].raw.contains("![kode_bukan_gambar](x.png)"));
        assert_eq!(blks[1].kind, "image");
        assert_eq!(blks[1].img_caption, "Lencana Base64");
        assert!(!blks[1].img_failed);
        assert_eq!((blks[1].img_w, blks[1].img_h), (40, 20));
        assert_eq!(blks[2].kind, "prose");

        // Badge-linked image [![alt](data:...)](href) + BubbleWindow summary strips base64
        let badge_md = format!("[![Badge](data:image/png;base64,{b64})](https://example.com/badge)");
        let badge_blks = parse_message_blocks(&badge_md);
        assert_eq!(badge_blks[0].kind, "image");
        assert_eq!(badge_blks[0].img_url, "https://example.com/badge");
        let bubble_txt = preprocess_markdown(&badge_md);
        assert!(!bubble_txt.contains("iVBOR"));
        assert!(bubble_txt.contains("[Gambar: Badge]"));

        // Local file path + invalid missing path error state
        let tmp = std::env::temp_dir().join("mini_assist_unit_test_img.png");
        std::fs::write(&tmp, &png).expect("write tmp png");
        let file_md = format!("![Lokal PNG]({})", tmp.display());
        let file_blks = parse_message_blocks(&file_md);
        assert_eq!(file_blks[0].kind, "image");
        assert!(!file_blks[0].img_failed);
        assert_eq!((file_blks[0].img_w, file_blks[0].img_h), (40, 20));
        let _ = std::fs::remove_file(&tmp);

        let missing = parse_message_blocks("![Hilang](C:\\tidak_ada_file_12345.png)");
        assert_eq!(missing[0].kind, "image");
        assert!(missing[0].img_failed);
    }

    #[test]
    fn test_color_emoji_rows_leading_badges_and_shortcodes() {
        let md = concat!(
            ":tada: :sparkles: :rocket:\n\n",
            "### :rocket: Judul Berwarna\n",
            "- :bulb: Poin pertama dengan `:rocket:` literal di inline code"
        );
        let blks = parse_message_blocks(md);
        assert_eq!(blks[0].kind, "emoji-row");
        assert_eq!(blks[0].emojis.row_count(), 3);
        assert_eq!(blks[1].kind, "heading");
        assert!(blks[1].has_emoji_img);
        assert_eq!(blks[1].raw, "Judul Berwarna");
        assert_eq!(blks[2].kind, "list");
        assert!(blks[2].has_emoji_img);
        assert!(blks[2].raw.contains("`:rocket:`"), "Shortcode in backticks stayed literal");
    }

    #[test]
    fn test_remote_http_image_fetch_and_pump() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind http");
        let port = listener.local_addr().unwrap().port();
        let png = make_png_bytes(52, 26);

        let srv = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf);
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    png.len()
                );
                let _ = stream.write_all(hdr.as_bytes());
                let _ = stream.write_all(&png);
                let _ = stream.flush();
            }
        });

        let url = format!("http://127.0.0.1:{port}/img.png");
        let msg = message("assistant", "AEMEATH", &format!("![Remote]({url})"), "", true);
        assert!(msg.blocks.row_data(0).unwrap().img_loading);

        let vec_model = slint::VecModel::from(vec![msg]);
        for _ in 0..40 {
            if pump_remote_images(&vec_model) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let updated = vec_model.row_data(0).unwrap();
        let blk = updated.blocks.row_data(0).unwrap();
        assert!(!blk.img_loading);
        assert!(!blk.img_failed);
        assert_eq!((blk.img_w, blk.img_h), (52, 26));
        let _ = srv.join();
    }

    #[test]
    fn test_win32_clipboard_roundtrip() {
        let sample = "Baris 1: halo Startorch ✨\nBaris 2: `a | b` -> OK";
        crate::win32::copy_text(sample);
        let got = crate::win32::read_clipboard_text().expect("read clipboard");
        assert_eq!(got, sample);
    }

    #[test]
    fn test_streaming_fallback_styled_skip_and_svg_cache() {
        let mut assistant = message("assistant", "AEMEATH", "## Judul\n\nParagraf *miring*", "", true);
        assert!(assistant.blocks.row_count() > 0);
        assert_eq!(
            assistant.styled,
            slint::StyledText::default(),
            "unused fallback styled must be skipped when rich blocks render"
        );

        let user = message("user", "ANDA", "Halo biasa", "", false);
        assert_eq!(user.blocks.row_count(), 0);
        assert_ne!(user.styled, slint::StyledText::default());

        set_body(&mut assistant, "Baris plain fallback");
        assert!(assistant.blocks.row_count() > 0);
        assert_eq!(assistant.styled, slint::StyledText::default());

        let svg = r##"<svg viewBox="0 0 120 60"><rect width="120" height="60" fill="#3CE8FF"/></svg>"##;
        let first = decode_svg_image(svg).expect("first svg decode");
        let second = decode_svg_image(svg).expect("cached svg decode");
        assert_eq!((first.1, first.2), (second.1, second.2));
        assert_eq!(first.3, second.3);
    }

    #[test]
    fn test_search_source_item_and_partial_copy_segments() {
        let src = build_search_source_item(
            "Berita Nasional",
            "https://www.antaranews.com/berita/1",
            "Ringkasan paragraf berita.",
            "",
            "2026/10/17",
        );
        assert_eq!(src.site_name.as_str(), "ANTARA News");
        assert_eq!(src.badge_text.as_str(), "A");
        assert_eq!(src.date.as_str(), "2026/10/17");

        let body = "### Judul\n\nParagraf pembuka ✅ dan ❌.\n\n- [x] Poin done\n\n```rust\nlet x = 1;\n```";
        let segs = partial_copy_segments(body, true);
        assert!(segs.len() >= 4);
        assert_eq!(segs[0].kind.as_str(), "Judul");
        assert_eq!(segs[1].kind.as_str(), "Paragraf");
        assert_eq!(segs[2].kind.as_str(), "Poin");
        assert!(segs[2].text.as_str().starts_with("✅ "));
        assert_eq!(segs[3].kind.as_str(), "Kode (rust)");

        let cited = normalize_inline_md("Lihat [reference:0] dan [citation:7].");
        assert_eq!(cited, "Lihat [1] dan [7].");
    }

    #[test]
    fn test_inline_emoji_extraction_bold_heading_badge_and_banner_strip() {
        let leaked = concat!(
            "Searching for images...Relevant images foundRelevant images found",
            "Berdasarkan gambar yang kamu kirim, pria tersebut adalah **Huang Haibo (黃海波)** 🧑‍🌾.\n\n",
            "# **🎬 Kilas Balik & Kehidupan**\n\n",
            "```txt\nSearching for images...\n```"
        );
        let blks = parse_message_blocks(leaked);
        assert_eq!(blks[0].kind, "prose");
        assert!(!blks[0].raw.contains("Searching for images"));
        assert!(!blks[0].raw.contains("Relevant images found"));
        assert!(blks[0].raw.starts_with("Berdasarkan gambar"));
        assert!(!blks[0].raw.contains("🧑"));
        assert_eq!(blks[0].emojis.row_count(), 1);

        assert_eq!(blks[1].kind, "heading");
        assert!(blks[1].has_emoji_img);
        assert_eq!(blks[1].raw.as_str(), "Kilas Balik & Kehidupan");

        assert_eq!(blks[2].kind, "code");
        assert_eq!(blks[2].raw.as_str(), "Searching for images...");
    }

    #[test]
    fn test_format_key_test_badge_and_overlay() {
        let ok = app_core::AccountTestResult {
            ok: true,
            name: "chrome:Default".into(),
            latency_ms: Some(412),
            reply: Some("OK".into()),
            error: None,
        };
        assert_eq!(
            format_key_test_badge(&ok),
            ("OK · 412 ms".to_string(), "green".to_string())
        );

        let limit = app_core::AccountTestResult {
            ok: false,
            name: "edge:Default".into(),
            latency_ms: None,
            reply: None,
            error: Some("completion 429 rate limit exceeded".into()),
        };
        assert_eq!(
            format_key_test_badge(&limit),
            ("Limit kuota".to_string(), "amber".to_string())
        );

        let dead = app_core::AccountTestResult {
            ok: false,
            name: "edge:Default".into(),
            latency_ms: None,
            reply: None,
            error: Some("upstream 40003: token invalid".into()),
        };
        assert_eq!(
            format_key_test_badge(&dead),
            ("Token mati".to_string(), "red".to_string())
        );

        let mut items = vec![VaultKeyItem {
            id: "chrome:Default".into(),
            badge: "Key 1".into(),
            email: "a@gmail.com".into(),
            source: "chrome".into(),
            profile: "Chrome".into(),
            status_text: "Utama · Siap".into(),
            tone: "green".into(),
            is_primary: true,
            diag_text: "".into(),
            diag_tone: "".into(),
        }];
        let mut map = std::collections::HashMap::new();
        map.insert("chrome:Default".to_string(), format_key_test_badge(&ok));
        apply_key_diag_results(&mut items, &map);
        assert_eq!(items[0].diag_text.as_str(), "OK · 412 ms");
        assert_eq!(items[0].diag_tone.as_str(), "green");
    }

    #[test]
    fn test_i18n_model_formatting_id_and_en() {
        let accounts = vec![
            app_core::AccountEntry {
                name: "chrome:Default".into(),
                email: "a@gmail.com".into(),
                source: "chrome".into(),
                profile: "Default".into(),
                priority: 1,
                available: true,
                busy: false,
                cooling_down: false,
                cooldown_secs: 0,
                last_error: String::new(),
            },
            app_core::AccountEntry {
                name: "edge:Default".into(),
                email: "b@gmail.com".into(),
                source: "edge".into(),
                profile: "Default".into(),
                priority: 2,
                available: false,
                busy: false,
                cooling_down: true,
                cooldown_secs: 45,
                last_error: "429 rate limit".into(),
            },
        ];

        let id_keys = vault_keys_from_for(app_core::AppLang::Id, &accounts);
        assert_eq!(id_keys[0].status_text.as_str(), "Utama · Siap");
        assert_eq!(id_keys[1].status_text.as_str(), "Limit (45d)");

        let en_keys = vault_keys_from_for(app_core::AppLang::En, &accounts);
        assert_eq!(en_keys[0].status_text.as_str(), "Primary · Ready");
        assert_eq!(en_keys[1].status_text.as_str(), "Rate limit (45s)");

        let dead = app_core::AccountTestResult {
            ok: false,
            name: "edge:Default".into(),
            latency_ms: None,
            reply: None,
            error: Some("40003 invalid token".into()),
        };
        assert_eq!(
            format_key_test_badge_for(app_core::AppLang::En, &dead).0,
            "Expired token"
        );

        let topic_list = vec![app_core::TopicItem {
            id: 1,
            name: "Demo".into(),
            category: "dev".into(),
            created_at: "2026-11-20T09:15:00Z".into(),
        }];
        let en_topics = topics_from_for(app_core::AppLang::En, &topic_list, None, &|_| 1);
        assert_eq!(en_topics[0].count.as_str(), "1 message");
        let en_pending = pending_topic_for(app_core::AppLang::En, "New", "");
        assert_eq!(en_pending.count.as_str(), "0 messages");
        assert_eq!(en_pending.when.as_str(), "Just now");
        let en_cats = topic_categories_for(app_core::AppLang::En, &en_topics, "");
        assert_eq!(en_cats[0].name.as_str(), "All");
        assert!(en_cats[0].active);

        let body = "### Title\n\nParagraph text.\n\n```rust\nlet x = 1;\n```";
        let segs = partial_copy_segments_for(app_core::AppLang::En, body, true);
        assert_eq!(segs[0].kind.as_str(), "Heading");
        assert_eq!(segs[1].kind.as_str(), "Paragraph");
        assert_eq!(segs[2].kind.as_str(), "Code (rust)");
    }
}
