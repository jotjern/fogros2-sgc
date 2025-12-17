//! Core data structures for the Global Data Plane (GDP) protocol.
//!
//! GDPName: A 4-byte identifier derived from SHA256(topic_name, topic_type, certificate).
//! This ensures nodes with matching credentials get the same topic ID without coordination.
//!
//! GDPPacket: The unit of data transfer. Contains action, destination, source, and payload.
//! Serialized as JSON header + null byte + binary payload for WebRTC transport.

use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::num::ParseIntError;
use std::str::FromStr;
use strum_macros::EnumIter;

/// Actions that can be performed on GDP packets.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize, Hash, EnumIter, Default)]
pub enum GdpAction {
    #[default]
    Noop = 0,
    Forward = 1,      // Forward payload to destination
    Advertise = 2,    // Announce topic availability
    AdvertiseResponse = 3,
    RibGet = 4,       // Query routing information
    RibReply = 5,
    Nack = 6,
    Control = 7,
}

/// 4-byte unique identifier for topics/nodes.
/// Derived from SHA256(topic_name, topic_type, cert)[0..4].
/// Same inputs always produce the same GDPName across all nodes.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Serialize, Deserialize, Hash, Default)]
pub struct GDPName(pub [u8; 4]);

impl fmt::Display for GDPName {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:02x}{:02x}{:02x}{:02x}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

impl FromStr for GDPName {
    type Err = ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut bytes = [0u8; 4];
        for (i, chunk) in s.as_bytes().chunks(2).take(4).enumerate() {
            if let Ok(chunk_str) = std::str::from_utf8(chunk) {
                bytes[i] = u8::from_str_radix(chunk_str, 16)?;
            }
        }
        Ok(GDPName(bytes))
    }
}

pub fn generate_random_gdp_name() -> GDPName {
    GDPName([
        rand::thread_rng().gen(),
        rand::thread_rng().gen(),
        rand::thread_rng().gen(),
        rand::thread_rng().gen(),
    ])
}

/// Generate deterministic GDPName from topic metadata.
/// Nodes with the same topic_name, topic_type, and certificate get the same GDPName.
pub fn get_gdp_name_from_topic(topic_name: &str, topic_type: &str, cert: &[u8]) -> [u8; 4] {
    let mut hasher = Sha256::new();
    info!(
        "Name is generated from topic_name: {}, topic_type: {}, cert: (too long, not printed)",
        topic_name, topic_type
    );
    hasher.update(topic_name);
    hasher.update(topic_type);
    hasher.update(cert);
    let result = hasher.finalize();
    
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&result[..4]);
    bytes
}

/// Main packet structure for ROS message transport over WebRTC.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct GDPPacket {
    pub action: GdpAction,
    pub gdpname: GDPName,      // Destination
    pub source: GDPName,
    pub payload: Option<Vec<u8>>,
    pub name_record: Option<GDPNameRecord>,
}

/// Wire format header. Sent as JSON followed by null byte, then binary payload.
#[derive(Debug, PartialEq, Serialize, Deserialize, Clone, Copy)]
pub struct GDPHeaderInTransit {
    pub action: GdpAction,
    pub destination: GDPName,
    pub length: usize,         // Length of payload that follows
}

pub(crate) trait Packet {
    fn get_byte_payload(&self) -> Option<&Vec<u8>>;
    fn get_header(&self) -> GDPHeaderInTransit;
}

impl Packet for GDPPacket {
    fn get_byte_payload(&self) -> Option<&Vec<u8>> {
        self.payload.as_ref()
    }

    fn get_header(&self) -> GDPHeaderInTransit {
        let name_record_len = self.name_record.as_ref()
            .and_then(|r| serde_json::to_string(r).ok())
            .map(|s| s.len())
            .unwrap_or(0);
        
        let payload_len = self.payload.as_ref().map(|p| p.len()).unwrap_or(0);
        
        GDPHeaderInTransit {
            action: self.action,
            destination: self.gdpname,
            length: payload_len + name_record_len,
        }
    }
}

impl fmt::Display for GDPPacket {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.payload {
            Some(payload) => {
                let content = std::str::from_utf8(payload)
                    .map(|s| s.trim_matches(char::from(0)))
                    .unwrap_or("<binary>");
                write!(f, "{}: {:?}", self.gdpname, content)
            }
            None => write!(f, "{}: <no payload>", self.gdpname),
        }
    }
}

/// Metadata stored in the RIB for topic resolution.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct GDPNameRecord {
    pub record_type: GDPNameRecordType,
    pub gdpname: GDPName,
    pub source_gdpname: GDPName,
    pub webrtc_offer: Option<String>,
    pub ip_address: Option<String>,
    pub ros: Option<(String, String)>,  // (topic_name, topic_type)
    pub indirect: Option<GDPName>,       // For forwarding to another node
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub enum GDPNameRecordType {
    EMPTY,
    INFO,    // Inform existence, don't replace
    QUERY,
    UPDATE,  // Replace existing
    MERGE,   // Merge with existing
    DELETE,
}

/// An edge in the routing tree: publisher -> subscriber.
#[derive(Clone)]
pub struct Connection {
    pub publisher: GDPName,
    pub subscriber: GDPName,
}

impl ToString for Connection {
    fn to_string(&self) -> String {
        format!("{}-{}", self.publisher, self.subscriber)
    }
}

impl FromStr for Connection {
    type Err = ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 2 {
            return Err("invalid".parse::<i32>().unwrap_err());
        }
        
        Ok(Connection {
            publisher: GDPName::from_str(parts[0])?,
            subscriber: GDPName::from_str(parts[1])?,
        })
    }
}
