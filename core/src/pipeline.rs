//! GDP packet construction utilities.

use crate::structs::{GDPName, GDPPacket, GdpAction};

/// Wrap raw bytes (e.g., serialized ROS message) into a GDP Forward packet.
pub fn construct_gdp_forward_from_bytes(
    destination: GDPName, source: GDPName, buffer: Vec<u8>,
) -> GDPPacket {
    GDPPacket {
        action: GdpAction::Forward,
        gdpname: destination,
        source: source,
        payload: Some(buffer),
        name_record: None,
    }
}
