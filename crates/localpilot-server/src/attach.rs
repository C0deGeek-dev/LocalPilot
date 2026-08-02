//! Connection-scoped session attach: bind one connection to exactly one
//! session, once.
//!
//! The server is *one connection = one session*. A connection names its session
//! a single time — open a fresh one, resume one by id, or resume one by name —
//! and is bound to it for its lifetime. This module is that dispatch seam: it
//! decodes a [`AttachTarget`] into the matching registry operation and returns
//! the bound [`SessionId`], from which the caller builds a
//! [`SessionHost`](crate::host::SessionHost) via
//! [`SessionRegistry::get`](crate::registry::SessionRegistry::get).
//!
//! A missing id or unknown name never panics: each resolves to a typed
//! [`AttachError`] a transport can render as a
//! [`ServerEvent::Error`](localpilot_rpc::ServerEvent::Error). The resume-by-id
//! path is guarded against a *never-seen* id: [`resume_by_id`] replays a durable
//! event log, and a non-existent log replays as empty, which would otherwise
//! mint an empty ghost session under the caller's id. This module confirms the
//! id is known to the store first and maps a miss to [`AttachError::UnknownId`].
//!
//! [`AttachTarget`]: localpilot_rpc::AttachTarget
//! [`resume_by_id`]: crate::registry::SessionRegistry::resume_by_id

use localpilot_core::SessionId;
use localpilot_rpc::AttachTarget;
use localpilot_store::Store;

use crate::registry::{RegistryError, SessionFactory, SessionRegistry};

/// A failure to bind a connection to its session.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AttachError {
    /// No session with this id is known to the workspace store, so there is
    /// nothing to resume. Distinct from a store or replay failure: the id is
    /// simply not one this workspace has ever recorded.
    #[error("no session with id {0} exists in this workspace")]
    UnknownId(SessionId),
    /// No session carries this name in the workspace store index.
    #[error("no session is named {0:?}")]
    UnknownName(String),
    /// The requested attach mode is one this server build does not understand —
    /// a newer client asked for a mode added to [`AttachTarget`] after this
    /// build. Reported instead of panicking on the non-exhaustive enum.
    ///
    /// [`AttachTarget`]: localpilot_rpc::AttachTarget
    #[error("this server build does not support the requested attach mode")]
    UnsupportedTarget,
    /// A registry operation failed — the factory could not build a runtime, the
    /// event log could not be read, or the id was already present.
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

/// Bind a connection to its session per the decoded [`AttachTarget`], returning
/// the [`SessionId`] it is now bound to.
///
/// Routes each target to the registry: `OpenNew` → [`open_new`], `ResumeId` →
/// [`resume_by_id`] (behind the known-id guard), `ResumeName` →
/// [`resume_by_name`]. The caller builds a
/// [`SessionHost`](crate::host::SessionHost) from
/// [`registry.get(id)`](SessionRegistry::get) with the returned id.
///
/// [`AttachTarget`]: localpilot_rpc::AttachTarget
/// [`open_new`]: SessionRegistry::open_new
/// [`resume_by_id`]: SessionRegistry::resume_by_id
/// [`resume_by_name`]: SessionRegistry::resume_by_name
///
/// # Errors
/// Returns [`AttachError::UnknownId`] / [`AttachError::UnknownName`] when the
/// requested session does not exist, [`AttachError::UnsupportedTarget`] for a
/// mode this build does not know, or [`AttachError::Registry`] wrapping any
/// underlying registry/store/factory failure. Never panics.
pub async fn attach(
    target: AttachTarget,
    registry: &SessionRegistry,
    factory: &dyn SessionFactory,
    store: &Store,
) -> Result<SessionId, AttachError> {
    match target {
        AttachTarget::OpenNew => Ok(registry.open_new(factory).await?),
        AttachTarget::ResumeId { session_id } => {
            if !session_is_known(store, session_id)? {
                return Err(AttachError::UnknownId(session_id));
            }
            Ok(registry.resume_by_id(session_id, factory).await?)
        }
        AttachTarget::ResumeName { name } => registry
            .resume_by_name(&name, store, factory)
            .await
            .map_err(|err| match err {
                // Normalise the registry's own unknown-name into the attach
                // vocabulary; every other registry failure stays wrapped.
                RegistryError::UnknownName(name) => AttachError::UnknownName(name),
                other => AttachError::Registry(other),
            }),
        // `AttachTarget` is `#[non_exhaustive]`: a mode added in a newer client
        // is a typed error here, not a panic.
        _ => Err(AttachError::UnsupportedTarget),
    }
}

/// Whether `id` names a session the workspace store has recorded. Mirrors how
/// resume-by-name resolves through the store index, so a resume-by-id sees the
/// same source of truth. Returns the registry error on a store read failure.
fn session_is_known(store: &Store, id: SessionId) -> Result<bool, RegistryError> {
    Ok(store.list_sessions()?.iter().any(|entry| entry.id == id))
}
