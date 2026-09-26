use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    pub(crate) fn try_of_bytes<E>(
        bytes: &[u8],
        mut check: impl FnMut() -> Result<(), E>,
    ) -> Result<Self, E> {
        let mut hasher = Sha256::new();
        for chunk in bytes.chunks(64 * 1024) {
            check()?;
            hasher.update(chunk);
        }
        check()?;
        Ok(Self(hasher.finalize().into()))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0[..8] {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct HashedBytes {
    _bytes: Arc<[u8]>,
    #[serde(skip)]
    _hash: Arc<OnceLock<ContentHash>>,
}

impl HashedBytes {
    pub(crate) fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            _bytes: bytes.into(),
            _hash: Arc::default(),
        }
    }

    pub(crate) fn hash(&self) -> ContentHash {
        *self
            ._hash
            .get_or_init(|| ContentHash::of_bytes(&self._bytes))
    }
}

impl AsRef<[u8]> for HashedBytes {
    fn as_ref(&self) -> &[u8] {
        &self._bytes
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
    content: Mutex<HashMap<ContentHash, Arc<str>>>,
    intern_requests: AtomicUsize,
    materializations: AtomicUsize,
    unique_utf8_bytes: AtomicUsize,
}

impl ContentStore {
    pub fn intern(&self, text: &str) -> ContentHash {
        self._intern_with_hash(text, ContentHash::of_bytes(text.as_bytes()))
    }

    pub(crate) fn intern_bytes(
        &self,
        bytes: &HashedBytes,
    ) -> Result<ContentHash, std::str::Utf8Error> {
        let text = std::str::from_utf8(bytes.as_ref())?;
        Ok(self._intern_with_hash(text, bytes.hash()))
    }

    fn _intern_with_hash(&self, text: &str, id: ContentHash) -> ContentHash {
        self.intern_requests.fetch_add(1, Ordering::Relaxed);
        let mut content = self.content.lock().unwrap();
        content.entry(id).or_insert_with(|| {
            self.unique_utf8_bytes
                .fetch_add(text.len(), Ordering::Relaxed);
            Arc::<str>::from(text)
        });
        id
    }

    pub fn get(&self, id: ContentHash) -> Option<Arc<str>> {
        self.materializations.fetch_add(1, Ordering::Relaxed);
        self.content.lock().unwrap().get(&id).cloned()
    }

    pub fn contains(&self, id: ContentHash) -> bool {
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

    pub fn retain(&self, mut keep: impl FnMut(ContentHash) -> bool) {
        let mut content = self.content.lock().unwrap();
        content.retain(|hash, _| keep(*hash));
        self.unique_utf8_bytes.store(
            content.values().map(|value| value.len()).sum(),
            Ordering::Relaxed,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayon::prelude::*;

    #[test]
    fn immutable_bytes_share_the_hash_and_preserve_raw_serialization() {
        let bytes = HashedBytes::new(b"shared source".as_slice());
        let cloned = bytes.clone();
        assert!(Arc::ptr_eq(&bytes._bytes, &cloned._bytes));
        assert!(Arc::ptr_eq(&bytes._hash, &cloned._hash));
        assert!(bytes._hash.get().is_none());
        let expected = ContentHash::of_bytes(bytes.as_ref());
        assert_eq!(cloned.hash(), expected);
        assert_eq!(bytes._hash.get(), Some(&expected));
        let encoded = serde_json::to_vec(&bytes).unwrap();
        assert_eq!(encoded, serde_json::to_vec(bytes.as_ref()).unwrap());
        let restored: HashedBytes = serde_json::from_slice(&encoded).unwrap();
        assert!(restored._hash.get().is_none());
        assert_eq!(restored.hash(), expected);
        assert_eq!(restored.as_ref(), bytes.as_ref());
    }

    #[test]
    fn concurrent_byte_interning_reuses_one_content_allocation() {
        let bytes = HashedBytes::new(vec![b'x'; 64 * 1024]);
        for workers in [1, 4] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .build()
                .unwrap();
            let store = ContentStore::default();
            let ids = pool.install(|| {
                (0..32)
                    .into_par_iter()
                    .map(|_| store.intern_bytes(&bytes).unwrap())
                    .collect::<Vec<_>>()
            });
            assert_eq!(ids, vec![bytes.hash(); 32]);
            assert_eq!(
                store.stats(),
                ContentStoreStats {
                    intern_requests: 32,
                    materializations: 0,
                    unique_contents: 1,
                    unique_utf8_bytes: 64 * 1024,
                },
            );
            assert_eq!(store.get(ids[0]).unwrap().as_bytes(), bytes.as_ref());
        }
    }

    #[test]
    fn byte_interning_rejects_invalid_utf8_without_storing_it() {
        let store = ContentStore::default();
        let bytes = HashedBytes::new([0xff].as_slice());
        assert!(store.intern_bytes(&bytes).is_err());
        assert!(bytes._hash.get().is_none());
        assert_eq!(store.stats(), ContentStore::default().stats());
    }

    #[test]
    fn controlled_hashing_checks_between_chunks() {
        let mut checks = 0;
        let result = ContentHash::try_of_bytes(&vec![0; 128 * 1024], || {
            checks += 1;
            (checks < 2).then_some(()).ok_or("cancelled")
        });

        assert_eq!(result, Err("cancelled"));
        assert_eq!(checks, 2);
    }

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
