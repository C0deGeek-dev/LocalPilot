//! The constrained-decoding capability is declared by a local server (which can
//! constrain decoding to a JSON schema) and not by a hosted endpoint.
#![allow(clippy::unwrap_used)]

use localpilot_llm::{ModelProvider, OpenAiProvider, SourceType};

#[test]
fn a_local_server_declares_constrained_decoding() {
    let provider = OpenAiProvider::new(
        "local",
        "Local",
        SourceType::LocalServer,
        "http://127.0.0.1:8080",
        None,
    );
    assert!(
        provider.declaration().capabilities.constrained_decoding,
        "a local OpenAI-compatible server can constrain decoding"
    );
}

#[test]
fn a_hosted_endpoint_does_not_declare_constrained_decoding() {
    let provider = OpenAiProvider::new(
        "hosted",
        "Hosted",
        SourceType::OfficialApi,
        "https://api.example.invalid",
        None,
    );
    assert!(
        !provider.declaration().capabilities.constrained_decoding,
        "a hosted endpoint keeps native tool-calling, no schema constraint"
    );
}
