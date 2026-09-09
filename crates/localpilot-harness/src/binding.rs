//! The identity a plan records for the brief it was built against.
//!
//! A plan is only meaningful relative to the requirements it was generated
//! from, so `PROGRESS.md` carries the revision of the `brief.md` that produced
//! it. Comparing the recorded revision with the brief on disk is what makes
//! "this plan no longer belongs to this brief" a fact rather than a guess.
//!
//! Three properties this format has to hold, because it is written into a
//! document that outlives the build that wrote it:
//!
//! * **The revision is over the parsed brief, not its bytes.** Byte hashing
//!   would report a brief as changed when an editor rewrites its line endings,
//!   adds trailing whitespace, or reflows a blank line — a false stale on every
//!   project shared between Windows and Unix, and one the user cannot see the
//!   cause of. Canonicalising through the parsed document means the revision
//!   changes exactly when the requirements change.
//! * **The canonical form is unambiguous and frozen.** Every boundary in it is
//!   a declared byte length, so no item's content can be mistaken for a
//!   delimiter. `v1` never changes meaning; a different canonicalisation is
//!   `v2`. It is deliberately *not* the document's rendered text: tying plan
//!   validity to a presentation function would mean that adjusting the blank
//!   line between two sections silently stales every plan on every machine —
//!   the same false-stale failure, wearing a different hat.
//! * **A binding this build cannot interpret is its own answer.** An unknown
//!   algorithm or version is not malformed (the document is fine), not stale
//!   (that cannot be known), and certainly not current. It is
//!   [`BindingSupport::Unsupported`], which is what lets a future `v2` ship
//!   without making today's plans look stale to an older build.

use sha2::{Digest, Sha256};

use crate::brief::Brief;

/// The tag written into the document: the hash, then the canonicalisation
/// version. The version is the one that can actually change — the hash is just
/// SHA-256 — so it is what the tag carries.
const TAG_V1: &str = "sha256-v1";

/// The identity of one brief revision, as written into `PROGRESS.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefRevision(String);

/// What a recorded binding means to this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingSupport {
    /// This build understands the binding and it matches the current brief.
    Current,
    /// This build understands the binding and it is for different requirements.
    Stale,
    /// This build does not understand the tag — a newer canonicalisation, or a
    /// hand-edited value. Whether the plan matches is unknowable, so it is
    /// neither `Current` nor `Stale`.
    Unsupported,
}

impl BriefRevision {
    /// Derive the revision of `brief` under the current canonicalisation.
    #[must_use]
    pub fn of(brief: &Brief) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(canonical_v1(brief));
        Self(format!("{TAG_V1}:{:x}", hasher.finalize()))
    }

    /// The value as written into the document: `sha256-v1:<64 hex>`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Classify a binding recorded in a plan against this revision.
    #[must_use]
    pub fn classify(&self, recorded: &str) -> BindingSupport {
        let recorded = recorded.trim();
        let Some((tag, digest)) = recorded.split_once(':') else {
            return BindingSupport::Unsupported;
        };
        if tag != TAG_V1 {
            return BindingSupport::Unsupported;
        }
        // A `v1` tag whose digest is not a 64-character LOWERCASE hex string was
        // not written by this format. The case matters: `v1` is frozen, it emits
        // lowercase, and accepting uppercase would mean the same digest compares
        // unequal and gets reported as *stale* — telling the user their
        // requirements changed, which is a claim about their work rather than
        // about the file. Unsupported is the honest answer for anything this
        // format did not write.
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return BindingSupport::Unsupported;
        }
        if recorded == self.0 {
            BindingSupport::Current
        } else {
            BindingSupport::Stale
        }
    }
}

impl std::fmt::Display for BriefRevision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Canonicalisation `v1`, frozen.
///
/// Layout, for each field in this exact order — name, summary, requirements,
/// constraints, non_goals, acceptance_criteria, risks:
///
/// ```text
/// <tag byte length> <tag bytes> <item count> [<item byte length> <item bytes>]*
/// ```
///
/// with every length a `u64` in big-endian bytes. Because every boundary is a
/// declared length rather than a separator, no field name or item content can
/// be confused for structure, and two different briefs cannot produce the same
/// bytes by rearranging where a delimiter appears to fall.
///
/// Items are trimmed, so incidental whitespace is not a requirements change.
/// `risks` participates: adding a rollback plan changes what the plan was built
/// to satisfy.
fn canonical_v1(brief: &Brief) -> Vec<u8> {
    let mut out = Vec::new();
    field(&mut out, "name", std::slice::from_ref(&brief.name));
    field(&mut out, "summary", std::slice::from_ref(&brief.summary));
    field(&mut out, "requirements", &brief.requirements);
    field(&mut out, "constraints", &brief.constraints);
    field(&mut out, "non_goals", &brief.non_goals);
    field(&mut out, "acceptance_criteria", &brief.acceptance_criteria);
    field(&mut out, "risks", &brief.risks);
    out
}

fn field(out: &mut Vec<u8>, tag: &str, items: &[String]) {
    length_prefixed(out, tag.as_bytes());
    out.extend_from_slice(&(items.len() as u64).to_be_bytes());
    for item in items {
        length_prefixed(out, item.trim().as_bytes());
    }
}

fn length_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brief() -> Brief {
        Brief {
            name: "thing".to_string(),
            summary: "Do the thing.".to_string(),
            requirements: vec!["It works".to_string()],
            constraints: vec!["Be small".to_string()],
            non_goals: vec!["World peace".to_string()],
            acceptance_criteria: vec!["A test passes".to_string()],
            risks: Vec::new(),
        }
    }

    const BRIEF_TEXT: &str = "# Brief: thing\n\n## Summary\n\nDo the thing.\n\n\
## Requirements\n\n- It works\n\n## Constraints\n\n- Be small\n\n\
## Non-Goals\n\n- World peace\n\n## Acceptance Criteria\n\n- A test passes\n";

    #[test]
    fn the_recorded_value_is_the_versioned_tag_and_a_full_digest() {
        let revision = BriefRevision::of(&brief());
        let (tag, digest) = revision.as_str().split_once(':').unwrap();
        assert_eq!(tag, "sha256-v1");
        assert_eq!(digest.len(), 64, "the full SHA-256, never a prefix");
        assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn semantically_identical_documents_hash_equally() {
        // LF, CRLF, and padded input describe the same requirements, so they
        // must produce the same revision. If they did not, every project shared
        // between Windows and Unix would show a plan that went stale for no
        // reason the user could see.
        let lf = Brief::parse(BRIEF_TEXT).unwrap();
        let crlf = Brief::parse(&BRIEF_TEXT.replace('\n', "\r\n")).unwrap();
        let padded = Brief::parse(&BRIEF_TEXT.replace("- It works", "- It works   ")).unwrap();

        assert_eq!(BriefRevision::of(&lf), BriefRevision::of(&crlf));
        assert_eq!(BriefRevision::of(&lf), BriefRevision::of(&padded));
    }

    #[test]
    fn reflowing_a_multiline_summary_moves_the_revision() {
        // The documented boundary of v1's invariance. `Brief::parse` keeps the
        // line breaks inside a summary, and `canonical_v1` trims only the whole
        // field, so re-wrapping the paragraph reaches the digest. Pinned here so
        // the spec's claim is enforced rather than merely asserted.
        let mut wrapped = brief();
        wrapped.summary = "Do the thing.\nThen stop.".to_string();
        let mut joined = brief();
        joined.summary = "Do the thing. Then stop.".to_string();
        assert_ne!(BriefRevision::of(&wrapped), BriefRevision::of(&joined));
    }

    #[test]
    fn every_typed_field_affects_the_revision() {
        let base = BriefRevision::of(&brief());
        /// One field, and a change to it that must move the revision.
        type Mutation = (&'static str, fn(&mut Brief));

        let mutations: Vec<Mutation> = vec![
            ("name", |b| b.name = "other".to_string()),
            ("summary", |b| b.summary = "Do something else.".to_string()),
            ("requirements", |b| {
                b.requirements = vec!["It works differently".to_string()];
            }),
            ("constraints", |b| {
                b.constraints = vec!["Be large".to_string()];
            }),
            ("non_goals", |b| b.non_goals = vec!["War".to_string()]),
            ("acceptance_criteria", |b| {
                b.acceptance_criteria = vec!["Two tests pass".to_string()];
            }),
            ("risks", |b| b.risks = vec!["It might not".to_string()]),
        ];
        for (field, mutate) in mutations {
            let mut changed = brief();
            mutate(&mut changed);
            assert_ne!(
                base,
                BriefRevision::of(&changed),
                "{field} is part of what the plan was built to satisfy"
            );
        }
    }

    #[test]
    fn moving_an_item_between_fields_changes_the_revision() {
        // Field tags are inside the canonical bytes, so the same text under a
        // different heading is a different brief.
        let mut moved = brief();
        moved.requirements = Vec::new();
        moved.constraints = vec!["Be small".to_string(), "It works".to_string()];
        assert_ne!(BriefRevision::of(&brief()), BriefRevision::of(&moved));
    }

    #[test]
    fn lengths_are_declared_so_items_cannot_be_confused_with_structure() {
        // Two briefs that would collide under a naive newline-joined form: the
        // difference is only where an item boundary falls.
        let mut split = brief();
        split.requirements = vec!["It works".to_string(), "quickly".to_string()];
        let mut joined = brief();
        joined.requirements = vec!["It works\nquickly".to_string()];
        assert_ne!(
            BriefRevision::of(&split),
            BriefRevision::of(&joined),
            "one item containing a newline is not two items"
        );
    }

    #[test]
    fn a_matching_binding_is_current_and_a_different_one_is_stale() {
        let revision = BriefRevision::of(&brief());
        assert_eq!(
            revision.classify(revision.as_str()),
            BindingSupport::Current
        );
        assert_eq!(
            revision.classify(&format!("  {revision}  ")),
            BindingSupport::Current,
            "surrounding whitespace in the document is not a mismatch"
        );

        let other = BriefRevision::of(&{
            let mut b = brief();
            b.requirements = vec!["Something else".to_string()];
            b
        });
        assert_eq!(revision.classify(other.as_str()), BindingSupport::Stale);
    }

    #[test]
    fn an_unknown_tag_is_unsupported_rather_than_stale() {
        // The whole point of the version: a plan written by a future
        // canonicalisation must not be reported as "your requirements changed".
        let revision = BriefRevision::of(&brief());
        for recorded in [
            "sha256-v2:0000000000000000000000000000000000000000000000000000000000000000",
            "blake3-v1:0000000000000000000000000000000000000000000000000000000000000000",
            "sha256:0000000000000000",
            "nonsense",
            "",
        ] {
            assert_eq!(
                revision.classify(recorded),
                BindingSupport::Unsupported,
                "{recorded:?} is not something this build can compare"
            );
        }
    }

    #[test]
    fn a_v1_tag_with_a_malformed_digest_is_unsupported_not_stale() {
        let revision = BriefRevision::of(&brief());
        assert_eq!(
            revision.classify("sha256-v1:not-a-digest"),
            BindingSupport::Unsupported
        );
        assert_eq!(
            revision.classify("sha256-v1:abc123"),
            BindingSupport::Unsupported,
            "a short digest was not written by this format"
        );
    }

    #[test]
    fn an_uppercased_digest_is_unsupported_rather_than_stale() {
        // `v1` emits lowercase. An uppercase digest is the same *number* but not
        // something this format wrote, and reporting it as stale would assert
        // that the user's requirements changed — which nothing here supports.
        let revision = BriefRevision::of(&brief());
        let shouted = revision
            .as_str()
            .to_uppercase()
            .replace("SHA256-V1", "sha256-v1");
        assert_ne!(shouted, revision.as_str());
        assert_eq!(revision.classify(&shouted), BindingSupport::Unsupported);
    }
}
