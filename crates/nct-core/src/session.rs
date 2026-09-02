// Session environment (env.set overrides host env for child processes) —
// mirrors the Map kept in kernel.mjs, exposed to env.* and proc.* tools.
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct SessionEnv {
    inner: Mutex<BTreeMap<String, String>>,
}

impl SessionEnv {
    pub fn new() -> SessionEnv {
        SessionEnv { inner: Mutex::new(BTreeMap::new()) }
    }

    pub fn set(&self, name: &str, value: &str) {
        self.inner.lock().unwrap().insert(name.to_string(), value.to_string());
    }

    pub fn get(&self, name: &str) -> Option<String> {
        self.inner.lock().unwrap().get(name).cloned()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.inner.lock().unwrap().contains_key(name)
    }

    pub fn snapshot(&self) -> BTreeMap<String, String> {
        self.inner.lock().unwrap().clone()
    }
}
