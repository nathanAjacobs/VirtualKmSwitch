//! The control protocol: what the controller may ask, and who may ask it.
//!
//! The JSON shape -- PascalCase keys, enum values as strings, nulls omitted -- is the contract with
//! the C# controller, which reads it with System.Text.Json's defaults. Changing a name here is a
//! protocol change.

use std::collections::HashMap;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Refuse anything larger. A control message is small, and this bounds a hostile client.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// The production pipe name.
pub const PIPE_NAME: &str = "VirtualKmSwitch.Control";

/// Privilege required by a control operation.
///
/// The split is what lets the controller run unelevated. Without it, either the controller needs
/// administrator rights permanently, or any local user could silently pair the machine with
/// someone else's -- handing them every keystroke typed on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ControlTier {
    /// Available to the interactive user: engage, disengage, read status, edit local hotkeys.
    Runtime,

    /// Requires Administrators: change who this machine trusts.
    Administrative,
}

/// Operations the controller can ask the service to perform.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum ControlVerb {
    // --- Runtime tier ---
    GetStatus,
    Engage,
    Disengage,
    GetHotkeys,
    SetHotkeys,

    /// Reads this machine's own options, such as whether the focus decoy is used.
    GetSettings,

    /// Changes this machine's own options.
    SetSettings,

    /// Reads which machines this one is paired with. Reading it is not itself privileged.
    GetMachines,

    /// Closes the pairing window early. Runtime tier, unlike opening it: see `RUNTIME_VERBS`.
    ClosePairingWindow,

    // --- Administrative tier ---
    /// Opens the pairing window and returns the one-time code to show.
    OpenPairingWindow,

    /// Pairs with a machine whose window is open, given the code it shows.
    PairWithCode,

    /// Stops trusting a paired machine.
    RemovePeer,

    /// Anything this build does not recognise. Classified administrative, and then refused.
    #[serde(other)]
    Unknown,
}

/// Verbs default to Administrative: one added later without being classified here is locked down
/// rather than silently exposed to any local user -- the safe direction to fail.
const RUNTIME_VERBS: &[ControlVerb] = &[
    ControlVerb::GetStatus,
    ControlVerb::Engage,
    ControlVerb::Disengage,
    // Hotkeys are local to this machine and affect nobody else's, so editing them is a runtime
    // operation rather than one requiring administrator rights.
    ControlVerb::GetHotkeys,
    ControlVerb::SetHotkeys,
    // Likewise: how this machine blocks its own input is a preference of whoever sits at it.
    ControlVerb::GetSettings,
    ControlVerb::SetSettings,
    // The list is already on the screen of anyone sitting at the machine, so requiring elevation to
    // read it would buy nothing and push people towards running the controller elevated.
    ControlVerb::GetMachines,
    // Closing pairing early can only narrow who may pair, never widen it; the worst a local user can
    // do with it is cancel someone else's code. Requiring elevation would mean a second UAC prompt
    // just to take a code off the screen.
    ControlVerb::ClosePairingWindow,
];

pub fn tier_for(verb: ControlVerb) -> ControlTier {
    if RUNTIME_VERBS.contains(&verb) {
        ControlTier::Runtime
    } else {
        ControlTier::Administrative
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ControlRequest {
    pub verb: ControlVerb,

    /// Free-form arguments, interpreted per verb.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<HashMap<String, String>>,
}

impl ControlRequest {
    pub fn new(verb: ControlVerb) -> Self {
        Self { verb, arguments: None }
    }

    pub fn with(mut self, key: &str, value: impl Into<String>) -> Self {
        self.arguments.get_or_insert_with(HashMap::new).insert(key.to_owned(), value.into());
        self
    }

    pub fn argument(&self, key: &str) -> Option<&str> {
        self.arguments.as_ref()?.get(key).map(String::as_str)
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ControlResponse {
    pub success: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<HashMap<String, String>>,
}

impl ControlResponse {
    pub fn ok() -> Self {
        Self { success: true, error: None, values: None }
    }

    pub fn ok_with(values: HashMap<String, String>) -> Self {
        Self { success: true, error: None, values: Some(values) }
    }

    pub fn fail(error: impl Into<String>) -> Self {
        Self { success: false, error: Some(error.into()), values: None }
    }

    pub fn value(&self, key: &str) -> Option<&str> {
        self.values.as_ref()?.get(key).map(String::as_str)
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

fn checked_length(header: [u8; 4]) -> io::Result<usize> {
    let length = i32::from_le_bytes(header);
    if length <= 0 || length as usize > MAX_MESSAGE_BYTES {
        // Hostile or corrupt: drop the connection rather than allocate on demand.
        return Err(invalid("control message length out of range"));
    }

    Ok(length as usize)
}

/// Reads one framed message. `Ok(None)` means the peer closed the connection cleanly.
pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }

    let mut payload = vec![0u8; checked_length(header)?];
    reader.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

pub async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_MESSAGE_BYTES {
        return Err(invalid("control message too large"));
    }

    // One buffer, one write: a pipe in message-agnostic byte mode still delivers it in order, and a
    // single write cannot interleave with anything.
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as i32).to_le_bytes());
    frame.extend_from_slice(payload);
    writer.write_all(&frame).await?;
    writer.flush().await
}

/// Blocking counterparts, for clients that are not running a runtime.
pub fn read_message_blocking<R: Read>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }

    let mut payload = vec![0u8; checked_length(header)?];
    reader.read_exact(&mut payload)?;
    Ok(Some(payload))
}

pub fn write_message_blocking<W: Write>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_MESSAGE_BYTES {
        return Err(invalid("control message too large"));
    }

    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as i32).to_le_bytes());
    frame.extend_from_slice(payload);
    writer.write_all(&frame)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_verbs_are_available_to_the_interactive_user() {
        for verb in [
            ControlVerb::GetStatus,
            ControlVerb::Engage,
            ControlVerb::Disengage,
            ControlVerb::GetHotkeys,
            ControlVerb::SetHotkeys,
            ControlVerb::GetMachines,
            ControlVerb::ClosePairingWindow,
        ] {
            assert_eq!(ControlTier::Runtime, tier_for(verb), "{verb:?}");
        }
    }

    #[test]
    fn changing_who_is_trusted_requires_administrator() {
        for verb in [
            ControlVerb::OpenPairingWindow,
            ControlVerb::PairWithCode,
            ControlVerb::RemovePeer,
        ] {
            assert_eq!(ControlTier::Administrative, tier_for(verb), "{verb:?}");
        }
    }

    #[test]
    fn unclassified_verbs_default_to_administrative() {
        // A verb added later without being classified must fail closed.
        assert_eq!(ControlTier::Administrative, tier_for(ControlVerb::Unknown));
    }

    #[test]
    fn a_verb_this_build_does_not_know_decodes_as_unknown_rather_than_failing() {
        // A controller newer than the service then gets a refusal it can show, instead of a
        // dropped connection that looks like the service has crashed.
        let request: ControlRequest = serde_json::from_str(r#"{"Verb":"ClaimHost"}"#).unwrap();
        assert_eq!(ControlVerb::Unknown, request.verb);
    }

    #[test]
    fn the_wire_shape_matches_what_the_controller_expects() {
        let request = ControlRequest::new(ControlVerb::Engage).with("nodeId", "abc");
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(r#"{"Verb":"Engage","Arguments":{"nodeId":"abc"}}"#, json);

        let response = ControlResponse::fail("nope");
        assert_eq!(r#"{"Success":false,"Error":"nope"}"#, serde_json::to_string(&response).unwrap());

        // Arguments are optional, and absent rather than null when there are none.
        let bare: ControlRequest = serde_json::from_str(r#"{"Verb":"GetStatus"}"#).unwrap();
        assert!(bare.arguments.is_none());
        assert_eq!(r#"{"Verb":"GetStatus"}"#, serde_json::to_string(&bare).unwrap());
    }

    #[test]
    fn oversized_and_non_positive_lengths_are_refused() {
        assert!(checked_length((MAX_MESSAGE_BYTES as i32 + 1).to_le_bytes()).is_err());
        assert!(checked_length(0i32.to_le_bytes()).is_err());
        assert!(checked_length((-5i32).to_le_bytes()).is_err());
        assert_eq!(10, checked_length(10i32.to_le_bytes()).unwrap());
    }
}
