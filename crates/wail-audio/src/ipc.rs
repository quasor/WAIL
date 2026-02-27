/// Length-prefixed IPC framing for Plugin ↔ App communication.
///
/// Frame format over Unix socket / TCP:
/// ```text
/// [4 bytes] payload_length: u32 LE
/// [N bytes] payload (AudioWire binary data)
/// ```
///
/// Simple, zero-copy-friendly framing. The payload is always an AudioWire
/// message (which starts with "WAIL" magic and is self-describing).
///
/// The framer operates on byte buffers, not sockets directly, so it's
/// testable without I/O.
pub struct IpcFramer;

impl IpcFramer {
    /// Wrap a payload in a length-prefixed frame.
    pub fn encode_frame(payload: &[u8]) -> Vec<u8> {
        let len = payload.len() as u32;
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    /// Try to extract a complete frame from a receive buffer.
    ///
    /// Returns `Some((payload, consumed))` if a complete frame is available,
    /// where `consumed` is the total bytes used (4-byte header + payload).
    /// Returns `None` if more data is needed.
    pub fn decode_frame(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
        if buf.len() < 4 {
            return None;
        }
        let payload_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        let total = 4 + payload_len;
        if buf.len() < total {
            return None;
        }
        let payload = buf[4..total].to_vec();
        Some((payload, total))
    }
}

/// Accumulating receive buffer that handles partial reads.
///
/// Data arrives in arbitrary chunks from the socket. This buffer
/// accumulates bytes and yields complete frames when available.
pub struct IpcRecvBuffer {
    buf: Vec<u8>,
}

impl Default for IpcRecvBuffer {
    fn default() -> Self {
        Self {
            buf: Vec::with_capacity(64 * 1024),
        }
    }
}

impl IpcRecvBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append received bytes from a socket read.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Try to extract the next complete frame.
    /// Returns `None` if more data is needed.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        let (payload, consumed) = IpcFramer::decode_frame(&self.buf)?;
        self.buf.drain(..consumed);
        Some(payload)
    }

    /// Number of buffered bytes not yet consumed.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- IpcFramer ---

    #[test]
    fn encode_frame_prepends_length() {
        let payload = vec![1u8, 2, 3, 4, 5];
        let frame = IpcFramer::encode_frame(&payload);
        assert_eq!(frame.len(), 9);
        assert_eq!(&frame[0..4], &5u32.to_le_bytes());
        assert_eq!(&frame[4..], &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn encode_empty_payload() {
        let frame = IpcFramer::encode_frame(&[]);
        assert_eq!(frame.len(), 4);
        assert_eq!(&frame[0..4], &0u32.to_le_bytes());
    }

    #[test]
    fn decode_frame_from_complete_buffer() {
        let payload = vec![10u8, 20, 30];
        let frame = IpcFramer::encode_frame(&payload);
        let (decoded, consumed) = IpcFramer::decode_frame(&frame).unwrap();
        assert_eq!(decoded, vec![10, 20, 30]);
        assert_eq!(consumed, 7);
    }

    #[test]
    fn decode_returns_none_for_incomplete_header() {
        assert!(IpcFramer::decode_frame(&[0, 0]).is_none());
    }

    #[test]
    fn decode_returns_none_for_incomplete_payload() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&10u32.to_le_bytes());
        buf.extend_from_slice(&[1, 2, 3]);
        assert!(IpcFramer::decode_frame(&buf).is_none());
    }

    #[test]
    fn decode_frame_with_trailing_data() {
        let frame1 = IpcFramer::encode_frame(&[0xAA, 0xBB]);
        let frame2 = IpcFramer::encode_frame(&[0xCC]);
        let mut buf = frame1.clone();
        buf.extend_from_slice(&frame2);

        let (payload, consumed) = IpcFramer::decode_frame(&buf).unwrap();
        assert_eq!(payload, vec![0xAA, 0xBB]);
        assert_eq!(consumed, 6);
        assert_eq!(buf.len() - consumed, 5);
    }

    // --- IpcRecvBuffer ---

    #[test]
    fn recv_buffer_empty_on_create() {
        let buf = IpcRecvBuffer::new();
        assert_eq!(buf.buffered(), 0);
    }

    #[test]
    fn recv_buffer_yields_complete_frame() {
        let mut buf = IpcRecvBuffer::new();
        let frame = IpcFramer::encode_frame(&[1, 2, 3]);
        buf.push(&frame);

        let payload = buf.next_frame().unwrap();
        assert_eq!(payload, vec![1, 2, 3]);
        assert_eq!(buf.buffered(), 0);
    }

    #[test]
    fn recv_buffer_handles_partial_delivery() {
        let mut buf = IpcRecvBuffer::new();
        let frame = IpcFramer::encode_frame(&[10, 20, 30, 40, 50]);

        buf.push(&frame[..3]);
        assert!(buf.next_frame().is_none());
        assert_eq!(buf.buffered(), 3);

        buf.push(&frame[3..]);
        let payload = buf.next_frame().unwrap();
        assert_eq!(payload, vec![10, 20, 30, 40, 50]);
        assert_eq!(buf.buffered(), 0);
    }

    #[test]
    fn recv_buffer_handles_multiple_frames_in_one_push() {
        let mut buf = IpcRecvBuffer::new();
        let frame1 = IpcFramer::encode_frame(&[0xAA]);
        let frame2 = IpcFramer::encode_frame(&[0xBB, 0xCC]);

        let mut combined = frame1;
        combined.extend_from_slice(&frame2);
        buf.push(&combined);

        let p1 = buf.next_frame().unwrap();
        assert_eq!(p1, vec![0xAA]);

        let p2 = buf.next_frame().unwrap();
        assert_eq!(p2, vec![0xBB, 0xCC]);

        assert!(buf.next_frame().is_none());
        assert_eq!(buf.buffered(), 0);
    }

    #[test]
    fn recv_buffer_handles_byte_at_a_time() {
        let mut buf = IpcRecvBuffer::new();
        let frame = IpcFramer::encode_frame(&[42]);

        for (i, &byte) in frame.iter().enumerate() {
            buf.push(&[byte]);
            if i < frame.len() - 1 {
                assert!(buf.next_frame().is_none(), "Should not yield frame at byte {i}");
            }
        }

        let payload = buf.next_frame().unwrap();
        assert_eq!(payload, vec![42]);
    }

    // --- Integration with AudioWire ---

    #[test]
    fn roundtrip_audio_wire_through_ipc_framing() {
        use crate::{AudioInterval, AudioWire};

        let interval = AudioInterval {
            index: 7,
            opus_data: vec![0xDE, 0xAD, 0xBE, 0xEF],
            sample_rate: 48000,
            channels: 2,
            num_frames: 960,
            bpm: 140.0,
            quantum: 4.0,
            bars: 4,
        };

        let wire_bytes = AudioWire::encode(&interval);
        let ipc_frame = IpcFramer::encode_frame(&wire_bytes);

        let mut recv = IpcRecvBuffer::new();
        recv.push(&ipc_frame);

        let received_wire = recv.next_frame().unwrap();
        let decoded = AudioWire::decode(&received_wire).unwrap();

        assert_eq!(decoded.index, 7);
        assert_eq!(decoded.opus_data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(decoded.sample_rate, 48000);
        assert_eq!(decoded.channels, 2);
        assert!((decoded.bpm - 140.0).abs() < f64::EPSILON);
    }
}
