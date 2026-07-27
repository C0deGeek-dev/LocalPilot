//! Release version parsing and ordering.
//!
//! Moved here from the CLI's update command so the cache, the resolver, and the
//! updater all rank versions the same way. Two components ranking releases
//! differently is how a resolver picks a version the updater would not have
//! installed.

/// A pre-release channel, ordered `alpha < beta < rc < release`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Alpha,
    Beta,
    Rc,
}

impl Channel {
    /// The marker that separates the release core from the pre-release number.
    #[must_use]
    pub fn marker(self) -> &'static str {
        match self {
            Channel::Alpha => "-alpha.",
            Channel::Beta => "-beta.",
            Channel::Rc => "-rc.",
        }
    }

    fn rank(self) -> u64 {
        match self {
            Channel::Alpha => 0,
            Channel::Beta => 1,
            Channel::Rc => 2,
        }
    }
}

/// A parsed `major.minor.patch[-{alpha,beta,rc}.N]` version. A release (no
/// pre-release) sorts above its pre-releases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Option<(Channel, u64)>,
}

impl Version {
    /// Parse a version, tolerating a leading `v` and a trailing `git describe`
    /// suffix. Returns `None` when the core is not `major.minor[.patch]`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let core = text.trim().trim_start_matches('v');
        let channels = [Channel::Alpha, Channel::Beta, Channel::Rc];
        let split = channels.into_iter().find_map(|c| {
            core.split_once(c.marker())
                .map(|(rel, rest)| (rel, c, rest))
        });
        let (release, prerelease) = match split {
            Some((release, channel, rest)) => {
                // Take only the leading digits, dropping any `git describe`
                // suffix (e.g. `-26-gabc1234-dirty`).
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                (release, Some((channel, digits.parse().ok()?)))
            }
            // Drop any `git describe` suffix (e.g. `-2-gabc1234`).
            None => (core.split('-').next()?, None),
        };
        let mut parts = release.split('.');
        Some(Version {
            major: parts.next()?.parse().ok()?,
            minor: parts.next()?.parse().ok()?,
            patch: parts.next().unwrap_or("0").parse().ok()?,
            prerelease,
        })
    }

    /// Sort key: a release (`prerelease = None`) is newer than any of its
    /// pre-releases, which order `alpha < beta < rc` then by number.
    #[must_use]
    pub fn key(&self) -> (u64, u64, u64, u64, u64) {
        let (channel, number) = match self.prerelease {
            Some((channel, number)) => (channel.rank(), number),
            None => (u64::MAX, u64::MAX),
        };
        (self.major, self.minor, self.patch, channel, number)
    }

    /// The canonical `major.minor.patch[-channel.N]` rendering, without a `v`.
    /// This is the on-disk directory name, so it must round-trip through
    /// [`Version::parse`].
    #[must_use]
    pub fn to_dir_name(&self) -> String {
        match self.prerelease {
            Some((channel, number)) => format!(
                "{}.{}.{}{}{}",
                self.major,
                self.minor,
                self.patch,
                channel.marker(),
                number
            ),
            None => format!("{}.{}.{}", self.major, self.minor, self.patch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_outranks_its_prereleases() {
        let release = Version::parse("2.5.0").expect("parses");
        for pre in ["2.5.0-alpha.1", "2.5.0-beta.9", "2.5.0-rc.1"] {
            let pre = Version::parse(pre).expect("parses");
            assert!(
                release.key() > pre.key(),
                "{pre:?} should sort below the release"
            );
        }
    }

    #[test]
    fn channels_order_alpha_beta_rc() {
        let a = Version::parse("1.0.0-alpha.1").expect("parses");
        let b = Version::parse("1.0.0-beta.1").expect("parses");
        let c = Version::parse("1.0.0-rc.1").expect("parses");
        assert!(a.key() < b.key() && b.key() < c.key());
    }

    #[test]
    fn a_git_describe_suffix_is_ignored() {
        let plain = Version::parse("2.5.0").expect("parses");
        let described = Version::parse("v2.5.0-26-gabc1234-dirty").expect("parses");
        assert_eq!(
            plain, described,
            "a describe suffix must not change the version"
        );
    }

    #[test]
    fn a_leading_v_is_optional() {
        assert_eq!(Version::parse("v1.2.3"), Version::parse("1.2.3"));
    }

    #[test]
    fn a_missing_patch_defaults_to_zero() {
        assert_eq!(Version::parse("1.2"), Version::parse("1.2.0"));
    }

    #[test]
    fn nonsense_does_not_parse() {
        for bad in ["", "banana", "1.x.3", "v"] {
            assert!(Version::parse(bad).is_none(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn the_directory_name_round_trips() {
        for text in ["2.5.0", "1.0.0-alpha.1", "10.20.30-rc.7"] {
            let version = Version::parse(text).expect("parses");
            let name = version.to_dir_name();
            assert_eq!(name, text, "directory name should be canonical");
            assert_eq!(
                Version::parse(&name).as_ref(),
                Some(&version),
                "a cache directory name must parse back to the same version"
            );
        }
    }
}
