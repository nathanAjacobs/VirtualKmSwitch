//! The machine list, as the controller needs to show it.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::protocol::ControlResponse;

/// One machine.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MachineInfo {
    pub node_id: Uuid,

    /// What the machine calls itself.
    pub display_name: String,

    /// SHA-256 of its certificate: the identity everything else authenticates against.
    pub thumbprint: String,

    pub is_local: bool,

    /// Whether there is a live link to it right now. Always false for this machine.
    pub is_connected: bool,

    /// A name to show. Falls back to a short form of the node id rather than to something invented
    /// like "Machine 2", which would read as information when it is really a position in a list.
    pub label: String,
}

impl MachineInfo {
    pub fn new(node_id: Uuid, display_name: &str, thumbprint: &str, is_local: bool, is_connected: bool) -> Self {
        let label = if display_name.trim().is_empty() {
            node_id.simple().to_string()[..12].to_owned()
        } else {
            display_name.to_owned()
        };

        Self {
            node_id,
            display_name: display_name.to_owned(),
            thumbprint: thumbprint.to_owned(),
            is_local,
            is_connected,
            label,
        }
    }
}

/// Why the paired-machine list on disk could not be read normally.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum TrustHealth {
    /// Read normally, or there was nothing to read yet.
    #[default]
    Ok,

    /// The primary file was damaged and the backup was used.
    RecoveredFromBackup,

    /// Neither copy was readable. Machines must be paired again.
    Unreadable,
}

/// Everything the controller needs to describe pairing in one round trip.
///
/// Sent whole rather than as scalar values, because a list of machines does not survive being
/// flattened into string keys without inventing an encoding -- and a half-updated list on screen is
/// exactly the kind of thing that gets someone typing into the wrong computer.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MachinesView {
    pub local_node_id: Uuid,

    pub health: TrustHealth,

    /// Set when `health` is not `Ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_detail: Option<String>,

    /// This machine first, then every machine it is paired with.
    pub machines: Vec<MachineInfo>,

    /// Whether this machine is currently accepting a pairing, and for how much longer.
    pub pairing_open: bool,
    pub pairing_seconds_left: u64,
}

impl MachinesView {
    /// The response key this travels under.
    pub const VALUE_KEY: &'static str = "machines";

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Decodes a view from a response, or `None` if it carries none.
    ///
    /// Never an empty list in place of a missing one: "nothing paired" and "the service did not
    /// answer that question" look identical on screen, and only one of them is true.
    pub fn from_response(response: &ControlResponse) -> Option<Self> {
        let json = response.value(Self::VALUE_KEY)?;
        serde_json::from_str(json).ok()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn a_machine_with_no_name_falls_back_to_its_id_rather_than_an_invented_name() {
        let id = Uuid::new_v4();
        let machine = MachineInfo::new(id, "", "", false, false);
        assert_eq!(&id.simple().to_string()[..12], machine.label);
    }

    #[test]
    fn a_named_machine_shows_its_name() {
        assert_eq!("Workshop", MachineInfo::new(Uuid::new_v4(), "Workshop", "", false, false).label);
    }

    #[test]
    fn a_view_survives_the_round_trip_through_a_control_response() {
        let view = MachinesView {
            local_node_id: Uuid::new_v4(),
            health: TrustHealth::RecoveredFromBackup,
            health_detail: Some("primary was empty".into()),
            machines: vec![MachineInfo::new(Uuid::new_v4(), "Desk", "AB", true, false)],
            pairing_open: true,
            pairing_seconds_left: 42,
        };

        let response = ControlResponse::ok_with(HashMap::from([(MachinesView::VALUE_KEY.to_owned(), view.to_json())]));

        assert_eq!(Some(view), MachinesView::from_response(&response));
    }

    #[test]
    fn the_wire_shape_matches_what_the_controller_expects() {
        let view = MachinesView::default();
        let json = view.to_json();

        assert!(json.contains(r#""LocalNodeId":"00000000-0000-0000-0000-000000000000""#), "{json}");
        assert!(json.contains(r#""Health":"Ok""#), "{json}");
        assert!(!json.contains("HealthDetail"), "nulls are omitted: {json}");
    }

    #[test]
    fn a_response_carrying_no_view_decodes_to_nothing() {
        assert!(MachinesView::from_response(&ControlResponse::ok()).is_none());
        assert!(MachinesView::from_response(&ControlResponse::fail("nope")).is_none());

        let malformed =
            ControlResponse::ok_with(HashMap::from([(MachinesView::VALUE_KEY.to_owned(), "{ not json".to_owned())]));
        assert!(MachinesView::from_response(&malformed).is_none());
    }
}
