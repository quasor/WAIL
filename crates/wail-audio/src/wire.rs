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
/// Total header: 48 bytes + opus_data
pub struct AudioWire;

const MAGIC: &[u8; 4] = b"WAIL";
const VERSION: u8 = 2;
const HEADER_SIZE: usize = 48;

const FRAME_MAGIC: &[u8; 4] = b"WAIF";
const FRAME_HEADER_SIZE: usize = 21; // 4+1+2+8+4+2
const FRAME_FINAL_EXTRA: usize = 28; // 4+4+8+8+4

const FRAME_FLAG_STEREO: u8 = 0x01;
const FRAME_FLAG_FINAL: u8 = 0x02;

impl AudioWire {
    /// Serialize an AudioInterval into the binary wire format.
    pub fn encode(interval: &super::AudioInterval) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_SIZE + interval.opus_data.len());

        // Magic
        buf.extend_from_slice(MAGIC);
        // Version
        buf.push(VERSION);
        // Flags: bit 0 = stereo
        buf.push(if interval.channels == 2 { 1 } else { 0 });
        // Stream ID
        buf.extend_from_slice(&interval.stream_id.to_le_bytes());
        // Interval index
        buf.extend_from_slice(&interval.index.to_le_bytes());
        // Sample rate
        buf.extend_from_slice(&interval.sample_rate.to_le_bytes());
        // Num frames
        buf.extend_from_slice(&interval.num_frames.to_le_bytes());
        // BPM
        buf.extend_from_slice(&interval.bpm.to_le_bytes());
        // Quantum
        buf.extend_from_slice(&interval.quantum.to_le_bytes());
        // Bars
        buf.extend_from_slice(&interval.bars.to_le_bytes());
        // Opus data length
        buf.extend_from_slice(&(interval.opus_data.len() as u32).to_le_bytes());
        // Opus data
        buf.extend_from_slice(&interval.opus_data);

        buf
    }

    /// Deserialize the binary wire format into an AudioInterval.
    pub fn decode(data: &[u8]) -> Result<super::AudioInterval> {
        if data.len() < HEADER_SIZE {
            anyhow::bail!(
                "Audio wire data too short: {} bytes, need at least {HEADER_SIZE}",
                data.len()
            );
        }

        // Magic
        if &data[0..4] != MAGIC {
            anyhow::bail!("Invalid audio wire magic: {:?}", &data[0..4]);
        }

        // Version (accept v1 and v2)
        let version = data[4];
        if version != 1 && version != 2 {
            anyhow::bail!("Unsupported audio wire version: {version}");
        }

        // Flags
        let flags = data[5];
        let channels = if flags & 1 != 0 { 2 } else { 1 };

        // Stream ID (v1: reserved/zero, v2: explicit)
        let stream_id = if version >= 2 {
            u16::from_le_bytes(data[6..8].try_into()?)
        } else {
            0
        };

        // Interval index
        let index = i64::from_le_bytes(data[8..16].try_into()?);

        // Sample rate
        let sample_rate = u32::from_le_bytes(data[16..20].try_into()?);

        // Num frames
        let num_frames = u32::from_le_bytes(data[20..24].try_into()?);

        // BPM
        let bpm = f64::from_le_bytes(data[24..32].try_into()?);

        // Quantum
        let quantum = f64::from_le_bytes(data[32..40].try_into()?);

        // Bars
        let bars = u32::from_le_bytes(data[40..44].try_into()?);

        // Opus data length
        let opus_len = u32::from_le_bytes(data[44..48].try_into()?) as usize;

        if data.len() < HEADER_SIZE + opus_len {
            anyhow::bail!(
                "Audio wire data truncated: expected {} bytes of opus data, got {}",
                opus_len,
                data.len() - HEADER_SIZE
            );
        }

        let opus_data = data[HEADER_SIZE..HEADER_SIZE + opus_len].to_vec();

        Ok(super::AudioInterval {
            index,
            stream_id,
            opus_data,
            sample_rate,
            channels,
            num_frames,
            bpm,
            quantum,
            bars,
        })
    }
}

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
    use crate::AudioInterval;

    #[test]
    fn wire_roundtrip() {
        let interval = AudioInterval {
            index: 42,
            stream_id: 0,
            opus_data: vec![1, 2, 3, 4, 5],
            sample_rate: 48000,
            channels: 2,
            num_frames: 96000,
            bpm: 120.0,
            quantum: 4.0,
            bars: 4,
        };

        let encoded = AudioWire::encode(&interval);
        assert_eq!(&encoded[0..4], b"WAIL");
        assert_eq!(encoded[4], 2); // version
        assert_eq!(encoded[5], 1); // stereo flag

        let decoded = AudioWire::decode(&encoded).unwrap();
        assert_eq!(decoded.index, 42);
        assert_eq!(decoded.sample_rate, 48000);
        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.num_frames, 96000);
        assert!((decoded.bpm - 120.0).abs() < f64::EPSILON);
        assert!((decoded.quantum - 4.0).abs() < f64::EPSILON);
        assert_eq!(decoded.bars, 4);
        assert_eq!(decoded.opus_data, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn wire_mono() {
        let interval = AudioInterval {
            index: 0,
            stream_id: 0,
            opus_data: vec![],
            sample_rate: 48000,
            channels: 1,
            num_frames: 48000,
            bpm: 90.0,
            quantum: 3.0,
            bars: 2,
        };

        let encoded = AudioWire::encode(&interval);
        assert_eq!(encoded[5], 0); // mono flag

        let decoded = AudioWire::decode(&encoded).unwrap();
        assert_eq!(decoded.channels, 1);
    }

    #[test]
    fn wire_v2_stream_id_roundtrip() {
        let interval = AudioInterval {
            index: 10,
            stream_id: 5,
            opus_data: vec![0xAB],
            sample_rate: 48000,
            channels: 2,
            num_frames: 960,
            bpm: 120.0,
            quantum: 4.0,
            bars: 4,
        };

        let encoded = AudioWire::encode(&interval);
        // stream_id at bytes 6-7
        assert_eq!(u16::from_le_bytes([encoded[6], encoded[7]]), 5);

        let decoded = AudioWire::decode(&encoded).unwrap();
        assert_eq!(decoded.stream_id, 5);
    }

    #[test]
    fn wire_v1_backward_compat() {
        // Manually construct v1 wire data (version=1, reserved=0)
        let mut data = vec![0u8; 48 + 2];
        data[0..4].copy_from_slice(b"WAIL");
        data[4] = 1; // version 1
        data[5] = 1; // stereo
        data[6..8].copy_from_slice(&[0, 0]); // reserved
        data[8..16].copy_from_slice(&42i64.to_le_bytes());
        data[16..20].copy_from_slice(&48000u32.to_le_bytes());
        data[20..24].copy_from_slice(&960u32.to_le_bytes());
        data[24..32].copy_from_slice(&120.0f64.to_le_bytes());
        data[32..40].copy_from_slice(&4.0f64.to_le_bytes());
        data[40..44].copy_from_slice(&4u32.to_le_bytes());
        data[44..48].copy_from_slice(&2u32.to_le_bytes()); // opus_len = 2
        data[48..50].copy_from_slice(&[0xDE, 0xAD]);

        let decoded = AudioWire::decode(&data).unwrap();
        assert_eq!(decoded.index, 42);
        assert_eq!(decoded.stream_id, 0); // v1 defaults to stream 0
        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.opus_data, vec![0xDE, 0xAD]);
    }

    #[test]
    fn wire_rejects_bad_magic() {
        let mut data = vec![0u8; 48];
        data[0..4].copy_from_slice(b"NOPE");
        assert!(AudioWire::decode(&data).is_err());
    }

    #[test]
    fn wire_rejects_truncated() {
        let data = vec![0u8; 10];
        assert!(AudioWire::decode(&data).is_err());
    }

    // §5.2 — Unknown version byte returns a graceful Err, not a panic.
    #[test]
    fn decode_unknown_version_returns_err() {
        let mut data = vec![0u8; 50]; // 48-byte header + 2 opus bytes
        data[0..4].copy_from_slice(b"WAIL");
        data[4] = 99; // unsupported version
        data[5] = 0; // flags
        data[6..8].copy_from_slice(&[0, 0]); // stream_id
        data[8..16].copy_from_slice(&0i64.to_le_bytes());
        data[16..20].copy_from_slice(&48000u32.to_le_bytes());
        data[20..24].copy_from_slice(&960u32.to_le_bytes());
        data[24..32].copy_from_slice(&120.0f64.to_le_bytes());
        data[32..40].copy_from_slice(&4.0f64.to_le_bytes());
        data[40..44].copy_from_slice(&4u32.to_le_bytes());
        data[44..48].copy_from_slice(&2u32.to_le_bytes()); // opus_len = 2

        let result = AudioWire::decode(&data);
        assert!(result.is_err(), "Unknown version must return Err");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Unsupported") || msg.contains("version"),
            "Error message should mention version: {msg}"
        );
    }

    // §5.2 — An interval with zero frames and empty opus data encodes and decodes cleanly.
    #[test]
    fn encode_zero_frame_interval_does_not_panic() {
        let interval = AudioInterval {
            index: 0,
            stream_id: 0,
            opus_data: vec![],
            sample_rate: 48000,
            channels: 1,
            num_frames: 0,
            bpm: 120.0,
            quantum: 4.0,
            bars: 4,
        };

        let encoded = AudioWire::encode(&interval);
        let decoded = AudioWire::decode(&encoded).unwrap();
        assert_eq!(decoded.num_frames, 0);
        assert!(decoded.opus_data.is_empty());
    }

    // §5.2 — A very long interval (large num_frames, large opus payload) round-trips
    // without integer overflow or panic.
    #[test]
    fn encode_large_interval_roundtrips_without_overflow() {
        // Simulate ~60s of audio at 48 kHz = 2,880,000 frames (fits in u32).
        let large_num_frames: u32 = 2_880_000;
        // Use a realistically large-ish Opus payload (200 KB).
        let opus_data = vec![0xAB; 200_000];

        let interval = AudioInterval {
            index: i64::MAX - 1,
            stream_id: u16::MAX,
            opus_data: opus_data.clone(),
            sample_rate: 48000,
            channels: 2,
            num_frames: large_num_frames,
            bpm: 30.0,
            quantum: 4.0,
            bars: 4,
        };

        let encoded = AudioWire::encode(&interval);
        let decoded = AudioWire::decode(&encoded).unwrap();

        assert_eq!(decoded.index, i64::MAX - 1);
        assert_eq!(decoded.stream_id, u16::MAX);
        assert_eq!(decoded.num_frames, large_num_frames);
        assert_eq!(decoded.opus_data, opus_data);
    }

    // --- AudioFrameWire tests ---

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
