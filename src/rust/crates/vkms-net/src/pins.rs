//! The set of certificate thumbprints this node trusts, and the machine each belongs to.
//!
//! This is the whole of steady-state authentication. Pairing establishes a pin once; every
//! connection afterwards is authorised by thumbprint alone, so the pairing code is never needed
//! again and a leaked code cannot impersonate a paired machine.
//!
//! Nothing is trusted by default. An unpinned certificate is rejected however well-formed it is:
//! there is no CA to fall back on, and falling back to "looks valid" would accept any self-signed
//! certificate on the network.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use uuid::Uuid;

use crate::identity::thumbprint_of;

#[derive(Default)]
pub struct PinStore {
    pins: RwLock<HashMap<String, Uuid>>,
}

/// Thumbprints are hex; they compare case-insensitively wherever they came from.
fn key(thumbprint: &str) -> String {
    thumbprint.trim().to_ascii_uppercase()
}

impl PinStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self) -> usize {
        self.read().len()
    }

    pub fn thumbprints(&self) -> Vec<String> {
        self.read().keys().cloned().collect()
    }

    /// Trusts a machine's certificate, replacing any previous pin for that machine.
    ///
    /// A machine that re-pairs with a new certificate must not leave its old thumbprint trusted:
    /// revoking a compromised key has to actually revoke it.
    pub fn pin(&self, thumbprint: &str, node_id: Uuid) {
        let mut pins = self.write();
        let thumbprint = key(thumbprint);
        pins.retain(|existing, owner| *owner != node_id || *existing == thumbprint);
        pins.insert(thumbprint, node_id);
    }

    pub fn resolve(&self, thumbprint: &str) -> Option<Uuid> {
        self.read().get(&key(thumbprint)).copied()
    }

    pub fn is_trusted(&self, thumbprint: &str) -> bool {
        !thumbprint.trim().is_empty() && self.resolve(thumbprint).is_some()
    }

    /// Stops trusting a machine. Returns whether anything was removed.
    pub fn revoke(&self, node_id: Uuid) -> bool {
        let mut pins = self.write();
        let before = pins.len();
        pins.retain(|_, owner| *owner != node_id);
        pins.len() != before
    }

    pub fn clear(&self) {
        self.write().clear();
    }

    /// Resolves the machine a certificate belongs to, if it is pinned.
    ///
    /// Chain and name checks are deliberately absent: every certificate here is self-signed and
    /// carries no meaningful name, so those checks would always fail and tell us nothing. The pin
    /// replaces them, and is strictly stronger -- it identifies one exact key rather than anything a
    /// CA happened to sign.
    pub fn validate(&self, certificate_der: &[u8]) -> Option<Uuid> {
        self.resolve(&thumbprint_of(certificate_der))
    }

    /// Replaces the whole set. One write lock, so a handshake never observes a pin that is trusted
    /// both before and after the replacement as transiently missing.
    pub fn import(&self, pins: &HashMap<String, Uuid>) {
        let keep: HashSet<String> = pins.keys().map(|thumbprint| key(thumbprint)).collect();
        let mut current = self.write();

        for (thumbprint, node_id) in pins {
            current.insert(key(thumbprint), *node_id);
        }

        current.retain(|thumbprint, _| keep.contains(thumbprint));
    }

    pub fn export(&self) -> HashMap<String, Uuid> {
        self.read().clone()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Uuid>> {
        self.pins.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Uuid>> {
        self.pins.write().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pinned_thumbprint_resolves_regardless_of_case() {
        let pins = PinStore::new();
        let node = Uuid::new_v4();
        pins.pin("abcd", node);

        assert_eq!(Some(node), pins.resolve("ABCD"));
        assert!(pins.is_trusted("AbCd"));
        assert!(!pins.is_trusted(""));
    }

    #[test]
    fn re_pinning_a_machine_retires_its_old_certificate() {
        let pins = PinStore::new();
        let node = Uuid::new_v4();
        pins.pin("OLD", node);
        pins.pin("NEW", node);

        assert!(!pins.is_trusted("OLD"));
        assert!(pins.is_trusted("NEW"));
        assert_eq!(1, pins.count());
    }

    #[test]
    fn revoking_removes_every_pin_for_that_machine_only() {
        let pins = PinStore::new();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        pins.pin("A", a);
        pins.pin("B", b);

        assert!(pins.revoke(a));
        assert!(!pins.is_trusted("A"));
        assert!(pins.is_trusted("B"));
        assert!(!pins.revoke(a));
    }

    #[test]
    fn importing_replaces_the_set() {
        let pins = PinStore::new();
        let (kept, dropped) = (Uuid::new_v4(), Uuid::new_v4());
        pins.pin("KEPT", kept);
        pins.pin("DROPPED", dropped);

        pins.import(&HashMap::from([("kept".to_owned(), kept)]));

        assert!(pins.is_trusted("KEPT"));
        assert!(!pins.is_trusted("DROPPED"));
        assert_eq!(1, pins.count());
    }
}
