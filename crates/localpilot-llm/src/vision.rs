//! Vision-capability resolution.
//!
//! A model's vision (image-input) support resolves from two best-effort signals
//! with a fixed precedence — an explicit per-provider config declaration wins, a
//! discovery-time server probe is the fallback, and the default is no vision.
//! Config wins so a user can always override a wrong probe; an unknown probe
//! (`None`) never asserts a capability.

/// Where a model's resolved vision capability was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisionSource {
    /// An explicit per-provider `supports_vision` config declaration (or a
    /// LocalBox auto-declaration on a vision launch).
    Config,
    /// A best-effort, read-only discovery-time server probe.
    Probe,
    /// Neither declared nor probed — the default (no vision).
    Default,
}

impl VisionSource {
    /// A stable lowercase token for surfacing the source (e.g. in `models` JSON).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            VisionSource::Config => "config",
            VisionSource::Probe => "probe",
            VisionSource::Default => "default",
        }
    }
}

/// Resolve a model's vision capability **and** report which signal decided it,
/// with the precedence config > probe > false. An explicit config value always
/// wins (even `Some(false)` over a probe `Some(true)`); otherwise the probe
/// decides; otherwise the default is no vision.
#[must_use]
pub fn resolve_vision_with_source(
    config: Option<bool>,
    probe: Option<bool>,
) -> (bool, VisionSource) {
    match (config, probe) {
        (Some(value), _) => (value, VisionSource::Config),
        (None, Some(value)) => (value, VisionSource::Probe),
        (None, None) => (false, VisionSource::Default),
    }
}

/// Resolve a model's vision capability with the precedence config > probe > false.
/// The bool-only convenience over [`resolve_vision_with_source`].
#[must_use]
pub fn resolve_vision(config: Option<bool>, probe: Option<bool>) -> bool {
    resolve_vision_with_source(config, probe).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_wins_over_the_probe_in_both_directions() {
        // A config declaration is authoritative, even against a contrary probe.
        assert_eq!(
            resolve_vision_with_source(Some(true), Some(false)),
            (true, VisionSource::Config)
        );
        assert_eq!(
            resolve_vision_with_source(Some(false), Some(true)),
            (false, VisionSource::Config)
        );
    }

    #[test]
    fn the_probe_decides_when_config_is_absent() {
        assert_eq!(
            resolve_vision_with_source(None, Some(true)),
            (true, VisionSource::Probe)
        );
        assert_eq!(
            resolve_vision_with_source(None, Some(false)),
            (false, VisionSource::Probe)
        );
    }

    #[test]
    fn neither_signal_is_the_default_off() {
        assert_eq!(
            resolve_vision_with_source(None, None),
            (false, VisionSource::Default)
        );
        assert!(!resolve_vision(None, None));
    }

    #[test]
    fn source_tokens_are_stable() {
        assert_eq!(VisionSource::Config.as_str(), "config");
        assert_eq!(VisionSource::Probe.as_str(), "probe");
        assert_eq!(VisionSource::Default.as_str(), "default");
    }
}
