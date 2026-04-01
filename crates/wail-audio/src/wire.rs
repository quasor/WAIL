use anyhow::Result;

/// Binary wire format for audio intervals over WebRTC DataChannels.
///
/// Format (all integers are little-endian):
/// ```text
/// [4 bytes] magic: "WAIL"
/// [1 byte]  version: 2 (v1 accepted for backward compat)
/// [1 byte]  flags: bit 0 = stereo (0=mono, 1=stereo)
/// [2 bytes] stream_id: u16 LE (v1: was reserved/zero)
/// [8 bytes] interval_index: i64
/// [4 bytes] sample_rate: u32
/// [4 bytes] num_frames: u32 (source samples per channel)
/// [8 bytes] bpm: f64
/// [8 bytes] quantum: f64
/// [4 bytes] bars: u32
/// [4 bytes] opus_data_len: u32
/// [N bytes] opus_data
/// ```
///
const FRAME_MAGIC: &[u8; 4] = b"WAIF";
const FRAME_HEADER_SIZE: usize = 21; // 4+1+2+8+4+2
const FRAME_FINAL_EXTRA: usize = 28; // 4+4+8+8+4

const FRAME_FLAG_STEREO: u8 = 0x01;
const FRAME_FLAG_FINAL: u8 = 0x02;


/// Binary wire format for streaming audio frames over WebRTC DataChannels.
///
/// Each frame carries a single 20ms Opus packet. The final frame of an
/// interval includes metadata so the receiver can reconstruct an AudioInterval.
///
/// Format (all integers are little-endian):
/// ```text
/// [4 bytes] magic: "WAIF"
/// [1 byte]  flags: bit 0 = stereo, bit 1 = final (last frame of interval)
/// [2 bytes] stream_id: u16
/// [8 bytes] interval_index: i64
/// [4 bytes] frame_number: u32  (0-indexed within interval)
/// [2 bytes] opus_len: u16
/// [N bytes] opus_data
///
/// If final flag set, append:
/// [4 bytes] sample_rate: u32
/// [4 bytes] total_frames: u32
/// [8 bytes] bpm: f64
/// [8 bytes] quantum: f64
/// [4 bytes] bars: u32
/// ```
pub struct AudioFrameWire;

impl AudioFrameWire {
    pub fn encode(frame: &super::AudioFrame) -> Vec<u8> {
        let extra = if frame.is_final { FRAME_FINAL_EXTRA } else { 0 };
        let mut buf = Vec::with_capacity(FRAME_HEADER_SIZE + frame.opus_data.len() + extra);

        buf.extend_from_slice(FRAME_MAGIC);

        let mut flags: u8 = 0;
        if frame.channels == 2 {
            flags |= FRAME_FLAG_STEREO;
        }
        if frame.is_final {
            flags |= FRAME_FLAG_FINAL;
        }
        buf.push(flags);

        buf.extend_from_slice(&frame.stream_id.to_le_bytes());
        buf.extend_from_slice(&frame.interval_index.to_le_bytes());
        buf.extend_from_slice(&frame.frame_number.to_le_bytes());
        buf.extend_from_slice(&(frame.opus_data.len() as u16).to_le_bytes());
        buf.extend_from_slice(&frame.opus_data);

        if frame.is_final {
            buf.extend_from_slice(&frame.sample_rate.to_le_bytes());
            buf.extend_from_slice(&frame.total_frames.to_le_bytes());
            buf.extend_from_slice(&frame.bpm.to_le_bytes());
            buf.extend_from_slice(&frame.quantum.to_le_bytes());
            buf.extend_from_slice(&frame.bars.to_le_bytes());
        }

        buf
    }

    pub fn decode(data: &[u8]) -> Result<super::AudioFrame> {
        if data.len() < FRAME_HEADER_SIZE {
            anyhow::bail!(
                "Audio frame wire data too short: {} bytes, need at least {FRAME_HEADER_SIZE}",
                data.len()
            );
        }

        if &data[0..4] != FRAME_MAGIC {
            anyhow::bail!("Invalid audio frame wire magic: {:?}", &data[0..4]);
        }

        let flags = data[4];
        let channels = if flags & FRAME_FLAG_STEREO != 0 { 2 } else { 1 };
        let is_final = flags & FRAME_FLAG_FINAL != 0;

        let stream_id = u16::from_le_bytes(data[5..7].try_into()?);
        let interval_index = i64::from_le_bytes(data[7..15].try_into()?);
        let frame_number = u32::from_le_bytes(data[15..19].try_into()?);
        let opus_len = u16::from_le_bytes(data[19..21].try_into()?) as usize;

        if data.len() < FRAME_HEADER_SIZE + opus_len {
            anyhow::bail!(
                "Audio frame wire truncated: expected {} opus bytes, got {}",
                opus_len,
                data.len() - FRAME_HEADER_SIZE
            );
        }

        let opus_data = data[FRAME_HEADER_SIZE..FRAME_HEADER_SIZE + opus_len].to_vec();

        let (sample_rate, total_frames, bpm, quantum, bars) = if is_final {
            let meta_start = FRAME_HEADER_SIZE + opus_len;
            if data.len() < meta_start + FRAME_FINAL_EXTRA {
                anyhow::bail!(
                    "Audio frame final metadata truncated: need {} more bytes",
                    meta_start + FRAME_FINAL_EXTRA - data.len()
                );
            }
            let sr = u32::from_le_bytes(data[meta_start..meta_start + 4].try_into()?);
            let tf = u32::from_le_bytes(data[meta_start + 4..meta_start + 8].try_into()?);
            let b = f64::from_le_bytes(data[meta_start + 8..meta_start + 16].try_into()?);
            let q = f64::from_le_bytes(data[meta_start + 16..meta_start + 24].try_into()?);
            let bars = u32::from_le_bytes(data[meta_start + 24..meta_start + 28].try_into()?);
            (sr, tf, b, q, bars)
        } else {
            (0, 0, 0.0, 0.0, 0)
        };

        Ok(super::AudioFrame {
            interval_index,
            stream_id,
            frame_number,
            channels,
            opus_data,
            is_final,
            sample_rate,
            total_frames,
            bpm,
            quantum,
            bars,
        })
    }
}

/// Minimal WAIF frame header fields extracted without allocation.
#[derive(Debug, Clone, Copy)]
pub struct WaifHeaderPeek {
    pub interval_index: i64,
    pub frame_number: u32,
    pub is_final: bool,
    /// Only valid when `is_final` is true.
    pub total_frames: u32,
}

/// Zero-copy peek at a WAIF frame header. Returns `None` if the data
/// is too short or doesn't have the WAIF magic.
pub fn peek_waif_header(data: &[u8]) -> Option<WaifHeaderPeek> {
    if data.len() < FRAME_HEADER_SIZE || &data[0..4] != FRAME_MAGIC {
        return None;
    }
    let flags = data[4];
    let is_final = flags & FRAME_FLAG_FINAL != 0;
    let interval_index = i64::from_le_bytes(data[7..15].try_into().ok()?);
    let frame_number = u32::from_le_bytes(data[15..19].try_into().ok()?);
    let total_frames = if is_final {
        let opus_len = u16::from_le_bytes(data[19..21].try_into().ok()?) as usize;
        let meta_start = FRAME_HEADER_SIZE + opus_len;
        if data.len() < meta_start + FRAME_FINAL_EXTRA {
            return None;
        }
        u32::from_le_bytes(data[meta_start + 4..meta_start + 8].try_into().ok()?)
    } else {
        0
    };
    Some(WaifHeaderPeek {
        interval_index,
        frame_number,
        is_final,
        total_frames,
    })
}

/// Rewrite the interval_index field in a WAIF frame header in-place.
///
/// Used by the session layer to remap a remote peer's interval index to
/// the local session's synced index before forwarding to the recv plugin.
/// Returns true if the rewrite was performed.
pub fn rewrite_waif_interval_index(data: &mut [u8], new_index: i64) -> bool {
    if data.len() >= FRAME_HEADER_SIZE && &data[0..4] == FRAME_MAGIC {
        data[7..15].copy_from_slice(&new_index.to_le_bytes());
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_wire_non_final_roundtrip() {
        let frame = crate::AudioFrame {
            interval_index: 42,
            stream_id: 3,
            frame_number: 7,
            channels: 2,
            opus_data: vec![0xDE, 0xAD, 0xBE, 0xEF],
            is_final: false,
            sample_rate: 0,
            total_frames: 0,
            bpm: 0.0,
            quantum: 0.0,
            bars: 0,
        };

        let encoded = AudioFrameWire::encode(&frame);
        assert_eq!(&encoded[0..4], b"WAIF");
        assert_eq!(encoded[4], FRAME_FLAG_STEREO); // stereo, not final
        // Total: 21 header + 4 opus = 25 bytes
        assert_eq!(encoded.len(), 25);

        let decoded = AudioFrameWire::decode(&encoded).unwrap();
        assert_eq!(decoded.interval_index, 42);
        assert_eq!(decoded.stream_id, 3);
        assert_eq!(decoded.frame_number, 7);
        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.opus_data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(!decoded.is_final);
    }

    #[test]
    fn frame_wire_final_roundtrip() {
        let frame = crate::AudioFrame {
            interval_index: 10,
            stream_id: 0,
            frame_number: 399,
            channels: 1,
            opus_data: vec![0xAB],
            is_final: true,
            sample_rate: 48000,
            total_frames: 400,
            bpm: 120.0,
            quantum: 4.0,
            bars: 4,
        };

        let encoded = AudioFrameWire::encode(&frame);
        assert_eq!(&encoded[0..4], b"WAIF");
        assert_eq!(encoded[4], FRAME_FLAG_FINAL); // mono + final
        // Total: 21 header + 1 opus + 28 final metadata = 50
        assert_eq!(encoded.len(), 50);

        let decoded = AudioFrameWire::decode(&encoded).unwrap();
        assert_eq!(decoded.interval_index, 10);
        assert_eq!(decoded.frame_number, 399);
        assert_eq!(decoded.channels, 1);
        assert!(decoded.is_final);
        assert_eq!(decoded.sample_rate, 48000);
        assert_eq!(decoded.total_frames, 400);
        assert!((decoded.bpm - 120.0).abs() < f64::EPSILON);
        assert!((decoded.quantum - 4.0).abs() < f64::EPSILON);
        assert_eq!(decoded.bars, 4);
    }

    #[test]
    fn frame_wire_rejects_bad_magic() {
        let mut data = vec![0u8; 25];
        data[0..4].copy_from_slice(b"NOPE");
        assert!(AudioFrameWire::decode(&data).is_err());
    }

    #[test]
    fn frame_wire_rejects_truncated() {
        assert!(AudioFrameWire::decode(&[0u8; 10]).is_err());
    }

    #[test]
    fn frame_wire_rejects_truncated_final_metadata() {
        let frame = crate::AudioFrame {
            interval_index: 0,
            stream_id: 0,
            frame_number: 0,
            channels: 1,
            opus_data: vec![0xAB],
            is_final: true,
            sample_rate: 48000,
            total_frames: 1,
            bpm: 120.0,
            quantum: 4.0,
            bars: 4,
        };

        let mut encoded = AudioFrameWire::encode(&frame);
        // Truncate the final metadata
        encoded.truncate(encoded.len() - 10);
        assert!(AudioFrameWire::decode(&encoded).is_err());
    }

    // --- peek_waif_header tests ---

    #[test]
    fn peek_waif_header_non_final() {
        let frame = crate::AudioFrame {
            interval_index: 42,
            stream_id: 3,
            frame_number: 7,
            channels: 2,
            opus_data: vec![0xDE, 0xAD],
            is_final: false,
            sample_rate: 0,
            total_frames: 0,
            bpm: 0.0,
            quantum: 0.0,
            bars: 0,
        };
        let encoded = AudioFrameWire::encode(&frame);
        let peek = peek_waif_header(&encoded).unwrap();
        assert_eq!(peek.interval_index, 42);
        assert_eq!(peek.frame_number, 7);
        assert!(!peek.is_final);
        assert_eq!(peek.total_frames, 0);
    }

    #[test]
    fn peek_waif_header_final_frame() {
        let frame = crate::AudioFrame {
            interval_index: 10,
            stream_id: 0,
            frame_number: 49,
            channels: 1,
            opus_data: vec![0xAB],
            is_final: true,
            sample_rate: 48000,
            total_frames: 50,
            bpm: 120.0,
            quantum: 4.0,
            bars: 4,
        };
        let encoded = AudioFrameWire::encode(&frame);
        let peek = peek_waif_header(&encoded).unwrap();
        assert_eq!(peek.interval_index, 10);
        assert_eq!(peek.frame_number, 49);
        assert!(peek.is_final);
        assert_eq!(peek.total_frames, 50);
    }

    #[test]
    fn peek_waif_header_too_short() {
        assert!(peek_waif_header(&[0u8; 10]).is_none());
    }

    #[test]
    fn peek_waif_header_wrong_magic() {
        let mut data = vec![0u8; 25];
        data[0..4].copy_from_slice(b"NOPE");
        assert!(peek_waif_header(&data).is_none());
    }

    #[test]
    fn rewrite_waif_interval_index_roundtrip() {
        let frame = crate::AudioFrame {
            interval_index: 5,
            stream_id: 3,
            frame_number: 7,
            channels: 2,
            opus_data: vec![0xAA; 100],
            is_final: false,
            sample_rate: 0,
            total_frames: 0,
            bpm: 0.0,
            quantum: 0.0,
            bars: 0,
        };
        let mut data = AudioFrameWire::encode(&frame);

        // Verify original index.
        let header = peek_waif_header(&data).unwrap();
        assert_eq!(header.interval_index, 5);

        // Rewrite to 42.
        assert!(rewrite_waif_interval_index(&mut data, 42));

        // Verify new index, other fields unchanged.
        let header = peek_waif_header(&data).unwrap();
        assert_eq!(header.interval_index, 42);
        assert_eq!(header.frame_number, 7);
        assert_eq!(header.is_final, false);

        // Full decode confirms stream_id and opus_data intact.
        let decoded = AudioFrameWire::decode(&data).unwrap();
        assert_eq!(decoded.interval_index, 42);
        assert_eq!(decoded.stream_id, 3);
        assert_eq!(decoded.opus_data, vec![0xAA; 100]);
    }

    #[test]
    fn rewrite_waif_interval_index_short_data() {
        let mut data = vec![0u8; 10];
        assert!(!rewrite_waif_interval_index(&mut data, 42));
    }

    #[test]
    fn rewrite_waif_interval_index_wrong_magic() {
        let mut data = vec![0u8; 25];
        data[0..4].copy_from_slice(b"NOPE");
        assert!(!rewrite_waif_interval_index(&mut data, 42));
    }
}
