// net.feed — RSS / Atom feed parsing into structured JSON (pure Rust via
// feed-rs). An agent can ask for a feed's entries directly instead of getting
// raw XML it has to parse itself.
use serde_json::json;
use serde_json::Value;

pub fn parse_feed(body: &str) -> Result<Value, String> {
    let feed = feed_rs::parser::parse(body.as_bytes()).map_err(|e| e.to_string())?;
    let title = feed.title.map(|t| t.content).unwrap_or_default();
    let mut entries: Vec<Value> = Vec::new();
    for e in feed.entries.iter().take(100) {
        let link = e
            .links
            .iter()
            .find(|l| !l.href.is_empty())
            .map(|l| l.href.clone())
            .unwrap_or_default();
        let published = e
            .published
            .map(|d| d.to_rfc3339())
            .unwrap_or_else(|| e.updated.map(|d| d.to_rfc3339()).unwrap_or_default());
        let summary = e
            .summary
            .as_ref()
            .map(|s| s.content.clone())
            .or_else(|| e.content.as_ref().and_then(|c| c.body.clone()))
            .unwrap_or_default();
        entries.push(json!({
            "title": e.title.as_ref().map(|t| t.content.clone()).unwrap_or_default(),
            "link": link,
            "published": published,
            "summary": summary,
        }));
    }
    Ok(json!({
        "title": title,
        "entries": entries,
        "count": entries.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::parse_feed;
    use serde_json::json;

    #[test]
    fn parses_rss() {
        let rss = r#"<?xml version="1.0"?>
<rss version="2.0"><channel>
<title>My Feed</title>
<item><title>First</title><link>https://x.dev/1</link><description>hello</description></item>
<item><title>Second</title><pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate></item>
</channel></rss>"#;
        let v = parse_feed(rss).unwrap();
        assert_eq!(v["title"], json!("My Feed"));
        assert_eq!(v["count"], json!(2));
        assert_eq!(v["entries"][0]["title"], json!("First"));
        assert_eq!(v["entries"][0]["link"], json!("https://x.dev/1"));
    }

    #[test]
    fn parses_atom() {
        let atom = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
<title>Atom Feed</title>
<entry><title>Post</title><link href="https://y.dev/a"/><updated>2024-01-02T00:00:00Z</updated></entry>
</feed>"#;
        let v = parse_feed(atom).unwrap();
        assert_eq!(v["title"], json!("Atom Feed"));
        assert_eq!(v["count"], json!(1));
        assert_eq!(v["entries"][0]["link"], json!("https://y.dev/a"));
    }
}
