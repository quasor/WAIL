use nih_plug::prelude::*;

#[derive(Params)]
pub struct WailSendParams {
    /// Stream index (1–14). Each index sends on a separate stream.
    /// Index 0 is reserved for the built-in test tone generator.
    /// Same index from the same peer is mixed together on the receive side.
    #[id = "stream_index"]
    pub stream_index: IntParam,

    /// When enabled, input audio passes through to the plugin output
    /// instead of being silenced.
    #[id = "passthrough"]
    pub passthrough: BoolParam,
}

impl Default for WailSendParams {
    fn default() -> Self {
        Self {
            stream_index: IntParam::new("Stream Index", 1, IntRange::Linear { min: 1, max: 14 }),
            passthrough: BoolParam::new("Passthrough", false),
        }
    }
}
