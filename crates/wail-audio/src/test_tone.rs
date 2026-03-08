//! Shared test tone generation and audio validation utilities.
//!
//! Used by both `wail-e2e` (two-machine tests) and `wail-tauri` (test mode)
//! to generate synthetic audio and validate received audio without a DAW.

use anyhow::{bail, Result};

use crate::codec::{AudioDecoder, AudioEncoder};
use crate::interval::AudioInterval;
use crate::wire::{AudioFrameWire, AudioWire};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u16 = 2;
const OPUS_BITRATE: u32 = 128;

/// Result of validating received audio data.
pub struct AudioValidation {
    /// Wire format: "WAIL" (interval) or "WAIF" (frame)
    pub format: String,
    /// Total wire size in bytes
    pub size_bytes: usize,
    /// RMS energy of decoded PCM (0.0 = silence)
    pub rms: f32,
    /// Human-readable detail string
    pub detail: String,
}

/// Encode a synthetic sine-wave test interval in WAIL wire format.
///
/// Generates a single 20ms Opus frame (960 samples at 48kHz) of a sine wave
/// at the given frequency, wraps it in AudioWire binary format.
pub fn encode_test_interval(
    index: i64,
    freq: f32,
    bpm: f64,
    bars: u32,
    quantum: f64,
) -> Result<Vec<u8>> {
    let num_frames = 960u32; // 20ms at 48kHz
    let num_samples = num_frames as usize * CHANNELS as usize;
    let mut samples = vec![0.0f32; num_samples];
    for i in 0..num_frames as usize {
        let val =
            (2.0 * std::f32::consts::PI * freq * i as f32 / SAMPLE_RATE as f32).sin() * 0.5;
        samples[i * 2] = val;
        samples[i * 2 + 1] = val;
    }

    let mut encoder = AudioEncoder::new(SAMPLE_RATE, CHANNELS, OPUS_BITRATE)?;
    let opus_data = encoder.encode_interval(&samples)?;

    let interval = AudioInterval {
        index,
        stream_id: 0,
        opus_data,
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS,
        num_frames,
        bpm,
        quantum,
        bars,
    };
    Ok(AudioWire::encode(&interval))
}

/// Validate received audio wire data: decode, check for silence, return details.
pub fn validate_audio(data: &[u8]) -> Result<AudioValidation> {
    if data.len() < 4 {
        bail!("audio data too short ({} bytes)", data.len());
    }

    if &data[0..4] == b"WAIL" {
        let decoded = AudioWire::decode(data)?;
        if decoded.opus_data.is_empty() {
            bail!("opus_data is empty");
        }

        let mut decoder = AudioDecoder::new(decoded.sample_rate, decoded.channels)?;
        let pcm = decoder.decode_interval(&decoded.opus_data)?;
        let rms_val = rms(&pcm);

        let detail = format!(
            "WAIL interval: {} bytes, {}/{} frames, RMS={rms_val:.4}, idx={}",
            data.len(),
            decoded.num_frames,
            decoded.channels,
            decoded.index,
        );

        Ok(AudioValidation {
            format: "WAIL".into(),
            size_bytes: data.len(),
            rms: rms_val,
            detail,
        })
    } else if &data[0..4] == b"WAIF" {
        let frame = AudioFrameWire::decode(data)?;
        let detail = format!(
            "WAIF frame: {} bytes, frame #{}, interval {}, final={}",
            data.len(),
            frame.frame_number,
            frame.interval_index,
            frame.is_final,
        );

        // WAIF frames don't carry enough context for full decode without
        // assembling all frames of an interval, so RMS is 0 here.
        Ok(AudioValidation {
            format: "WAIF".into(),
            size_bytes: data.len(),
            rms: 0.0,
            detail,
        })
    } else {
        bail!(
            "unknown wire format: magic={:?}",
            &data[..data.len().min(4)]
        );
    }
}

/// RMS (root mean square) energy of an audio buffer.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / samples.len() as f64).sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let wire = encode_test_interval(42, 440.0, 120.0, 4, 4.0).unwrap();
        let validation = validate_audio(&wire).unwrap();
        assert_eq!(validation.format, "WAIL");
        assert!(validation.rms > 0.01, "RMS should be non-trivial for 440Hz sine");
        assert!(validation.detail.contains("idx=42"));
    }

    #[test]
    fn rms_of_silence_is_zero() {
        let silence = vec![0.0f32; 1920];
        assert_eq!(rms(&silence), 0.0);
    }

    #[test]
    fn rms_of_signal_is_nonzero() {
        let mut samples = vec![0.0f32; 1920];
        for i in 0..960 {
            let val = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48000.0).sin() * 0.5;
            samples[i * 2] = val;
            samples[i * 2 + 1] = val;
        }
        assert!(rms(&samples) > 0.1);
    }

    #[test]
    fn validate_rejects_garbage() {
        let garbage = vec![0u8; 10];
        assert!(validate_audio(&garbage).is_err());
    }

    #[test]
    fn validate_rejects_short_data() {
        let short = vec![0u8; 2];
        assert!(validate_audio(&short).is_err());
    }
}
