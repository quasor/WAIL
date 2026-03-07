pub mod bridge;
pub mod codec;
pub mod interval;
pub mod ipc;
pub mod ring;
pub mod slot;
pub mod wire;

#[cfg(test)]
mod pipeline;

pub use bridge::AudioBridge;
pub use codec::{nearest_opus_rate, AudioDecoder, AudioEncoder};
pub use interval::{AudioInterval, IntervalRecorder};
pub use ipc::{IpcFramer, IpcMessage, IpcRecvBuffer, IPC_ROLE_RECV, IPC_ROLE_SEND, IPC_TAG_AUDIO_PUB, IPC_TAG_PEER_JOINED_PUB, IPC_TAG_PEER_LEFT_PUB};
pub use ring::{CompletedInterval, IntervalRing, PeerSlot, MAX_REMOTE_PEERS};
pub use slot::{ClientChannelMapping, SlotTable};
pub use wire::AudioWire;
