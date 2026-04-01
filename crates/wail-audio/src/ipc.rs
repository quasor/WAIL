/// IPC role byte sent by plugins on connect to identify themselves.
pub const IPC_ROLE_SEND: u8 = 0x00;
/// IPC role byte sent by plugins on connect to identify themselves.
pub const IPC_ROLE_RECV: u8 = 0x01;

/// Length-prefixed IPC framing for Plugin ↔ App communication.
///
/// Frame format over TCP:
/// ```text
/// [4 bytes] payload_length: u32 LE
/// [N bytes] payload (IpcMessage-encoded data)
/// ```
///
/// Message format inside each frame:
/// ```text
/// [1 byte]  tag (0x01 = AudioInterval)
/// [1 byte]  peer_id_len
/// [N bytes] peer_id (UTF-8, empty for outgoing from plugin)
/// [M bytes] AudioWire data
/// ```
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

const IPC_TAG_AUDIO: u8 = 0x01;
const IPC_TAG_PEER_JOINED: u8 = 0x02;
const IPC_TAG_PEER_LEFT: u8 = 0x03;
const IPC_TAG_PEER_NAME: u8 = 0x04;
const IPC_TAG_AUDIO_FRAME: u8 = 0x05;
const IPC_TAG_METRICS: u8 = 0x06;

/// IPC message encoding for Plugin ↔ App communication.
///
/// Each IPC frame payload contains a tagged message:
/// - `0x01` AudioInterval: peer_id + AudioWire data
/// - `0x02` PeerJoined: peer_id + identity (for slot affinity)
/// - `0x03` PeerLeft: peer_id
/// - `0x04` PeerName: peer_id + display_name
pub struct IpcMessage;

impl IpcMessage {
    /// Encode an audio interval message.
    pub fn encode_audio(peer_id: &str, wire_data: &[u8]) -> Vec<u8> {
        let pid_bytes = peer_id.as_bytes();
        let pid_len = pid_bytes.len().min(255) as u8;
        let mut msg = Vec::with_capacity(2 + pid_len as usize + wire_data.len());
        msg.push(IPC_TAG_AUDIO);
        msg.push(pid_len);
        msg.extend_from_slice(&pid_bytes[..pid_len as usize]);
        msg.extend_from_slice(wire_data);
        msg
    }

    /// Decode an audio interval message. Returns `(peer_id, wire_data)`.
    pub fn decode_audio(payload: &[u8]) -> Option<(String, Vec<u8>)> {
        if payload.len() < 2 {
            return None;
        }
        if payload[0] != IPC_TAG_AUDIO {
            return None;
        }
        let pid_len = payload[1] as usize;
        if payload.len() < 2 + pid_len {
            return None;
        }
        let peer_id = String::from_utf8_lossy(&payload[2..2 + pid_len]).to_string();
        let wire_data = payload[2 + pid_len..].to_vec();
        Some((peer_id, wire_data))
    }

    /// Encode a PeerJoined message: tag + peer_id_len + peer_id + identity_len + identity.
    pub fn encode_peer_joined(peer_id: &str, identity: &str) -> Vec<u8> {
        let pid = peer_id.as_bytes();
        let pid_len = pid.len().min(255) as u8;
        let ident = identity.as_bytes();
        let ident_len = ident.len().min(255) as u8;
        let mut msg = Vec::with_capacity(3 + pid_len as usize + ident_len as usize);
        msg.push(IPC_TAG_PEER_JOINED);
        msg.push(pid_len);
        msg.extend_from_slice(&pid[..pid_len as usize]);
        msg.push(ident_len);
        msg.extend_from_slice(&ident[..ident_len as usize]);
        msg
    }

    /// Decode a PeerJoined message. Returns `(peer_id, identity)`.
    pub fn decode_peer_joined(payload: &[u8]) -> Option<(String, String)> {
        if payload.len() < 2 || payload[0] != IPC_TAG_PEER_JOINED {
            return None;
        }
        let pid_len = payload[1] as usize;
        if payload.len() < 2 + pid_len + 1 {
            return None;
        }
        let peer_id = String::from_utf8_lossy(&payload[2..2 + pid_len]).to_string();
        let ident_start = 2 + pid_len;
        let ident_len = payload[ident_start] as usize;
        if payload.len() < ident_start + 1 + ident_len {
            return None;
        }
        let identity = String::from_utf8_lossy(&payload[ident_start + 1..ident_start + 1 + ident_len]).to_string();
        Some((peer_id, identity))
    }

    /// Encode a PeerLeft message: tag + peer_id_len + peer_id.
    pub fn encode_peer_left(peer_id: &str) -> Vec<u8> {
        let pid = peer_id.as_bytes();
        let pid_len = pid.len().min(255) as u8;
        let mut msg = Vec::with_capacity(2 + pid_len as usize);
        msg.push(IPC_TAG_PEER_LEFT);
        msg.push(pid_len);
        msg.extend_from_slice(&pid[..pid_len as usize]);
        msg
    }

    /// Decode a PeerLeft message. Returns `peer_id`.
    pub fn decode_peer_left(payload: &[u8]) -> Option<String> {
        if payload.len() < 2 || payload[0] != IPC_TAG_PEER_LEFT {
            return None;
        }
        let pid_len = payload[1] as usize;
        if payload.len() < 2 + pid_len {
            return None;
        }
        Some(String::from_utf8_lossy(&payload[2..2 + pid_len]).to_string())
    }

    /// Encode a PeerName message: tag + peer_id_len + peer_id + name_len + display_name.
    pub fn encode_peer_name(peer_id: &str, display_name: &str) -> Vec<u8> {
        let pid = peer_id.as_bytes();
        let pid_len = pid.len().min(255) as u8;
        let name = display_name.as_bytes();
        let name_len = name.len().min(255) as u8;
        let mut msg = Vec::with_capacity(3 + pid_len as usize + name_len as usize);
        msg.push(IPC_TAG_PEER_NAME);
        msg.push(pid_len);
        msg.extend_from_slice(&pid[..pid_len as usize]);
        msg.push(name_len);
        msg.extend_from_slice(&name[..name_len as usize]);
        msg
    }

    /// Decode a PeerName message. Returns `(peer_id, display_name)`.
    pub fn decode_peer_name(payload: &[u8]) -> Option<(String, String)> {
        if payload.len() < 2 || payload[0] != IPC_TAG_PEER_NAME {
            return None;
        }
        let pid_len = payload[1] as usize;
        if payload.len() < 2 + pid_len + 1 {
            return None;
        }
        let peer_id = String::from_utf8_lossy(&payload[2..2 + pid_len]).to_string();
        let name_start = 2 + pid_len;
        let name_len = payload[name_start] as usize;
        if payload.len() < name_start + 1 + name_len {
            return None;
        }
        let display_name = String::from_utf8_lossy(&payload[name_start + 1..name_start + 1 + name_len]).to_string();
        Some((peer_id, display_name))
    }

    /// Encode a streaming audio frame message (no peer_id — send plugin only).
    pub fn encode_audio_frame(wire_data: &[u8]) -> Vec<u8> {
        let mut msg = Vec::with_capacity(1 + wire_data.len());
        msg.push(IPC_TAG_AUDIO_FRAME);
        msg.extend_from_slice(wire_data);
        msg
    }

    /// Decode a streaming audio frame message. Returns the AudioFrameWire bytes.
    pub fn decode_audio_frame(payload: &[u8]) -> Option<Vec<u8>> {
        if payload.len() < 2 {
            return None;
        }
        if payload[0] != IPC_TAG_AUDIO_FRAME {
            return None;
        }
        Some(payload[1..].to_vec())
    }

    /// Encode a plugin metrics report: tag + decode_failures(u64 LE).
    pub fn encode_metrics(decode_failures: u64) -> Vec<u8> {
        let mut msg = Vec::with_capacity(9);
        msg.push(IPC_TAG_METRICS);
        msg.extend_from_slice(&decode_failures.to_le_bytes());
        msg
    }

    /// Decode a plugin metrics report. Returns `decode_failures` count.
    pub fn decode_metrics(payload: &[u8]) -> Option<u64> {
        if payload.len() < 9 || payload[0] != IPC_TAG_METRICS {
            return None;
        }
        Some(u64::from_le_bytes(payload[1..9].try_into().ok()?))
    }

    /// Get the tag byte from a payload, if any.
    pub fn tag(payload: &[u8]) -> Option<u8> {
        payload.first().copied()
    }
}

/// IPC message tag constants for matching in recv plugin.
pub const IPC_TAG_AUDIO_PUB: u8 = IPC_TAG_AUDIO;
pub const IPC_TAG_PEER_JOINED_PUB: u8 = IPC_TAG_PEER_JOINED;
pub const IPC_TAG_PEER_LEFT_PUB: u8 = IPC_TAG_PEER_LEFT;
pub const IPC_TAG_PEER_NAME_PUB: u8 = IPC_TAG_PEER_NAME;
pub const IPC_TAG_AUDIO_FRAME_PUB: u8 = IPC_TAG_AUDIO_FRAME;
pub const IPC_TAG_METRICS_PUB: u8 = IPC_TAG_METRICS;

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

    // --- IpcMessage ---

    #[test]
    fn ipc_message_roundtrip_with_peer_id() {
        let wire_data = vec![0x01, 0x02, 0x03, 0x04];
        let encoded = IpcMessage::encode_audio("peer-abc", &wire_data);
        let (peer_id, decoded_wire) = IpcMessage::decode_audio(&encoded).unwrap();
        assert_eq!(peer_id, "peer-abc");
        assert_eq!(decoded_wire, wire_data);
    }

    #[test]
    fn ipc_message_roundtrip_empty_peer_id() {
        let wire_data = vec![0xAA, 0xBB];
        let encoded = IpcMessage::encode_audio("", &wire_data);
        let (peer_id, decoded_wire) = IpcMessage::decode_audio(&encoded).unwrap();
        assert_eq!(peer_id, "");
        assert_eq!(decoded_wire, wire_data);
    }

    #[test]
    fn ipc_message_decode_rejects_short() {
        assert!(IpcMessage::decode_audio(&[]).is_none());
        assert!(IpcMessage::decode_audio(&[0x01]).is_none());
    }

    #[test]
    fn ipc_message_decode_rejects_wrong_tag() {
        assert!(IpcMessage::decode_audio(&[0xFF, 0x00]).is_none());
    }

    #[test]
    fn ipc_message_through_framing() {
        let wire_data = vec![0xDE, 0xAD];
        let msg = IpcMessage::encode_audio("peer-1", &wire_data);
        let frame = IpcFramer::encode_frame(&msg);

        let mut recv = IpcRecvBuffer::new();
        recv.push(&frame);
        let payload = recv.next_frame().unwrap();

        let (peer_id, decoded) = IpcMessage::decode_audio(&payload).unwrap();
        assert_eq!(peer_id, "peer-1");
        assert_eq!(decoded, vec![0xDE, 0xAD]);
    }

    // --- IPC Role ---

    #[test]
    fn ipc_role_byte_constants_exist() {
        assert_eq!(IPC_ROLE_SEND, 0x00);
        assert_eq!(IPC_ROLE_RECV, 0x01);
    }

    // --- Peer lifecycle messages ---

    #[test]
    fn peer_joined_roundtrip() {
        let encoded = IpcMessage::encode_peer_joined("peer-abc", "uuid-1234");
        let (peer_id, identity) = IpcMessage::decode_peer_joined(&encoded).unwrap();
        assert_eq!(peer_id, "peer-abc");
        assert_eq!(identity, "uuid-1234");
    }

    #[test]
    fn peer_joined_empty_identity() {
        let encoded = IpcMessage::encode_peer_joined("peer-abc", "");
        let (peer_id, identity) = IpcMessage::decode_peer_joined(&encoded).unwrap();
        assert_eq!(peer_id, "peer-abc");
        assert_eq!(identity, "");
    }

    #[test]
    fn peer_joined_rejects_wrong_tag() {
        let encoded = IpcMessage::encode_audio("peer-1", &[0xAA]);
        assert!(IpcMessage::decode_peer_joined(&encoded).is_none());
    }

    #[test]
    fn peer_left_roundtrip() {
        let encoded = IpcMessage::encode_peer_left("peer-xyz");
        let peer_id = IpcMessage::decode_peer_left(&encoded).unwrap();
        assert_eq!(peer_id, "peer-xyz");
    }

    #[test]
    fn peer_left_rejects_wrong_tag() {
        let encoded = IpcMessage::encode_audio("peer-1", &[0xBB]);
        assert!(IpcMessage::decode_peer_left(&encoded).is_none());
    }

    #[test]
    fn tag_extraction() {
        assert_eq!(IpcMessage::tag(&[IPC_TAG_AUDIO, 0x00]), Some(IPC_TAG_AUDIO));
        assert_eq!(IpcMessage::tag(&[IPC_TAG_PEER_JOINED, 0x05]), Some(IPC_TAG_PEER_JOINED));
        assert_eq!(IpcMessage::tag(&[IPC_TAG_PEER_LEFT, 0x03]), Some(IPC_TAG_PEER_LEFT));
        assert_eq!(IpcMessage::tag(&[IPC_TAG_PEER_NAME, 0x02]), Some(IPC_TAG_PEER_NAME));
        assert_eq!(IpcMessage::tag(&[]), None);
    }

    // --- PeerName messages ---

    #[test]
    fn peer_name_roundtrip() {
        let encoded = IpcMessage::encode_peer_name("peer-abc", "Ringo");
        let (peer_id, name) = IpcMessage::decode_peer_name(&encoded).unwrap();
        assert_eq!(peer_id, "peer-abc");
        assert_eq!(name, "Ringo");
    }

    #[test]
    fn peer_name_empty_display_name() {
        let encoded = IpcMessage::encode_peer_name("peer-abc", "");
        let (peer_id, name) = IpcMessage::decode_peer_name(&encoded).unwrap();
        assert_eq!(peer_id, "peer-abc");
        assert_eq!(name, "");
    }

    #[test]
    fn peer_name_rejects_wrong_tag() {
        let encoded = IpcMessage::encode_audio("peer-1", &[0xAA]);
        assert!(IpcMessage::decode_peer_name(&encoded).is_none());
    }

    #[test]
    fn peer_name_rejects_truncated() {
        assert!(IpcMessage::decode_peer_name(&[]).is_none());
        assert!(IpcMessage::decode_peer_name(&[IPC_TAG_PEER_NAME]).is_none());
    }

    // --- Audio frame messages ---

    #[test]
    fn audio_frame_roundtrip() {
        let wire_data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let encoded = IpcMessage::encode_audio_frame(&wire_data);
        assert_eq!(encoded[0], IPC_TAG_AUDIO_FRAME);

        let decoded = IpcMessage::decode_audio_frame(&encoded).unwrap();
        assert_eq!(decoded, wire_data);
    }

    #[test]
    fn audio_frame_rejects_wrong_tag() {
        let encoded = IpcMessage::encode_audio("peer-1", &[0xAA]);
        assert!(IpcMessage::decode_audio_frame(&encoded).is_none());
    }

    #[test]
    fn audio_frame_rejects_short() {
        assert!(IpcMessage::decode_audio_frame(&[]).is_none());
        assert!(IpcMessage::decode_audio_frame(&[IPC_TAG_AUDIO_FRAME]).is_none());
    }

    #[test]
    fn audio_frame_tag_constant() {
        assert_eq!(IPC_TAG_AUDIO_FRAME_PUB, 0x05);
    }

    // --- Metrics messages ---

    #[test]
    fn metrics_roundtrip() {
        let encoded = IpcMessage::encode_metrics(42);
        assert_eq!(encoded[0], IPC_TAG_METRICS);
        assert_eq!(encoded.len(), 9);
        let decoded = IpcMessage::decode_metrics(&encoded).unwrap();
        assert_eq!(decoded, 42);
    }

    #[test]
    fn metrics_rejects_wrong_tag() {
        let encoded = IpcMessage::encode_audio("peer-1", &[0xAA]);
        assert!(IpcMessage::decode_metrics(&encoded).is_none());
    }

    #[test]
    fn metrics_rejects_short() {
        assert!(IpcMessage::decode_metrics(&[]).is_none());
        assert!(IpcMessage::decode_metrics(&[IPC_TAG_METRICS]).is_none());
    }

    #[test]
    fn metrics_tag_constant() {
        assert_eq!(IPC_TAG_METRICS_PUB, 0x06);
    }
}
