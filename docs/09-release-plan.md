# Release Plan

> **Historical — superseded.** This is the pre-1.0 release plan. LocalPilot ships
> as part of the coordinated LocalX release train (currently v2.0.2); the live
> ceremony lives in the private LocalHub repo (`policies/coordinated-release.md`).
> Kept for history.

## Versioning

Use semantic versioning after `0.1.0`.

Pre-1.0 compatibility:

- CLI commands may change
- config schema may change
- documented migration notes are required

## Release Channels

### Nightly

Built from main. No stability guarantee.

### Alpha

Feature-complete enough for real users. Known gaps documented.

### Stable

Requires:

- at least two provider implementations
- mature permission model
- stable config schema
- installer coverage
- documented extension API

## Public Alpha Criteria

- clean-room audit complete
- no private endpoints
- no prohibited product framing
- `cargo test --workspace` green
- basic TUI usable
- harness can complete a small repo task
- docs explain provider setup
- security model documented

## Installer Targets

V1:

- cargo install
- GitHub release archives
- PowerShell install script
- shell install script

Later:

- Homebrew tap
- winget
- Scoop
- npm wrapper package if needed

## Release Checklist

Before tagging:

- update changelog
- run full test matrix
- run dependency audit
- run clean-room scan
- verify license files
- verify no `.env` or token-like content
- verify generated archives contain expected files
- create signed tag if signing is configured

## Clean-Room Scan Terms

Scan for:

- "source-map"
- "leaked"
- "free build"
- "fork of"
- "private endpoint"
- vendor product names used as identity
- personal absolute paths
- browser-cookie auth for hosted models

Provider names in official API docs are allowed when used only as integration
labels.

