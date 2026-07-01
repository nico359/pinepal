// SPDX-License-Identifier: GPL-3.0-or-later
// GFDI message format for Garmin watches.
// Implements the binary envelope (size + message_id + payload + CRC-16)
// and notification-specific message builders.
// Ported from watchd / GadgetBridge Garmin protocol.

use crate::cobs::CobsCodec;

// ---- Message IDs ----

pub const MSG_RESPONSE: u16 = 5000;
pub const MSG_DEVICE_INFORMATION: u16 = 5024;
pub const MSG_CONFIGURATION: u16 = 5050;
pub const MSG_CURRENT_TIME_REQUEST: u16 = 5052;
pub const MSG_NOTIFICATION_UPDATE: u16 = 5033;
pub const MSG_NOTIFICATION_CONTROL: u16 = 5034;
pub const MSG_NOTIFICATION_SUBSCRIPTION: u16 = 5036;
pub const MSG_SYSTEM_EVENT: u16 = 5030;
pub const MSG_FIND_MY_PHONE_REQUEST: u16 = 5039;
pub const MSG_SYNCHRONIZATION: u16 = 5037;
pub const MSG_AUTH_NEGOTIATION: u16 = 5101;

// ---- Status codes ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Ack = 0,
    Nack = 1,
    Unsupported = 2,
    DecodeError = 3,
    CrcError = 4,
    LengthError = 5,
}

impl Status {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Status::Ack),
            1 => Some(Status::Nack),
            2 => Some(Status::Unsupported),
            3 => Some(Status::DecodeError),
            4 => Some(Status::CrcError),
            5 => Some(Status::LengthError),
            _ => None,
        }
    }
}

// ---- Notification types ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum NotificationType {
    GenericPhone = 1,
    GenericSms = 2,
    GenericEmail = 3,
    GenericChat = 4,
    GenericSocial = 5,
    GenericNavigation = 6,
    GenericCalendar = 7,
    GenericAlarmClock = 8,
    Generic = 9,
}

impl NotificationType {
    fn category_value(self) -> u8 {
        match self {
            Self::GenericPhone => 1,
            Self::GenericEmail => 6,
            Self::GenericSms | Self::GenericChat => 12,
            Self::GenericNavigation => 10,
            Self::GenericSocial => 4,
            Self::GenericCalendar => 5,
            Self::GenericAlarmClock | Self::Generic => 0,
        }
    }

    fn flags(self) -> u8 {
        0x02 | 0x10 // foreground + ACTION_DECLINE (makes notifications dismissible)
    }
}

// ---- Notification update type ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NotificationUpdateKind {
    Add = 0,
    Modify = 1,
    Remove = 2,
}

// ---- CRC-16 (GadgetBridge/Garmin variant) ----

fn crc16(data: &[u8]) -> u16 {
    const TABLE: [u16; 16] = [
        0x0000, 0xCC01, 0xD801, 0x1400, 0xF001, 0x3C00, 0x2800, 0xE401,
        0xA001, 0x6C00, 0x7800, 0xB401, 0x5000, 0x9C01, 0x8801, 0x4400,
    ];
    let mut crc: u16 = 0;
    for &byte in data {
        let b = byte as u16;
        crc = (((crc >> 4) & 0x0FFF) ^ TABLE[(crc & 0x0F) as usize])
            ^ TABLE[(b & 0x0F) as usize];
        crc = (((crc >> 4) & 0x0FFF) ^ TABLE[(crc & 0x0F) as usize])
            ^ TABLE[((b >> 4) & 0x0F) as usize];
    }
    crc
}

// ---- GFDI Envelope ----

/// Wrap a payload in a GFDI envelope: [size:2][msg_id:2][payload][crc:2]
pub fn wrap_envelope(message_id: u16, payload: &[u8]) -> Vec<u8> {
    let packet_size = (2 + 2 + payload.len() + 2) as u16;
    let mut buf = Vec::with_capacity(packet_size as usize);
    buf.extend_from_slice(&packet_size.to_le_bytes());
    buf.extend_from_slice(&message_id.to_le_bytes());
    buf.extend_from_slice(payload);
    let crc = crc16(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    buf
}

/// Build a Response (5000) message: [orig_msg_id:2][status:1]
pub fn build_response(original_msg_id: u16, status: Status) -> Vec<u8> {
    let payload = {
        let mut p = Vec::with_capacity(3);
        p.extend_from_slice(&original_msg_id.to_le_bytes());
        p.push(status as u8);
        p
    };
    wrap_envelope(MSG_RESPONSE, &payload)
}

// ---- Notification messages ----

/// Build a NotificationUpdate (5033) message payload.
/// Format: [update_type:1][flags:1][category:1][count:1][notification_id:4][phone_flags:1]
pub fn build_notification_update(
    kind: NotificationUpdateKind,
    notif_type: NotificationType,
    notification_id: i32,
    count: u8,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(9);
    payload.push(kind as u8);
    payload.push(notif_type.flags());
    payload.push(notif_type.category_value());
    payload.push(count);
    payload.extend_from_slice(&notification_id.to_le_bytes());
    let phone_flags = 0x02u8; // NEW_ACTIONS
    payload.push(phone_flags);
    wrap_envelope(MSG_NOTIFICATION_UPDATE, &payload)
}

/// COBS-encode a GFDI message for BLE transmission.
pub fn encode_for_ble(message: &[u8]) -> Vec<u8> {
    CobsCodec::encode(message)
}

/// Decode incoming BLE data through COBS and parse the GFDI envelope.
/// Returns Some((message_id, payload)) if a complete message was decoded.
pub fn decode_from_ble(
    codec: &mut CobsCodec,
    raw: &[u8],
) -> Vec<(u16, Vec<u8>)> {
    codec.receive_bytes(raw);
    let mut messages = Vec::new();
    while let Some(decoded) = codec.take_message() {
        if decoded.len() >= 4 {
            let msg_id = u16::from_le_bytes([decoded[2], decoded[3]]);
            // If bit 15 is set, decode sequence-numbered message ID
            let msg_id = if (msg_id & 0x8000) != 0 {
                (msg_id & 0xFF) + 5000
            } else {
                msg_id
            };
            messages.push((msg_id, decoded));
        }
    }
    messages
}

// ---- Device information parsing ----

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub protocol_version: u16,
    pub product_number: u16,
    pub software_version: u16,
    pub max_packet_size: u16,
    pub bluetooth_friendly_name: String,
    pub device_name: String,
}

pub fn parse_device_info(decoded: &[u8]) -> Option<DeviceInfo> {
    if decoded.len() < 16 {
        return None;
    }
    // Skip size(2) + msg_id(2) = 4 bytes
    let data = &decoded[4..];
    let protocol_version = u16::from_le_bytes([data[0], data[1]]);
    let product_number = u16::from_le_bytes([data[2], data[3]]);
    let _unit_number = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let software_version = u16::from_le_bytes([data[8], data[9]]);
    let max_packet_size = u16::from_le_bytes([data[10], data[11]]);
    let friendly_len = data[12] as usize;
    let ofs = 13;
    let bluetooth_friendly_name = if ofs + friendly_len <= data.len() {
        String::from_utf8_lossy(&data[ofs..ofs + friendly_len]).into_owned()
    } else {
        String::new()
    };
    let device_len_offset = ofs + friendly_len;
    let device_len = data.get(device_len_offset).copied().unwrap_or(0) as usize;
    let dev_ofs = device_len_offset + 1;
    let device_name = if dev_ofs + device_len <= data.len() {
        String::from_utf8_lossy(&data[dev_ofs..dev_ofs + device_len]).into_owned()
    } else {
        String::new()
    };
    Some(DeviceInfo {
        protocol_version,
        product_number,
        software_version,
        max_packet_size,
        bluetooth_friendly_name,
        device_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_envelope() {
        let msg = wrap_envelope(MSG_RESPONSE, &[0xAA, 0x13, 0x00]);
        // packet_size = 2(size) + 2(msg_id) + 3(payload) + 2(crc) = 9
        assert_eq!(u16::from_le_bytes([msg[0], msg[1]]), 9);
        assert_eq!(u16::from_le_bytes([msg[2], msg[3]]), MSG_RESPONSE);
        assert_eq!(&msg[4..7], &[0xAA, 0x13, 0x00]);
        assert_eq!(msg.len(), 9);
    }

    #[test]
    fn test_crc_consistency() {
        let msg1 = wrap_envelope(MSG_RESPONSE, &[0x00]);
        let msg2 = wrap_envelope(MSG_RESPONSE, &[0x00]);
        assert_eq!(&msg1[msg1.len() - 2..], &msg2[msg2.len() - 2..]);
    }

    #[test]
    fn test_roundtrip_ble() {
        let msg = build_notification_update(
            NotificationUpdateKind::Add,
            NotificationType::Generic,
            42,
            1,
        );
        let encoded = encode_for_ble(&msg);
        let mut codec = CobsCodec::new();
        let decoded = decode_from_ble(&mut codec, &encoded);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].0, MSG_NOTIFICATION_UPDATE);
        assert_eq!(decoded[0].1, msg);
    }

    #[test]
    fn test_notification_update_payload() {
        let msg = build_notification_update(
            NotificationUpdateKind::Add,
            NotificationType::GenericEmail,
            100,
            3,
        );
        let packet_size = u16::from_le_bytes([msg[0], msg[1]]);
        assert_eq!(msg.len() as u16, packet_size);
        // Check payload bytes (after size+msg_id = 4 bytes)
        let payload = &msg[4..msg.len() - 2];
        assert_eq!(payload[0], NotificationUpdateKind::Add as u8); // update type
        assert_eq!(payload[3], 3); // count
        let id = i32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
        assert_eq!(id, 100);
    }

    #[test]
    fn test_build_response() {
        let resp = build_response(MSG_NOTIFICATION_CONTROL, Status::Ack);
        // size should be 2+2+3+2 = 9
        assert_eq!(u16::from_le_bytes([resp[0], resp[1]]), 9);
        // original msg id in payload
        assert_eq!(
            u16::from_le_bytes([resp[4], resp[5]]),
            MSG_NOTIFICATION_CONTROL
        );
        assert_eq!(resp[6], Status::Ack as u8);
    }
}
