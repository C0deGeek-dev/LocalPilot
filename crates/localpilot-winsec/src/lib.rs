//! Owner-only Windows named pipes and contained command trees (ADR-0189).
//!
//! Tokio creates a named pipe with the default security descriptor, whose
//! DACL grants read access to Everyone and to the anonymous account. This
//! crate creates each pipe instance with a protected DACL whose only entry is
//! the current user's SID, and reads a live pipe's DACL back for tests and
//! diagnostics.
//!
//! It is the one crate in the workspace allowed unsafe code, and only its
//! `ffi` module uses it; everything it exports is safe. On other platforms it
//! is empty.

#![deny(
    unsafe_code,
    reason = "Win32 security and containment calls; only the ffi module allows it (ADR-0189)"
)]

#[cfg(windows)]
mod ffi;

#[cfg(windows)]
pub use pipes::{canonical_sddl, current_user_sid, owner_only_pipe, owner_trustee, pipe_dacl};

#[cfg(windows)]
pub use ffi::job::{spawn_in_job, ProcessJob};

/// The SDDL of a protected DACL whose only entry grants `sid` full access: no
/// inherited entries, no Everyone, no anonymous, no SYSTEM. Plain text, so it
/// builds (and is tested) on every platform.
#[must_use]
pub fn owner_only_sddl(sid: &str) -> String {
    format!("D:P(A;;GA;;;{sid})")
}

#[cfg(windows)]
mod pipes {
    use std::io;

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    use crate::{ffi, owner_only_sddl};

    /// The current process user's SID, in string form (`S-1-5-21-...`).
    ///
    /// # Errors
    /// The process token cannot be opened or read.
    pub fn current_user_sid() -> io::Result<String> {
        ffi::current_user_sid()
    }

    /// Create the pipe instance `name` with `options`, admitting only the
    /// current user. Use it for every instance of a pipe, the first and each
    /// replacement: an instance's DACL is its own.
    ///
    /// # Errors
    /// The user's SID cannot be read, the descriptor cannot be built, or the
    /// pipe cannot be created (for example, a `first_pipe_instance` claim on a
    /// live pipe).
    pub fn owner_only_pipe(options: &ServerOptions, name: &str) -> io::Result<NamedPipeServer> {
        let sid = ffi::current_user_sid()?;
        let sd = ffi::SecurityDescriptor::from_sddl(&owner_only_sddl(&sid))?;
        ffi::create_pipe(options, name, &sd)
    }

    /// A live pipe instance's DACL, as SDDL (for example
    /// `D:P(A;;FA;;;S-1-5-21-...)`; the system stores generic rights mapped
    /// to the pipe's own).
    ///
    /// # Errors
    /// The pipe's security information cannot be read.
    pub fn pipe_dacl(server: &NamedPipeServer) -> io::Result<String> {
        ffi::dacl_sddl(server)
    }

    /// `sddl` as the system writes it back: canonical, with well-known SIDs
    /// abbreviated (the built-in Administrator prints as `LA`, for example).
    ///
    /// # Errors
    /// `sddl` is not a valid security descriptor.
    pub fn canonical_sddl(sddl: &str) -> io::Result<String> {
        ffi::canonical_sddl(sddl)
    }

    /// The current user as the system names it in a DACL's text: its SID
    /// string, or the alias Windows prints for a well-known account. Compare
    /// against this, not the raw SID, when reading [`pipe_dacl`].
    ///
    /// # Errors
    /// As [`current_user_sid`] and [`canonical_sddl`].
    pub fn owner_trustee() -> io::Result<String> {
        let text = ffi::canonical_sddl(&owner_only_sddl(&ffi::current_user_sid()?))?;
        Ok(text
            .rsplit(";;;")
            .next()
            .unwrap_or_default()
            .trim_end_matches(')')
            .to_owned())
    }
}

#[cfg(all(test, windows))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

    fn unique(tag: &str) -> String {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!(r"\\.\pipe\lp-winsec-{tag}-{}-{n}", std::process::id())
    }

    /// The DACL admits exactly one principal, the owner, and is protected.
    fn assert_owner_only(dacl: &str, sid: &str) {
        assert!(dacl.starts_with("D:P"), "not protected: {dacl}");
        assert_eq!(
            dacl.matches("(A;").count(),
            1,
            "more than one allow entry: {dacl}"
        );
        assert_eq!(
            dacl.matches("(D;").count(),
            0,
            "unexpected deny entry: {dacl}"
        );
        assert!(
            dacl.ends_with(&format!(";;;{sid})")),
            "not the owner's entry: {dacl}"
        );
        for other in [";WD)", ";AN)", ";SY)", ";BA)", ";BU)"] {
            assert!(!dacl.contains(other), "{other} in {dacl}");
        }
    }

    #[test]
    fn the_owner_trustee_is_the_sid_or_its_alias() {
        // A well-known account (the built-in Administrator a CI runner uses)
        // prints as an alias such as `LA`; anyone else as the SID string.
        let sid = current_user_sid().unwrap();
        let trustee = owner_trustee().unwrap();
        let alias = trustee.len() == 2 && trustee.chars().all(|c| c.is_ascii_uppercase());
        assert!(trustee == sid || alias, "{trustee} for {sid}");
        // The built-in Administrator's SID, whoever runs this, prints as LA.
        let domain = sid.rsplit_once('-').map(|(d, _)| d).unwrap();
        if domain.starts_with("S-1-5-21-") {
            let admin = canonical_sddl(&owner_only_sddl(&format!("{domain}-500"))).unwrap();
            assert!(admin.ends_with(";;;LA)"), "{admin}");
        }
    }

    #[test]
    fn the_sid_is_the_users_string_sid() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "{sid}");
    }

    #[tokio::test]
    async fn every_instance_admits_only_the_owner_and_the_owner_can_connect() {
        // The owner as Windows prints it: the SID, or an alias such as `LA`.
        let sid = owner_trustee().unwrap();
        let name = unique("owner");
        let first = owner_only_pipe(ServerOptions::new().first_pipe_instance(true), &name).unwrap();
        let dacl = pipe_dacl(&first).unwrap();
        println!("owner-only DACL: {dacl}");
        assert_owner_only(&dacl, &sid);
        // A replacement instance, as a server makes after each accept.
        let second = owner_only_pipe(&ServerOptions::new(), &name).unwrap();
        assert_owner_only(&pipe_dacl(&second).unwrap(), &sid);
        // The owner itself still connects.
        let client = ClientOptions::new().open(&name);
        assert!(client.is_ok(), "{client:?}");
        first.connect().await.unwrap();
    }

    #[tokio::test]
    async fn a_second_first_instance_claim_is_still_refused() {
        let name = unique("single");
        let _first =
            owner_only_pipe(ServerOptions::new().first_pipe_instance(true), &name).unwrap();
        assert!(owner_only_pipe(ServerOptions::new().first_pipe_instance(true), &name).is_err());
    }

    #[test]
    fn the_default_dacl_this_replaces_is_wider() {
        // Why this crate exists: tokio's default instance is not owner-only.
        let name = unique("default");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _g = rt.enter();
        let plain = ServerOptions::new().create(&name).unwrap();
        let dacl = pipe_dacl(&plain).unwrap();
        println!("default DACL: {dacl}");
        assert!(
            dacl.matches("(A;").count() > 1 || dacl.contains(";WD)") || dacl.contains(";AN)"),
            "the default DACL admitted only one principal: {dacl}"
        );
    }
}

#[cfg(test)]
mod portable_tests {
    #[test]
    fn the_descriptor_is_protected_and_names_only_the_owner() {
        let sddl = super::owner_only_sddl("S-1-5-21-1-2-3-1001");
        assert_eq!(sddl, "D:P(A;;GA;;;S-1-5-21-1-2-3-1001)");
        assert_eq!(sddl.matches("(A;").count(), 1);
    }
}
