//! Fuzzes the OpenAI-compatible SSE stream decoder with arbitrary bytes:
//! untrusted network input must never panic the decoder, only produce events
//! or typed stream errors.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    localpilot_llm::fuzzing::openai_sse(data);
});
