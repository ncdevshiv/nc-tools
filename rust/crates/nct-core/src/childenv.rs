// Child-process environment normalization — mirrors src/kernel/childenv.mjs.
// Windows launchers sometimes carry a minimal 'PATH' next to the full
// case-variant 'Path'; hand children the full 'Path' when it is strictly
// longer. Session overrides (env.set) win over host values.
use std::collections::BTreeMap;

/// Build the child environment: host env + session overrides, with the
/// Windows PATH/Path length heuristic applied last.
pub fn child_env(session: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    for (k, v) in session {
        env.insert(k.clone(), v.clone());
    }
    if cfg!(windows) {
        // Only apply the heuristic when both case variants exist, exactly as
        // the JS implementation does (Path strictly longer than PATH wins).
        if let (Some(path), Some(upper)) = (env.get("Path").cloned(), env.get("PATH").cloned()) {
            if path.len() > upper.len() {
                env.insert("PATH".into(), path);
            }
        }
    }
    env
}
