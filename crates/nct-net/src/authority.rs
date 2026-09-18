// Authority learning — the self-improving part of keyless search. The kernel
// journals what its agent actually does: every successful net.fetch bumps the
// domain's fetch count, every net.cite bumps (heavier). net.search applies a
// bounded authority term to the neural rerank, so sources that proved useful
// ON THIS MACHINE rank higher over time. No telemetry, no key, no global
// model — the reputation map is private to the workspace.
use std::collections::HashMap;
use std::fs;
use std::io::Write;

use serde_json::{json, Value};

use nct_core::errors::ToolError;

const MAX_ENTRIES: usize = 5_000;
// authority alone can never dominate relevance — influence is capped by
// config (net_authority_influence, default 0.05) and the final score clamps at 1.0

pub struct AuthorityStore {
    domains: HashMap<String, u64>,
    path: std::path::PathBuf,
}

fn store_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join(".nc-tools").join("authority.json")
}

impl AuthorityStore {
    pub fn load(root: &std::path::Path) -> AuthorityStore {
        let path = store_path(root);
        let domains = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|v| v["domains"].as_object().cloned())
            .map(|obj| {
                obj.into_iter()
                    .filter_map(|(k, v)| v.as_u64().map(|n| (k, n)))
                    .collect::<HashMap<String, u64>>()
            })
            .unwrap_or_default();
        AuthorityStore { domains, path }
    }

    /// host-key bump: +1 weight (fetches bump 1, cites bump 3)
    pub fn bump(&mut self, host: &str, weight: u64) {
        if host.is_empty() {
            return;
        }
        let host = host.to_lowercase();
        let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
        let entry = self.domains.entry(host).or_insert(0);
        *entry = entry.saturating_add(weight);
        // bound the map: drop the coldest tail when it grows past the cap
        if self.domains.len() > MAX_ENTRIES {
            let mut pairs: Vec<(String, u64)> = self.domains.drain().collect();
            pairs.sort_by_key(|(_, n)| *n);
            pairs.reverse();
            pairs.truncate(MAX_ENTRIES);
            self.domains = pairs.into_iter().collect();
        }
        let _ = self.persist();
    }

    /// Bounded reputation in [0, 4]: log-scale so a single hot domain can't
    /// saturate the signal.
    pub fn score(&self, host: &str) -> f64 {
        let host = host.to_lowercase();
        let host = host.strip_prefix("www.").unwrap_or(&host);
        let n = self.domains.get(host).copied().unwrap_or(0);
        if n == 0 {
            return 0.0;
        }
        (n as f64).ln().min(4.0)
    }

    pub fn total_tracked(&self) -> usize {
        self.domains.len()
    }

    fn persist(&self) -> Result<(), ToolError> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let v = json!({ "domains": self.domains, "schema": "nc-tools/authority@1" });
        let tmp = self.path.with_extension("part");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(serde_json::to_string_pretty(&v)?.as_bytes())?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_grows_logarithmically_and_is_bounded() {
        let dir = std::env::temp_dir().join(format!("nct-auth-{}", std::process::id()));
        let mut a = AuthorityStore {
            domains: HashMap::new(),
            path: dir.join("authority.json"),
        };
        assert_eq!(a.score("example.com"), 0.0);
        for _ in 0..10 {
            a.bump("example.com", 1);
        }
        let s10 = a.score("example.com");
        for _ in 0..990 {
            a.bump("example.com", 1);
        }
        let s1000 = a.score("example.com");
        assert!(s1000 > s10, "more usage must score higher");
        assert!(
            s1000 <= 4.0 + f64::EPSILON,
            "score must clamp at 4.0, got {s1000}"
        );
        // www-stripping: same host
        assert_eq!(a.score("www.example.com"), s1000);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn authority_persists_and_survives_reload() {
        let dir = std::env::temp_dir().join(format!("nct-auth2-{}", std::process::id()));
        // mirror the production layout: the store lives under <root>/.nc-tools
        {
            let mut a = AuthorityStore::load(&dir);
            a.bump("docs.python.org", 3);
            a.bump("docs.python.org", 3);
        }
        let reloaded = AuthorityStore::load(&dir);
        assert!(
            reloaded.score("docs.python.org") > 0.0,
            "reload must restore the map"
        );
        let _ = fs::remove_dir_all(dir);
    }
}
