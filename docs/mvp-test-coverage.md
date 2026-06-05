# Required MVP test coverage

Maps each [08-testing.md](08-testing.md) "Required MVP Tests" entry to a concrete
test in the workspace. All run offline by default; live provider tests are opt-in
behind `UNSHACKLED_LIVE_TESTS`.

## Config (`unshackled-config`, `tests/config.rs`)

| Required | Test |
| --- | --- |
| default config loads | `default_config_loads` |
| project overrides user | `project_overrides_user` |
| env overrides project | `env_overrides_project` |
| CLI overrides env | `cli_overrides_env` |
| secrets redacted in debug | `secrets_never_appear_in_debug_output` |

## Provider (`unshackled-llm`)

| Required | Test |
| --- | --- |
| text request translates | `openai::tests::request_body_round_trips_reasoning_for_continuity`, `tests/http.rs::streams_text_from_a_chunked_sse_response` |
| tool schema translates | `tools::tests` (schema gen) + `openai` tool translation |
| streaming text parses | `openai::tests::parses_streaming_text_deltas` |
| streaming tool call parses | `openai::tests::assembles_incremental_tool_call_arguments` |
| reasoning events parse | `openai::tests::parses_reasoning_and_usage` |
| malformed stream → typed error | `openai::tests::malformed_chunk_yields_typed_decode_error`, `tests/http.rs::malformed_stream_body_yields_a_typed_decode_error` |
| quota reset metadata classified | `error::tests::distinguishes_quota_from_rate_limit`, `tests/http.rs::quota_exhaustion_is_classified_with_reset_metadata` |

## Tools (`unshackled-tools`, `tests/tools.rs`)

| Required | Test |
| --- | --- |
| read in workspace / deny outside | `read_file_inside_workspace_is_allowed_and_outside_is_denied` |
| write in workspace / deny outside | `write_file_in_workspace_and_denied_outside` |
| edit exact / reject ambiguous | `edit_file_exact_match_and_rejects_ambiguous` |
| shell read-only / destructive denied | `run_shell_allows_read_only_and_denies_destructive_non_interactive` |

## Harness (`unshackled-harness`)

| Required | Test |
| --- | --- |
| parse valid brief / reject missing section | `brief::tests::*` |
| parse valid progress / reject duplicate step | `progress::tests::*` |
| next incomplete step / mark complete | `worker::tests::selects_the_first_incomplete_step`, `progress::tests::mark_complete_updates_a_step` |
| attempt counter increment | `worker::tests` (StepLoop) |
| rule retry / discard path | `worker::tests::retry_keeps_context_within_budget`, `discard_resets_context_within_budget` |
| replan cap | `worker::tests::replans_are_capped` |
| golden-task smoke | `tests/evals.rs` golden tasks |
| quota pause/resume at a step boundary | `unshackled-quota::tests::each_safety_gate_blocks_resume` |

## Recovery (`unshackled-recovery`)

| Required | Test |
| --- | --- |
| slash flood outside code detected | `detect::tests::slash_flood_outside_code_is_detected` |
| slash-like inside fenced code not detected | `detect::tests::slash_like_content_inside_fenced_code_is_not_flagged` |
| repeated-token loop only after threshold | `detect::tests::repeated_token_loop_only_after_threshold` |
| malformed tool calls trigger recovery | `engine::tests::malformed_tool_call_triggers_a_repair_attempt` |
| exhausted recovery cannot complete a step | `engine::tests::exhausted_recovery_marks_degraded_and_blocks_steps` |

## Context (`unshackled-harness`, `unshackled-localmind`)

| Required | Test |
| --- | --- |
| compaction preserves tool-result pairing | `compaction::tests::compaction_preserves_tool_result_pairing` |
| compaction preserves step contract | covered by `resume` e2e (step survives a turn) |
| accepted memory can be searched and deleted | `localmind_store::tests::accepted_memory_can_be_listed_and_deleted`, `crates/unshackled-cli/tests/memory.rs::memory_inspect_delete_and_disable` |
| stale memory not injected when disabled | `crates/unshackled-cli/tests/memory.rs::memory_inspect_delete_and_disable` |

## Store (`unshackled-store`)

| Required | Test |
| --- | --- |
| transcript write/read round trip | `tests::transcript_write_read_roundtrip` |
| interrupted write leaves no corrupt session | `tests::interrupted_write_leaves_no_corrupt_session` |
| redaction before persistence | `tests::redaction_is_applied_before_persistence` |
