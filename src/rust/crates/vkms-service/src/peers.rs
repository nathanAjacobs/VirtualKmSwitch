//! The machines this one is paired with, as they sit on disk.
//!
//! Purely local. Each machine keeps its own list, written only by itself, and never exchanges it:
//! pairing A with B tells nobody else anything.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

/// One paired machine.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PairedPeer {
    pub node_id: Uuid,

    /// SHA-256 of its certificate: the identity everything else authenticates against.
    pub thumbprint: String,

    /// What it called itself when it paired.
    #[serde(default)]
    pub display_name: String,

    /// Seconds since the Unix epoch, for display only.
    #[serde(default)]
    pub paired_at: u64,

    /// Where it last accepted mesh connections, as `ip:port`.
    ///
    /// A hint, not an identity: whatever answers there is still judged by its certificate. Kept so
    /// that after a restart this machine can reach its peers on a network where beacons do not
    /// arrive, instead of waiting to be dialled.
    #[serde(default)]
    pub last_address: String,
}

impl PairedPeer {
    pub fn last_endpoint(&self) -> Option<std::net::SocketAddr> {
        self.last_address.parse().ok()
    }
}

/// The file's contents.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PersistedPeers {
    #[serde(default = "schema_version")]
    pub schema_version: i32,

    #[serde(default)]
    pub peers: Vec<PairedPeer>,

    /// Fields this build does not know, kept so that a newer build's data survives being rewritten
    /// by an older one.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn schema_version() -> i32 {
    1
}

impl Default for PersistedPeers {
    fn default() -> Self {
        Self { schema_version: schema_version(), peers: Vec::new(), extra: Map::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        let json = r#"{ "SchemaVersion": 1, "Peers": [], "FutureSetting": { "Nested": 42 } }"#;
        let parsed: PersistedPeers = serde_json::from_str(json).unwrap();

        assert!(parsed.extra.contains_key("FutureSetting"));
        assert!(serde_json::to_string(&parsed).unwrap().contains("FutureSetting"));
    }

    #[test]
    fn missing_fields_take_their_defaults() {
        let parsed: PersistedPeers = serde_json::from_str("{}").unwrap();
        assert_eq!(1, parsed.schema_version);
        assert!(parsed.peers.is_empty());
    }
}
