//! Answers the control pipe the controller connects to.
//!
//! Everything unknown is refused outright, so a controller built against a newer protocol fails
//! visibly instead of appearing to work.

use std::collections::HashMap;
use std::sync::Arc;

use uuid::Uuid;
use vkms_ipc::pipe_server::{BoxFuture, ControlHandler};
use vkms_ipc::{ControlRequest, ControlResponse, ControlTier, ControlVerb, MachinesView};

use crate::mesh_host::MeshHost;
use crate::registry::PeerRegistry;
use crate::router::{InputRouter, PeerSender};
use crate::settings::{LocalSettingsDocument, MAX_SLOT, MIN_SLOT};
use crate::supervisor::AgentChannels;

/// The setting that says whether the focus decoy takes the foreground while blocking.
const FOCUS_DECOY: &str = "focusDecoy";

pub struct ControlEndpoint {
    pub registry: Arc<PeerRegistry>,
    pub router: Arc<InputRouter>,
    pub mesh: Arc<MeshHost>,
    pub channels: Arc<AgentChannels>,
}

/// Starts driving `peer`, or says precisely why not.
///
/// Pairing first, then reachability. They fail for completely different reasons, and a single
/// "cannot engage" would leave the user unable to tell which.
pub fn try_engage(registry: &PeerRegistry, mesh: &MeshHost, router: &InputRouter, peer: Uuid) -> Result<(), String> {
    if peer == registry.local_node_id() {
        return Err("This machine is already the one you are typing on.".into());
    }

    if !registry.is_paired(peer) {
        return Err("That machine is not paired with this one.".into());
    }

    if !mesh.is_connected(peer) {
        return Err("That machine is paired but not connected right now, so it cannot be driven.".into());
    }

    router.engage(peer);
    Ok(())
}

impl ControlEndpoint {
    pub fn handler(self: &Arc<Self>) -> ControlHandler {
        let endpoint = Arc::clone(self);
        Arc::new(move |request, tier| {
            let endpoint = Arc::clone(&endpoint);
            Box::pin(async move { endpoint.handle(request, tier).await }) as BoxFuture<ControlResponse>
        })
    }

    pub async fn handle(&self, request: ControlRequest, tier: ControlTier) -> ControlResponse {
        log::info!("control request {:?} from a {tier:?} caller", request.verb);

        match request.verb {
            ControlVerb::GetStatus => self.status(),
            ControlVerb::GetMachines => self.describe(),
            ControlVerb::GetHotkeys => self.get_hotkeys(),
            ControlVerb::SetHotkeys => self.set_hotkeys(&request),
            ControlVerb::GetSettings => self.get_settings(),
            ControlVerb::SetSettings => self.set_settings(&request),
            ControlVerb::Engage => self.engage(&request),
            ControlVerb::Disengage => {
                // Never an error, even when nothing was engaged. This is what a person reaches for
                // when they are not sure what state they are in.
                self.router.disengage();
                ControlResponse::ok()
            }
            ControlVerb::OpenPairingWindow => self.open_pairing().await,
            ControlVerb::ClosePairingWindow => {
                self.mesh.close_pairing_window().await;
                self.describe()
            }
            ControlVerb::PairWithCode => self.pair(&request).await,
            ControlVerb::RemovePeer => self.remove(&request).await,
            ControlVerb::Unknown => ControlResponse::fail("That request is not supported by this version of the service."),
        }
    }

    fn view(&self) -> MachinesView {
        let mesh = Arc::clone(&self.mesh);
        self.registry.describe(move |peer| mesh.is_connected(peer), self.mesh.pairing_left())
    }

    fn describe(&self) -> ControlResponse {
        with_view(&self.view(), HashMap::new())
    }

    fn status(&self) -> ControlResponse {
        let desktops = self.channels.desktops();

        ControlResponse::ok_with(HashMap::from([
            ("agents".to_owned(), desktops.join(",")),
            ("agentCount".to_owned(), desktops.len().to_string()),
            ("capturedFromAgents".to_owned(), self.router.forwarded_from_agents().to_string()),
            ("deliveredToAgents".to_owned(), self.router.delivered_to_agents().to_string()),
            ("localNodeId".to_owned(), self.registry.local_node_id().to_string()),
            ("machineName".to_owned(), self.registry.display_name().to_owned()),
            ("pairedPeers".to_owned(), self.registry.peers().len().to_string()),
            ("meshRunning".to_owned(), self.mesh.is_running().to_string()),
            ("connectedPeers".to_owned(), self.mesh.connected_peers().len().to_string()),
            ("discoveredPeers".to_owned(), self.mesh.discovered_peer_count().to_string()),
            ("pairingOpen".to_owned(), self.mesh.pairing_left().is_some().to_string()),
            ("engagedWith".to_owned(), self.router.target().map(|peer| peer.to_string()).unwrap_or_default()),
            ("sentToPeer".to_owned(), self.router.sent_to_peer().to_string()),
        ]))
    }

    fn engage(&self, request: &ControlRequest) -> ControlResponse {
        let Some(peer) = node_id_argument(request) else {
            return ControlResponse::fail("A 'nodeId' argument is required.");
        };

        match try_engage(&self.registry, &self.mesh, &self.router, peer) {
            Ok(()) => ControlResponse::ok(),
            Err(reason) => ControlResponse::fail(reason),
        }
    }

    /// This machine's own hotkey bindings, as "slot" -> node id (empty when unassigned).
    fn get_hotkeys(&self) -> ControlResponse {
        let settings = self.registry.settings().load();

        let values = (MIN_SLOT..=MAX_SLOT)
            .map(|slot| (slot.to_string(), settings.target_for(slot).map(|id| id.to_string()).unwrap_or_default()))
            .collect();

        ControlResponse::ok_with(values)
    }

    fn set_hotkeys(&self, request: &ControlRequest) -> ControlResponse {
        let Some(slot) = request.argument("slot").and_then(|text| text.trim().parse::<i32>().ok()) else {
            return ControlResponse::fail("A numeric 'slot' argument is required.");
        };

        // An empty target clears the slot: unbinding is a normal thing to want, and a separate verb
        // for it would only add a way to get it wrong.
        let target_text = request.argument("target").unwrap_or_default().trim();
        let target = if target_text.is_empty() {
            Uuid::nil()
        } else {
            match Uuid::parse_str(target_text) {
                Ok(target) => target,
                Err(_) => return ControlResponse::fail(format!("'{target_text}' is not a valid machine id.")),
            }
        };

        // Checked against the paired list, so a slot can never point at a stranger: a binding like
        // that would look configured and simply do nothing when pressed.
        if !target.is_nil() && !self.registry.is_paired(target) {
            return ControlResponse::fail("That machine is not paired with this one.");
        }

        let mut refusal = None;
        let result = self.registry.settings().update(|settings| match settings.bind(slot, target) {
            Ok(updated) => updated,
            Err(reason) => {
                refusal = Some(reason);
                settings.clone()
            }
        });

        match (refusal, result) {
            (Some(reason), _) => ControlResponse::fail(reason),
            (None, Err(error)) => ControlResponse::fail(format!("The hotkeys could not be saved: {error}")),
            (None, Ok(_)) => {
                log::info!(
                    "hotkey slot {slot} {}",
                    if target.is_nil() { "cleared".to_owned() } else { format!("bound to {target}") }
                );
                ControlResponse::ok()
            }
        }
    }

    /// This machine's own options, as name -> "true"/"false".
    fn get_settings(&self) -> ControlResponse {
        let settings = self.registry.settings().load();

        ControlResponse::ok_with(HashMap::from([(
            FOCUS_DECOY.to_owned(),
            settings.focus_decoy_enabled.to_string(),
        )]))
    }

    /// Changes the options named in the arguments, leaving the rest as they are.
    fn set_settings(&self, request: &ControlRequest) -> ControlResponse {
        let Some(text) = request.argument(FOCUS_DECOY) else {
            return ControlResponse::fail(format!("A '{FOCUS_DECOY}' argument is required."));
        };

        let Ok(enabled) = text.trim().to_ascii_lowercase().parse::<bool>() else {
            return ControlResponse::fail(format!("'{text}' is not true or false."));
        };

        let result = self
            .registry
            .settings()
            .update(|settings| LocalSettingsDocument { focus_decoy_enabled: enabled, ..settings.clone() });

        if let Err(error) = result {
            return ControlResponse::fail(format!("The setting could not be saved: {error}"));
        }

        // Applied only once saved, so what the agents do never disagrees with what the next
        // restart will load.
        self.channels.set_focus_decoy(enabled);
        log::info!("focus decoy {}", if enabled { "enabled" } else { "disabled" });

        self.get_settings()
    }

    async fn open_pairing(&self) -> ControlResponse {
        match self.mesh.open_pairing_window().await {
            // The code goes back exactly once, in the response to the act that created it, so it is
            // not sitting in the controller's memory for the whole session.
            Ok((code, remaining)) => with_view(
                &self.view(),
                HashMap::from([
                    ("code".to_owned(), code),
                    ("closesInSeconds".to_owned(), remaining.as_secs().to_string()),
                ]),
            ),
            Err(error) => ControlResponse::fail(format!("Pairing could not be opened: {error}")),
        }
    }

    async fn pair(&self, request: &ControlRequest) -> ControlResponse {
        let Some(code) = request.argument("code").filter(|code| !code.trim().is_empty()) else {
            return ControlResponse::fail("A 'code' argument is required.");
        };

        match self.mesh.pair_with_code(code, request.argument("host")).await {
            Ok(outcome) => with_view(
                &self.view(),
                HashMap::from([
                    ("pairedWith".to_owned(), outcome.peer_node_id.to_string()),
                    ("pairedName".to_owned(), outcome.peer_display_name),
                ]),
            ),
            Err(reason) => ControlResponse::fail(reason),
        }
    }

    async fn remove(&self, request: &ControlRequest) -> ControlResponse {
        let Some(peer) = node_id_argument(request) else {
            return ControlResponse::fail("A 'nodeId' argument is required.");
        };

        if peer == self.registry.local_node_id() {
            return ControlResponse::fail("This machine cannot unpair itself.");
        }

        // Released before the link goes, so the release can still reach it.
        if self.router.target() == Some(peer) {
            self.router.disengage();
        }

        match self.registry.remove_peer(peer) {
            Ok(true) => {
                self.mesh.kick();
                self.describe()
            }
            Ok(false) => ControlResponse::fail("That machine is not paired with this one."),
            Err(error) => ControlResponse::fail(format!("The change could not be saved: {error}")),
        }
    }
}

fn node_id_argument(request: &ControlRequest) -> Option<Uuid> {
    request.argument("nodeId").and_then(|text| Uuid::parse_str(text.trim()).ok())
}

fn with_view(view: &MachinesView, mut values: HashMap<String, String>) -> ControlResponse {
    values.insert(MachinesView::VALUE_KEY.to_owned(), view.to_json());
    ControlResponse::ok_with(values)
}
