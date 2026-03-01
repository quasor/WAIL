pub mod bridge;
pub mod codec;
pub mod interval;
pub mod ipc;
pub mod ring;
pub mod wire;

#[cfg(test)]
mod pipeline;

pub use bridge::AudioBridge;
pub use codec::{nearest_opus_rate, AudioDecoder, AudioEncoder};
pub use interval::{AudioInterval, IntervalRecorder, IntervalPlayer};
pub use ipc::{IpcFramer, IpcMessage, IpcRecvBuffer};
pub use ring::{CompletedInterval, IntervalRing, PeerSlot, MAX_REMOTE_PEERS};
pub use wire::AudioWire;
