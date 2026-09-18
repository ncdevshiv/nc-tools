// W-Net-2b keyless sources: structured/JSON/RSS endpoints validated live
// (2026-09-02) rather than HTML scrapers. Bing RSS and Google News RSS give
// general-web + news coverage without keys; StackExchange/OpenAlex/arXiv/npm/
// crates cover Q&A/academic/package intents; the SearXNG fleet borrows other
// people's anti-bot infrastructure, probing instances at runtime and keeping
// only what answers format=json (most public instances 429 it — the fleet is
// designed to work with ANY number of survivors, including zero).
use serde_json::Value;

use nct_core::errors::ToolError;

use super::engines::RawResult;

// ---- RSS (Bing general web, Google News) --------------------------------------

/// Minimal RSS/Atom item parse: walk <item> (or <entry>) blocks, pull
/// title/link/description(summary) with CDATA + entity handling. Machine-
/// generated feeds have stable shapes; this stays tolerant to attribute noise.
pub fn parse_rss(body: &str, limit: usize) -> Vec<RawResult> {
    let mut out = Vec::new();
    for block in split_blocks(body, limit) {
        let title = tag(&block, "title").unwrap_or_default();
        let link = link_of(&block);
        if title.is_empty() || link.is_empty() {
            continue;
        }
        let snippet = strip_html(
            &tag(&block, "description")
                .or_else(|| tag(&block, "summary"))
                .unwrap_or_default(),
        );
        out.push(RawResult {
            title,
            url: link,
            snippet,
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

fn split_blocks(body: &str, limit: usize) -> Vec<String> {
    let open = if body.contains("<item>") {
        "<item>"
    } else {
        "<entry>"
    };
    let close = if body.contains("<item>") {
        "</item>"
    } else {
        "</entry>"
    };
    let mut blocks = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find(open) {
        let after = &rest[start + open.len()..];
        match after.find(close) {
            Some(end) => {
                blocks.push(after[..end].to_string());
                rest = &after[end + close.len()..];
            }
            None => break,
        }
        if blocks.len() > limit * 3 {
            break;
        }
    }
    blocks
}

fn tag(block: &str, name: &str) -> Option<String> {
    // matches <name>…</name> and <name attr="…">…</name>
    let start = block.find(&format!("<{name}"))?;
    let after_open = block[start..].find('>').map(|i| start + i + 1)?;
    let end = block[after_open..]
        .find(&format!("</{name}>"))
        .map(|i| after_open + i)?;
    Some(decode_xml(&block[after_open..end]))
}

fn link_of(block: &str) -> String {
    // RSS <link>text</link>; Atom <link href="…"/>; Google News wraps in <link/>
    if let Some(l) = tag(block, "link") {
        if !l.is_empty() {
            return l;
        }
    }
    // Atom: <link rel="alternate" href="URL" …/>
    if let Some(idx) = block.find("<link") {
        if let Some(rest) = block[idx..].find("href=\"").map(|i| &block[idx + i + 6..]) {
            if let Some(end) = rest.find('"') {
                return decode_xml(&rest[..end]);
            }
        }
    }
    String::new()
}

fn decode_xml(s: &str) -> String {
    let s = s.trim();
    let s = if let Some(stripped) = s.strip_prefix("<![CDATA[") {
        stripped.strip_suffix("]]>").unwrap_or(stripped).to_string()
    } else {
        s.to_string()
    };
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---- StackExchange ---------------------------------------------------------------

pub fn parse_stackexchange(body: &str, limit: usize) -> Result<Vec<RawResult>, ToolError> {
    let v: Value = serde_json::from_str(body).map_err(|e| {
        ToolError::new(
            "ERR_ENGINE",
            format!("stackexchange returned invalid json: {e}"),
        )
    })?;
    let mut out = Vec::new();
    if let Some(items) = v["items"].as_array() {
        for item in items {
            let title = decode_xml(item["title"].as_str().unwrap_or_default())
                .trim()
                .to_string();
            let link = item["link"].as_str().unwrap_or_default().to_string();
            if title.is_empty() || link.is_empty() {
                continue;
            }
            let score = item["score"].as_i64().unwrap_or(0);
            let answered = item["is_answered"].as_bool().unwrap_or(false);
            let tags = item["tags"]
                .as_array()
                .map(|t| {
                    t.iter()
                        .filter_map(|x| x.as_str())
                        .take(3)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            out.push(RawResult {
                title,
                url: link,
                snippet: format!(
                    "score {score}{}{}",
                    if answered { ", answered" } else { "" },
                    if tags.is_empty() {
                        String::new()
                    } else {
                        format!(", tags: {tags}")
                    }
                ),
            });
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

// ---- OpenAlex --------------------------------------------------------------------

pub fn parse_openalex(body: &str, limit: usize) -> Result<Vec<RawResult>, ToolError> {
    let v: Value = serde_json::from_str(body).map_err(|e| {
        ToolError::new("ERR_ENGINE", format!("openalex returned invalid json: {e}"))
    })?;
    let mut out = Vec::new();
    if let Some(results) = v["results"].as_array() {
        for r in results {
            let title = r["display_name"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            if title.is_empty() {
                continue;
            }
            let doi = r["doi"].as_str().unwrap_or_default().to_string();
            let landing = r["primary_location"]["landing_page_url"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let url = if !landing.is_empty() {
                landing
            } else if !doi.is_empty() {
                doi
            } else {
                continue;
            };
            let year = r["publication_year"].as_i64().unwrap_or(0);
            let cited_by = r["cited_by_count"].as_i64().unwrap_or(0);
            let venue = r["primary_location"]["source"]["display_name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let mut bits: Vec<String> = Vec::new();
            if year > 0 {
                bits.push(year.to_string());
            }
            if cited_by > 0 {
                bits.push(format!("cited by {cited_by}"));
            }
            if !venue.is_empty() {
                bits.push(venue);
            }
            out.push(RawResult {
                title,
                url,
                snippet: bits.join(", "),
            });
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

// ---- arXiv (Atom) ------------------------------------------------------------------

pub fn parse_arxiv(body: &str, limit: usize) -> Vec<RawResult> {
    let mut out = Vec::new();
    for block in split_blocks(body, limit) {
        let title = tag(&block, "title")
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if title.is_empty() {
            continue;
        }
        let link = link_of(&block);
        let summary = strip_html(&tag(&block, "summary").unwrap_or_default())
            .chars()
            .take(300)
            .collect::<String>();
        out.push(RawResult {
            title,
            url: link,
            snippet: summary,
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

// ---- package registries --------------------------------------------------------------

pub fn parse_npm(body: &str, limit: usize) -> Result<Vec<RawResult>, ToolError> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| ToolError::new("ERR_ENGINE", format!("npm returned invalid json: {e}")))?;
    let mut out = Vec::new();
    if let Some(objects) = v["objects"].as_array() {
        for o in objects {
            let pkg = &o["package"];
            let name = pkg["name"].as_str().unwrap_or_default().to_string();
            if name.is_empty() {
                continue;
            }
            let desc = pkg["description"].as_str().unwrap_or_default().to_string();
            let version = pkg["version"].as_str().unwrap_or_default().to_string();
            let link = pkg["links"]["npm"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| format!("https://www.npmjs.com/package/{name}"));
            out.push(RawResult {
                title: format!(
                    "{name} (npm{})",
                    if version.is_empty() {
                        String::new()
                    } else {
                        format!(" v{version}")
                    }
                ),
                url: link,
                snippet: desc,
            });
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

pub fn parse_crates(body: &str, limit: usize) -> Result<Vec<RawResult>, ToolError> {
    let v: Value = serde_json::from_str(body).map_err(|e| {
        ToolError::new(
            "ERR_ENGINE",
            format!("crates.io returned invalid json: {e}"),
        )
    })?;
    let mut out = Vec::new();
    if let Some(crates) = v["crates"].as_array() {
        for c in crates {
            let name = c["id"].as_str().unwrap_or_default().to_string();
            if name.is_empty() {
                continue;
            }
            let desc = c["description"].as_str().unwrap_or_default().to_string();
            let downloads = c["downloads"].as_i64().unwrap_or(0);
            let version = c["max_version"].as_str().unwrap_or_default().to_string();
            out.push(RawResult {
                title: format!(
                    "{name} (crates.io{})",
                    if version.is_empty() {
                        String::new()
                    } else {
                        format!(" v{version}")
                    }
                ),
                url: format!("https://crates.io/crates/{name}"),
                snippet: format!(
                    "{}{}",
                    desc,
                    if downloads > 0 {
                        format!(" — {downloads} downloads")
                    } else {
                        String::new()
                    }
                ),
            });
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

// ---- SearXNG instance JSON ----------------------------------------------------------

pub fn parse_searxng(body: &str, limit: usize) -> Result<Vec<RawResult>, ToolError> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| ToolError::new("ERR_ENGINE", format!("searxng returned invalid json: {e}")))?;
    let mut out = Vec::new();
    if let Some(results) = v["results"].as_array() {
        for r in results {
            let title = r["title"].as_str().unwrap_or_default().trim().to_string();
            let url = r["url"].as_str().unwrap_or_default().to_string();
            if title.is_empty() || url.is_empty() {
                continue;
            }
            out.push(RawResult {
                title,
                url,
                snippet: r["content"].as_str().unwrap_or_default().to_string(),
            });
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

/// SearXNG HTML fallback: most public instances 429 the JSON API but serve
/// HTML to browsers. Parses the standard SearXNG result article h3 > a, with
/// p.result-content for snippets — structures that are stable across themes.
/// Only called when JSON 429s/403s (the fleet falls back to HTML explicitly).
pub fn parse_searxng_html(body: &str, base_url: &str, limit: usize) -> Vec<RawResult> {
    use scraper::Selector;
    let doc = scraper::Html::parse_document(body);
    let sel = Selector::parse("article.result, div.result")
        .unwrap_or_else(|_| Selector::parse("article").unwrap());
    let title_sel = Selector::parse("h3 a, h3 > a, a.result-link")
        .unwrap_or_else(|_| Selector::parse("h3 a").unwrap());
    let snippet_sel = Selector::parse("p.result-content, p.content")
        .unwrap_or_else(|_| Selector::parse("p").unwrap());
    let link_sel = Selector::parse("a").unwrap();
    let mut out = Vec::new();
    for article in doc.select(&sel) {
        let title_el = article
            .select(&title_sel)
            .next()
            .or_else(|| article.select(&link_sel).next());
        let Some(a) = title_el else { continue };
        let title = a.text().collect::<String>().trim().to_string();
        let href = a.value().attr("href").unwrap_or_default().to_string();
        if title.is_empty() || href.is_empty() {
            continue;
        }
        // searxng proxies external URLs via /search?q=… or returns the raw
        // absolute URL — absolutize against the instance for proxy paths.
        let url = if href.starts_with("http://") || href.starts_with("https://") {
            href
        } else {
            let root = base_url.trim_end_matches('/').to_string();
            if href.starts_with('/') {
                format!("{root}{href}")
            } else {
                format!("{root}/{href}")
            }
        };
        let snippet = article
            .select(&snippet_sel)
            .next()
            .map(|e| e.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        out.push(RawResult {
            title,
            url,
            snippet,
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Extract candidate SearXNG instance URLs from the searx.space registry JSON.
/// Keeps https instances whose last probe was 200, skips onion/i2p/ygg hosts.
pub fn instances_from_searxspace(body: &str, max_candidates: usize) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(instances) = v["instances"].as_object() else {
        return Vec::new();
    };
    let mut candidates: Vec<(String, u64)> = Vec::new();
    for (url, info) in instances {
        if !url.starts_with("https://") {
            continue;
        }
        let host = url.trim_start_matches("https://");
        if host.contains(".onion") || host.contains(".i2p") || host.contains(".ygg") {
            continue;
        }
        let status = info["http"]["status_code"].as_u64().unwrap_or(0);
        if status != 200 {
            continue;
        }
        // uptime field is an object with day/week/month percentages when present
        // (Map index PANICS on missing keys — .get only)
        let uptime = info["uptime"]
            .as_object()
            .and_then(|u| u.get("month"))
            .and_then(|m| m.as_f64())
            .unwrap_or(0.0);
        candidates.push((url.to_string(), (uptime * 1000.0) as u64));
    }
    candidates.sort_by_key(|b| std::cmp::Reverse(b.1));
    candidates
        .into_iter()
        .take(max_candidates)
        .map(|(u, _)| u)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bing_rss_fixture_parses() {
        let body = r#"<rss version="2.0"><channel>
<title>Bing: rust programming language</title>
<item><title>Rust Programming Language
</title><link>https://rust-lang.org/
</link><description>A language empowering everyone to build reliable and efficient software.
</description><pubDate>Wed, 02 Sep 2026 03:41:00 GMT
</pubDate></item>
<item><title>The Rust Book &amp; More</title><link>https://doc.rust-lang.org/book/</link><description>Learn Rust.</description></item>
</channel></rss>"#;
        let rs = parse_rss(body, 10);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].url, "https://rust-lang.org/");
        assert!(rs[0].snippet.starts_with("A language empowering"));
        assert_eq!(rs[1].title, "The Rust Book & More");
    }

    #[test]
    fn gnews_atom_style_with_cdata_parses() {
        let body = r#"<rss><channel>
<item><title><![CDATA[Rust ships new release - InfoWorld]]></title><link>https://news.google.com/rss/articles/ABC123</link><source>InfoWorld</source></item>
</channel></rss>"#;
        let rs = parse_rss(body, 10);
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].title, "Rust ships new release - InfoWorld");
        assert!(rs[0].url.contains("news.google.com"));
    }

    #[test]
    fn atom_entry_with_href_link_parses() {
        let body = r#"<feed xmlns="http://www.w3.org/2005/Atom">
<entry><title>Attention Is All You Need</title><link rel="alternate" type="text/html" href="https://arxiv.org/abs/1706.03762"/><summary>We propose a new simple architecture, the Transformer, based solely on attention mechanisms.</summary></entry>
</feed>"#;
        let rs = parse_rss(body, 10);
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].url, "https://arxiv.org/abs/1706.03762");
        assert!(rs[0].snippet.contains("Transformer"));
    }

    #[test]
    fn stackexchange_fixture_parses() {
        let body = r#"{"items":[{"tags":["rust"],"score":4,"is_answered":true,"title":"rust async update of HashMap with &quot;or_insert_with&quot;","link":"https://stackoverflow.com/questions/75504895/x"}]}"#;
        let rs = parse_stackexchange(body, 10).unwrap();
        assert_eq!(
            rs[0].title,
            "rust async update of HashMap with \"or_insert_with\""
        );
        assert!(rs[0].snippet.contains("score 4") && rs[0].snippet.contains("answered"));
    }

    #[test]
    fn openalex_fixture_parses() {
        let body = r#"{"results":[{"display_name":"Attention Is All You Need","publication_year":2017,"cited_by_count":90000,"doi":"https://doi.org/10.48550/arxiv.1706.03762","primary_location":{"landing_page_url":"https://arxiv.org/abs/1706.03762","source":{"display_name":"arXiv"}}}]}"#;
        let rs = parse_openalex(body, 10).unwrap();
        assert_eq!(rs[0].url, "https://arxiv.org/abs/1706.03762");
        assert!(rs[0].snippet.contains("2017") && rs[0].snippet.contains("90000"));
    }

    #[test]
    fn npm_crates_fixtures_parse() {
        let npm = r#"{"objects":[{"package":{"name":"react","version":"19.0.0","description":"React is a JavaScript library","links":{"npm":"https://www.npmjs.com/package/react"}}}]}"#;
        let rs = parse_npm(npm, 10).unwrap();
        assert_eq!(rs[0].title, "react (npm v19.0.0)");
        assert_eq!(rs[0].url, "https://www.npmjs.com/package/react");

        let crates = r#"{"crates":[{"id":"serde","description":"Serialization framework","downloads":300000000,"max_version":"1.0.219"}]}"#;
        let rs = parse_crates(crates, 10).unwrap();
        assert_eq!(rs[0].url, "https://crates.io/crates/serde");
        assert!(rs[0].snippet.contains("300000000"));
    }

    #[test]
    fn searxng_json_parses() {
        let body = r#"{"results":[{"title":"Rust","url":"https://www.rust-lang.org/","content":"Official site","engines":["google","bing"]}]}"#;
        let rs = parse_searxng(body, 10).unwrap();
        assert_eq!(rs[0].url, "https://www.rust-lang.org/");
    }

    #[test]
    fn searxng_html_fallback_parses_article_results() {
        let body = r#"<html><body>
        <article class="result">
          <h3><a href="https://www.rust-lang.org/">Rust Programming Language</a></h3>
          <p class="result-content">A language empowering everyone to build reliable and efficient software.</p>
        </article>
        <article class="result">
          <h3><a href="https://doc.rust-lang.org/book/">The Rust Book</a></h3>
          <p class="result-content">Learn Rust.</p>
        </article>
        </body></html>"#;
        let rs = parse_searxng_html(body, "https://searx.example/", 10);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].url, "https://www.rust-lang.org/");
        assert_eq!(rs[0].title, "Rust Programming Language");
        assert!(rs[0].snippet.contains("empowering everyone"));
    }

    #[test]
    fn searxng_html_absolutizes_internal_links() {
        let body = r#"<article class="result"><h3><a href="/search?url=https%3A%2F%2Fexample.org">Example</a></h3></article>"#;
        let rs = parse_searxng_html(body, "https://searx.instance/", 10);
        assert_eq!(rs.len(), 1);
        assert!(rs[0].url.starts_with("https://searx.instance/"));
    }

    #[test]
    fn searxspace_registry_filters_to_https_200() {
        let body = r#"{"instances":{
            "https://good.example/":{"http":{"status_code":200},"uptime":{"month":99.5}},
            "http://plain.example/":{"http":{"status_code":200}},
            "https://down.example/":{"http":{"status_code":503}},
            "https://abc.onion/":{"http":{"status_code":200}},
            "https://better.example/":{"http":{"status_code":200},"uptime":{"month":100.0}}
        }}"#;
        let urls = instances_from_searxspace(body, 10);
        assert_eq!(
            urls,
            vec!["https://better.example/", "https://good.example/"]
        );
    }
}
