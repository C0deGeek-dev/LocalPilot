//! The action setting each host resolves, in one place.
//!
//! Pure functions only: nothing here prints or reads the environment except the
//! full-auto latch, so `doctor` and the tests can ask the same question the
//! launch sites ask.

use localpilot_sandbox::Profile;

/// Map the `--permission` / `--bypass` flags to a permission profile. `--bypass`
/// wins, and neither `bypass` nor `unrestricted` is ever the default. The flag
/// is validated by [`permission_value_parser`]; a name that still gets here
/// unrecognised fails closed, to `readonly`.
#[must_use]
pub fn resolve_profile(permission: Option<&str>, bypass: bool) -> Profile {
    if bypass {
        return Profile::Bypass;
    }
    match permission {
        None | Some("default") => Profile::Default,
        Some("relaxed") => Profile::Relaxed,
        Some("bypass") => Profile::Bypass,
        Some("unrestricted") => Profile::Unrestricted,
        Some(_) => Profile::ReadOnly,
    }
}

/// Map the configured `[permissions] profile` to a permission profile. The default
/// (no-argument) REPL has no `--permission`/`--bypass` flags to consult, so it reads
/// the profile from config instead of always assuming `Default` — otherwise a
/// project that opted into `profile = "bypass"` would still be prompted per action.
///
/// The sole caller is the `tui`-gated default REPL, so this is dead code in a
/// non-`tui` build; the test below still exercises it under either feature set.
#[must_use]
pub fn resolve_profile_from_config(config: &localpilot_config::Config) -> Profile {
    // A run launched with every prompt pre-approved outranks the configured
    // profile. It is a narrower, louder, more deliberate statement than a config
    // file: someone typed it on this run, having been shown what it costs.
    if crate::bypass::engaged() {
        return Profile::Unrestricted;
    }
    match config.permissions.profile {
        localpilot_config::PermissionProfile::Default => Profile::Default,
        localpilot_config::PermissionProfile::Relaxed => Profile::Relaxed,
        localpilot_config::PermissionProfile::Readonly => Profile::ReadOnly,
        localpilot_config::PermissionProfile::Bypass => Profile::Bypass,
        localpilot_config::PermissionProfile::Unrestricted => Profile::Unrestricted,
    }
}

/// Who can drive a session, which decides whether the configuration may widen it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// The user's own terminal or editor starts and watches the process.
    Local,
    /// A session that other devices can drive (`serve`, the auto-spawned daemon).
    Remote,
}

/// The profile the configured action mode selects, and why. `None` when no
/// action mode is configured. Pure: prints nothing.
pub(crate) fn action_decision(
    config: &localpilot_config::Config,
    permission: Option<&str>,
    bypass: bool,
) -> Option<(Profile, &'static str)> {
    let mode = config.permissions.action_mode?;
    Some(if permission == Some("readonly") {
        (Profile::ReadOnly, "explicit launch restriction")
    } else if crate::bypass::engaged() {
        (Profile::Unrestricted, "explicit full-auto launch")
    } else if permission.is_some() || bypass {
        (
            resolve_profile(permission, bypass),
            "explicit launch profile",
        )
    } else if config.permissions.profile == localpilot_config::PermissionProfile::Readonly {
        (Profile::ReadOnly, "configured read-only restriction")
    } else {
        match mode {
            localpilot_config::ActionMode::Normal => (Profile::Default, "Normal"),
            localpilot_config::ActionMode::AcceptAll => (Profile::Unrestricted, "Accept all"),
        }
    })
}

/// The profile a host of this reach runs with when no flag is given, and
/// whether the configuration asked for more than that host may grant. Pure.
#[must_use]
pub fn configured_host_profile(
    config: &localpilot_config::Config,
    reach: Reach,
) -> (Profile, bool) {
    match reach {
        Reach::Local => (
            action_decision(config, None, false)
                .map_or_else(|| resolve_profile_from_config(config), |(p, _)| p),
            false,
        ),
        Reach::Remote => {
            let configured = resolve_profile_from_config(config);
            let widened = config.permissions.action_mode
                == Some(localpilot_config::ActionMode::AcceptAll)
                || matches!(configured, Profile::Bypass | Profile::Unrestricted);
            let profile = match configured {
                Profile::Bypass | Profile::Unrestricted => Profile::Default,
                other => other,
            };
            (profile, widened)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_config::{ActionMode, Config, PermissionProfile};

    fn config(profile: PermissionProfile, mode: Option<ActionMode>) -> Config {
        let mut config = Config::default();
        config.permissions.profile = profile;
        config.permissions.action_mode = mode;
        config
    }

    #[test]
    fn narrowing_applies_to_every_host() {
        for profile in [
            PermissionProfile::Readonly,
            PermissionProfile::Default,
            PermissionProfile::Relaxed,
        ] {
            let config = config(profile, None);
            let (local, _) = configured_host_profile(&config, Reach::Local);
            let (remote, widened) = configured_host_profile(&config, Reach::Remote);
            assert_eq!(local, remote);
            assert!(!widened);
        }
        let readonly = config(PermissionProfile::Readonly, None);
        assert_eq!(
            configured_host_profile(&readonly, Reach::Remote).0,
            Profile::ReadOnly
        );
    }

    #[test]
    fn widening_applies_locally_and_never_to_served_sessions() {
        let accept_all = config(PermissionProfile::Default, Some(ActionMode::AcceptAll));
        assert_eq!(
            configured_host_profile(&accept_all, Reach::Local).0,
            Profile::Unrestricted
        );
        assert_eq!(
            configured_host_profile(&accept_all, Reach::Remote),
            (Profile::Default, true)
        );
        let bypass = config(PermissionProfile::Bypass, None);
        assert_eq!(
            configured_host_profile(&bypass, Reach::Local).0,
            Profile::Bypass
        );
        assert_eq!(
            configured_host_profile(&bypass, Reach::Remote),
            (Profile::Default, true)
        );
    }

    #[test]
    fn a_configured_readonly_restriction_beats_accept_all_everywhere() {
        let both = config(PermissionProfile::Readonly, Some(ActionMode::AcceptAll));
        assert_eq!(
            configured_host_profile(&both, Reach::Local).0,
            Profile::ReadOnly
        );
        assert_eq!(
            configured_host_profile(&both, Reach::Remote).0,
            Profile::ReadOnly
        );
    }
}
