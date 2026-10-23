//! The pairing messages, which travel as JSON inside the ordinary frame envelope.
//!
//! JSON here and hand-rolled binary on the input path, for the same reason in both cases: this
//! exchange happens a handful of times in a machine's life and benefits from being easy to read and
//! extend, while an input frame happens eight thousand times a second and benefits from costing
//! twelve bytes.

use std::io;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

use crate::codec::{self, FrameRead, FrameType};

/// Every step is bounded. A pairing is attended -- somebody is watching a progress indicator -- so a
/// peer that stops responding must fail quickly rather than hold the window, which is the one
/// interval in which this port accepts anyone at all.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(10);

const BUFFER_SIZE: usize = 8 * 1024;

/// What the machine the code was typed into opens with.
#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct EnrollRequest {
    pub node_id: Uuid,
    pub display_name: String,

    /// Where the joiner accepts mesh connections, so the other machine can reach it at once
    /// rather than waiting to hear a beacon -- which some networks never deliver.
    pub mesh_port: u16,

    /// Base64 nonce chosen by the joiner.
    pub initiator_nonce: String,
}

/// The reply from the machine showing the code: its identity, its nonce, and its half of the proof.
#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct EnrollChallenge {
    pub node_id: Uuid,
    pub display_name: String,
    pub mesh_port: u16,
    pub responder_nonce: String,
    pub proof: String,

    /// The certificate thumbprint this side observed coming from the joiner.
    ///
    /// Untrusted, and used only to explain a failure, never to accept one. If it does not match what
    /// the joiner knows it presented, something re-terminated TLS in between, and the joiner can say
    /// so instead of reporting a wrong code. An interceptor that lies here gains nothing: the proof
    /// was computed over the certificate it really presented, so it still fails.
    pub observed_peer_thumbprint: String,

    /// Set instead of a proof when the request is refused, so the joiner can say why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
}

/// The joiner's half of the proof, sent once it is satisfied about the other machine.
#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct EnrollConfirmation {
    pub proof: String,

    /// The thumbprint the joiner observed. Diagnostic only, exactly as above.
    pub observed_peer_thumbprint: String,
}

/// The final word from the machine showing the code.
///
/// Without it the joiner would record the pairing as soon as it had sent its proof, and so could
/// come away trusting a machine that went on to refuse it.
#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct EnrollAck {
    pub accepted: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

pub async fn write<S, T>(stream: &mut S, message: &T) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(message)?;
    let mut frame = vec![0u8; codec::HEADER_SIZE + payload.len()];

    let written = codec::write_control(&mut frame, FrameType::Control, &payload)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the pairing message is too large to frame"))?;

    stream.write_all(&frame[..written]).await?;
    stream.flush().await
}

/// Reads one message, or `None` if the peer closed, sent something malformed, or was too slow.
/// Anything can connect to this port while the window is open, so malformed input is an expected
/// condition rather than a fault.
pub async fn read<S, T>(stream: &mut S) -> Option<T>
where
    S: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut filled = 0;

    loop {
        let read = tokio::time::timeout(STEP_TIMEOUT, stream.read(&mut buffer[filled..])).await.ok()?.ok()?;
        if read == 0 {
            return None;
        }

        filled += read;

        match codec::read_frame(&buffer[..filled]) {
            FrameRead::Incomplete if filled < buffer.len() => continue,
            FrameRead::Complete { frame_type: FrameType::Control, payload, .. } => {
                return serde_json::from_slice(payload).ok();
            }
            _ => return None,
        }
    }
}
