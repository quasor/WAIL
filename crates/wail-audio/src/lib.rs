pub mod bridge;
pub mod codec;
pub mod frame_assembler;
pub mod interval;
pub mod ipc;
pub mod ring;
pub mod slot;
pub mod wire;

#[cfg(feature = "analysis")]
pub mod fft_analysis;
pub mod test_tone;


pub use bridge::AudioBridge;
pub use codec::{nearest_opus_rate, AudioDecoder, AudioEncoder};
pub use frame_assembler::{AssembledInterval, FrameAssembler};
pub use interval::{AudioFrame, AudioInterval, IntervalRecorder};
pub use ipc::{IpcFramer, IpcMessage, IpcRecvBuffer, IPC_ROLE_RECV, IPC_ROLE_SEND, IPC_TAG_AUDIO_FRAME_PUB, IPC_TAG_AUDIO_PUB, IPC_TAG_METRICS_PUB, IPC_TAG_PEER_JOINED_PUB, IPC_TAG_PEER_LEFT_PUB, IPC_TAG_PEER_NAME_PUB};
pub use ring::{CompletedInterval, IntervalRing, PeerSlot, MAX_REMOTE_PEERS};
pub use slot::{ClientChannelMapping, SlotTable};
pub use wire::{peek_waif_header, rewrite_waif_interval_index, AudioFrameWire, WaifHeaderPeek};
