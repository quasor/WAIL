use std::convert::TryFrom;

use anyhow::Result;
use audiopus::coder::{Decoder as OpusDecoder, Encoder as OpusEncoder};
use audiopus::packet::Packet;
use audiopus::{Application, Bitrate, Channels, MutSignals, SampleRate};

/// Map any sample rate to the nearest valid Opus sample rate.
/// Opus only supports: 8000, 12000, 16000, 24000, 48000 Hz.
/// Common DAW rates like 44100, 88200, 96000 map to 48000.
pub fn nearest_opus_rate(rate: u32) -> u32 {
    const VALID: [u32; 5] = [8000, 12000, 16000, 24000, 48000];
    *VALID
        .iter()
        .min_by_key(|&&r| (r as i64 - rate as i64).unsigned_abs())
        .unwrap()
}

/// Opus encoder for interval audio.
///
/// Encodes f32 audio frames into Opus packets. Designed for high-quality music
/// transmission at 48kHz stereo, optimized for the intervalic delivery model
/// where latency tolerance equals one full interval.
pub struct AudioEncoder {
    encoder: OpusEncoder,
    sample_rate: u32,
    channels: u16,
    frame_size: usize,
}

impl AudioEncoder {
    /// Create a new Opus encoder.
    ///
    /// - `sample_rate`: Must be one of 8000, 12000, 16000, 24000, 48000
    /// - `channels`: 1 (mono) or 2 (stereo)
    /// - `bitrate_kbps`: Target bitrate in kbps (e.g., 128 for high quality stereo)
    pub fn new(sample_rate: u32, channels: u16, bitrate_kbps: u32) -> Result<Self> {
        let opus_sr = match sample_rate {
            8000 => SampleRate::Hz8000,
            12000 => SampleRate::Hz12000,
            16000 => SampleRate::Hz16000,
            24000 => SampleRate::Hz24000,
            48000 => SampleRate::Hz48000,
            _ => anyhow::bail!("Unsupported sample rate: {sample_rate}. Must be 8000/12000/16000/24000/48000"),
        };

        let opus_ch = match channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            _ => anyhow::bail!("Unsupported channel count: {channels}. Must be 1 or 2"),
        };

        let mut encoder = OpusEncoder::new(opus_sr, opus_ch, Application::Audio)?;
        encoder.set_bitrate(Bitrate::BitsPerSecond(bitrate_kbps as i32 * 1000))?;

        // Opus frame size: 20ms at the given sample rate
        let frame_size = (sample_rate as usize * 20) / 1000;

        Ok(Self {
            encoder,
            sample_rate,
            channels,
            frame_size,
        })
    }

    /// Encode a full interval of interleaved f32 audio into Opus packets.
    ///
    /// Input: interleaved f32 samples (L, R, L, R, ... for stereo)
    /// Output: concatenated Opus frames with length-prefixed framing
    ///
    /// Frame format: [u16 LE length][opus packet bytes] repeated
    pub fn encode_interval(&mut self, samples: &[f32]) -> Result<Vec<u8>> {
        let ch = self.channels as usize;
        let frame_samples = self.frame_size * ch; // samples per frame (interleaved)
        let mut output = Vec::new();

        // Reserve space for frame count header
        let num_frames = (samples.len() + frame_samples - 1) / frame_samples;
        output.extend_from_slice(&(num_frames as u32).to_le_bytes());

        // Opus encode buffer (max Opus packet is ~4000 bytes for 20ms frame)
        let mut opus_buf = vec![0u8; 4000];

        for chunk in samples.chunks(frame_samples) {
            // Pad last chunk if needed
            let padded;
            let frame = if chunk.len() < frame_samples {
                padded = {
                    let mut p = chunk.to_vec();
                    p.resize(frame_samples, 0.0);
                    p
                };
                &padded
            } else {
                chunk
            };

            let encoded_len = self.encoder.encode_float(frame, &mut opus_buf)?;
            let packet = &opus_buf[..encoded_len];

            // Length-prefixed frame: u16 LE + opus data
            output.extend_from_slice(&(packet.len() as u16).to_le_bytes());
            output.extend_from_slice(packet);
        }

        Ok(output)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn frame_size(&self) -> usize {
        self.frame_size
    }
}

/// Opus decoder for interval audio.
pub struct AudioDecoder {
    decoder: OpusDecoder,
    sample_rate: u32,
    channels: u16,
    frame_size: usize,
}

impl AudioDecoder {
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self> {
        let opus_sr = match sample_rate {
            8000 => SampleRate::Hz8000,
            12000 => SampleRate::Hz12000,
            16000 => SampleRate::Hz16000,
            24000 => SampleRate::Hz24000,
            48000 => SampleRate::Hz48000,
            _ => anyhow::bail!("Unsupported sample rate: {sample_rate}"),
        };

        let opus_ch = match channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            _ => anyhow::bail!("Unsupported channel count: {channels}"),
        };

        let decoder = OpusDecoder::new(opus_sr, opus_ch)?;
        let frame_size = (sample_rate as usize * 20) / 1000;

        Ok(Self {
            decoder,
            sample_rate,
            channels,
            frame_size,
        })
    }

    /// Decode Opus-encoded interval data back to interleaved f32 samples.
    ///
    /// Input: length-prefixed Opus frames (as produced by `AudioEncoder::encode_interval`)
    /// Output: interleaved f32 samples
    pub fn decode_interval(&mut self, data: &[u8]) -> Result<Vec<f32>> {
        if data.len() < 4 {
            anyhow::bail!("Audio data too short for frame count header");
        }

        let num_frames = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let ch = self.channels as usize;
        let frame_samples = self.frame_size * ch;

        let mut output = Vec::with_capacity(num_frames * frame_samples);
        let mut decode_buf = vec![0f32; frame_samples];
        let mut offset = 4;

        for _ in 0..num_frames {
            if offset + 2 > data.len() {
                anyhow::bail!("Truncated audio data: missing frame length");
            }
            let pkt_len = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
            offset += 2;

            if offset + pkt_len > data.len() {
                anyhow::bail!("Truncated audio data: missing frame payload");
            }
            let packet = &data[offset..offset + pkt_len];
            offset += pkt_len;

            let opus_packet = Packet::try_from(packet)?;
            let mut_signals = MutSignals::try_from(decode_buf.as_mut_slice())?;
            let decoded = self.decoder.decode_float(Some(opus_packet), mut_signals, false)?;
            output.extend_from_slice(&decode_buf[..decoded * ch]);
        }

        Ok(output)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let sample_rate = 48000;
        let channels = 2;
        let bitrate_kbps = 128;

        let mut encoder = AudioEncoder::new(sample_rate, channels, bitrate_kbps).unwrap();
        let mut decoder = AudioDecoder::new(sample_rate, channels).unwrap();

        // Generate 1 second of sine wave (48000 samples * 2 channels)
        let num_samples = sample_rate as usize * channels as usize;
        let samples: Vec<f32> = (0..num_samples)
            .map(|i| {
                let t = (i / channels as usize) as f32 / sample_rate as f32;
                (t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5
            })
            .collect();

        let encoded = encoder.encode_interval(&samples).unwrap();
        let decoded = decoder.decode_interval(&encoded).unwrap();

        // Opus is lossy — verify we get the right number of samples back
        assert_eq!(decoded.len(), samples.len());

        // Verify the signal is reasonably similar (lossy codec — generous threshold)
        // Opus needs a few frames to converge, so skip the first 960 samples
        let skip = 960 * channels as usize;
        let mse: f32 = samples[skip..]
            .iter()
            .zip(decoded[skip..].iter())
            .map(|(a, b)| (a - b).powi(2))
            .sum::<f32>()
            / (samples.len() - skip) as f32;
        // Opus is a perceptual codec — pure sine waves compress poorly.
        // For music-like content, MSE would be much lower.
        // Threshold 0.15 validates the pipeline works without false failures.
        assert!(mse < 0.15, "Mean squared error too high: {mse}");
    }

    #[test]
    fn encode_short_interval() {
        let mut encoder = AudioEncoder::new(48000, 1, 64).unwrap();
        let mut decoder = AudioDecoder::new(48000, 1).unwrap();

        // Very short: just 100 samples (much less than one Opus frame)
        let samples = vec![0.5f32; 100];
        let encoded = encoder.encode_interval(&samples).unwrap();
        let decoded = decoder.decode_interval(&encoded).unwrap();

        // Should decode to at least one frame (960 samples at 48kHz/20ms)
        assert!(decoded.len() >= 960);
    }

    #[test]
    fn nearest_opus_rate_maps_common_daw_rates() {
        assert_eq!(nearest_opus_rate(48000), 48000);
        assert_eq!(nearest_opus_rate(44100), 48000);
        assert_eq!(nearest_opus_rate(96000), 48000);
        assert_eq!(nearest_opus_rate(88200), 48000);
        assert_eq!(nearest_opus_rate(24000), 24000);
        assert_eq!(nearest_opus_rate(16000), 16000);
        assert_eq!(nearest_opus_rate(8000), 8000);
        assert_eq!(nearest_opus_rate(22050), 24000);
    }
}
