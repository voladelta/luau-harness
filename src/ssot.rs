//! String Seed of Thought (https://pub.sakana.ai/ssot/): deliberately different answers from one model. Each sample gets
//! fresh host entropy; the model lists the root-level dimensions along which answers differ, picks one option per
//! dimension from a different part of the seed, then answers. In blind tests on DeepSeek V4.1 Flash this, not the
//! paper's self-generated string, moved answers off the model's default. Autoresearch explore workers derive their ideas
//! this way, and `examples/tools/ssot.lua` samples with it from a tool.

/// Fresh OS entropy, so no earlier call or fixed PRNG state can predict the seed.
pub fn seed() -> std::io::Result<String> {
    use base64::Engine;
    let mut bytes = [0u8; 24];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}
