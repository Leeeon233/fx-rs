//! Bounded wire protocol for the detached terminal supervisor.

use std::io::{Read, Write};
use std::path::PathBuf;

use fx_core::ToolOutput;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::monitor::{MonitorDefinition, MonitorOperation};

pub const PROTOCOL_MINIMUM: u16 = 6;
pub const PROTOCOL_CURRENT: u16 = 6;
pub const CAPABILITY_FRAMED_JSON: u64 = 1 << 0;
pub const CAPABILITY_TERMINAL_SESSIONS: u64 = 1 << 1;
pub const CAPABILITY_CANCELLATION: u64 = 1 << 2;
pub const CAPABILITY_TERMINAL_MONITORS: u64 = 1 << 3;
pub const MAXIMUM_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolHello {
    pub minimum: u16,
    pub current: u16,
    pub capabilities: u64,
}

impl ProtocolHello {
    pub const fn local() -> Self {
        Self {
            minimum: PROTOCOL_MINIMUM,
            current: PROTOCOL_CURRENT,
            capabilities: CAPABILITY_FRAMED_JSON
                | CAPABILITY_TERMINAL_SESSIONS
                | CAPABILITY_CANCELLATION
                | CAPABILITY_TERMINAL_MONITORS,
        }
    }

    pub fn negotiate(self, peer: Self) -> Result<u16, ProtocolError> {
        if self.minimum == 0
            || peer.minimum == 0
            || self.minimum > self.current
            || peer.minimum > peer.current
        {
            return Err(ProtocolError::InvalidHello);
        }
        let minimum = self.minimum.max(peer.minimum);
        let current = self.current.min(peer.current);
        if minimum > current {
            return Err(ProtocolError::IncompatibleVersion);
        }
        if self.capabilities & CAPABILITY_FRAMED_JSON == 0
            || peer.capabilities & CAPABILITY_FRAMED_JSON == 0
        {
            return Err(ProtocolError::MissingCapability);
        }
        Ok(current)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostRequest {
    Hello {
        hello: ProtocolHello,
    },
    Ping,
    Terminal {
        request_id: String,
        operation: Box<TerminalOperation>,
    },
    Cancel {
        request_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostResponse {
    Hello {
        hello: ProtocolHello,
        negotiated: u16,
        instance_id: String,
    },
    Pong,
    Terminal {
        output: ToolOutput,
    },
    Cancelled {
        accepted: bool,
    },
    Failure {
        code: String,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalBackend {
    Native,
    Tmux,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReturnCondition {
    Started,
    Exit,
    Quiet { duration_ms: u64 },
    Match { pattern: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalKey {
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    Insert,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalWrite {
    Text { text: String },
    Keys { keys: Vec<TerminalKey> },
    Controls { controls: Vec<u8> },
    Paste { text: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalSignal {
    Hangup,
    Interrupt,
    Quit,
    Terminate,
    Kill,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalClosePolicy {
    Graceful,
    Force,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TerminalOperation {
    Start {
        backend: TerminalBackend,
        workspace_root: PathBuf,
        cwd: PathBuf,
        initial_monitors: Vec<MonitorDefinition>,
        command: Option<String>,
        shell: PathBuf,
        arguments: Vec<String>,
        sandbox_profile: Option<String>,
        rows: u16,
        columns: u16,
        return_when: Option<ReturnCondition>,
        wait_ceiling_ms: Option<u64>,
    },
    Read {
        session_id: String,
        cursor_segment: u64,
        cursor_offset: u64,
    },
    Screen {
        session_id: String,
    },
    Write {
        session_id: String,
        write: TerminalWrite,
    },
    Wait {
        session_id: String,
        return_when: ReturnCondition,
        wait_ceiling_ms: u64,
    },
    Monitor {
        session_id: String,
        operation: MonitorOperation,
    },
    Inspect {
        session_id: String,
        after_event_id: Option<u64>,
        acknowledge_event_id: Option<u64>,
        max_events: u16,
    },
    List {
        backend: Option<TerminalBackend>,
    },
    Resize {
        session_id: String,
        rows: u16,
        columns: u16,
    },
    Signal {
        session_id: String,
        signal: TerminalSignal,
    },
    Close {
        session_id: String,
        close_policy: TerminalClosePolicy,
    },
}

pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<(), ProtocolError> {
    let payload = serde_json::to_vec(value).map_err(ProtocolError::Encode)?;
    if payload.is_empty() || payload.len() > MAXIMUM_FRAME_BYTES {
        return Err(ProtocolError::FrameSize);
    }
    let length = u32::try_from(payload.len()).map_err(|_| ProtocolError::FrameSize)?;
    writer
        .write_all(&length.to_be_bytes())
        .map_err(ProtocolError::Io)?;
    writer.write_all(&payload).map_err(ProtocolError::Io)?;
    writer.flush().map_err(ProtocolError::Io)
}

pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T, ProtocolError> {
    let mut prefix = [0_u8; 4];
    reader.read_exact(&mut prefix).map_err(ProtocolError::Io)?;
    let length =
        usize::try_from(u32::from_be_bytes(prefix)).map_err(|_| ProtocolError::FrameSize)?;
    if length == 0 || length > MAXIMUM_FRAME_BYTES {
        return Err(ProtocolError::FrameSize);
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).map_err(ProtocolError::Io)?;
    serde_json::from_slice(&payload).map_err(ProtocolError::Decode)
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(std::io::Error),
    Encode(serde_json::Error),
    Decode(serde_json::Error),
    FrameSize,
    InvalidHello,
    IncompatibleVersion,
    MissingCapability,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "terminal host I/O: {error}"),
            Self::Encode(error) => write!(formatter, "encode terminal host frame: {error}"),
            Self::Decode(error) => write!(formatter, "decode terminal host frame: {error}"),
            Self::FrameSize => formatter.write_str("terminal host frame has an invalid size"),
            Self::InvalidHello => formatter.write_str("terminal host hello is invalid"),
            Self::IncompatibleVersion => {
                formatter.write_str("terminal host protocol versions are incompatible")
            }
            Self::MissingCapability => {
                formatter.write_str("terminal host is missing a required capability")
            }
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Encode(error) | Self::Decode(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_round_trip_is_exact_and_bounded() {
        let request = HostRequest::Hello {
            hello: ProtocolHello::local(),
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).unwrap();
        assert_eq!(
            read_frame::<HostRequest>(&mut bytes.as_slice()).unwrap(),
            request
        );

        let oversized = u32::try_from(MAXIMUM_FRAME_BYTES + 1)
            .unwrap()
            .to_be_bytes();
        assert!(matches!(
            read_frame::<HostRequest>(&mut oversized.as_slice()),
            Err(ProtocolError::FrameSize)
        ));
    }

    #[test]
    fn negotiation_rejects_invalid_disjoint_and_incapable_peers() {
        assert_eq!(
            ProtocolHello::local()
                .negotiate(ProtocolHello::local())
                .unwrap(),
            PROTOCOL_CURRENT
        );
        assert!(matches!(
            ProtocolHello::local().negotiate(ProtocolHello {
                minimum: 2,
                current: 2,
                capabilities: CAPABILITY_FRAMED_JSON,
            }),
            Err(ProtocolError::IncompatibleVersion)
        ));
        assert!(matches!(
            ProtocolHello::local().negotiate(ProtocolHello {
                minimum: PROTOCOL_MINIMUM,
                current: PROTOCOL_CURRENT,
                capabilities: 0,
            }),
            Err(ProtocolError::MissingCapability)
        ));
    }
}
