// robots.txt parsing + isAllowed, plus sitemap discovery. Longest-match rule
// semantics per RFC 9309: within a group, the most specific (longest) rule
// that matches the path wins; Allow beats Disallow on equal length.
use serde_json::{json, Value};
use url::Url;

#[derive(Debug, Clone, PartialEq)]
enum Rule {
    Allow(String),
    Disallow(String),
}

#[derive(Debug, Default)]
struct Group {
    /// Which agents this group applies to ("*" included)
    agents: Vec<String>,
    rules: Vec<Rule>,
    crawl_delay: Option<f64>,
}

/// Parsed robots.txt for one origin.
#[derive(Debug, Default)]
pub struct Robots {
    groups: Vec<Group>,
    pub sitemaps: Vec<String>,
    /// True when the file existed (missing robots.txt = allow all)
    pub exists: bool,
}

/// Pattern match per RFC 9309: `*` wildcard, `$` anchor, literal otherwise.
fn pattern_matches(pattern: &str, path: &str) -> bool {
    fn rec(p: &[u8], s: &[u8]) -> bool {
        if p.is_empty() {
            // robots rules match as PREFIX: an exhausted pattern matches any
            // remaining path (Disallow: / blocks everything below it)
            return true;
        }
        match p[0] {
            b'*' => {
                // collapse consecutive stars
                let rest = &p[1..];
                if rest.is_empty() {
                    return true;
                }
                for i in 0..=s.len() {
                    if rec(rest, &s[i..]) {
                        return true;
                    }
                }
                false
            }
            b'$' if p.len() == 1 => s.is_empty(),
            _ => !s.is_empty() && s[0] == p[0] && rec(&p[1..], &s[1..]),
        }
    }
    rec(pattern.as_bytes(), path.as_bytes())
}

impl Robots {
    pub fn parse(raw: &str) -> Robots {
        let mut robots = Robots::default();
        robots.exists = true;
        // Classic group structure: a run of User-agent lines opens/extends a
        // group; the rules that follow attach to it; a UA line after a rule
        // starts the next group.
        let mut pending = Group::default();
        let mut last_was_agent = false;

        for line in raw.lines() {
            // strip comment
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = match line.split_once(':') {
                Some((k, v)) => (k.trim().to_lowercase(), v.trim().to_string()),
                None => continue,
            };
            match key.as_str() {
                "user-agent" => {
                    if !last_was_agent && !pending.agents.is_empty() {
                        robots.groups.push(std::mem::take(&mut pending));
                    }
                    let a = value.to_lowercase();
                    if !pending.agents.contains(&a) {
                        pending.agents.push(a);
                    }
                    last_was_agent = true;
                }
                "disallow" | "allow" => {
                    last_was_agent = false;
                    if pending.agents.is_empty() {
                        continue;
                    }
                    // empty disallow = allow everything (RFC 9309)
                    if key == "disallow" && value.is_empty() {
                        continue;
                    }
                    pending.rules.push(if key == "disallow" { Rule::Disallow(value) } else { Rule::Allow(value) });
                }
                "crawl-delay" => {
                    last_was_agent = false;
                    if let Ok(d) = value.parse::<f64>() {
                        pending.crawl_delay = Some(d);
                    }
                }
                "sitemap" => {
                    if !robots.sitemaps.contains(&value) {
                        robots.sitemaps.push(value);
                    }
                }
                _ => {}
            }
        }
        if !pending.agents.is_empty() {
            robots.groups.push(pending);
        }
        robots
    }

    fn group_for(&self, agent: &str) -> Option<&Group> {
        let agent = agent.to_lowercase();
        // exact agent match first, then *
        self.groups
            .iter()
            .find(|g| g.agents.iter().any(|a| *a == agent))
            .or_else(|| self.groups.iter().find(|g| g.agents.iter().any(|a| a == "*")))
    }

    /// RFC 9309 isAllowed: longest matching rule wins; ties go to Allow.
    pub fn is_allowed(&self, agent: &str, url: &Url) -> bool {
        let Some(group) = self.group_for(agent) else { return true };
        if group.rules.is_empty() {
            return true;
        }
        // path + query is what robots rules match (RFC 9309)
        let pathq = match url.query() {
            Some(q) => format!("{}?{}", url.path(), q),
            None => url.path().to_string(),
        };
        let mut best_len: Option<usize> = None;
        let mut best_allow = true;
        for rule in &group.rules {
            let (pattern, allow) = match rule {
                Rule::Allow(p) => (p, true),
                Rule::Disallow(p) => (p, false),
            };
            if pattern_matches(pattern, &pathq) {
                match best_len {
                    Some(l) if l > pattern.len() => {}
                    Some(l) if l == pattern.len() => {
                        best_allow = best_allow || allow; // tie → allow
                    }
                    _ => {
                        best_len = Some(pattern.len());
                        best_allow = allow;
                    }
                }
            }
        }
        best_allow
    }

    pub fn crawl_delay(&self, agent: &str) -> Option<f64> {
        self.group_for(agent).and_then(|g| g.crawl_delay)
    }
}

/// Structured metadata from JSON-LD / OG meta is in extract.rs; this report
/// shapes the net.robots result.

/// Description JSON for net.robots: allowed flag + discovered extras.
pub fn report(robot: &Robots, agent: &str, url: &Url, llms_txt: Option<Value>, content_signals: Option<String>) -> Value {
    json!({
        "url": url.to_string(),
        "agent": agent,
        "allowed": robot.is_allowed(agent, url),
        "robotsExists": robot.exists,
        "crawlDelaySec": robot.crawl_delay(agent),
        "sitemaps": robot.sitemaps,
        "llmsTxt": llms_txt,
        "contentSignals": content_signals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "User-agent: *\nDisallow: /private/\nAllow: /private/ok\nCrawl-delay: 2.5\nSitemap: https://x.dev/sitemap.xml\n\nUser-agent: badbot\nDisallow: /\n";

    #[test]
    fn parses_groups_and_rules() {
        let r = Robots::parse(SAMPLE);
        assert!(r.exists);
        assert_eq!(r.sitemaps, vec!["https://x.dev/sitemap.xml".to_string()]);
        assert!(r.is_allowed("Mozilla/5.0", &Url::parse("https://x.dev/public/page").unwrap()));
        assert!(!r.is_allowed("anyagent", &Url::parse("https://x.dev/private/secret").unwrap()));
        assert!(r.is_allowed("anyagent", &Url::parse("https://x.dev/private/ok/file").unwrap()));
        assert_eq!(r.crawl_delay("anyagent"), Some(2.5));
        assert!(!r.is_allowed("badbot", &Url::parse("https://x.dev/anything").unwrap()));
    }

    #[test]
    fn wildcard_and_anchor() {
        let r = Robots::parse("User-agent: *\nDisallow: /*.pdf$\n");
        assert!(!r.is_allowed("a", &Url::parse("https://x.dev/doc.pdf").unwrap()));
        assert!(r.is_allowed("a", &Url::parse("https://x.dev/doc.pdf.txt").unwrap()));
    }

    #[test]
    fn missing_file_allows_all() {
        let r = Robots::parse("");
        // empty string still marks exists=true; the caller treats "" specially
        assert!(r.is_allowed("a", &Url::parse("https://x.dev/x").unwrap()));
    }
}
