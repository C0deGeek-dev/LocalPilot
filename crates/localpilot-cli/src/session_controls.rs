//! Session-only work-limit and action-mode controls for the full-screen host.
//!
//! The slash commands `/limits` and `/actions` are a thin layer over the
//! runtime's existing switches. This module holds the wording and the one rule
//! that decides whether an action change is allowed, so both are testable
//! without a terminal. Nothing here writes configuration or a stored grant: a
//! change lasts until the session ends.

use localpilot_config::WorkLimitMode;
use localpilot_sandbox::Profile;
use localpilot_slash::{ActionSetting, Profile as UiProfile, WorkLimitSetting};
use localpilot_terminal_ui::{AppModel, SessionControl};

/// The config mode a `/limits` argument selects.
pub(crate) const fn limit_mode(setting: WorkLimitSetting) -> WorkLimitMode {
    match setting {
        WorkLimitSetting::Automatic => WorkLimitMode::Automatic,
        WorkLimitSetting::Wide => WorkLimitMode::Wide,
        WorkLimitSetting::VeryWide => WorkLimitMode::VeryWide,
        WorkLimitSetting::Off => WorkLimitMode::Off,
    }
}

/// The spelling `/limits` accepts for a mode.
pub(crate) const fn limit_label(mode: WorkLimitMode) -> &'static str {
    match mode {
        WorkLimitMode::Automatic => "automatic",
        WorkLimitMode::Wide => "wide",
        WorkLimitMode::VeryWide => "very-wide",
        WorkLimitMode::Off => "off",
    }
}

/// What `/limits` with no argument prints.
pub(crate) fn describe_limits(
    effective: WorkLimitMode,
    configured: WorkLimitMode,
    changed_here: bool,
) -> String {
    let source = if changed_here {
        format!(
            "set in this session; configured: {}",
            limit_label(configured)
        )
    } else {
        "from configuration".to_string()
    };
    format!(
        "work limits: {} ({source}). /limits automatic|wide|very-wide|off changes it for this session only",
        limit_label(effective)
    )
}

/// The name an action profile goes by in `/actions`.
pub(crate) const fn action_label(profile: Profile) -> &'static str {
    match profile {
        Profile::Default => "Normal",
        Profile::Unrestricted => "Accept all",
        Profile::Relaxed => "relaxed profile",
        Profile::Bypass => "BYPASS profile",
        Profile::ReadOnly => "read-only",
    }
}

/// What `/actions` with no argument prints.
pub(crate) fn describe_actions(current: Profile, changed_here: bool) -> String {
    let source = if changed_here {
        "set in this session"
    } else {
        "from launch and configuration"
    };
    format!(
        "actions: {} ({source}). /actions normal|accept-all changes it for this session only; /relaxed and /bypass remain the narrower older profiles",
        action_label(current)
    )
}

/// The slash argument of the action mode a profile stands for, or empty when
/// none of the listed choices does (an older, narrower profile).
const fn action_argument(profile: Profile) -> &'static str {
    match profile {
        Profile::Default => "normal",
        Profile::Unrestricted => "accept-all",
        Profile::Relaxed | Profile::Bypass | Profile::ReadOnly => "",
    }
}

/// Tell the settings screen what the work limits are now, so its row and list
/// show the runtime's value rather than the last one asked for.
pub(crate) fn publish_limits(
    app: &mut AppModel,
    effective: WorkLimitMode,
    configured: WorkLimitMode,
    changed_here: bool,
) {
    let value = if changed_here {
        format!(
            "{} (this session; configured: {})",
            limit_label(effective),
            limit_label(configured)
        )
    } else {
        format!("{} (from configuration)", limit_label(effective))
    };
    app.set_session_control(
        SessionControl::WorkLimits,
        limit_label(effective),
        &value,
        !changed_here,
    );
}

/// Tell the settings screen what the action mode is now.
pub(crate) fn publish_actions(app: &mut AppModel, current: Profile, changed_here: bool) {
    let source = if changed_here {
        "this session"
    } else {
        "launch and configuration"
    };
    let value = format!("{} ({source})", action_label(current));
    app.set_session_control(
        SessionControl::Actions,
        action_argument(current),
        &value,
        !changed_here,
    );
}

/// The outcome of asking for an action mode.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ActionChange {
    /// Apply this profile; `warn` when the notice is a caution.
    Apply {
        profile: UiProfile,
        notice: String,
        warn: bool,
    },
    /// Leave everything as it is.
    Refuse(String),
}

/// Decide an `/actions` change. A read-only session never widens: that
/// restriction was chosen at launch or in configuration, and a session command
/// is not the place to lift it.
pub(crate) fn plan_action_change(current: Profile, setting: ActionSetting) -> ActionChange {
    if current == Profile::ReadOnly {
        return ActionChange::Refuse(
            "actions not changed: this session is read-only, and /actions does not lift that. Start a new session without --permission readonly to change it".to_string(),
        );
    }
    match setting {
        ActionSetting::Normal => ActionChange::Apply {
            profile: UiProfile::Default,
            notice: "actions: Normal (in force from the next tool call; this session only)"
                .to_string(),
            warn: false,
        },
        ActionSetting::AcceptAll => ActionChange::Apply {
            profile: UiProfile::Unrestricted,
            notice: "actions: Accept all (in force from the next tool call; this session only). Tools can modify or delete files outside the workspace, and subprocesses can use the network with your OS account's permissions. /actions normal turns it off".to_string(),
            warn: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_all_is_the_unrestricted_profile_and_normal_is_default() {
        let ActionChange::Apply { profile, warn, .. } =
            plan_action_change(Profile::Default, ActionSetting::AcceptAll)
        else {
            panic!("accept-all should apply");
        };
        assert_eq!(profile, UiProfile::Unrestricted);
        assert!(warn);
        let ActionChange::Apply { profile, warn, .. } =
            plan_action_change(Profile::Unrestricted, ActionSetting::Normal)
        else {
            panic!("normal should apply");
        };
        assert_eq!(profile, UiProfile::Default);
        assert!(!warn);
    }

    #[test]
    fn a_read_only_session_is_never_widened_or_changed() {
        for setting in [ActionSetting::Normal, ActionSetting::AcceptAll] {
            assert!(matches!(
                plan_action_change(Profile::ReadOnly, setting),
                ActionChange::Refuse(_)
            ));
        }
    }

    #[test]
    fn the_query_names_a_precise_legacy_profile_instead_of_a_mode() {
        assert!(describe_actions(Profile::Relaxed, false).contains("relaxed profile"));
        assert!(describe_actions(Profile::Bypass, true).contains("BYPASS profile"));
        assert!(describe_actions(Profile::Unrestricted, true)
            .contains("Accept all (set in this session)"));
        assert!(describe_actions(Profile::Default, false).contains("Normal (from launch"));
    }

    #[test]
    fn the_limit_query_shows_effective_mode_and_source() {
        let kept = describe_limits(WorkLimitMode::Automatic, WorkLimitMode::Automatic, false);
        assert!(kept.contains("automatic (from configuration)"));
        let changed = describe_limits(WorkLimitMode::Wide, WorkLimitMode::Automatic, true);
        assert!(changed.contains("wide (set in this session; configured: automatic)"));
    }

    #[test]
    fn every_limit_setting_maps_to_its_config_mode() {
        for (setting, mode) in [
            (WorkLimitSetting::Automatic, WorkLimitMode::Automatic),
            (WorkLimitSetting::Wide, WorkLimitMode::Wide),
            (WorkLimitSetting::VeryWide, WorkLimitMode::VeryWide),
            (WorkLimitSetting::Off, WorkLimitMode::Off),
        ] {
            assert_eq!(limit_mode(setting), mode);
            assert_eq!(limit_label(mode), setting.label());
        }
    }
}
