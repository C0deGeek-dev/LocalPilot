//! A workspace's own endpoint ignores the endpoint override, so a process that
//! reaches several workspaces cannot send them all to one overridden server.

use localpilot_server::{endpoint_overridden, Endpoint};

#[test]
fn the_override_never_merges_two_workspaces() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    // This test binary is its own process; nothing else reads the variable.
    std::env::set_var(
        localpilot_server::transport::ENDPOINT_ENV,
        if cfg!(windows) {
            r"\\.\pipe\localpilot-override-test"
        } else {
            "/tmp/localpilot-override-test.sock"
        },
    );
    assert!(endpoint_overridden());
    let resolved_a = Endpoint::resolve(a.path()).unwrap().display();
    let resolved_b = Endpoint::resolve(b.path()).unwrap().display();
    assert_eq!(resolved_a, resolved_b, "the override applies to resolve");
    let own_a = Endpoint::for_workspace(a.path()).unwrap().display();
    let own_b = Endpoint::for_workspace(b.path()).unwrap().display();
    assert_ne!(own_a, own_b, "each workspace keeps its own endpoint");
    assert_ne!(own_a, resolved_a);
    assert_ne!(own_b, resolved_b);
    std::env::set_var(localpilot_server::transport::ENDPOINT_ENV, "  ");
    assert!(!endpoint_overridden(), "a blank value is no override");
}
