// SPDX-License-Identifier: GPL-3.0-or-later
// COBS (Consistent Overhead Byte Stuffing) codec for Garmin watches.
// Garmin variant includes leading and trailing 0x00 framing bytes.
// Ported from watchd (https://github.com/alistair23/watchd).

use std::time::{Duration, Instant};

const BUFFER_TIMEOUT_MS: u64 = 1500;

pub struct CobsCodec {
    buffer: Vec<u8>,
    last_update: Option<Instant>,
    decoded_message: Option<Vec<u8>>,
    buffer_timeout: Duration,
}

impl Default for CobsCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl CobsCodec {
    pub fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(10_000),
            last_update: None,
            decoded_message: None,
            buffer_timeout: Duration::from_millis(BUFFER_TIMEOUT_MS),
        }
    }

    /// Accumulate received bytes and attempt to decode a COBS frame.
    /// The buffer auto-clears after a timeout of inactivity.
    pub fn receive_bytes(&mut self, bytes: &[u8]) {
        let now = Instant::now();
        if let Some(last) = self.last_update {
            if now.duration_since(last) > self.buffer_timeout {
                self.reset();
            }
        }
        self.last_update = Some(now);
        self.buffer.extend_from_slice(bytes);
        let _ = self.decode();
    }

    pub fn reset(&mut self) {
        self.buffer.clear();
        self.decoded_message = None;
    }

    /// Consume and return a decoded message if one is ready.
    pub fn take_message(&mut self) -> Option<Vec<u8>> {
        self.decoded_message.take()
    }

    pub fn has_message(&self) -> bool {
        self.decoded_message.is_some()
    }

    fn decode(&mut self) -> Result<(), ()> {
        if self.decoded_message.is_some() || self.buffer.len() < 4 {
            return Ok(());
        }
        // Wait for trailing 0x00
        if self.buffer.last() != Some(&0) {
            return Ok(());
        }
        let buf_end = self.buffer.len() - 1;
        if self.buffer[0] != 0 {
            return Ok(());
        }

        let mut decoded = Vec::with_capacity(buf_end);
        let mut pos = 1;
        while pos < buf_end {
            let code = self.buffer[pos];
            if code == 0 {
                break;
            }
            pos += 1;
            let code_val = code as usize;
            let payload_size = code_val.saturating_sub(1);

            for _ in 0..payload_size {
                if pos >= buf_end {
                    return Ok(());
                }
                decoded.push(self.buffer[pos]);
                pos += 1;
            }
            if code_val != 0xFF {
                if payload_size == 0 || pos < buf_end {
                    decoded.push(0);
                }
            }
        }

        self.decoded_message = Some(decoded);
        self.buffer.drain(..=buf_end);
        Ok(())
    }

    /// Encode data using Garmin-variant COBS (leading + trailing 0x00).
    pub fn encode(data: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(data.len() * 2 + 2);
        encoded.push(0);

        if data.is_empty() {
            encoded.push(0x01);
            encoded.push(0);
            return encoded;
        }

        let mut pos = 0;
        let mut last_was_zero = false;
        let mut last_payload_size = 0;

        while pos < data.len() {
            let start = pos;
            let mut zero_pos = pos;
            while zero_pos < data.len() && data[zero_pos] != 0 {
                zero_pos += 1;
            }
            let mut payload_size = zero_pos - start;
            let mut cur = start;

            while payload_size >= 0xFE {
                encoded.push(0xFF);
                encoded.extend_from_slice(&data[cur..cur + 0xFE]);
                payload_size -= 0xFE;
                cur += 0xFE;
            }

            encoded.push((payload_size + 1) as u8);
            if payload_size > 0 {
                encoded.extend_from_slice(&data[cur..cur + payload_size]);
            }
            last_payload_size = payload_size;

            if zero_pos < data.len() {
                pos = zero_pos + 1;
                last_was_zero = true;
            } else {
                pos = zero_pos;
                last_was_zero = false;
            }
        }

        if last_was_zero && pos >= data.len() && last_payload_size > 0 {
            encoded.push(1);
        }
        encoded.push(0);
        encoded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_simple() {
        let data = vec![0x01, 0x02, 0x03];
        let enc = CobsCodec::encode(&data);
        let mut codec = CobsCodec::new();
        codec.receive_bytes(&enc);
        assert_eq!(codec.take_message(), Some(data));
    }

    #[test]
    fn roundtrip_with_zeros() {
        let data = vec![0x01, 0x00, 0x02, 0x00, 0x03];
        let enc = CobsCodec::encode(&data);
        let mut codec = CobsCodec::new();
        codec.receive_bytes(&enc);
        assert_eq!(codec.take_message(), Some(data));
    }

    #[test]
    fn decode_partial_then_complete() {
        let data = vec![0xAA; 20];
        let enc = CobsCodec::encode(&data);
        let mid = enc.len() / 2;
        let mut codec = CobsCodec::new();
        codec.receive_bytes(&enc[..mid]);
        assert!(!codec.has_message());
        codec.receive_bytes(&enc[mid..]);
        assert_eq!(codec.take_message(), Some(data));
    }
}
