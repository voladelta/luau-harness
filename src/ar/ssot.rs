//! String Seed of Thought (https://pub.sakana.ai/ssot/): deliberately different answers from one model. Each sample gets
//! fresh host entropy; the model lists the root-level dimensions along which answers differ, picks one option per
//! dimension from a different part of the seed, then answers. In blind tests on DeepSeek V4.1 Flash this, not the
//! paper's self-generated string, moved answers off the model's default. Autoresearch explore workers derive their ideas
//! this way, and `examples/tools/ssot.lua` samples with it from a tool.

/// Fresh OS entropy (the same as a tool's `nonce()`), so no earlier call or fixed PRNG state can predict the seed.
pub fn seed() -> std::io::Result<String> {
    crate::tools::nonce()
}
