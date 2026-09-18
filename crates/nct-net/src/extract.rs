// HTML → markdown extraction, trafilatura/readability-style: score candidate
// blocks by text density, link density, and class/id noise signals, keep the
// best contiguous region, then render it as markdown. The confidence value is
// the agreement between two independent density estimators over the same DOM
// (block scores vs whole-subtree text concentration) — low agreement flags a
// likely mis-cut so callers can escalate.
use std::sync::OnceLock;

use scraper::{ElementRef, Html, Node, Selector};
use serde_json::{json, Value};

use nct_core::errors::ToolError;

fn sel(pattern: &'static str) -> &'static Selector {
    static CACHE: OnceLock<std::collections::HashMap<&'static str, Selector>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let mut m = std::collections::HashMap::new();
            for p in [
                "a",
                "a[href]",
                "title",
                "body",
                "article",
                "tr",
                "meta[property=\"og:title\"]",
                "meta[property=\"og:site_name\"]",
                "meta[name=\"author\"]",
                "meta[property=\"article:author\"]",
                "meta[name=\"description\"]",
                "meta[property=\"og:description\"]",
                "meta[property=\"article:published_time\"]",
            ] {
                if let Ok(s) = Selector::parse(p) {
                    m.insert(p, s);
                }
            }
            m
        })
        .get(pattern)
        .unwrap_or_else(|| panic!("static selector must parse: {pattern}"))
}

pub struct Extracted {
    pub markdown: String,
    pub title: Option<String>,
    pub links: Vec<String>,
    pub confidence: f64,
}

/// Unsupported/noise containers: dropped before scoring.
const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "iframe", "form", "nav", "aside", "footer",
    "header", "button", "select", "input", "textarea", "video", "audio", "canvas", "dialog",
];

/// Class/id substrings that mark boilerplate (trafilatura's discard-list spirit).
const NOISE_WORDS: &[&str] = &[
    "comment",
    "promo",
    "advert",
    "sponsor",
    "cookie",
    "banner",
    "related",
    "recommended",
    "sidebar",
    "share",
    "social",
    "subscribe",
    "newsletter",
    "popup",
    "modal",
    "footer",
    "menu",
    "breadcrumb",
    "pagination",
    "widget",
    "survey",
    "captcha",
    "advertisement",
    "skip-link",
    "visually-hidden",
    "sr-only",
];

const MAX_DEPTH: usize = 40;

/// Extract the main content of `html` as markdown. `base` absolutizes links.
pub fn extract(html: &str, base: &url::Url) -> Result<Extracted, ToolError> {
    let document = Html::parse_document(html);
    let og_title = document
        .select(sel("meta[property=\"og:title\"]"))
        .next()
        .and_then(|m| m.value().attr("content").map(String::from))
        .map(|t| normalize_ws(&t))
        .filter(|t| !t.is_empty());
    let site_name = document
        .select(sel("meta[property=\"og:site_name\"]"))
        .next()
        .and_then(|m| m.value().attr("content").map(String::from));
    let title = og_title.or_else(|| {
        document
            .select(sel("title"))
            .next()
            .map(|t| normalize_ws(&t.text().collect::<String>()))
            .filter(|t| !t.is_empty())
            .map(|t| strip_site_suffix(&t, site_name.as_deref()))
    });

    let links: Vec<String> = document
        .select(sel("a[href]"))
        .filter_map(|a| a.value().attr("href"))
        .filter(|h| !h.starts_with('#') && !h.starts_with("javascript:") && !h.starts_with("data:"))
        .filter_map(|h| base.join(h).ok())
        .map(|u| u.to_string())
        .collect();

    // Prefer an unambiguous <article>; otherwise score under <body>.
    let articles: Vec<ElementRef> = document.select(sel("article")).collect();
    let root = match articles.len() {
        1 => articles[0],
        _ => match document.select(sel("body")).next() {
            Some(b) => b,
            None => {
                return Ok(Extracted {
                    markdown: String::new(),
                    title,
                    links,
                    confidence: 0.0,
                })
            }
        },
    };

    let blocks = child_blocks(&root);
    let (start, end, _score) = best_window(&blocks);
    let mut parts = Vec::new();
    for b in &blocks[start..end] {
        let rendered = render_element(b, 0, base);
        if !rendered.trim().is_empty() {
            parts.push(rendered.trim().to_string());
        }
    }
    let markdown = parts.join("\n\n");

    // Consensus: extractor A = block-window render (above). Extractor B =
    // every paragraph-like descendant rendered wholesale, no windowing.
    // Agreement (token overlap of B inside A's markdown) is the confidence:
    // a mis-cut window drops B's paragraphs and the overlap collapses.
    let para_sel = Selector::parse("p, h1, h2, h3, h4, h5, h6, li, pre, blockquote").unwrap();
    let mut b_parts: Vec<String> = Vec::new();
    for p in root.select(&para_sel) {
        let t = inline(&p, base);
        if !t.trim().is_empty() {
            b_parts.push(t.trim().to_string());
        }
    }
    let b_tokens = word_set(&b_parts.join(" "));
    let confidence = if b_tokens.is_empty() {
        0.0
    } else {
        let a_tokens = word_set(&markdown);
        let overlap = b_tokens.intersection(&a_tokens).count() as f64 / b_tokens.len() as f64;
        (overlap * 100.0).round() / 100.0
    };

    Ok(Extracted {
        markdown,
        title,
        links,
        confidence,
    })
}

fn word_set(s: &str) -> std::collections::HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().any(|c| c.is_alphanumeric()))
        .map(|w| w.to_lowercase())
        .collect()
}

/// True when the element or an ancestor-looking attribute marks it as noise.
fn is_noise(el: &ElementRef) -> bool {
    let v = el.value();
    let hay = format!(
        "{} {}",
        v.attr("class").unwrap_or_default(),
        v.attr("id").unwrap_or_default()
    )
    .to_lowercase();
    if NOISE_WORDS.iter().any(|w| hay.contains(w)) {
        return true;
    }
    if let Some(style) = v.attr("style") {
        let s = style.replace(' ', "").to_lowercase();
        if s.contains("display:none") || s.contains("visibility:hidden") || s.contains("opacity:0;")
        {
            return true;
        }
    }
    v.attr("hidden").is_some() || v.attr("aria-hidden").map(|a| a == "true").unwrap_or(false)
}

fn child_blocks<'a>(root: &ElementRef<'a>) -> Vec<ElementRef<'a>> {
    root.children()
        .filter_map(ElementRef::wrap)
        .filter(|el| !SKIP_TAGS.contains(&el.value().name()) && !is_noise(el))
        .collect()
}

/// One block's content score: total text, penalized by link density and
/// negative for boilerplate-shaped blocks. Never negative (weak == 0).
fn score_block(el: &ElementRef) -> f64 {
    let text: String = el.text().collect::<String>();
    let chars = text.trim().chars().count() as f64;
    if chars == 0.0 {
        return 0.0;
    }
    let link_chars: usize = el
        .select(sel("a"))
        .map(|a| a.text().collect::<String>().trim().chars().count())
        .sum();
    let ld = (link_chars as f64 / chars).min(1.0);
    // trafilatura-style link-density penalty: nav-heavy blocks collapse
    let base = chars * (1.0 - ld).powi(2);
    // tiny fragments (single short line like "Home | About") are weak
    let small = if chars < 40.0 { 0.25 } else { 1.0 };
    (base * small).max(0.0)
}

/// Best contiguous window [start,end) of blocks by summed score. O(n²) over
/// direct children only — pages have bounded top-level block counts.
fn best_window(blocks: &[ElementRef]) -> (usize, usize, f64) {
    let n = blocks.len();
    if n == 0 {
        return (0, 0, 0.0);
    }
    let scores: Vec<f64> = blocks.iter().map(score_block).collect();
    let mut best = (
        0usize,
        n.min(1),
        scores.first().copied().unwrap_or(0.0).max(0.0),
    );
    for start in 0..n {
        let mut sum = 0.0f64;
        for (end, score) in scores.iter().enumerate().take(n).skip(start) {
            sum += *score;
            if sum > best.2 + f64::EPSILON {
                best = (start, end + 1, sum);
            }
        }
    }
    if best.2 <= 0.0 {
        // nothing scored positive: keep everything (degenerate pages)
        return (0, n, 0.0);
    }
    let (mut s, mut e) = (best.0, best.1);
    while s < e - 1 && scores[s] <= 0.0 {
        s += 1;
    }
    while e > s + 1 && scores[e - 1] <= 0.0 {
        e -= 1;
    }
    (s, e, best.2)
}

// ---- DOM → markdown ---------------------------------------------------------

fn render_element(el: &ElementRef, depth: usize, base: &url::Url) -> String {
    if depth > MAX_DEPTH {
        return String::new();
    }
    let name = el.value().name();
    match name {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = name.as_bytes()[1] - b'0';
            format!("{} {}", "#".repeat(level as usize), inline(el, base).trim())
        }
        "p" | "section" | "div" | "main" | "span" | "body" | "center" => inline(el, base),
        "br" => "\n".to_string(),
        "hr" => "---".to_string(),
        "blockquote" => {
            let inner = inline(el, base).trim().to_string();
            inner
                .lines()
                .map(|l| format!("> {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
        "pre" => code_block(el),
        "ul" | "ol" => render_list(el, depth + 1, base),
        "table" => render_table(el, base),
        "dl" => render_dl(el, base),
        // inline containers rendered in place of text when standalone
        "a" | "strong" | "b" | "em" | "i" | "code" | "small" | "sup" | "sub" => inline(el, base),
        "img" => img_markdown(el, base),
        "figure" | "figcaption" | "details" | "summary" | "time" | "abbr" | "address"
        | "article" | "dt" | "dd" | "td" | "tr" | "th" | "tbody" | "thead" | "caption" => {
            inline(el, base)
        }
        _ => inline(el, base),
    }
}

fn inline(el: &ElementRef, base: &url::Url) -> String {
    let mut out = String::new();
    // Whitespace-edge tracking: collapse_ws trims each text node's edges, so
    // boundary whitespace lives in `pending` — set when a piece lost leading
    // or trailing whitespace, consumed when the next piece is appended.
    // Source-adjacent elements ("a<b>c</b>d") stay glued; spaced ones don't.
    let mut pending = false;
    for child in el.children() {
        match child.value() {
            Node::Text(t) => {
                let core = collapse_ws(t);
                if core.is_empty() {
                    continue;
                }
                if t.chars().next().map(|c| c.is_whitespace()).unwrap_or(false) {
                    pending = true;
                }
                if pending && !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&core);
                pending = t.chars().last().map(|c| c.is_whitespace()).unwrap_or(false);
            }
            Node::Element(_) => {
                if let Some(child_el) = ElementRef::wrap(child) {
                    let piece = inline_child(&child_el, base);
                    if piece.trim().is_empty() {
                        continue;
                    }
                    if pending && !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(&piece);
                    pending = false;
                }
            }
            _ => {}
        }
    }
    out
}

fn inline_child(el: &ElementRef, base: &url::Url) -> String {
    let name = el.value().name();
    match name {
        "script" | "style" | "noscript" | "template" | "svg" | "iframe" => String::new(),
        "br" => "\n".to_string(),
        "a" => {
            let text = inline(el, base).trim().to_string();
            let href = el.value().attr("href").unwrap_or_default();
            if text.is_empty() {
                String::new()
            } else if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
                text
            } else {
                match base.join(href) {
                    Ok(abs) => format!("[{text}]({abs})"),
                    Err(_) => text,
                }
            }
        }
        "strong" | "b" => {
            let text = inline(el, base).trim().to_string();
            if text.is_empty() {
                String::new()
            } else {
                format!("**{text}**")
            }
        }
        "em" | "i" => {
            let text = inline(el, base).trim().to_string();
            if text.is_empty() {
                String::new()
            } else {
                format!("*{text}*")
            }
        }
        "code" => {
            let text = el.text().collect::<String>();
            if text.contains('\n') {
                text
            } else {
                format!("`{}`", text.trim())
            }
        }
        "img" => img_markdown(el, base),
        "pre" => code_block(el),
        "ul" | "ol" => format!("\n{}", render_list(el, 1, base)),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            format!("\n\n{}", render_element(el, MAX_DEPTH, base))
        }
        "p" | "div" | "section" | "blockquote" | "table" | "figure" | "article" => inline(el, base),
        _ => inline(el, base),
    }
}

fn img_markdown(el: &ElementRef, base: &url::Url) -> String {
    let src = el.value().attr("src").unwrap_or_default();
    if src.is_empty() || src.starts_with("data:") {
        return String::new();
    }
    let alt = el.value().attr("alt").unwrap_or_default();
    match base.join(src) {
        Ok(abs) => format!("![{alt}]({abs})"),
        Err(_) => String::new(),
    }
}

fn code_block(el: &ElementRef) -> String {
    let raw = el.text().collect::<String>();
    let trimmed = raw.trim_matches('\n');
    // language hint from class="language-x" / "lang-x" on <pre> OR the inner
    // <code> (highlight.js puts it on code, github on pre)
    let class_of = |el: &ElementRef| el.value().attr("class").unwrap_or_default().to_string();
    let mut class = class_of(el);
    for c in el.select(&scraper::Selector::parse("code").unwrap()) {
        let cc = class_of(&c);
        if !cc.is_empty() {
            class.push(' ');
            class.push_str(&cc);
        }
    }
    let lang = class
        .split_whitespace()
        .find_map(|c| {
            c.strip_prefix("language-")
                .or_else(|| c.strip_prefix("lang-"))
        })
        .unwrap_or("");
    if trimmed.contains("```") || lang.is_empty() {
        format!("```\n{trimmed}\n```")
    } else {
        format!("```{lang}\n{trimmed}\n```")
    }
}

fn render_list(list: &ElementRef, depth: usize, base: &url::Url) -> String {
    let ordered = list.value().name() == "ol";
    let indent = "  ".repeat(depth.saturating_sub(1));
    let mut out = String::new();
    let mut index = 0u32;
    for li in list.children().filter_map(ElementRef::wrap) {
        if li.value().name() != "li" {
            continue;
        }
        index += 1;
        let marker = if ordered {
            format!("{index}.")
        } else {
            "-".to_string()
        };
        let body = inline(&li, base);
        let mut lines = body.lines();
        let first = lines.next().unwrap_or_default().trim().to_string();
        out.push_str(&format!("{indent}{marker} {first}"));
        for l in lines {
            let t = l.trim();
            if !t.is_empty() {
                out.push_str(&format!("\n{indent}  {t}"));
            }
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn render_table(table: &ElementRef, base: &url::Url) -> String {
    let rows: Vec<Vec<String>> = table
        .select(sel("tr"))
        .map(|tr| {
            tr.children()
                .filter_map(ElementRef::wrap)
                .filter(|c| matches!(c.value().name(), "td" | "th"))
                .map(|c| collapse_ws(&inline(&c, base)).trim().replace('|', "\\|"))
                .collect()
        })
        .filter(|r: &Vec<String>| !r.is_empty())
        .collect();
    if rows.is_empty() {
        return String::new();
    }
    let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        let mut cells = row.clone();
        cells.resize(width, String::new());
        out.push_str(&format!("| {} |\n", cells.join(" | ")));
        if i == 0 {
            out.push_str(&format!("|{}|\n", vec![" --- "; width].join("|")));
        }
    }
    out.trim_end().to_string()
}

fn render_dl(dl: &ElementRef, base: &url::Url) -> String {
    let mut out = String::new();
    for el in dl.children().filter_map(ElementRef::wrap) {
        match el.value().name() {
            "dt" => out.push_str(&format!("**{}**\n", inline(&el, base).trim())),
            "dd" => out.push_str(&format!("{}\n", inline(&el, base).trim())),
            _ => {}
        }
    }
    out.trim_end().to_string()
}

// ---- text helpers -----------------------------------------------------------

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(if ch == '\n' { '\n' } else { ' ' });
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strip the site-name suffix agents don't need: "Page | Site" → "Page".
/// Only cuts when the trailing segment actually matches the known site name.
fn strip_site_suffix(title: &str, site_name: Option<&str>) -> String {
    let Some(site) = site_name else {
        return title.to_string();
    };
    let site = normalize_ws(site);
    if site.is_empty() {
        return title.to_string();
    }
    for sep in [" | ", " — ", " - "] {
        let needle = format!("{sep}{site}");
        if title.len() > needle.len() && title.to_lowercase().ends_with(&needle.to_lowercase()) {
            return title[..title.len() - needle.len()].trim_end().to_string();
        }
    }
    title.to_string()
}

/// Structured metadata from JSON-LD / OpenGraph / meta tags (no-LLM layer).
pub fn metadata(html: &str) -> Value {
    let document = Html::parse_document(html);
    let meta = |pattern: &'static str| {
        document
            .select(sel(pattern))
            .next()
            .and_then(|m| m.value().attr("content").map(String::from))
            .filter(|c| !c.trim().is_empty())
    };
    json!({
        "title": meta("meta[property=\"og:title\"]"),
        "siteName": meta("meta[property=\"og:site_name\"]"),
        "author": meta("meta[name=\"author\"]").or_else(|| meta("meta[property=\"article:author\"]")),
        "description": meta("meta[name=\"description\"]").or_else(|| meta("meta[property=\"og:description\"]")),
        "publishedTime": meta("meta[property=\"article:published_time\"]"),
    })
}
