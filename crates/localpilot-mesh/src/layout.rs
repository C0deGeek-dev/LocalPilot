//! Where each mailbox file lives (spec L-1, L-2).

use std::path::{Path, PathBuf};

/// The mailbox directory name, at the root of the anchor working tree.
pub const MAILBOX_DIR: &str = ".pair-programming";
/// The schema-1 pointer; for schema 2 it holds [`SENTINEL_PREFIX`] instead.
pub const ACTIVE_V1: &str = "active.json";
/// The schema-2 pointer.
pub const ACTIVE_V2: &str = "active.v2.json";
/// Schema-1 session record.
pub const SESSION_V1: &str = "session.json";
/// Schema-2 session record.
pub const SESSION_V2: &str = "session.v2.json";
/// The text that marks `active.json` as a schema-2 sentinel (spec S-7).
pub const SENTINEL_PREFIX: &str = "N-PARTY SESSION ";

/// One mailbox: the `.pair-programming` directory of an anchor tree.
#[derive(Debug, Clone)]
pub struct Mailbox {
    base: PathBuf,
}

impl Mailbox {
    /// The mailbox of the working tree rooted at `anchor`.
    #[must_use]
    pub fn at(anchor: &Path) -> Self {
        Self {
            base: anchor.join(MAILBOX_DIR),
        }
    }

    /// The mailbox directory itself.
    #[must_use]
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// The state lock (spec L-6).
    #[must_use]
    pub fn state_lock(&self) -> PathBuf {
        self.base.join(".state.lock")
    }

    #[must_use]
    pub fn active_v1(&self) -> PathBuf {
        self.base.join(ACTIVE_V1)
    }

    #[must_use]
    pub fn active_v2(&self) -> PathBuf {
        self.base.join(ACTIVE_V2)
    }

    /// A session's directory.
    #[must_use]
    pub fn session_dir(&self, sid: &str) -> PathBuf {
        self.base.join("sessions").join(sid)
    }

    fn area(&self, sid: &str, area: &str, role: &str, ext: &str) -> PathBuf {
        self.session_dir(sid)
            .join(area)
            .join(format!("{role}.{ext}"))
    }

    /// A participant's journal; only that participant writes it.
    #[must_use]
    pub fn journal(&self, sid: &str, role: &str) -> PathBuf {
        self.area(sid, "journal", role, "jsonl")
    }

    /// A participant's role lock (spec L-6).
    #[must_use]
    pub fn role_lock(&self, sid: &str, role: &str) -> PathBuf {
        self.area(sid, "journal", role, "lock")
    }

    #[must_use]
    pub fn latest(&self, sid: &str, role: &str) -> PathBuf {
        self.area(sid, "latest", role, "json")
    }

    #[must_use]
    pub fn cursor(&self, sid: &str, role: &str) -> PathBuf {
        self.area(sid, "cursor", role, "json")
    }

    #[must_use]
    pub fn health(&self, sid: &str, role: &str) -> PathBuf {
        self.area(sid, "health", role, "json")
    }

    #[must_use]
    pub fn endpoint(&self, sid: &str, role: &str) -> PathBuf {
        self.area(sid, "endpoints", role, "json")
    }

    #[must_use]
    pub fn pushes(&self, sid: &str, role: &str) -> PathBuf {
        self.area(sid, "pushes", role, "jsonl")
    }

    #[must_use]
    pub fn receipts(&self, sid: &str, role: &str) -> PathBuf {
        self.area(sid, "receipts", role, "jsonl")
    }

    /// `path` relative to the mailbox, for messages that must not depend on
    /// where the repository lives.
    #[must_use]
    pub fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.base).map_or_else(
            |_| path.display().to_string(),
            |p| p.to_string_lossy().replace('\\', "/"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_the_specified_layout() {
        let mb = Mailbox::at(Path::new("/repo"));
        let rel = |p: PathBuf| mb.relative(&p);
        assert_eq!(
            rel(mb.journal("S", "codex")),
            "sessions/S/journal/codex.jsonl"
        );
        assert_eq!(
            rel(mb.role_lock("S", "codex")),
            "sessions/S/journal/codex.lock"
        );
        assert_eq!(rel(mb.latest("S", "codex")), "sessions/S/latest/codex.json");
        assert_eq!(rel(mb.cursor("S", "codex")), "sessions/S/cursor/codex.json");
        assert_eq!(rel(mb.health("S", "codex")), "sessions/S/health/codex.json");
        assert_eq!(
            rel(mb.endpoint("S", "codex")),
            "sessions/S/endpoints/codex.json"
        );
        assert_eq!(
            rel(mb.pushes("S", "codex")),
            "sessions/S/pushes/codex.jsonl"
        );
        assert_eq!(
            rel(mb.receipts("S", "codex")),
            "sessions/S/receipts/codex.jsonl"
        );
        assert_eq!(rel(mb.state_lock()), ".state.lock");
        assert_eq!(rel(mb.active_v1()), "active.json");
        assert_eq!(rel(mb.active_v2()), "active.v2.json");
    }
}
