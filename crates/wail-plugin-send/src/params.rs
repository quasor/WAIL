use nih_plug::prelude::*;

#[derive(Params)]
pub struct WailSendParams {
    /// Stream index (0–30). Each index sends on a separate stream.
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
            stream_index: IntParam::new("Stream Index", 0, IntRange::Linear { min: 0, max: 30 }),
            passthrough: BoolParam::new("Passthrough", false),
        }
    }
}
