use std::collections::BTreeMap;

use crate::app::{PlanEntry, UsageTotals, WorkState};
use crate::editor::EditorSnapshot;
use crate::timeline::{ContentPoint, ItemId, Timeline};

/// Display identity and resume metadata for one session in a shared terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHeader {
    pub provider: String,
    pub model: String,
    pub session_id: String,
    pub session_name: Option<String>,
}

/// Stable identity of one peer pane in an exact-two presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerPane {
    A,
    B,
}

impl PeerPane {
    const fn index(self) -> usize {
        match self {
            Self::A => 0,
            Self::B => 1,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ActiveTool {
    pub(super) item_id: ItemId,
    pub(super) detail: String,
}

impl std::fmt::Debug for ActiveTool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActiveTool")
            .field("item_id", &self.item_id)
            .field(
                "detail",
                &format_args!("<{} bytes redacted>", self.detail.len()),
            )
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TimelineSearchState {
    pub(super) query: String,
    pub(super) matches: Vec<ContentPoint>,
    pub(super) selected: Option<usize>,
    pub(super) original_draft: EditorSnapshot,
}

#[derive(Debug, Clone)]
pub(super) struct SessionProjection {
    pub(super) header: SessionHeader,
    pub(super) timeline: Timeline,
    pub(super) timeline_search: Option<TimelineSearchState>,
    pub(super) work: WorkState,
    pub(super) plan: Vec<PlanEntry>,
    pub(super) usage: Option<UsageTotals>,
    pub(super) context_usage: Option<(usize, usize)>,
    pub(super) stream_bytes: usize,
    pub(super) active_assistant: Option<ItemId>,
    pub(super) active_reasoning: Option<ItemId>,
    pub(super) active_tools: BTreeMap<String, ActiveTool>,
    pub(super) active_insert_before: Option<ItemId>,
}

impl SessionProjection {
    pub(super) fn new(header: SessionHeader) -> Self {
        Self {
            header,
            timeline: Timeline::new(),
            timeline_search: None,
            work: WorkState::Idle,
            plan: Vec::new(),
            usage: None,
            context_usage: None,
            stream_bytes: 0,
            active_assistant: None,
            active_reasoning: None,
            active_tools: BTreeMap::new(),
            active_insert_before: None,
        }
    }

    pub(super) fn clear_conversation(&mut self) {
        self.timeline = Timeline::new();
        self.timeline_search = None;
        self.work = WorkState::Idle;
        self.plan.clear();
        self.usage = None;
        self.context_usage = None;
        self.stream_bytes = 0;
        self.active_assistant = None;
        self.active_reasoning = None;
        self.active_tools.clear();
        self.active_insert_before = None;
    }
}

#[derive(Debug, Clone)]
pub(super) enum ProjectionSet {
    Single(SessionProjection),
    Pair {
        projections: Box<[SessionProjection; 2]>,
        active: PeerPane,
    },
}

impl ProjectionSet {
    pub(super) fn single(projection: SessionProjection) -> Self {
        Self::Single(projection)
    }

    pub(super) fn pair(a: SessionProjection, b: SessionProjection) -> Self {
        Self::Pair {
            projections: Box::new([a, b]),
            active: PeerPane::A,
        }
    }

    pub(super) const fn active(&self) -> &SessionProjection {
        match self {
            Self::Single(projection) => projection,
            Self::Pair {
                projections,
                active,
            } => &projections[active.index()],
        }
    }

    pub(super) fn active_mut(&mut self) -> &mut SessionProjection {
        match self {
            Self::Single(projection) => projection,
            Self::Pair {
                projections,
                active,
            } => &mut projections[active.index()],
        }
    }

    pub(super) const fn is_pair(&self) -> bool {
        matches!(self, Self::Pair { .. })
    }

    pub(super) const fn active_pair_pane(&self) -> Option<PeerPane> {
        match self {
            Self::Single(_) => None,
            Self::Pair { active, .. } => Some(*active),
        }
    }

    #[cfg(test)]
    pub(super) fn select(&mut self, peer: PeerPane) {
        if let Self::Pair { active, .. } = self {
            *active = peer;
        }
    }

    #[cfg(test)]
    pub(super) fn projection(&self, peer: PeerPane) -> Option<&SessionProjection> {
        match self {
            Self::Single(_) => None,
            Self::Pair { projections, .. } => Some(&projections[peer.index()]),
        }
    }
}
