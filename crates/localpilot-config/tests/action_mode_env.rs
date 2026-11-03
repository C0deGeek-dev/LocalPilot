//! `LOCALPILOT_PERMISSIONS__ACTION_MODE` selects the action mode. This is its
//! own test binary because the process environment is global to a test run.
#![allow(clippy::unwrap_used)]

use localpilot_config::{load, ActionMode, CliOverrides, ConfigPaths};

const KEY: &str = "LOCALPILOT_PERMISSIONS__ACTION_MODE";

#[test]
fn the_environment_key_selects_the_action_mode_and_rejects_unknown_values() {
    let dir = tempfile::tempdir().unwrap();
    let paths = ConfigPaths {
        user: Some(dir.path().join("user.toml")),
        project: Some(dir.path().join(".localpilot.toml")),
    };
    let mode =
        || load(&paths, &CliOverrides::default()).map(|config| config.permissions.action_mode);

    std::env::remove_var(KEY);
    assert_eq!(mode().unwrap(), None, "unset keeps the legacy behavior");
    std::env::set_var(KEY, "accept_all");
    assert_eq!(mode().unwrap(), Some(ActionMode::AcceptAll));
    std::env::set_var(KEY, "normal");
    assert_eq!(mode().unwrap(), Some(ActionMode::Normal));
    std::env::set_var(KEY, "everything");
    assert!(mode().is_err(), "an unknown value is refused, not ignored");
    std::env::remove_var(KEY);
}
