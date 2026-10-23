//! The service's single view of whom this machine trusts.
//!
//! One instance, shared by the control endpoint, the mesh and pairing. Two components each holding
//! their own copy of who is paired is how a machine ends up accepting input from a peer another
//! part of the same process has already removed.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use uuid::Uuid;
use vkms_ipc::{MachineInfo, MachinesView, TrustHealth};
use vkms_net::{NodeIdentity, PinStore};

use crate::durable::{DurableFile, LoadOutcome};
use crate::peers::{PairedPeer, PersistedPeers};
use crate::settings::LocalSettingsStore;

pub const PEERS_FILE: &str = "peers.json";
pub const SETTINGS_FILE: &str = "local.json";

pub struct PeerRegistry {
    store: DurableFile,
    settings: LocalSettingsStore,
    local_node_id: Uuid,
    display_name: String,
    identity: Arc<NodeIdentity>,

    /// Who this machine will accept a TLS connection from, derived from the paired list.
    ///
    /// Derived, never stored separately. A pin file beside the list would be a second record of the
    /// same fact, and the two would eventually disagree -- at which point the machine either refuses
    /// a peer it considers paired, or accepts one it has already removed. The second is a security
    /// failure, and it is the one that fails silently.
    pins: Arc<PinStore>,

    state: Mutex<PersistedPeers>,
    load_outcome: LoadOutcome,
    load_error: Option<String>,
}

impl PeerRegistry {
    /// Opens the registry in `data_dir`, with this machine's TLS identity from `identity`.
    ///
    /// The identity is a parameter so tests need not write a key to disk; the service passes
    /// [`NodeIdentity::load_or_create`].
    pub fn open(
        data_dir: &Path,
        display_name: &str,
        identity: impl FnOnce(Uuid) -> io::Result<NodeIdentity>,
    ) -> io::Result<Self> {
        let settings = LocalSettingsStore::new(data_dir.join(SETTINGS_FILE));
        let local_node_id = settings.ensure_local_node_id()?;
        let identity = Arc::new(identity(local_node_id)?);

        let store = DurableFile::new(data_dir.join(PEERS_FILE));
        let loaded = store.load::<PersistedPeers>();

        match loaded.outcome {
            LoadOutcome::RecoveredFromBackup => log::warn!(
                "{PEERS_FILE} was unreadable ({}); the backup was used instead",
                loaded.error.as_deref().unwrap_or("unknown error")
            ),

            // Deliberately loud. Carrying on with an empty list looks identical to never having
            // paired anything, and the machine would quietly stop trusting machines it is paired with.
            LoadOutcome::Unrecoverable => log::error!(
                "neither {PEERS_FILE} nor its backup could be read ({}); machines must be paired again",
                loaded.error.as_deref().unwrap_or("unknown error")
            ),

            LoadOutcome::Absent => log::info!("no {PEERS_FILE} yet; this machine is not paired with anything"),
            LoadOutcome::Loaded => {}
        }

        let registry = Self {
            store,
            settings,
            local_node_id,
            display_name: display_name.to_owned(),
            identity,
            pins: Arc::new(PinStore::new()),
            state: Mutex::new(loaded.value.unwrap_or_default()),
            load_outcome: loaded.outcome,
            load_error: loaded.error,
        };

        registry.project_pins(&registry.lock());
        log::info!("{} paired machine(s); this machine is {}", registry.peers().len(), local_node_id);
        Ok(registry)
    }

    pub fn local_node_id(&self) -> Uuid {
        self.local_node_id
    }

    /// What this machine calls itself to the machines it pairs with.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn identity(&self) -> &Arc<NodeIdentity> {
        &self.identity
    }

    pub fn pins(&self) -> &Arc<PinStore> {
        &self.pins
    }

    pub fn settings(&self) -> &LocalSettingsStore {
        &self.settings
    }

    pub fn peers(&self) -> Vec<PairedPeer> {
        self.lock().peers.clone()
    }

    pub fn is_paired(&self, node_id: Uuid) -> bool {
        self.lock().peers.iter().any(|peer| peer.node_id == node_id)
    }

    pub fn find(&self, node_id: Uuid) -> Option<PairedPeer> {
        self.lock().peers.iter().find(|peer| peer.node_id == node_id).cloned()
    }

    pub fn health(&self) -> (TrustHealth, Option<String>) {
        let health = match self.load_outcome {
            LoadOutcome::RecoveredFromBackup => TrustHealth::RecoveredFromBackup,
            LoadOutcome::Unrecoverable => TrustHealth::Unreadable,
            _ => TrustHealth::Ok,
        };

        (health, self.load_error.clone())
    }

    /// Records a machine that has just paired with this one, and gives it the next free hotkey so it
    /// works without configuration.
    ///
    /// A machine pairing again -- reinstalled, with a new certificate -- replaces its old entry, so
    /// the old certificate stops being trusted.
    pub fn add_peer(
        &self,
        node_id: Uuid,
        thumbprint: &str,
        display_name: &str,
        address: Option<SocketAddr>,
    ) -> io::Result<()> {
        if node_id == self.local_node_id || node_id.is_nil() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a machine cannot pair with itself"));
        }

        if thumbprint.eq_ignore_ascii_case(self.identity.thumbprint()) {
            // A peer presenting our own certificate is holding a key it should not have.
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "that machine presented this machine's own certificate"));
        }

        let paired_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs();

        {
            let mut state = self.lock();
            let mut updated = state.clone();
            updated.peers.retain(|peer| peer.node_id != node_id);
            updated.peers.push(PairedPeer {
                node_id,
                thumbprint: thumbprint.to_ascii_uppercase(),
                display_name: display_name.to_owned(),
                paired_at,
                last_address: address.map(|address| address.to_string()).unwrap_or_default(),
            });

            self.commit(&mut state, updated)?;
        }

        self.settings.update(|settings| match (settings.slot_for(node_id), settings.next_free_slot()) {
            (None, Some(slot)) => settings.bind(slot, node_id).unwrap_or_else(|_| settings.clone()),
            _ => settings.clone(),
        })?;

        log::info!("paired with {display_name} ({node_id})");
        Ok(())
    }

    /// Remembers where a paired machine was last reached. Not worth a write when nothing changed.
    pub fn note_address(&self, node_id: Uuid, address: SocketAddr) {
        let mut state = self.lock();
        let text = address.to_string();

        let Some(index) = state.peers.iter().position(|peer| peer.node_id == node_id && peer.last_address != text) else {
            return;
        };

        let mut updated = state.clone();
        updated.peers[index].last_address = text;

        if let Err(error) = self.commit(&mut state, updated) {
            log::warn!("could not record where {node_id} was reached: {error}");
        }
    }

    /// Stops trusting a machine, and clears any hotkey pointing at it. Returns whether it was paired.
    pub fn remove_peer(&self, node_id: Uuid) -> io::Result<bool> {
        let removed = {
            let mut state = self.lock();
            if !state.peers.iter().any(|peer| peer.node_id == node_id) {
                return Ok(false);
            }

            let mut updated = state.clone();
            updated.peers.retain(|peer| peer.node_id != node_id);
            self.commit(&mut state, updated)?;
            true
        };

        let paired: HashSet<Uuid> = self.peers().iter().map(|peer| peer.node_id).collect();
        self.settings.update(|settings| settings.prune_removed(&paired))?;

        log::info!("unpaired {node_id}");
        Ok(removed)
    }

    /// Describes this machine and the machines it is paired with, for the controller.
    pub fn describe(&self, is_connected: impl Fn(Uuid) -> bool, pairing_left: Option<Duration>) -> MachinesView {
        let (health, health_detail) = self.health();

        let mut machines =
            vec![MachineInfo::new(self.local_node_id, &self.display_name, self.identity.thumbprint(), true, false)];

        let mut peers = self.peers();
        peers.sort_by(|a, b| {
            a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()).then(a.node_id.cmp(&b.node_id))
        });

        machines.extend(peers.iter().map(|peer| {
            MachineInfo::new(peer.node_id, &peer.display_name, &peer.thumbprint, false, is_connected(peer.node_id))
        }));

        MachinesView {
            local_node_id: self.local_node_id,
            health,
            health_detail: (health != TrustHealth::Ok).then_some(health_detail).flatten(),
            machines,
            pairing_open: pairing_left.is_some(),
            pairing_seconds_left: pairing_left.map_or(0, |left| left.as_secs()),
        }
    }

    /// Saves first and updates memory only if that worked, so what is trusted in memory is never
    /// ahead of what would be trusted after a restart.
    fn commit(&self, state: &mut PersistedPeers, updated: PersistedPeers) -> io::Result<()> {
        self.store.save(&updated)?;
        *state = updated;

        // Kept in the one place every change funnels through, so a new operation cannot change who
        // is paired and forget to change who is trusted.
        self.project_pins(state);
        Ok(())
    }

    fn project_pins(&self, state: &PersistedPeers) {
        let trusted: HashMap<String, Uuid> = state
            .peers
            .iter()
            // This machine's own thumbprint is deliberately never trusted: nothing dials itself.
            .filter(|peer| peer.node_id != self.local_node_id && !peer.thumbprint.trim().is_empty())
            .map(|peer| (peer.thumbprint.clone(), peer.node_id))
            .collect();

        self.pins.import(&trusted);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PersistedPeers> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    const PEER_THUMBPRINT: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("vkms-registry-{}", Uuid::new_v4().simple())))
        }

        fn open(&self) -> PeerRegistry {
            PeerRegistry::open(&self.0, "Desk", |_| NodeIdentity::generate("Test")).unwrap()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_fresh_machine_is_described_as_itself_and_nothing_else() {
        let dir = TempDir::new();
        let view = dir.open().describe(|_| false, None);

        assert_eq!(1, view.machines.len());
        assert!(view.machines[0].is_local);
        assert_eq!(view.local_node_id, view.machines[0].node_id);
        assert_eq!(TrustHealth::Ok, view.health);
        assert!(!view.pairing_open);
    }

    #[test]
    fn pairing_a_machine_trusts_its_certificate() {
        let dir = TempDir::new();
        let registry = dir.open();
        let peer = Uuid::new_v4();

        registry.add_peer(peer, PEER_THUMBPRINT, "Laptop", None).unwrap();

        assert_eq!(Some(peer), registry.pins().resolve(PEER_THUMBPRINT));
        assert!(registry.is_paired(peer));
    }

    #[test]
    fn this_machines_own_certificate_is_never_trusted() {
        let dir = TempDir::new();
        let registry = dir.open();
        let own = registry.identity().thumbprint().to_owned();

        assert!(!registry.pins().is_trusted(&own));
        assert!(registry.add_peer(Uuid::new_v4(), &own, "Impostor", None).is_err());
        assert!(registry.add_peer(registry.local_node_id(), PEER_THUMBPRINT, "Self", None).is_err());
    }

    #[test]
    fn an_unknown_certificate_is_never_trusted() {
        let dir = TempDir::new();
        let registry = dir.open();
        registry.add_peer(Uuid::new_v4(), PEER_THUMBPRINT, "Laptop", None).unwrap();

        assert!(!registry.pins().is_trusted("3333333333333333333333333333333333333333333333333333333333333333"));
    }

    #[test]
    fn removing_a_machine_revokes_its_certificate() {
        let dir = TempDir::new();
        let registry = dir.open();
        let peer = Uuid::new_v4();
        registry.add_peer(peer, PEER_THUMBPRINT, "Laptop", None).unwrap();

        assert!(registry.remove_peer(peer).unwrap());

        // The point of the whole arrangement: a removal that left the pin behind would take a
        // machine off the list while it carried on being allowed to type into this one.
        assert!(!registry.pins().is_trusted(PEER_THUMBPRINT));
        assert!(!registry.remove_peer(peer).unwrap());
    }

    #[test]
    fn trust_survives_a_restart_without_a_file_of_its_own() {
        let dir = TempDir::new();
        let peer = Uuid::new_v4();
        dir.open().add_peer(peer, PEER_THUMBPRINT, "Laptop", None).unwrap();

        let restarted = dir.open();
        assert_eq!(Some(peer), restarted.pins().resolve(PEER_THUMBPRINT));
    }

    #[test]
    fn a_removal_stays_removed_across_a_restart() {
        let dir = TempDir::new();
        let peer = Uuid::new_v4();
        let first = dir.open();
        first.add_peer(peer, PEER_THUMBPRINT, "Laptop", None).unwrap();
        first.remove_peer(peer).unwrap();

        assert!(!dir.open().pins().is_trusted(PEER_THUMBPRINT));
    }

    #[test]
    fn a_machine_that_pairs_again_with_a_new_certificate_loses_the_old_one() {
        let dir = TempDir::new();
        let registry = dir.open();
        let peer = Uuid::new_v4();
        const NEW: &str = "4444444444444444444444444444444444444444444444444444444444444444";

        registry.add_peer(peer, PEER_THUMBPRINT, "Laptop", None).unwrap();
        registry.add_peer(peer, NEW, "Laptop", None).unwrap();

        assert!(registry.pins().is_trusted(NEW));
        assert!(!registry.pins().is_trusted(PEER_THUMBPRINT));
        assert_eq!(1, registry.peers().len());
    }

    #[test]
    fn a_newly_paired_machine_gets_the_next_free_hotkey() {
        let dir = TempDir::new();
        let registry = dir.open();
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());

        registry.add_peer(first, PEER_THUMBPRINT, "First", None).unwrap();
        registry.add_peer(second, "5555", "Second", None).unwrap();

        let settings = registry.settings().load();
        assert_eq!(Some(first), settings.target_for(1));
        assert_eq!(Some(second), settings.target_for(2));

        // Pairing again does not move it to another key.
        registry.add_peer(first, PEER_THUMBPRINT, "First", None).unwrap();
        assert_eq!(Some(1), registry.settings().load().slot_for(first));
    }

    #[test]
    fn removing_a_machine_clears_its_hotkey_and_leaves_the_slot_empty() {
        let dir = TempDir::new();
        let registry = dir.open();
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        registry.add_peer(first, PEER_THUMBPRINT, "First", None).unwrap();
        registry.add_peer(second, "5555", "Second", None).unwrap();

        registry.remove_peer(first).unwrap();

        let settings = registry.settings().load();
        assert_eq!(None, settings.target_for(1));
        assert_eq!(Some(second), settings.target_for(2));
    }

    #[test]
    fn a_damaged_list_is_reported_rather_than_presented_as_nothing_paired() {
        let dir = TempDir::new();
        dir.open().add_peer(Uuid::new_v4(), PEER_THUMBPRINT, "Laptop", None).unwrap();
        fs::write(dir.0.join(PEERS_FILE), "{ not json").unwrap();

        let registry = dir.open();
        let view = registry.describe(|_| false, None);

        assert_eq!(TrustHealth::RecoveredFromBackup, view.health);
        assert!(view.health_detail.is_some());
        assert!(registry.pins().is_trusted(PEER_THUMBPRINT));
    }

    #[test]
    fn an_unreadable_list_is_reported_as_such() {
        let dir = TempDir::new();
        dir.open().add_peer(Uuid::new_v4(), PEER_THUMBPRINT, "Laptop", None).unwrap();
        fs::write(dir.0.join(PEERS_FILE), "{ not json").unwrap();
        fs::write(dir.0.join(format!("{PEERS_FILE}.bak")), "{ also not json").unwrap();

        let view = dir.open().describe(|_| false, None);
        assert_eq!(TrustHealth::Unreadable, view.health);
        assert_eq!(1, view.machines.len());
    }

    #[test]
    fn the_description_reports_connection_and_pairing_state() {
        let dir = TempDir::new();
        let registry = dir.open();
        let peer = Uuid::new_v4();
        registry.add_peer(peer, PEER_THUMBPRINT, "Laptop", None).unwrap();

        let view = registry.describe(|id| id == peer, Some(Duration::from_secs(90)));

        let laptop = view.machines.iter().find(|machine| machine.node_id == peer).unwrap();
        assert!(laptop.is_connected);
        assert_eq!("Laptop", laptop.label);
        assert!(view.pairing_open);
        assert_eq!(90, view.pairing_seconds_left);
    }
}
