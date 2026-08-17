use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContentId([u8; 32]);

impl ContentId {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for ContentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0[..8] {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentStoreStats {
    pub intern_requests: usize,
    pub materializations: usize,
    pub unique_contents: usize,
    pub unique_utf8_bytes: usize,
}

#[derive(Default)]
pub struct ContentStore {
    content: Mutex<HashMap<ContentId, Arc<str>>>,
    intern_requests: AtomicUsize,
    materializations: AtomicUsize,
    unique_utf8_bytes: AtomicUsize,
}

impl ContentStore {
    pub fn intern(&self, text: &str) -> ContentId {
        self.intern_requests.fetch_add(1, Ordering::Relaxed);
        let id = content_id(text.as_bytes());
        let mut content = self.content.lock().unwrap();
        content.entry(id).or_insert_with(|| {
            self.unique_utf8_bytes
                .fetch_add(text.len(), Ordering::Relaxed);
            Arc::<str>::from(text)
        });
        id
    }

    pub fn get(&self, id: ContentId) -> Option<Arc<str>> {
        self.materializations.fetch_add(1, Ordering::Relaxed);
        self.content.lock().unwrap().get(&id).cloned()
    }

    pub fn contains(&self, id: ContentId) -> bool {
        self.content.lock().unwrap().contains_key(&id)
    }

    pub fn stats(&self) -> ContentStoreStats {
        let content = self.content.lock().unwrap();
        ContentStoreStats {
            intern_requests: self.intern_requests.load(Ordering::Relaxed),
            materializations: self.materializations.load(Ordering::Relaxed),
            unique_contents: content.len(),
            unique_utf8_bytes: self.unique_utf8_bytes.load(Ordering::Relaxed),
        }
    }
}

fn content_id(bytes: &[u8]) -> ContentId {
    ContentId(Sha256::digest(bytes).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_content_reuses_one_allocation() {
        let store = ContentStore::default();
        let first = store.intern("shared source");
        let second = store.intern("shared source");

        assert_eq!(first, second);
        assert_eq!(
            store.stats(),
            ContentStoreStats {
                intern_requests: 2,
                materializations: 0,
                unique_contents: 1,
                unique_utf8_bytes: 13,
            }
        );
        assert_eq!(&*store.get(first).unwrap(), "shared source");
        assert_eq!(store.stats().materializations, 1);
    }
}
