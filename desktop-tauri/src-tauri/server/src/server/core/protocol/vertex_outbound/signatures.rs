use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

// Anthropic/Responses only guarantee that the call ID survives a client round trip.
const MAX_ENTRIES: usize = 4096;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_SIGNATURE_BYTES: usize = 256 * 1024;
const TTL: Duration = Duration::from_secs(24 * 60 * 60);

pub(super) const HISTORY_SIGNATURE: &str = "skip_thought_signature_validator";

pub(super) fn signature(value: &Value) -> Option<&str> {
    [
        value.pointer("/extra_content/google/thought_signature"),
        value.get("thoughtSignature"),
        value.get("thought_signature"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .find(|text| !text.trim().is_empty())
}

struct Entry {
    key: [u8; 32],
    signature: Option<String>,
    inserted: Instant,
}

#[derive(Default)]
struct SignatureCache {
    entries: VecDeque<Entry>,
    bytes: usize,
}

impl SignatureCache {
    fn prune(&mut self, now: Instant) {
        while self
            .entries
            .front()
            .is_some_and(|entry| now.saturating_duration_since(entry.inserted) >= TTL)
        {
            self.pop_front();
        }
    }

    fn pop_front(&mut self) {
        if let Some(entry) = self.entries.pop_front() {
            self.bytes -= entry.signature.as_ref().map_or(0, String::len);
        }
    }

    fn insert(&mut self, key: [u8; 32], signature: Option<&str>, now: Instant) {
        self.prune(now);
        let bytes = signature.map_or(0, str::len);
        if bytes > MAX_SIGNATURE_BYTES {
            return;
        }
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            if let Some(entry) = self.entries.remove(index) {
                self.bytes -= entry.signature.as_ref().map_or(0, String::len);
            }
        }
        while self.entries.len() >= MAX_ENTRIES || self.bytes + bytes > MAX_BYTES {
            self.pop_front();
        }
        self.bytes += bytes;
        self.entries.push_back(Entry {
            key,
            signature: signature.map(str::to_owned),
            inserted: now,
        });
    }

    // Some(None) is an upstream call deliberately emitted without a signature,
    // such as the second call in a parallel batch. It is not a cache miss.
    fn get(&mut self, key: &[u8; 32], now: Instant) -> Option<Option<String>> {
        self.prune(now);
        self.entries
            .iter()
            .find(|entry| &entry.key == key)
            .map(|entry| entry.signature.clone())
    }
}

fn cache() -> &'static Mutex<SignatureCache> {
    static CACHE: OnceLock<Mutex<SignatureCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(SignatureCache::default()))
}

fn key(model: &str, id: &str, name: &str, args: &Value) -> [u8; 32] {
    let mut hash = Sha256::new();
    for field in [model, id, name, &args.to_string()] {
        hash.update((field.len() as u64).to_le_bytes());
        hash.update(field.as_bytes());
    }
    hash.finalize().into()
}

pub(super) fn remember(model: &str, id: &str, name: &str, args: &Value, signature: Option<&str>) {
    if let Ok(mut cache) = cache().lock() {
        cache.insert(key(model, id, name, args), signature, Instant::now());
    }
}

pub(super) fn recall(model: &str, id: &str, name: &str, args: &Value) -> Option<Option<String>> {
    if id.is_empty() {
        return None;
    }
    cache()
        .lock()
        .ok()?
        .get(&key(model, id, name, args), Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn distinguishes_unsigned_calls_from_misses_and_expires_entries() {
        let mut cache = SignatureCache::default();
        let now = Instant::now();
        cache.insert([1; 32], Some("signed"), now);
        cache.insert([2; 32], None, now);
        assert_eq!(cache.get(&[1; 32], now), Some(Some("signed".into())));
        assert_eq!(cache.get(&[2; 32], now), Some(None));
        assert_eq!(cache.get(&[3; 32], now), None);
        assert_eq!(cache.get(&[1; 32], now + TTL), None);
        assert!(cache.entries.is_empty());
        assert_eq!(cache.bytes, 0);
    }

    #[test]
    fn bounds_entry_count_and_signature_bytes() {
        let mut cache = SignatureCache::default();
        let now = Instant::now();
        for index in 0..=MAX_ENTRIES {
            let mut key = [0; 32];
            key[..8].copy_from_slice(&(index as u64).to_le_bytes());
            cache.insert(key, None, now);
        }
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert_eq!(cache.get(&[0; 32], now), None);
        let signature = "s".repeat(MAX_SIGNATURE_BYTES);
        for index in 0..40 {
            cache.insert([index; 32], Some(&signature), now);
        }
        assert!(cache.bytes <= MAX_BYTES);
        cache.insert([255; 32], Some(&"s".repeat(MAX_SIGNATURE_BYTES + 1)), now);
        assert_eq!(cache.get(&[255; 32], now), None);
    }

    #[test]
    fn fingerprint_isolates_model_id_name_and_arguments() {
        let original = key("m", "id", "Bash", &json!({"command":"pwd"}));
        for different in [
            key("m2", "id", "Bash", &json!({"command":"pwd"})),
            key("m", "id2", "Bash", &json!({"command":"pwd"})),
            key("m", "id", "Read", &json!({"command":"pwd"})),
            key("m", "id", "Bash", &json!({"command":"ls"})),
        ] {
            assert_ne!(original, different);
        }
    }
}
