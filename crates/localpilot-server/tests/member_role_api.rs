//! Downstream compatibility coverage for the public, growing member-role enum.

use localpilot_server::swarm::MemberRole;

/// This match lives in an integration-test crate, so the wildcard is required
/// by `#[non_exhaustive]` and proves future roles will not break downstream
/// consumers that follow the public contract.
fn downstream_label(role: MemberRole) -> &'static str {
    match role {
        MemberRole::Coordinator => "coordinator",
        MemberRole::Worker => "worker",
        MemberRole::Peer => "peer",
        _ => "future role",
    }
}

#[test]
fn known_roles_keep_their_public_labels() {
    assert_eq!(downstream_label(MemberRole::Coordinator), "coordinator");
    assert_eq!(downstream_label(MemberRole::Worker), "worker");
    assert_eq!(downstream_label(MemberRole::Peer), "peer");
}
