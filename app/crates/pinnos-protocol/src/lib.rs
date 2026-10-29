//! Messages between `pinnos-core` and `pinnos-face`.
//!
//! Transport: the Windows named pipe at [`PIPE_NAME`]. Each frame is a 4-byte little-endian
//! length followed by that many bytes of UTF-8 JSON. The face connects as a client; the core
//! serves. Either side may send at any time.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

pub const PIPE_NAME: &str = r"\\.\pipe\pinnos-core";

/// Bumped on any breaking change. Peers with different versions refuse each other.
pub const PROTOCOL_VERSION: u32 = 2;

/// Frames larger than this are rejected to bound memory on a corrupt stream.
pub const MAX_FRAME: u32 = 4 * 1024 * 1024;

/// How often the core sends [`CoreMsg::Heartbeat`]. The face treats the link as lost after
/// three missed beats.
pub const HEARTBEAT_MS: u64 = 1000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum CoreMsg {
    /// First frame the core sends on a new connection.
    Hello { protocol: u32, core_version: String, pid: u32, started_unix_ms: u64 },
    Heartbeat { seq: u64, unix_ms: u64 },
    /// Show and focus the window (tray "Open PinnOS" or icon double-click).
    Show,
    /// Exit the face process. The core keeps running.
    Exit,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FaceMsg {
    /// First frame the face sends on a new connection.
    Hello { protocol: u32, face_version: String, pid: u32 },
    /// The window is now hidden to the tray (close button, Ctrl+Q, Alt+F4).
    Hidden,
    /// The window is visible again.
    Shown,
}

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let len = u32::try_from(body.len()).map_err(io::Error::other)?;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let msg = CoreMsg::Heartbeat { seq: 7, unix_ms: 42 };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: CoreMsg = read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn rejects_oversized_frame() {
        let mut buf = (MAX_FRAME + 1).to_le_bytes().to_vec();
        buf.extend([0u8; 8]);
        let err = read_frame::<_, CoreMsg>(&mut buf.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
