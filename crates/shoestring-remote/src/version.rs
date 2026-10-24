//! Protocol version and capabilities, exchanged in the handshake.
//!
//! Each side sends a [`PeerInfo`] at the end of its first message (the
//! client's `Hello` or `Graft`, the server's `Ready`). Decoders ignore bytes
//! past the fields they know, so peers from before this exchange read these
//! messages unchanged, and a message without the trailing fields marks such a
//! **legacy** peer. Every release before the exchange speaks protocol 1 with
//! no capabilities.
//!
//! - [`PROTOCOL_VERSION`] changes only for an incompatible change. Peers on
//!   different versions refuse each other with a readable reason (see
//!   [`incompatibility`]) instead of failing to decode mid-session.
//! - [`CAPABILITIES`] is a bitset of optional features. Each side uses only
//!   the bits both set, and ignores bits it doesn't know, so a new optional
//!   feature is a new bit, not a version change.

use crate::{put_str, put_u16, put_u32, Reader, Result};

/// This build's protocol version.
pub const PROTOCOL_VERSION: u16 = 1;

/// The version a legacy peer (one that sends no [`PeerInfo`]) speaks.
pub const LEGACY_PROTOCOL: u16 = 1;

/// The optional features this build supports. None are defined yet; each
/// future one gets its own bit here.
pub const CAPABILITIES: u32 = 0;

/// What one side told the other about itself in the handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub protocol: u16,
    pub capabilities: u32,
    /// The peer's package version (`0.9.1`), for messages and logs. Empty for
    /// a legacy peer.
    pub software: String,
}

impl PeerInfo {
    /// This build.
    pub fn ours() -> Self {
        PeerInfo {
            protocol: PROTOCOL_VERSION,
            capabilities: CAPABILITIES,
            software: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// A peer that sent no version information.
    pub fn legacy() -> Self {
        PeerInfo {
            protocol: LEGACY_PROTOCOL,
            capabilities: 0,
            software: String::new(),
        }
    }

    pub fn is_legacy(&self) -> bool {
        self.software.is_empty()
    }

    /// The optional features both sides support.
    pub fn shared_capabilities(&self, other: &PeerInfo) -> u32 {
        self.capabilities & other.capabilities
    }

    /// `0.9.1`, or a phrase for a legacy peer, for messages.
    fn describe(&self) -> String {
        if self.is_legacy() {
            "a release without version exchange".to_string()
        } else {
            self.software.clone()
        }
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_u16(out, self.protocol);
        put_u32(out, self.capabilities);
        put_str(out, &self.software);
    }

    /// Read the trailing fields, or [`PeerInfo::legacy`] when the message
    /// ends before them.
    pub(crate) fn decode_trailing(r: &mut Reader) -> Result<Self> {
        if r.is_at_end() {
            return Ok(PeerInfo::legacy());
        }
        Ok(PeerInfo {
            protocol: r.u16()?,
            capabilities: r.u32()?,
            software: r.string()?,
        })
    }
}

/// `None` when `server` and `client` can talk; otherwise why not, naming both
/// sides and which one to upgrade.
pub fn incompatibility(server: &PeerInfo, client: &PeerInfo) -> Option<String> {
    if server.protocol == client.protocol {
        return None;
    }
    let older = if server.protocol < client.protocol {
        "server"
    } else {
        "client"
    };
    Some(format!(
        "shoestring-remote-server ({}) speaks remote protocol {}, but \
         shoestring-remote-client ({}) speaks protocol {}; upgrade the {older} \
         so both match",
        server.describe(),
        server.protocol,
        client.describe(),
        client.protocol,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(protocol: u16, software: &str) -> PeerInfo {
        PeerInfo {
            protocol,
            capabilities: 0,
            software: software.to_string(),
        }
    }

    #[test]
    fn same_protocol_is_compatible_whatever_the_release() {
        assert_eq!(incompatibility(&peer(1, "0.9.1"), &peer(1, "0.12.0")), None);
        assert_eq!(
            incompatibility(&PeerInfo::legacy(), &PeerInfo::ours()),
            None
        );
    }

    #[test]
    fn mismatch_names_both_sides_and_the_one_to_upgrade() {
        let why = incompatibility(&peer(2, "0.11.0"), &peer(1, "0.10.0")).unwrap();
        assert!(why.contains("(0.11.0) speaks remote protocol 2"), "{why}");
        assert!(why.contains("(0.10.0) speaks protocol 1"), "{why}");
        assert!(why.ends_with("upgrade the client so both match"), "{why}");

        let why = incompatibility(&PeerInfo::legacy(), &peer(2, "0.11.0")).unwrap();
        assert!(why.contains("a release without version exchange"), "{why}");
        assert!(why.ends_with("upgrade the server so both match"), "{why}");
    }

    #[test]
    fn shared_capabilities_are_the_intersection() {
        let a = PeerInfo {
            capabilities: 0b1011,
            ..PeerInfo::ours()
        };
        let b = PeerInfo {
            capabilities: 0b0110,
            ..PeerInfo::ours()
        };
        assert_eq!(a.shared_capabilities(&b), 0b0010);
        assert_eq!(a.shared_capabilities(&PeerInfo::legacy()), 0);
    }
}
