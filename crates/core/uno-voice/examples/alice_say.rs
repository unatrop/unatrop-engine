//! End-to-end check for the Alice TTS path.
//!
//! Start `tools/alice-uniproxy/serve.py`, then:
//!   cargo run -p uno-voice --features alice-tts --example alice_say -- "привет" [voice]

use uno_voice::{AliceTts, TtsEngine, TtsOptions};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let text = args
        .next()
        .unwrap_or_else(|| "Привет, это проверка голоса".to_owned());
    let voice = args.next();

    let tts = AliceTts::from_env();
    let options = TtsOptions {
        voice,
        ..TtsOptions::default()
    };
    let utterance = tts.synthesize(&text, &options).await?;
    println!(
        "id={} sample_rate={} samples={} duration_ms={}",
        utterance.id,
        utterance.sample_rate,
        utterance.samples.len(),
        utterance.duration_ms()
    );
    Ok(())
}
