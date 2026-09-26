//! Opaque-token ↔ WPD object-id-string bimap.
//!
//! The neutral [`ObjectHandle`]/[`StorageId`] are opaque `u64` tokens (see their docs). WPD addresses
//! objects by *string* ids (`"DEVICE"`, `"s10001"`, `"o2C"`, …), so this maps between the two.
//!
//! Tokens are **deterministic**: a given WPD id always hashes to the same token, even across
//! processes. That matters for the CLI, which runs one process per command and must accept a
//! [`StorageId`] it printed in an earlier invocation. A per-session counter would break that; a hash of the
//! stable WPD storage id does not.
//!
//! The map only needs the **reverse** direction (token → string) stored, since the forward direction
//! is the pure [`token`] hash. We populate the reverse map as ids are observed (during enumeration /
//! listing) so any token we hand out can later be turned back into the WPD string to issue a call.

use crate::mtp::{ObjectHandle, StorageId};
use std::collections::HashMap;

/// FNV-1a (64-bit) constants.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The deterministic token for a WPD object-id string.
///
/// The high bit is forced set so a real token can never collide with the reserved sentinels
/// [`ObjectHandle::ROOT`] (`0`), [`ObjectHandle::ALL`]/[`StorageId::ALL`] (`0xFFFF_FFFF`) — all of
/// which have the high bit clear.
fn token(wpd_id: &str) -> u64 {
    let mut h = FNV_OFFSET;
    for b in wpd_id.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h | 0x8000_0000_0000_0000
}

/// Bidirectional map between opaque tokens and WPD object-id strings.
///
/// Objects and storages share one string id-space in WPD (a storage *is* an object id like
/// `"s10001"`), so one reverse map serves both [`ObjectHandle`] and [`StorageId`].
#[derive(Debug, Default)]
pub(crate) struct IdMap {
    /// token → WPD id string. Forward (string → token) is the pure [`token`] hash.
    reverse: HashMap<u64, String>,
}

impl IdMap {
    /// A fresh, empty map.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Intern a WPD id as an [`ObjectHandle`], remembering the reverse mapping.
    pub(crate) fn object(&mut self, wpd_id: &str) -> ObjectHandle {
        ObjectHandle(self.intern(wpd_id))
    }

    /// Intern a WPD id as a [`StorageId`], remembering the reverse mapping.
    pub(crate) fn storage(&mut self, wpd_id: &str) -> StorageId {
        StorageId(self.intern(wpd_id))
    }

    /// The WPD id string for an [`ObjectHandle`], if seen, or for the [`ObjectHandle::ROOT`] /
    /// [`ObjectHandle::ALL`] sentinels (which have no WPD string of their own and return `None`).
    pub(crate) fn object_id(&self, handle: ObjectHandle) -> Option<&str> {
        self.reverse.get(&handle.0).map(String::as_str)
    }

    /// The WPD id string for a [`StorageId`], if seen.
    pub(crate) fn storage_id(&self, storage: StorageId) -> Option<&str> {
        self.reverse.get(&storage.0).map(String::as_str)
    }

    /// Insert (idempotently) and return the token for a WPD id string.
    fn intern(&mut self, wpd_id: &str) -> u64 {
        let t = token(wpd_id);
        // Deterministic: re-interning the same string is a no-op. A 64-bit collision between two
        // distinct strings is astronomically unlikely; if it ever happened the first writer wins
        // and we'd misresolve, so assert in debug to catch it in tests/CI.
        match self.reverse.get(&t) {
            Some(existing) => debug_assert_eq!(existing, wpd_id, "WPD id token collision"),
            None => {
                self.reverse.insert(t, wpd_id.to_string());
            }
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_deterministic_across_calls_and_instances() {
        assert_eq!(token("s10001"), token("s10001"));
        let mut a = IdMap::new();
        let mut b = IdMap::new();
        assert_eq!(a.storage("s10001"), b.storage("s10001"));
        assert_eq!(a.object("o2C"), b.object("o2C"));
    }

    #[test]
    fn distinct_strings_get_distinct_tokens() {
        let ids = [
            "DEVICE", "s10001", "s10002", "o2C", "oA", "Download", "DCIM",
        ];
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            assert!(seen.insert(token(id)), "token collision for {id}");
        }
    }

    #[test]
    fn tokens_avoid_the_reserved_sentinels() {
        for id in ["DEVICE", "s10001", "o2C", "", "ALL", "0"] {
            let t = token(id);
            assert_ne!(t, ObjectHandle::ROOT.0, "{id} collided with ROOT");
            assert_ne!(t, ObjectHandle::ALL.0, "{id} collided with ALL");
            assert_ne!(t, StorageId::ALL.0, "{id} collided with StorageId::ALL");
            assert_ne!(t, 0);
            // High bit always set by construction.
            assert_ne!(t & 0x8000_0000_0000_0000, 0);
        }
    }

    #[test]
    fn round_trips_token_back_to_string() {
        let mut map = IdMap::new();
        let storage = map.storage("s10001");
        let obj = map.object("o2C");
        assert_eq!(map.storage_id(storage), Some("s10001"));
        assert_eq!(map.object_id(obj), Some("o2C"));
        // The same string interned as both kinds shares one token and resolves either way.
        let dev_as_obj = map.object("DEVICE");
        let dev_as_storage = map.storage("DEVICE");
        assert_eq!(dev_as_obj.0, dev_as_storage.0);
    }

    #[test]
    fn unknown_token_resolves_to_none() {
        let map = IdMap::new();
        assert_eq!(map.object_id(ObjectHandle(0x8000_0000_dead_beef)), None);
        assert_eq!(map.object_id(ObjectHandle::ROOT), None);
        assert_eq!(map.storage_id(StorageId::ALL), None);
    }

    #[test]
    fn reinterning_is_idempotent() {
        let mut map = IdMap::new();
        let a = map.object("o2C");
        let b = map.object("o2C");
        assert_eq!(a, b);
    }
}
