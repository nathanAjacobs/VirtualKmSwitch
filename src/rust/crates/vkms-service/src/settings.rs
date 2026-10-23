//! This machine's own settings: its identity and its hotkey bindings.
//!
//! Bindings are local by design. Which key drives which machine is a property of the keyboard in
//! front of you, so each machine numbers the others however its user likes.
//!
//! Bindings name a machine by node id rather than by position in a list. A machine that is unpaired
//! therefore leaves its slot unassigned instead of silently shifting every later slot up -- which
//! would mean pressing a familiar key and typing into a different computer.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

/// Bindable slots. Slot 0 returns control here and is fixed.
pub const MIN_SLOT: i32 = 1;
pub const MAX_SLOT: i32 = 9;

/// One hotkey slot and the machine it drives.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct HotkeyBindingSetting {
    pub slot: i32,
    pub target_node_id: Uuid,
}

/// Everything local to this machine.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LocalSettingsDocument {
    #[serde(default = "one")]
    pub schema_version: i32,

    /// This machine's identity, generated once and never changed. It survives unpairing every
    /// machine, so a machine that pairs again is recognisably the same one.
    #[serde(default)]
    pub local_node_id: Uuid,

    #[serde(default)]
    pub hotkeys: Vec<HotkeyBindingSetting>,

    #[serde(default = "yes")]
    pub clipboard_sync_enabled: bool,

    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn one() -> i32 {
    1
}

fn yes() -> bool {
    true
}

impl Default for LocalSettingsDocument {
    fn default() -> Self {
        Self {
            schema_version: 1,
            local_node_id: Uuid::nil(),
            hotkeys: Vec::new(),
            clipboard_sync_enabled: true,
            extra: Map::new(),
        }
    }
}

impl LocalSettingsDocument {
    /// The machine bound to a slot, if any.
    pub fn target_for(&self, slot: i32) -> Option<Uuid> {
        self.hotkeys.iter().find(|binding| binding.slot == slot).map(|binding| binding.target_node_id)
    }

    /// The slot a machine is bound to, if any.
    pub fn slot_for(&self, node_id: Uuid) -> Option<i32> {
        self.hotkeys.iter().find(|binding| binding.target_node_id == node_id).map(|binding| binding.slot)
    }

    /// The lowest unassigned slot, so a newly paired machine works without configuration.
    pub fn next_free_slot(&self) -> Option<i32> {
        (MIN_SLOT..=MAX_SLOT).find(|slot| self.target_for(*slot).is_none())
    }

    /// Assigns a slot, or clears it when `target` is nil.
    ///
    /// A machine occupies one slot at a time; binding it again moves it rather than leaving it under
    /// two keys that both claim to drive it.
    pub fn bind(&self, slot: i32, target: Uuid) -> Result<Self, String> {
        if !(MIN_SLOT..=MAX_SLOT).contains(&slot) {
            return Err(format!(
                "Slot must be between {MIN_SLOT} and {MAX_SLOT}. Slot 0 returns control to this machine and cannot be \
                 reassigned."
            ));
        }

        let mut bindings: Vec<_> = self
            .hotkeys
            .iter()
            .filter(|binding| binding.slot != slot && (target.is_nil() || binding.target_node_id != target))
            .cloned()
            .collect();

        if !target.is_nil() {
            bindings.push(HotkeyBindingSetting { slot, target_node_id: target });
        }

        bindings.sort_by_key(|binding| binding.slot);
        Ok(Self { hotkeys: bindings, ..self.clone() })
    }

    /// Clears bindings pointing at machines that are no longer paired.
    ///
    /// The slot is left empty rather than reused. Silently rebinding a familiar key to a different
    /// machine is the worst available outcome: the keystrokes go somewhere unexpected and nothing on
    /// screen explains why.
    pub fn prune_removed(&self, paired: &HashSet<Uuid>) -> Self {
        Self {
            hotkeys: self.hotkeys.iter().filter(|binding| paired.contains(&binding.target_node_id)).cloned().collect(),
            ..self.clone()
        }
    }
}

pub struct LocalSettingsStore {
    path: PathBuf,
    gate: Mutex<()>,
}

impl LocalSettingsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), gate: Mutex::new(()) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> LocalSettingsDocument {
        let _gate = self.gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.load_unlocked()
    }

    /// Losing local preferences is an inconvenience, not a reason to refuse to start -- unlike the
    /// trust list, nothing here decides who may type into this machine.
    fn load_unlocked(&self) -> LocalSettingsDocument {
        fs::read(&self.path)
            .ok()
            .filter(|bytes| !bytes.is_empty())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, settings: &LocalSettingsDocument) -> io::Result<()> {
        let _gate = self.gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.save_unlocked(settings)
    }

    fn save_unlocked(&self, settings: &LocalSettingsDocument) -> io::Result<()> {
        if let Some(directory) = self.path.parent() {
            fs::create_dir_all(directory)?;
        }

        let mut temporary = self.path.as_os_str().to_owned();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);

        fs::write(&temporary, serde_json::to_vec_pretty(settings)?)?;
        fs::rename(&temporary, &self.path)
    }

    /// Applies `change` to the stored settings and saves the result, as one step.
    pub fn update(&self, change: impl FnOnce(&LocalSettingsDocument) -> LocalSettingsDocument) -> io::Result<LocalSettingsDocument> {
        let _gate = self.gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let updated = change(&self.load_unlocked());
        self.save_unlocked(&updated)?;
        Ok(updated)
    }

    /// Reads this machine's node id, generating and persisting one on first use.
    pub fn ensure_local_node_id(&self) -> io::Result<Uuid> {
        let _gate = self.gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let settings = self.load_unlocked();

        if !settings.local_node_id.is_nil() {
            return Ok(settings.local_node_id);
        }

        let created = Uuid::new_v4();
        self.save_unlocked(&LocalSettingsDocument { local_node_id: created, ..settings })?;
        Ok(created)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("vkms-settings-{}", Uuid::new_v4().simple()));
            Self(path)
        }

        fn store(&self) -> LocalSettingsStore {
            LocalSettingsStore::new(self.0.join("local.json"))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn absent_settings_load_as_defaults_rather_than_failing() {
        let dir = TempDir::new();
        let settings = dir.store().load();
        assert!(settings.hotkeys.is_empty());
        assert!(settings.clipboard_sync_enabled);
    }

    #[test]
    fn bindings_round_trip() {
        let dir = TempDir::new();
        let store = dir.store();
        let machine = Uuid::new_v4();

        store.save(&store.load().bind(3, machine).unwrap()).unwrap();

        assert_eq!(Some(machine), store.load().target_for(3));
    }

    #[test]
    fn binding_a_machine_already_bound_moves_it_rather_than_duplicating() {
        let machine = Uuid::new_v4();
        let settings = LocalSettingsDocument::default().bind(1, machine).unwrap().bind(5, machine).unwrap();

        assert_eq!(None, settings.target_for(1));
        assert_eq!(Some(machine), settings.target_for(5));
        assert_eq!(1, settings.hotkeys.len());
    }

    #[test]
    fn binding_a_nil_target_clears_the_slot() {
        let settings = LocalSettingsDocument::default().bind(2, Uuid::new_v4()).unwrap().bind(2, Uuid::nil()).unwrap();
        assert_eq!(None, settings.target_for(2));
    }

    #[test]
    fn only_slots_one_to_nine_are_bindable() {
        // Slot 0 returns control to this machine and is fixed; binding it would take away the way
        // back.
        for slot in [0, 10, -1] {
            assert!(LocalSettingsDocument::default().bind(slot, Uuid::new_v4()).is_err(), "slot {slot}");
        }
    }

    #[test]
    fn an_unpaired_machine_leaves_its_slot_empty_rather_than_shifting_others_up() {
        let (one, two) = (Uuid::new_v4(), Uuid::new_v4());
        let settings = LocalSettingsDocument::default().bind(1, one).unwrap().bind(2, two).unwrap();

        let pruned = settings.prune_removed(&HashSet::from([two]));

        assert_eq!(None, pruned.target_for(1));
        assert_eq!(Some(two), pruned.target_for(2));
    }

    #[test]
    fn a_new_machine_takes_the_lowest_free_slot() {
        let settings = LocalSettingsDocument::default();
        assert_eq!(Some(1), settings.next_free_slot());

        let settings = settings.bind(1, Uuid::new_v4()).unwrap();
        assert_eq!(Some(2), settings.next_free_slot());

        let settings = settings.bind(3, Uuid::new_v4()).unwrap();
        assert_eq!(Some(2), settings.next_free_slot());
    }

    #[test]
    fn all_slots_taken_reports_none_rather_than_an_invalid_slot() {
        let mut settings = LocalSettingsDocument::default();
        for slot in MIN_SLOT..=MAX_SLOT {
            settings = settings.bind(slot, Uuid::new_v4()).unwrap();
        }

        assert_eq!(None, settings.next_free_slot());
    }

    #[test]
    fn corrupt_settings_fall_back_to_defaults() {
        let dir = TempDir::new();
        let store = dir.store();
        store.save(&store.load().bind(1, Uuid::new_v4()).unwrap()).unwrap();
        fs::write(store.path(), "{ not json").unwrap();

        assert!(store.load().hotkeys.is_empty());
    }

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        let dir = TempDir::new();
        let store = dir.store();
        store.save(&store.load().bind(1, Uuid::new_v4()).unwrap()).unwrap();

        let json = fs::read_to_string(store.path()).unwrap();
        let json = format!("{}, \"FutureOption\": true }}", json.trim_end().trim_end_matches('}'));
        fs::write(store.path(), json).unwrap();

        let loaded = store.load();
        assert!(loaded.extra.contains_key("FutureOption"));

        store.save(&loaded).unwrap();
        assert!(fs::read_to_string(store.path()).unwrap().contains("FutureOption"));
    }

    #[test]
    fn the_node_id_is_created_once_and_then_kept() {
        let dir = TempDir::new();
        let store = dir.store();

        let first = store.ensure_local_node_id().unwrap();
        let second = store.ensure_local_node_id().unwrap();

        assert!(!first.is_nil());
        assert_eq!(first, second);
    }

    #[test]
    fn settings_written_by_the_previous_controller_build_are_still_read() {
        // The C# service wrote this same file; its node id and bindings carry over unchanged.
        let dir = TempDir::new();
        let store = dir.store();
        fs::create_dir_all(&dir.0).unwrap();
        fs::write(
            store.path(),
            r#"{
              "SchemaVersion": 1,
              "LocalNodeId": "5d9d3f2a-8b1c-4e6f-9a0b-1c2d3e4f5a6b",
              "Hotkeys": [ { "Slot": 2, "TargetNodeId": "11111111-1111-1111-1111-111111111111" } ],
              "ClipboardSyncEnabled": true
            }"#,
        )
        .unwrap();

        let settings = store.load();
        assert_eq!("5d9d3f2a-8b1c-4e6f-9a0b-1c2d3e4f5a6b", settings.local_node_id.to_string());
        assert_eq!(Some(2), settings.slot_for(Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap()));
    }
}
