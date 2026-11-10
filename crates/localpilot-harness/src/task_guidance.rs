//! A record of the task guidance in each provider request.
//!
//! Guidance reaches a model by several routes: project instruction files a
//! context hook renders each turn, skill summaries and bodies a tool returned,
//! a fixed sentence in the system prompt. Each route knows its own source; none
//! knows what the finished request looks like after the prompt budget, a
//! compaction, or a recovery step. This module joins the two: every source that
//! was offered, and whether its text is in the request that was assembled.
//!
//! The record is read from the assembled request, not from the code that
//! intended to put the text there, so it cannot report guidance the model did
//! not receive. It is kept in memory for the running session and changes
//! nothing about a request. Only the turn loop's own requests are recorded;
//! a summarizer, judge or planning call carries no guidance of this kind.

use std::collections::VecDeque;

use localpilot_core::{ContentBlock, GuidanceSource, Message, Role, ToolUseId};
use sha2::{Digest, Sha256};

/// How many requests' records a session keeps. Older ones are dropped.
const MAX_REQUESTS: usize = 64;
/// How many guidance-carrying tool results a session follows. Older ones are
/// dropped; a session that asks for skills more often than this loses the
/// oldest from the record, never from the conversation.
const MAX_TOOL_RESULTS: usize = 256;
/// The shortest opening line that identifies a partly delivered segment. A
/// shorter line would match unrelated text.
const MIN_MARKER_CHARS: usize = 12;

/// A part of a context hook's text that is task guidance, with its source.
///
/// `text` is the part exactly as the hook emits it, so it can be looked up in
/// the request. A hook that bounds its output after rendering reports the
/// unbounded part here: a part the bound cut off then shows as not delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidanceSegment {
    pub source: GuidanceSource,
    pub text: String,
}

/// Where in a request a piece of guidance is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuidancePlacement {
    /// System text: the system prompt or the turn's system context.
    System,
    /// In the user's message, beside the question.
    BesideQuestion,
    /// The result of a tool the model called.
    ToolResult,
}

/// Whether a piece of guidance is in the assembled request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuidanceDelivery {
    /// All of it, unchanged.
    Intact,
    /// Part of it, or a changed version: cut by a budget, masked by a
    /// compaction, or annotated after the tool returned.
    Altered,
    /// None of it.
    Absent,
}

/// One piece of guidance in one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidanceEntry {
    pub source: GuidanceSource,
    pub placement: GuidancePlacement,
    /// SHA-256 of the text as its source produced it, in lowercase hex.
    pub content_sha256: String,
    /// The length of that text in characters.
    pub content_chars: usize,
    pub delivery: GuidanceDelivery,
}

/// The guidance offered for one assembled provider request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestGuidance {
    /// The position of this request among the session's turn-loop requests,
    /// from zero. Requests older than the kept window keep their number.
    pub request: u64,
    pub entries: Vec<GuidanceEntry>,
}

impl RequestGuidance {
    /// The entries whose text is in the request, whole or in part.
    pub fn delivered(&self) -> impl Iterator<Item = &GuidanceEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.delivery != GuidanceDelivery::Absent)
    }
}

/// Text looked up in the system and user text of a request.
struct TextSource {
    source: GuidanceSource,
    placement: GuidancePlacement,
    text: String,
}

/// A tool result looked up by its call id.
struct ToolSource {
    id: ToolUseId,
    sources: Vec<GuidanceSource>,
    sha256: String,
    chars: usize,
}

/// The session's guidance record: what is on offer, and what each request held.
#[derive(Default)]
pub(crate) struct TaskGuidanceLog {
    /// Set once, for the session: text in the system prompt.
    standing: Vec<TextSource>,
    /// Replaced every turn: what the context hooks contributed.
    turn: Vec<TextSource>,
    /// Grows with the session: tool results that carried guidance.
    tools: VecDeque<ToolSource>,
    requests: VecDeque<RequestGuidance>,
    next_request: u64,
    /// Requests captured since the current turn started.
    turn_requests: u64,
}

impl TaskGuidanceLog {
    /// Follow a fixed piece of system-prompt text for the rest of the session.
    /// Naming the same source again replaces its text.
    pub(crate) fn set_standing(&mut self, source: GuidanceSource, text: String) {
        self.standing.retain(|known| known.source.id != source.id);
        self.standing.push(TextSource {
            source,
            placement: GuidancePlacement::System,
            text,
        });
    }

    /// Mark the start of a turn: the next capture is its first request.
    pub(crate) fn start_turn(&mut self) {
        self.turn_requests = 0;
    }

    /// Whether a request of the current turn has been captured.
    pub(crate) fn has_requests_this_turn(&self) -> bool {
        self.turn_requests > 0
    }

    /// Replace this turn's hook-contributed guidance.
    pub(crate) fn begin_turn(
        &mut self,
        segments: impl IntoIterator<Item = (GuidancePlacement, GuidanceSegment)>,
    ) {
        self.turn = segments
            .into_iter()
            .map(|(placement, segment)| TextSource {
                source: segment.source,
                placement,
                text: segment.text,
            })
            .collect();
    }

    /// Follow a tool result that carries guidance, as it entered the history.
    pub(crate) fn tool_result(
        &mut self,
        id: &ToolUseId,
        output: &str,
        sources: Vec<GuidanceSource>,
    ) {
        if sources.is_empty() {
            return;
        }
        if self.tools.len() == MAX_TOOL_RESULTS {
            self.tools.pop_front();
        }
        self.tools.push_back(ToolSource {
            id: id.clone(),
            sources,
            sha256: sha256_hex(output),
            chars: output.chars().count(),
        });
    }

    /// Record what `messages`, an assembled request, holds of everything on
    /// offer.
    pub(crate) fn capture(&mut self, messages: &[Message]) {
        let mut entries = Vec::new();
        if !(self.standing.is_empty() && self.turn.is_empty()) {
            let prose = prose_of(messages);
            for known in self.standing.iter().chain(&self.turn) {
                entries.push(GuidanceEntry {
                    source: known.source.clone(),
                    placement: known.placement,
                    content_sha256: sha256_hex(&known.text),
                    content_chars: known.text.chars().count(),
                    delivery: text_delivery(&prose, &known.text),
                });
            }
        }
        for known in &self.tools {
            let delivery = match tool_output(messages, &known.id) {
                Some(output) if sha256_hex(output) == known.sha256 => GuidanceDelivery::Intact,
                Some(_) => GuidanceDelivery::Altered,
                None => GuidanceDelivery::Absent,
            };
            for source in &known.sources {
                entries.push(GuidanceEntry {
                    source: source.clone(),
                    placement: GuidancePlacement::ToolResult,
                    content_sha256: known.sha256.clone(),
                    content_chars: known.chars,
                    delivery,
                });
            }
        }
        if self.requests.len() == MAX_REQUESTS {
            self.requests.pop_front();
        }
        self.requests.push_back(RequestGuidance {
            request: self.next_request,
            entries,
        });
        self.next_request += 1;
        self.turn_requests += 1;
    }

    /// The kept records, oldest first.
    pub(crate) fn requests(&self) -> Vec<RequestGuidance> {
        self.requests.iter().cloned().collect()
    }
}

/// The system and user text of a request, one string per text block. Assistant
/// text and tool results are left out: a model quoting an instruction back is
/// not the instruction being delivered.
fn prose_of(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .filter(|message| matches!(message.role, Role::System | Role::User))
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Whether `text` is in `prose`: all of it, only its opening line, or nothing.
fn text_delivery(prose: &[&str], text: &str) -> GuidanceDelivery {
    let wanted = text.trim();
    if wanted.is_empty() {
        return GuidanceDelivery::Absent;
    }
    if prose.iter().any(|block| block.contains(wanted)) {
        return GuidanceDelivery::Intact;
    }
    let marker = wanted.lines().next().unwrap_or_default().trim();
    if marker.chars().count() >= MIN_MARKER_CHARS
        && prose.iter().any(|block| block.contains(marker))
    {
        return GuidanceDelivery::Altered;
    }
    GuidanceDelivery::Absent
}

/// The output of the tool result answering call `id`, if the request has one.
fn tool_output<'a>(messages: &'a [Message], id: &ToolUseId) -> Option<&'a str> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult(result) if &result.id == id => Some(result.output.as_str()),
            _ => None,
        })
}

fn sha256_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use localpilot_core::{GuidanceForm, GuidanceKind, ToolResult};

    fn source(id: &str) -> GuidanceSource {
        GuidanceSource::new(
            GuidanceKind::ProjectInstructions,
            GuidanceForm::Body,
            id,
            "repo-root",
            "applies to every turn",
        )
    }

    fn segment(id: &str, text: &str) -> (GuidancePlacement, GuidanceSegment) {
        (
            GuidancePlacement::System,
            GuidanceSegment {
                source: source(id),
                text: text.to_string(),
            },
        )
    }

    fn tool_message(id: &str, output: &str) -> Message {
        Message::new(
            Role::Tool,
            vec![ContentBlock::ToolResult(ToolResult::success(
                ToolUseId::from(id),
                output,
            ))],
        )
    }

    #[test]
    fn a_segment_is_intact_cut_or_absent_by_what_the_request_holds() {
        let mut log = TaskGuidanceLog::default();
        log.begin_turn([
            segment("whole", "<!-- AGENTS.md (repo-root) -->\nuse tabs"),
            segment("cut", "<!-- CLAUDE.md (repo-root) -->\nnever force push"),
            segment("gone", "<!-- web.instructions.md (repo-root) -->\nuse pnpm"),
        ]);
        log.capture(&[Message::text(
            Role::System,
            "rules:\n<!-- AGENTS.md (repo-root) -->\nuse tabs\n\n<!-- CLAUDE.md (repo-root) -->\nnever",
        )]);

        let deliveries: Vec<_> = log.requests()[0]
            .entries
            .iter()
            .map(|entry| (entry.source.id.clone(), entry.delivery))
            .collect();
        assert_eq!(
            deliveries,
            [
                ("whole".to_string(), GuidanceDelivery::Intact),
                ("cut".to_string(), GuidanceDelivery::Altered),
                ("gone".to_string(), GuidanceDelivery::Absent),
            ]
        );
    }

    #[test]
    fn text_an_assistant_repeats_does_not_count_as_delivered() {
        let mut log = TaskGuidanceLog::default();
        log.begin_turn([segment("rule", "always run the formatter first")]);
        log.capture(&[Message::text(
            Role::Assistant,
            "you told me: always run the formatter first",
        )]);
        assert_eq!(
            log.requests()[0].entries[0].delivery,
            GuidanceDelivery::Absent
        );
    }

    #[test]
    fn a_tool_result_is_followed_by_its_call_id_across_requests() {
        let mut log = TaskGuidanceLog::default();
        let id = ToolUseId::from("call-1");
        log.tool_result(&id, "Skill `deploy` body", vec![source("deploy")]);

        log.capture(&[tool_message("call-1", "Skill `deploy` body")]);
        log.capture(&[tool_message("call-1", "[older tool result removed]")]);
        log.capture(&[Message::text(Role::User, "next question")]);

        let seen: Vec<_> = log
            .requests()
            .iter()
            .map(|request| (request.request, request.entries[0].delivery))
            .collect();
        assert_eq!(
            seen,
            [
                (0, GuidanceDelivery::Intact),
                (1, GuidanceDelivery::Altered),
                (2, GuidanceDelivery::Absent),
            ]
        );
    }

    #[test]
    fn a_tool_result_without_sources_is_not_followed() {
        let mut log = TaskGuidanceLog::default();
        log.tool_result(&ToolUseId::from("call-1"), "plain output", Vec::new());
        log.capture(&[tool_message("call-1", "plain output")]);
        assert!(log.requests()[0].entries.is_empty());
    }

    #[test]
    fn the_record_keeps_a_bounded_window_and_stable_numbers() {
        let mut log = TaskGuidanceLog::default();
        for _ in 0..(MAX_REQUESTS + 3) {
            log.capture(&[]);
        }
        let kept = log.requests();
        assert_eq!(kept.len(), MAX_REQUESTS);
        assert_eq!(kept[0].request, 3);
        assert_eq!(kept[MAX_REQUESTS - 1].request, (MAX_REQUESTS + 2) as u64);
    }

    #[test]
    fn standing_text_is_named_once_however_often_it_is_set() {
        let mut log = TaskGuidanceLog::default();
        log.set_standing(source("notice"), "packages are installed".to_string());
        log.set_standing(source("notice"), "packages are installed".to_string());
        log.capture(&[Message::text(Role::System, "packages are installed")]);
        assert_eq!(log.requests()[0].entries.len(), 1);
        assert_eq!(log.requests()[0].delivered().count(), 1);
    }
}
