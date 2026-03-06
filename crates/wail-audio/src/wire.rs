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
}
