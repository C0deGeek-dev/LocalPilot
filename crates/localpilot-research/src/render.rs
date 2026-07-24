//! Render-required detection and the countable render-outcome taxonomy.
//!
//! Static HTTP extraction is the fast, deterministic default. Some pages, though,
//! deliver their real content only after JavaScript runs — a single-page-app
//! shell (`<div id="root"></div><script src=…>`), an iframe-embedded document,
//! or an explicit `Loading…` placeholder. The static reducer strips the scripts
//! and iframes, so those pages reduce to empty or shell-only text. Admitting
//! that shell as complete evidence is the silent failure this module exists to
//! prevent (LocalHub#37).
//!
//! [`render_signal`] inspects a fetched page's raw HTML and its already-reduced
//! readable text and reports whether the page *looks like* it needs rendering —
//! a cheap, dependency-free heuristic, never a browser. The binding layer uses
//! the signal (with the operator's render mode) to decide whether to render, to
//! recover an allowlisted iframe through the ordinary fetch path, or to record
//! an explicit [`RenderOutcome`] so a relevant-but-unrendered page is
//! inspectable instead of counted as complete.

/// Below this many readable characters (after reduction, trimmed) a page has
/// essentially no extracted content — the threshold that turns a framework
/// mount, hydration marker, or iframe body into a render signal. Sized above a
/// bare `Loading…` and a small cookie notice, below any real article.
const THIN_CONTENT_CHARS: usize = 200;

/// A page carrying at least this many `<script` tags is script-heavy — one of
/// the corroborating signals that thin content is a client-rendered shell
/// rather than a genuinely short static page.
const SCRIPT_HEAVY_TAGS: usize = 3;

/// Framework mount-point ids whose element, when present but empty, marks a
/// client-rendered shell whose content arrives via JavaScript.
const FRAMEWORK_MOUNTS: [&str; 4] = [
    "id=\"root\"",
    "id=\"app\"",
    "id=\"__next\"",
    "id=\"__nuxt\"",
];

/// Substrings that mark a hydrated/SSR-client framework payload — present in the
/// initial HTML of an app whose visible content is still JS-driven.
const HYDRATION_MARKERS: [&str; 6] = [
    "__next_data__",
    "window.__nuxt__",
    "data-reactroot",
    "data-server-rendered",
    "ng-version",
    "__svelte",
];

/// Reduced-text bodies that are explicit client-render placeholders, not
/// content: a page whose entire readable text is one of these needs rendering.
const PLACEHOLDER_BODIES: [&str; 5] = [
    "loading",
    "loading…",
    "loading...",
    "please enable javascript",
    "you need to enable javascript to run this app",
];

/// Why a statically-fetched page looks like it needs browser rendering. The
/// variant is diagnostic only — the binding layer decides what to do with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderSignal {
    /// A framework mount point (`#root`/`#app`/`#__next`/`#__nuxt`) is present
    /// but the page reduced to almost no readable text — an unhydrated shell.
    EmptyFrameworkMount,
    /// The initial HTML carries a hydration/SSR-client marker and reduced to
    /// almost no readable text.
    HydrationMarkers,
    /// The page's entire readable body is an explicit `Loading…` /
    /// enable-JavaScript placeholder.
    ClientPlaceholder,
    /// The substantive area is one or more iframes and the parent reduced to
    /// almost no readable text of its own.
    IframeOnly,
    /// Script-heavy HTML that still reduced to almost no readable text — the
    /// generic client-rendered-shell signal when no more specific one fired.
    ThinContent,
}

impl RenderSignal {
    /// A short, content-free reason string for the audit/accounting trail.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::EmptyFrameworkMount => "empty framework mount",
            Self::HydrationMarkers => "hydration markers, no static content",
            Self::ClientPlaceholder => "client-render placeholder",
            Self::IframeOnly => "iframe-only body",
            Self::ThinContent => "script-heavy, thin static content",
        }
    }
}

/// Whether a fetched page's real content is likely absent from its initial HTML
/// and would need JavaScript rendering (or iframe traversal) to recover. Takes
/// the raw `html` and the `reduced_text` the static reducer already produced.
/// Returns the most specific matching [`RenderSignal`], or `None` when the
/// static extraction already yielded substantive content (the common,
/// server-rendered case — no browser needed). Deterministic, total, allocates
/// only lowercase copies; never panics on malformed input.
#[must_use]
pub fn render_signal(html: &str, reduced_text: &str) -> Option<RenderSignal> {
    let readable = reduced_text.trim();
    let readable_len = readable.chars().count();
    let thin = readable_len < THIN_CONTENT_CHARS;

    // An explicit placeholder is a signal regardless of length heuristics: the
    // whole readable body is the placeholder text.
    let readable_lower = readable.to_ascii_lowercase();
    if PLACEHOLDER_BODIES.contains(&readable_lower.as_str()) {
        return Some(RenderSignal::ClientPlaceholder);
    }

    // Everything else needs the page to be thin: a page with real extracted
    // content is served, not a shell, whatever markup surrounds it.
    if !thin {
        return None;
    }

    let html_lower = html.to_ascii_lowercase();
    if FRAMEWORK_MOUNTS
        .iter()
        .any(|mount| html_lower.contains(mount))
    {
        return Some(RenderSignal::EmptyFrameworkMount);
    }
    if HYDRATION_MARKERS
        .iter()
        .any(|marker| html_lower.contains(marker))
    {
        return Some(RenderSignal::HydrationMarkers);
    }
    if html_lower.contains("<iframe") {
        return Some(RenderSignal::IframeOnly);
    }
    if html_lower.matches("<script").count() >= SCRIPT_HEAVY_TAGS {
        return Some(RenderSignal::ThinContent);
    }
    None
}

/// One countable outcome of the render decision for a single fetched page,
/// rendered content-free into the per-question retrieval accounting so a
/// reviewer can tell a page that needed rendering (and whether it got it) apart
/// from one that was simply irrelevant (LocalHub#37).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderOutcome {
    /// Static extraction produced substantive content — the renderer was not
    /// needed. The common path.
    StaticSufficient,
    /// A render signal fired and the operator's mode allows rendering, but no
    /// renderer is available (feature not compiled, or no browser found).
    RendererUnavailable,
    /// A render was attempted but exceeded its time budget.
    RenderTimeout,
    /// A render ran and the page genuinely had no substantive content — an
    /// honest empty, never fabricated evidence.
    NoSubstantiveContent,
    /// The main document rendered and yielded post-JavaScript content.
    RenderedMainDocument,
    /// One or more frames were rendered/traversed and contributed content.
    RenderedFrames,
    /// A subresource or frame the article needed was blocked by the allowlist,
    /// so the rendered DOM is incomplete — reported, not presented as complete.
    BlockedByPolicy,
    /// Embedded content the renderer cannot extract as prose (a PDF or other
    /// non-HTML frame, a login/CAPTCHA/paywall/video/canvas-only page).
    UnsupportedContent,
}

impl RenderOutcome {
    /// A stable, content-free label for the accounting line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::StaticSufficient => "static extraction sufficient",
            Self::RendererUnavailable => "render required, renderer unavailable",
            Self::RenderTimeout => "render timeout",
            Self::NoSubstantiveContent => "no substantive rendered content",
            Self::RenderedMainDocument => "rendered main document",
            Self::RenderedFrames => "rendered frames",
            Self::BlockedByPolicy => "render blocked by policy",
            Self::UnsupportedContent => "unsupported embedded content",
        }
    }
}

/// A per-request egress decision the renderer consults on every browser
/// navigation, redirect, subresource, and frame before it leaves the machine —
/// so browser rendering stays inside the same allowlist/audit boundary as a
/// static fetch (LocalHub#37, `docs/07-security-and-privacy.md`). The binding
/// layer implements this over its `WebAccess`; the renderer never sees the
/// allowlist directly, only this yes/no (and the gate audits as it decides).
pub trait RenderGate: Send + Sync {
    /// Whether the browser may issue a request to `url`. The gate sees the full
    /// URL so it can enforce the http/https-only rule and the host allowlist,
    /// and record the decision in the egress audit.
    fn allow(&self, url: &str) -> bool;
}

/// Bounds on one render: the caller maps its resolved rails into these so the
/// renderer cannot run unbounded.
#[derive(Debug, Clone, Copy)]
pub struct RenderBounds {
    /// Longest to wait for the DOM to stabilise after load before extracting —
    /// a bounded settle window, never an indefinite network-idle wait.
    pub settle: std::time::Duration,
    /// Hard ceiling on the whole render (navigation + settle + extraction).
    pub timeout: std::time::Duration,
    /// Maximum child frames to traverse (subject 04).
    pub max_frames: usize,
    /// Maximum nested-frame depth (subject 04).
    pub max_depth: usize,
}

impl Default for RenderBounds {
    fn default() -> Self {
        Self {
            settle: std::time::Duration::from_millis(1_500),
            timeout: std::time::Duration::from_secs(20),
            max_frames: 8,
            max_depth: 3,
        }
    }
}

/// A request to render one page.
#[derive(Debug, Clone)]
pub struct RenderRequest {
    /// The URL to render. The caller has already gated this host; the renderer
    /// re-gates every browser request through the [`RenderGate`].
    pub url: String,
    /// Bounds for this render.
    pub bounds: RenderBounds,
}

/// A rendered document: the post-JavaScript main document plus any extracted
/// frames, returned to the caller for the same reduction + admission a static
/// body gets.
#[derive(Debug, Clone, Default)]
pub struct RenderedDoc {
    /// The main document's post-JavaScript HTML (the serialized rendered DOM).
    pub html: String,
    /// Extracted frame documents (subject 04); empty until frame traversal.
    pub frames: Vec<RenderedFrame>,
    /// Browser requests (subresources, frames, redirects) the gate blocked
    /// during the render — so an incomplete DOM is reported, not presented as
    /// complete.
    pub blocked: usize,
}

/// One extracted frame document with its origin.
#[derive(Debug, Clone)]
pub struct RenderedFrame {
    /// The frame's source URL; `None` for an inline `srcdoc` frame.
    pub url: Option<String>,
    /// The frame document's post-JavaScript HTML.
    pub html: String,
}

/// A typed render failure the caller maps to a [`RenderOutcome`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderFailure {
    /// No renderer/browser is available (feature off, or no browser found).
    Unavailable,
    /// The render exceeded its time budget.
    Timeout,
    /// The render ran but produced no substantive content.
    NoContent,
    /// A resource the page needed was blocked by the egress policy, leaving the
    /// DOM incomplete.
    Blocked,
    /// The browser errored (launch/connection/protocol) — content-free detail
    /// for diagnostics, never page content.
    Browser(String),
}

impl RenderFailure {
    /// The countable outcome this failure records in the retrieval accounting.
    #[must_use]
    pub fn outcome(&self) -> RenderOutcome {
        match self {
            Self::Unavailable | Self::Browser(_) => RenderOutcome::RendererUnavailable,
            Self::Timeout => RenderOutcome::RenderTimeout,
            Self::NoContent => RenderOutcome::NoSubstantiveContent,
            Self::Blocked => RenderOutcome::BlockedByPolicy,
        }
    }
}

/// Renders a page's post-JavaScript content behind a [`RenderGate`]. The
/// concrete browser implementation lives in the optional `localpilot-render`
/// crate (behind the `render-browser` feature); the loop and binding layer
/// depend only on this trait, so a build without the browser dependency simply
/// has no renderer and records [`RenderOutcome::RendererUnavailable`].
#[async_trait::async_trait]
pub trait Renderer: Send + Sync {
    /// Render `request.url`, gating every browser request through `gate`, and
    /// return the rendered document or a typed failure.
    async fn render(
        &self,
        request: &RenderRequest,
        gate: &dyn RenderGate,
    ) -> Result<RenderedDoc, RenderFailure>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_rendered_article_has_no_signal() {
        // The control: a page with real extracted content never triggers
        // rendering, whatever scripts surround it.
        let html = "<html><body><article><h1>Guide</h1><p>...</p></article>\
                    <script src=\"/a.js\"></script></body></html>";
        let reduced = "Guide\n\n".to_string() + &"real documentation content. ".repeat(20);
        assert_eq!(render_signal(html, &reduced), None);
    }

    #[test]
    fn empty_react_shell_signals_render() {
        let html = "<html><body><div id=\"root\"></div>\
                    <script src=\"/app.js\"></script></body></html>";
        assert_eq!(
            render_signal(html, ""),
            Some(RenderSignal::EmptyFrameworkMount)
        );
    }

    #[test]
    fn next_hydration_marker_signals_render() {
        let html = "<html><body><div></div>\
                    <script id=\"__NEXT_DATA__\" type=\"application/json\">{}</script></body></html>";
        assert_eq!(
            render_signal(html, "  "),
            Some(RenderSignal::HydrationMarkers)
        );
    }

    #[test]
    fn loading_placeholder_signals_render_even_if_not_thin_by_scripts() {
        let html = "<html><body><div id=\"x\">Loading…</div></body></html>";
        assert_eq!(
            render_signal(html, "Loading…"),
            Some(RenderSignal::ClientPlaceholder)
        );
    }

    #[test]
    fn iframe_only_body_signals_render() {
        let html = "<html><body><nav>menu</nav>\
                    <iframe src=\"https://docs-frame.example/ch1\"></iframe></body></html>";
        assert_eq!(render_signal(html, "menu"), Some(RenderSignal::IframeOnly));
    }

    #[test]
    fn script_heavy_thin_page_signals_thin_content() {
        let html = "<html><head><script src=\"/1.js\"></script>\
                    <script src=\"/2.js\"></script><script src=\"/3.js\"></script></head>\
                    <body><div></div></body></html>";
        assert_eq!(render_signal(html, "x"), Some(RenderSignal::ThinContent));
    }

    #[test]
    fn thin_but_not_script_heavy_static_page_has_no_signal() {
        // A genuinely short static page (a stub) with no framework markers and
        // few scripts is not a shell — don't cry render.
        let html = "<html><body><p>Short but real.</p></body></html>";
        assert_eq!(render_signal(html, "Short but real."), None);
    }

    #[test]
    fn outcome_labels_are_stable_and_content_free() {
        assert_eq!(
            RenderOutcome::RendererUnavailable.label(),
            "render required, renderer unavailable"
        );
        assert_eq!(
            RenderOutcome::StaticSufficient.label(),
            "static extraction sufficient"
        );
    }
}
