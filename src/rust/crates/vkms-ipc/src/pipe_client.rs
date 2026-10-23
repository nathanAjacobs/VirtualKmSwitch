//! A blocking client for the control pipe, for tests and command-line tools. The controller has its
//! own, in C#, speaking the same protocol.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};
use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;

use crate::protocol::{self, ControlRequest, ControlResponse};

pub struct ControlPipeClient {
    pipe: File,
}

impl ControlPipeClient {
    /// Connects, retrying while the server is busy or not yet listening, until `timeout`.
    ///
    /// Identification level only: the service may *read* who is calling, which is how it decides
    /// what the caller is allowed to do, but may not act as the caller.
    pub fn connect(pipe_name: &str, timeout: Duration) -> io::Result<Self> {
        let path = format!(r"\\.\pipe\{pipe_name}");
        let deadline = Instant::now() + timeout;

        loop {
            match OpenOptions::new().read(true).write(true).security_qos_flags(SECURITY_IDENTIFICATION).open(&path) {
                Ok(pipe) => return Ok(Self { pipe }),
                Err(error) => {
                    let retryable = matches!(
                        error.raw_os_error().map(|code| code as u32),
                        Some(ERROR_PIPE_BUSY) | Some(ERROR_FILE_NOT_FOUND)
                    );

                    if !retryable || Instant::now() >= deadline {
                        return Err(error);
                    }

                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    /// Sends a request and waits for its response.
    ///
    /// The pipe carries no request identifiers, so responses are matched to requests purely by
    /// order; `&mut self` keeps exactly one in flight.
    pub fn send(&mut self, request: &ControlRequest) -> io::Result<ControlResponse> {
        let payload = serde_json::to_vec(request)?;
        protocol::write_message_blocking(&mut self.pipe, &payload)?;

        let Some(bytes) = protocol::read_message_blocking(&mut self.pipe)? else {
            return Ok(ControlResponse::fail("The service closed the connection."));
        };

        Ok(serde_json::from_slice(&bytes)?)
    }
}
