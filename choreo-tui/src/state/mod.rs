//! The TUI's full application state: the `App` struct, per-session display
//! state and render cache, history/input viewport plumbing, and the
//! page/overlay states.  The provider catalog, text-input machinery, page
//! states, and picker geometry live in sibling modules (`providers`, `input`,
//! `pages`, `layout`) and are re-exported here so the rest of the crate keeps
//! referring to `crate::state::*` unchanged.

use crate::RenderedImage;
use crate::image_worker::{ImageId, ImageJob, ImageResult, next_job_id};
use crate::selection::TextSelection;
use choreo_client_core::dispatch::{SessionStateData, ToolCallEvent};
use choreo_client_core::{ClientError, SessionView, TurnEventHandler, broken_pipe};
use choreo_proto::{
    AccountInfo, ClientMessage, OutputStream, ReasoningCapability, SessionStatus, SessionSummary,
    TokenUsage, ToolResultRecord, Turn, socket_path,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect, Size};
use ratatui::text::Line;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::markdown_render::{
    LineJoin, RenderedTurnLines, compute_visual_offsets, lines_height, plain_text_lines,
    reasoning_expanded_default, render_turn_lines, tool_result_default_collapsed,
};

// The input-editing key types are only referenced from the test module
// (`use super::*` feeds the unit tests); production key handling lives in
// `input.rs`, so gate the import to the test build to keep clippy clean.
#[cfg(test)]
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod input;
mod layout;
mod pages;
mod providers;

// Compatibility layer: every item moved into the sibling modules is
// re-exported here so `crate::state::X` references (in this crate and in
// `app_tests.rs`/`render_tests.rs`) keep resolving exactly as before.
pub(crate) use input::*;
pub(crate) use layout::*;
pub(crate) use pages::*;
pub(crate) use providers::*;

pub(crate) const STATUS_BAR_HEIGHT: u16 = 1;
pub(crate) const MIN_INPUT_CONTENT_LINES: u16 = 1;
pub(crate) const MAX_INPUT_CONTENT_LINES: u16 = 10;
pub(crate) const PAGE_SCROLL_LINES: usize = 3;

/// Horizontal padding (columns) on each side of the command input box.
///
/// The box draws only top/bottom borders, so the content area loses `INPUT_PAD`
/// columns on each side and nothing more.  Every code path that wraps input
/// text (height estimation, cursor movement, rendering) must use
/// [`input_inner_width`] so they all agree on where word-wrap happens;
/// otherwise a wrapped line can be computed in one path but not another.
pub(crate) const INPUT_PAD: u16 = 2;

/// Inner content width (columns) of the command input box for a given terminal
/// width: terminal width minus the horizontal padding on both sides.
pub(crate) fn input_inner_width(term_width: u16) -> usize {
    term_width.saturating_sub(INPUT_PAD * 2) as usize
}

pub(crate) const CTRL_HELP_LINE1: &str =
    "ctrl+h help  ctrl+q quit  ctrl+a accounts  ctrl+s sessions  ctrl+m models";
/// Help line for terminals WITHOUT the kitty keyboard protocol, where Ctrl+M
/// is byte 0x0D (indistinguishable from Enter) and the model selector is
/// rebound to Ctrl+O instead — see `App::keyboard_enhanced`.
pub(crate) const CTRL_HELP_LINE1_LEGACY: &str =
    "ctrl+h help  ctrl+q quit  ctrl+a accounts  ctrl+s sessions  ctrl+o models";
pub(crate) const CTRL_HELP_LINE2: &str =
    "esc stop  alt+enter continue  ctrl+up undo  ctrl+down redo  ctrl+r reasoning";

/// Per-turn content-line ranges used for click hit-testing, computed
/// alongside `height_prefix`.  Maps a content-line offset within the turn to
/// the reasoning header or the correct image index — no text-height
/// recomputation needed in the click handler.
#[derive(Debug)]
pub(crate) struct TurnLayout {
    /// (start, end) content-line range of the reasoning header row(s),
    /// relative to the turn's start.  None when the turn has no reasoning.
    pub reasoning_header_range: Option<(usize, usize)>,
    /// Whether this turn's reasoning section is expanded by default, derived
    /// from turn content at layout time (an explicit header-click override in
    /// `reasoning_override` takes precedence at render time).  Stored here so
    /// the per-frame render path can compute the effective state in O(1)
    /// without re-scanning turn strings.
    pub reasoning_default_expanded: bool,
    /// (start, end) content-line ranges for each displayed image,
    /// relative to the turn's start.  Empty when the turn has no images.
    pub image_ranges: Vec<(usize, usize)>,
    /// (start, end) content-line ranges for each tool result's collapsible
    /// header row, relative to the turn's start, aligned with
    /// `turn.tool_results`.  Empty when the turn has no tool results (or
    /// short-circuits on the error block).  Populated in lockstep with
    /// `height_prefix` and kept in sync by the streaming fast path.
    pub tool_result_header_ranges: Vec<(usize, usize)>,
}

/// Everything that identifies a render-cache entry.  Two keys are equal iff
/// the cached lines can be reused: same turn, same widths, the same
/// effective reasoning/tool-result collapse state, and — critically — the
/// same *content version* of the turn (see
/// [`SessionDisplayState::turn_versions`]).
///
/// The content version is what makes the key content-correct: streaming
/// growth (tool-result chunks, answer chunks), turn replacement
/// (`TurnAppended`), and snapshot merges (`SessionState`) all bump it, so a
/// full rebuild can never serve a stale cached rendering of a turn whose
/// text changed behind the key's other fields.  Without it, a rebuild
/// between a chunk and its fast-path refresh would reuse the pre-chunk
/// lines — the visible results froze while the scrollbar total kept
/// reflecting fresh content, and everything snapped back only when the
/// final `TurnAppended` invalidated the slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderCacheKey {
    /// Turn ID this entry belongs to, used to detect stale entries after
    /// turns are removed/reordered.
    pub turn_id: u32,
    /// Content width the lines were wrapped at.
    pub width: u16,
    /// Full viewport width when this entry was computed.  Stored alongside
    /// `width` (the content width) so the cache key guards against skew in
    /// `lines_height` and `compute_visual_offsets` computations, which
    /// depend on viewport width.
    pub viewport_width: u16,
    /// Reasoning visibility the cached lines were rendered with.
    pub reasoning_expanded: bool,
    /// Collapse state of each tool result (aligned with `turn.tool_results`)
    /// the cached lines were rendered with.
    pub tool_results_collapsed: Vec<bool>,
    /// Monotonic per-turn content version the lines were rendered with.
    /// Bumped by every event handler that mutates a turn's rendered content;
    /// a mismatch forces a recompute even when every other field matches.
    pub content_version: u64,
}

/// Cached render output for a turn: the lines plus the precomputed height,
/// cumulative visual offsets, and section-header semantic indexes.  Returned
/// from [`cached_or_compute_lines`] so callers can render and hit-test
/// without re-walking the lines.
#[derive(Debug, Clone)]
pub(crate) struct RenderedTurn {
    pub lines: Arc<[Line<'static>]>,
    pub height: usize,
    /// Cumulative visual-row offset for each semantic line.
    /// `visual_offsets[i]` = total visual rows covered by lines[0..=i].
    /// Used with `partition_point` to map a visual row → semantic line index
    /// in O(log n).
    pub visual_offsets: Arc<[usize]>,
    /// Per-line [`LineJoin`] copy metadata aligned with `lines`: how each
    /// rendered row glues to the row before it when a selection is copied
    /// (see the enum docs in `markdown_render`).  The selection extraction
    /// uses this to rejoin wrapped continuations into the original text
    /// instead of reproducing the renderer's line breaks.
    pub joins: Arc<[LineJoin]>,
    /// Display-column range `(start, end)` of each line's meaningful content,
    /// aligned with `lines` — see [`RenderedTurnLines::content_ranges`].
    /// Mouse selection clamps its highlight and its copy to these ranges so
    /// a drag never captures UI chrome (the `┃` gutter, indents, fill).
    pub content_ranges: Arc<[Option<(usize, usize)>]>,
    /// Semantic-line index of the reasoning header within `lines` (see
    /// [`RenderedTurnLines`]), so click hit-testing never re-scans the
    /// rendered output.
    pub reasoning_header_idx: Option<usize>,
    /// Semantic-line index of each tool result header within `lines` (see
    /// [`RenderedTurnLines`]), so click hit-testing never re-scans the
    /// rendered output.
    pub tool_result_header_idxs: Vec<usize>,
}

/// One slot of the render cache: the key the entry was rendered with plus the
/// rendered output.  The key is compared on lookup so a stale entry (state
/// changed without invalidation) is treated as a miss instead of being served.
#[derive(Debug, Clone)]
pub(crate) struct RenderedCache {
    pub key: RenderCacheKey,
    pub rendered: RenderedTurn,
}

/// Check `render_cache[index]` for a valid entry matching `key`.  On hit,
/// return the cached [`RenderedTurn`].  On miss, call `compute`, store the
/// result in `render_cache[index]`, and return it.
///
/// When `index` is out of bounds (in-band or because the cache is shorter than
/// expected), the result is still returned but not cached.
pub(crate) fn cached_or_compute_lines(
    cache: &mut [Option<RenderedCache>],
    index: usize,
    key: &RenderCacheKey,
    compute: impl FnOnce() -> RenderedTurnLines,
) -> RenderedTurn {
    if let Some(Some(cached)) = cache.get(index)
        && cached.key == *key
    {
        return cached.rendered.clone();
    }

    let rendered = compute();
    let lines: Arc<[Line<'static>]> = Arc::from(rendered.lines);
    let joins: Arc<[LineJoin]> = Arc::from(rendered.joins);
    let content_ranges: Arc<[Option<(usize, usize)>]> = Arc::from(rendered.content_ranges);
    let visual_offsets = compute_visual_offsets(&lines, key.viewport_width);
    // Pin the parallel-array invariant the selection machinery relies on:
    // every rendered line must carry a content range (`None` marks
    // pure-chrome rows), a cumulative visual-row offset, and a copy-join
    // record.  A mismatch here is a programming error in the renderer, not
    // a runtime condition.
    debug_assert_eq!(
        content_ranges.len(),
        lines.len(),
        "every rendered line must carry a content range"
    );
    debug_assert_eq!(
        visual_offsets.len(),
        lines.len(),
        "visual offsets must stay aligned with the rendered lines"
    );
    debug_assert_eq!(joins.len(), lines.len(), "joins must align with the lines");
    let turn = RenderedTurn {
        height: lines_height(&lines, key.viewport_width).max(1),
        visual_offsets,
        lines,
        joins,
        content_ranges,
        reasoning_header_idx: rendered.reasoning_header_idx,
        tool_result_header_idxs: rendered.tool_result_header_idxs,
    };
    if let Some(slot) = cache.get_mut(index) {
        *slot = Some(RenderedCache {
            key: key.clone(),
            rendered: turn.clone(),
        });
    }
    turn
}

pub(crate) struct SessionDisplayState {
    pub(crate) view: SessionView,
    pub(crate) visible_turn_ids: Vec<u32>,
    pub(crate) turn_heights: Vec<usize>,
    pub(crate) height_prefix: Vec<usize>,
    pub(crate) markers: Vec<Marker>,
    pub(crate) markers_dirty: bool,
    pub(crate) streaming_turn_index: Option<usize>,
    pub(crate) streaming_dirty: bool,
    pub(crate) content_dirty: bool,
    pub(crate) history_scroll: HistoryScrollState,
    pub(crate) turn_layouts: Vec<TurnLayout>,
    /// Per-turn explicit reasoning visibility (turn_id → expanded) set by
    /// clicking the reasoning header.  Absent entries fall back to
    /// [`reasoning_expanded_default`] (expanded while streaming, collapsed
    /// once a response exists).
    pub(crate) reasoning_override: HashMap<u32, bool>,
    /// Per-(turn, tool-call) explicit collapse state (turn_id → call_id →
    /// collapsed) set by clicking a tool result's header.  Absent entries
    /// fall back to [`tool_result_default_collapsed`] (quiet tools
    /// collapsed, everything else expanded).  Nested so the per-frame lookup
    /// can borrow the record's `call_id` instead of cloning it; keyed by
    /// call_id (not position) because a result's position is stable while
    /// its content streams in.
    pub(crate) tool_collapse_override: HashMap<u32, HashMap<String, bool>>,
    /// Monotonic per-turn content version, bumped by every event handler
    /// that mutates a turn's rendered content (streaming chunks, turn
    /// replacement, snapshot merges, undo/redo).  Included in the
    /// [`RenderCacheKey`] so a full rebuild can never reuse a cached
    /// rendering whose turn content changed behind the key's other fields.
    /// This is what makes the cache content-correct even when the streaming
    /// fast path was disarmed by an interleaved `mark_content_changed` (a
    /// `Done`/`TurnAppended`/`SessionState` from this or another session
    /// landing between chunks).
    pub(crate) turn_versions: HashMap<u32, u64>,
    pub(crate) render_cache: Vec<Option<RenderedCache>>,
    pub(crate) active: HashSet<u32>,
    pub(crate) live_input_estimate: u32,
    pub(crate) live_output_tokens: u32,
    pub(crate) progress_dirty: bool,
    pub(crate) status: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) selected_model: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) reasoning_capability: Option<ReasoningCapability>,
    pub(crate) account_name: Option<String>,
    pub(crate) working_dir: Option<String>,
    pub(crate) token_usage: Option<TokenUsage>,
    pub(crate) context_window: Option<u32>,
    pub(crate) last_prompt_tokens: Option<u32>,
    /// Unsent prompt draft: the text (and cursor position) the user had
    /// typed but not yet submitted when they last left this session.  The
    /// input bar is a single shared buffer, so each session stashes its own
    /// draft here and it is restored on the next visit — an unsubmitted
    /// prompt must never leak into a different session.  Cleared on submit
    /// and dropped when the session (and its display) is deleted.
    pub(crate) draft: String,
    pub(crate) draft_cursor: usize,
}

impl Default for SessionDisplayState {
    fn default() -> Self {
        Self {
            view: SessionView::new(),
            visible_turn_ids: Vec::new(),
            turn_heights: Vec::new(),
            height_prefix: Vec::new(),
            markers: Vec::new(),
            markers_dirty: true,
            streaming_turn_index: None,
            streaming_dirty: false,
            content_dirty: false,
            history_scroll: HistoryScrollState::new(),
            turn_layouts: Vec::new(),
            reasoning_override: HashMap::new(),
            tool_collapse_override: HashMap::new(),
            turn_versions: HashMap::new(),
            render_cache: Vec::new(),
            active: HashSet::new(),
            live_input_estimate: 0,
            live_output_tokens: 0,
            progress_dirty: false,
            status: None,
            error: None,
            selected_model: None,
            reasoning_effort: None,
            reasoning_capability: None,
            account_name: None,
            working_dir: None,
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
            draft: String::new(),
            draft_cursor: 0,
        }
    }
}

pub(crate) struct App {
    pub(crate) input: InputBuffer,
    pub(crate) next_request_id: u32,
    pub(crate) rendered_images: HashMap<u64, HashMap<u32, HashMap<usize, RenderedImage>>>,
    pub(crate) pending_job_idx: HashMap<ImageId, (u64, u32, usize)>,
    pub(crate) history_viewport: HistoryViewport,
    pub(crate) should_quit: bool,
    /// Why the TUI is exiting, when it is NOT a user-initiated quit (Ctrl+Q).
    /// Set when the daemon evicts this client, announces shutdown, or the
    /// connection drops; printed to the restored terminal after teardown so
    /// the user sees why the TUI left. `None` on a normal user quit.
    pub(crate) quit_message: Option<String>,
    pub(crate) image_job_tx: Option<crossbeam::channel::Sender<ImageJob>>,
    pub(crate) attached_session_id: Option<u64>,
    /// Account slug shown in the status bar — the attached session's account
    /// name (the account name is its slug). Replaces the inference provider
    /// slug that used to be shown here.
    pub(crate) attached_account_slug: Option<String>,
    pub(crate) attached_status: Option<SessionStatus>,
    pub(crate) attached_tool_groups: Vec<String>,
    /// Persistent latch of whether the daemon's credential keystore is locked.
    /// Latched from the daemon's lock-state broadcasts (`Locked`/`Unlocked`)
    /// and the subscribe-time lock-state push in `handle_daemon_message`;
    /// drives the persistent lock banner and the submit-time prompt guard.
    /// Defaults to `true` (assume locked until told otherwise — the safest
    /// reading for a client that has not yet heard the daemon's state). Not
    /// cleared by the per-keypress transient status/error clear, so the lock
    /// indication survives every keystroke.
    pub(crate) keystore_locked: bool,
    /// Once-per-connection keystore auto-bind state machine (shared policy in
    /// `choreo_client_core`, so TUI and GUI cannot drift): the first
    /// `KeystoreUnbound` report mints+sends a bind, later ones surface an
    /// error instead of re-minting (bind-loop guard — see
    /// [`choreo_client_core::KeystoreAutoBind`]).
    pub(crate) keystore_auto_bind: choreo_client_core::KeystoreAutoBind,
    pub(crate) page: Page,
    pub(crate) show_ctrl_help: bool,
    /// Whether the terminal implemented the kitty keyboard protocol we push
    /// at startup (probed via crossterm's `supports_keyboard_enhancement`
    /// before the terminal-event thread starts). On legacy terminals (Termux
    /// and friends) Ctrl+M is byte 0x0D — identical to Enter — so the model
    /// selector is rebound to Ctrl+O and hints reflect it. Defaults to `true`:
    /// kitty-capable terminals are the desktop majority, and every simulated
    /// KeyEvent in tests is a kitty-encoding event.
    pub(crate) keyboard_enhanced: bool,
    pub(crate) session_mgr: SessionManagerState,
    pub(crate) ai_providers: AIProvidersState,
    pub(crate) model_selector: ModelSelectorState,
    /// The live provider list for the new-account wizard's provider picker
    /// (S4). Initialized from the static `PROVIDER_OPTIONS` default and
    /// replaced wholesale whenever the daemon broadcasts `CatalogUpdated`, so
    /// the picker tracks the daemon's live catalog (cache + user overlay).
    pub(crate) providers: Vec<ProviderInfo>,
    pub(crate) scroll_accumulator: isize,
    pub(crate) scrollbar_dragging: bool,
    /// In-progress mouse text selection over the history pane (see
    /// `selection`).  `None` when no selection gesture is active; cleared on
    /// session switch and suspend so a stale rectangle never highlights a
    /// different session's content.
    pub(crate) text_selection: Option<TextSelection>,
    pub(crate) last_terminal_size: Option<(u16, u16)>,
    pub(crate) terminal_resized: bool,
    pub(crate) history_index: Option<usize>,
    /// The user's real draft while they are stepping through history: captured
    /// on the first Up press, restored by `restore_history_draft`.  Kept as a
    /// separate stash because the input bar holds a history entry while
    /// `history_index` is `Some`.  The cursor position is stashed alongside so
    /// exiting history navigation (or switching sessions mid-navigation)
    /// restores the exact editing position, not the end of the text.
    pub(crate) saved_draft: String,
    pub(crate) saved_draft_cursor: usize,
    /// The text of the history entry currently shown in the input bar, so
    /// `restore_history_draft` can tell whether the user edited the entry on
    /// top of it: a buffer that no longer matches the loaded entry *is* the
    /// user's real draft and must be kept, not discarded.
    pub(crate) history_entry_text: Option<String>,
    pub(crate) fullscreen_image_target: Option<(u64, u32, usize)>,
    pub(crate) status: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) session_displays: HashMap<u64, SessionDisplayState>,
    pub(crate) active_session_id: Option<u64>,
    /// The address string this session talks to (dial addr for TCP, unix
    /// socket path otherwise). It keys the per-daemon unlock key in
    /// known_servers, so every `Unlock`/`AddCredential`/record must use it
    /// consistently. Set by `run_app` from the connection mode; defaults to
    /// the socket path so tests (which bypass run_app) still have a valid
    /// value.
    pub(crate) connection_addr: String,
    /// The unlock key sent in the most recent `Unlock` or `AddCredential`,
    /// held until the daemon CONFIRMS it (an `Unlocked` or `CredentialAdded`
    /// reply) and then recorded per-daemon via `record_unlock_key`. Never
    /// persisted on send — only on confirmed success. Cleared once recorded
    /// (and when a send fails to avoid recording a stale key later).
    pub(crate) pending_unlock_key: Option<Vec<u8>>,
}

#[derive(Clone, Copy)]
pub(crate) struct HistoryViewport {
    pub(crate) width: u16,
    pub(crate) height: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct HistoryScrollState {
    pub(crate) scroll: usize,
    pub(crate) scroll_compensation: usize,
}

pub(crate) enum UiEvent {
    Daemon(Box<choreo_proto::DaemonMessage>),
    ReaderClosed,
}

impl HistoryViewport {
    pub(crate) fn new() -> Self {
        Self {
            width: 80,
            height: 24,
        }
    }

    pub(crate) fn update(&mut self, area: Rect) {
        self.width = area.width.max(1);
        self.height = area.height;
    }
}

impl HistoryScrollState {
    pub(crate) fn new() -> Self {
        Self {
            scroll: 0,
            scroll_compensation: 0,
        }
    }

    fn unclamped_effective_scroll(&self) -> usize {
        self.scroll.saturating_add(self.scroll_compensation)
    }

    pub(crate) fn clamp(&mut self, max_scroll: usize) {
        let effective = self.unclamped_effective_scroll();
        if effective <= max_scroll {
            return;
        }
        let overflow = effective - max_scroll;
        let compensation_reduction = self.scroll_compensation.min(overflow);
        self.scroll_compensation -= compensation_reduction;
        let remaining = overflow - compensation_reduction;
        self.scroll = self.scroll.saturating_sub(remaining);
    }

    pub(crate) fn effective_scroll(&self, max_scroll: usize) -> usize {
        self.unclamped_effective_scroll().min(max_scroll)
    }

    pub(crate) fn scroll_up(&mut self, amount: usize, max_scroll: usize) {
        self.scroll = self.scroll.saturating_add(amount);
        self.clamp(max_scroll);
    }

    pub(crate) fn scroll_down(&mut self, amount: usize, max_scroll: usize) {
        let compensation_reduction = self.scroll_compensation.min(amount);
        self.scroll_compensation -= compensation_reduction;
        let remaining = amount.saturating_sub(compensation_reduction);
        self.scroll = self.scroll.saturating_sub(remaining);
        self.clamp(max_scroll);
    }
}

/// Integer ceiling division: `ceil(a / b)`.
/// Returns 0 when `b == 0`.
fn ceil_div(a: usize, b: usize) -> usize {
    if b == 0 {
        return 0;
    }
    a.saturating_add(b).saturating_sub(1) / b
}

impl App {
    pub(crate) fn new() -> Self {
        Self {
            input: InputBuffer::new(),
            next_request_id: 1,
            rendered_images: HashMap::new(),
            history_viewport: HistoryViewport::new(),
            should_quit: false,
            quit_message: None,
            image_job_tx: None,
            pending_job_idx: HashMap::new(),
            attached_session_id: None,
            attached_account_slug: None,
            attached_status: None,
            attached_tool_groups: Vec::new(),
            // Assume locked until the daemon tells us otherwise (via the
            // subscribe-time lock-state push or a transition broadcast).
            keystore_locked: true,
            // No bind sent yet on this connection (see the field docs). The
            // latch lives for the whole `App` — `App::new()` runs once per
            // `run_app` (per process, i.e. per daemon connection), and the
            // UI loop does not rebuild `App` on reader errors, so
            // re-connecting means restarting the TUI with fresh state.
            keystore_auto_bind: choreo_client_core::KeystoreAutoBind::new(),
            page: Page::Chat,
            show_ctrl_help: true,
            keyboard_enhanced: true,
            session_mgr: SessionManagerState::new(),
            ai_providers: AIProvidersState::new(),
            model_selector: ModelSelectorState::new(),
            // Start from the static default; the daemon's CatalogUpdated
            // broadcast replaces it with the live list.  The picker must be
            // alphabetical, so sort the default here too (see `sort_providers`
            // and `set_providers`).
            providers: {
                let mut providers: Vec<ProviderInfo> = PROVIDER_OPTIONS
                    .iter()
                    .map(|(slug, display_name)| ProviderInfo {
                        slug: (*slug).to_string(),
                        display_name: (*display_name).to_string(),
                    })
                    .collect();
                sort_providers(&mut providers);
                providers
            },
            scroll_accumulator: 0,
            scrollbar_dragging: false,
            text_selection: None,
            history_index: None,
            saved_draft: String::new(),
            saved_draft_cursor: 0,
            history_entry_text: None,
            fullscreen_image_target: None,
            status: None,
            error: None,
            last_terminal_size: None,
            terminal_resized: false,
            session_displays: HashMap::new(),
            active_session_id: None,
            connection_addr: socket_path(),
            pending_unlock_key: None,
        }
    }

    /// The key that opens the model selector on THIS terminal: Ctrl+M on
    /// kitty-protocol terminals; Ctrl+O on legacy terminals where Ctrl+M is
    /// byte 0x0D (Enter) and can never reach the handler. Used by hint and
    /// status strings so the user is always told a key that actually works.
    pub(crate) fn model_selector_label(&self) -> &'static str {
        if self.keyboard_enhanced {
            "Ctrl+M"
        } else {
            "Ctrl+O"
        }
    }

    pub(crate) fn display_for(&mut self, session_id: u64) -> &mut SessionDisplayState {
        self.session_displays.entry(session_id).or_default()
    }

    /// Replace the live provider list from a daemon `CatalogUpdated`
    /// broadcast. Clamps the wizard's provider selection when the list
    /// shrank, so a catalog refresh that drops providers can never leave the
    /// selection pointing past the end of the list. Returns whether the list
    /// actually changed (identical payloads — e.g. the send-on-subscribe
    /// welcome — do not churn the status line).
    pub(crate) fn set_providers(&mut self, mut providers: Vec<ProviderInfo>) -> bool {
        // The wizard's picker is a flat alphabetical list; the daemon sends
        // the catalog in provenance order (see `sort_providers`), so re-sort
        // every incoming list before comparing/storing. Sorting first also
        // means a provider reorder alone never registers as a "change".
        sort_providers(&mut providers);
        if self.providers == providers {
            return false;
        }
        self.providers = providers;
        // Clamp the wizard's picker highlight when the list changed, so a
        // catalog refresh that drops providers can never leave the highlight
        // (or the scroll offset) pointing past the end of the narrowed list.
        self.ai_providers.wizard.clamp_focus(&self.providers);
        true
    }
    pub(crate) fn active_display(&mut self) -> Option<&mut SessionDisplayState> {
        self.session_displays.get_mut(&self.active_session_id?)
    }
    pub(crate) fn active_display_ref(&self) -> Option<&SessionDisplayState> {
        self.session_displays.get(&self.active_session_id?)
    }

    /// Whether a daemon message carrying the given wire session id is
    /// background noise that must not write the global status/error line.
    ///
    /// A connection-level reply (`None` — no origin session, e.g. a "no
    /// session attached" failure) resolves to the attached session, so it —
    /// like the attached session itself — keeps its fall-through feedback.
    /// Only a message about a real session that is not the attached session
    /// is suppressed.
    pub(crate) fn is_background_session_message(&self, reported_session_id: Option<u64>) -> bool {
        matches!(reported_session_id, Some(id) if self.attached_session_id != Some(id))
    }

    /// Resolve a daemon-reported session id to the session whose display it
    /// should update.  A connection-level reply carries `None` (no origin
    /// session — e.g. a "no session attached" failure, or a bare
    /// `GetReasoningEffort` reply without an attachment) and resolves to the
    /// attached session, so it never lands in a phantom display and defeats
    /// the attached-session routing in `is_background_session_message`.
    /// Returns `None` when there is no session to update — a `None` envelope
    /// with nothing attached.
    pub(crate) fn resolve_daemon_session(&self, session_id: Option<u64>) -> Option<u64> {
        match session_id {
            None => self.attached_session_id,
            Some(id) => Some(id),
        }
    }

    /// Number of lines needed for the status/error bar, based on the current
    /// message content and the available terminal width.  Returns 0 when there
    /// is no message to display.
    pub(crate) fn status_error_height(&self, width: u16) -> u16 {
        let text = if let Some(ref err) = self.error {
            err.as_str()
        } else if let Some(ref status) = self.status {
            status.as_str()
        } else {
            return 0;
        };
        // The status/error Paragraph is drawn inset by one column on each side
        // (render.rs `notify_area`), so it wraps at `width - 2` columns.
        // Measure at that same inner width, or a long message's reserved
        // height can fall short of the rows ratatui actually draws and the
        // tail gets clipped by the layout.
        let inner = width.saturating_sub(2);
        let lines = plain_text_lines(text, inner);
        lines_height(&lines, inner).max(1) as u16
    }

    /// Number of visual content lines the input box currently occupies,
    /// computed from the text and terminal width.
    pub(crate) fn input_bar_content_lines(&mut self, term_width: u16) -> u16 {
        // Must use the same inner width as the renderer (term_width minus the
        // INPUT_PAD padding on each side), or the box height can disagree with
        // the number of wrapped lines actually drawn — e.g. a wrap that the
        // renderer sees at the true inner width would not yet grow the box.
        let inner = input_inner_width(term_width);
        if inner < 1 {
            return 1;
        }
        let visual = cached_visual_lines(
            &self.input.text,
            inner,
            self.input.generation,
            &mut self.input.lines_cache,
        );
        (visual.len() as u16).clamp(MIN_INPUT_CONTENT_LINES, MAX_INPUT_CONTENT_LINES)
    }

    /// Total height of the input bar (content + borders).
    pub(crate) fn input_bar_height(&mut self, term_width: u16) -> u16 {
        self.input_bar_content_lines(term_width) + 2
    }

    /// The five vertical chunks of the Chat page: history, status/error, help,
    /// command input box, status bar.
    ///
    /// Single source of truth for the Chat page's vertical layout —
    /// `render_chat` draws into these chunks, `input_box_rect` hit-tests clicks
    /// against chunk 3, and `update_viewport_from_terminal_size` sizes the
    /// history viewport from chunk 0.  Every consumer runs the *identical*
    /// `Layout::split`, so they can never drift apart — including on terminals
    /// too small for the fixed chrome to fit, where the solver shrinks and
    /// relocates chunks rather than honouring every `Length`.
    pub(crate) fn chat_page_layout(&mut self, term_width: u16, term_height: u16) -> [Rect; 5] {
        let status_error_height = self.status_error_height(term_width);
        let help_height = if self.show_ctrl_help { 2u16 } else { 0u16 };
        let input_height = self.input_bar_height(term_width);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(status_error_height),
                Constraint::Length(help_height),
                Constraint::Length(input_height),
                Constraint::Length(STATUS_BAR_HEIGHT),
            ])
            .split(Rect {
                x: 0,
                y: 0,
                width: term_width,
                height: term_height,
            });
        [chunks[0], chunks[1], chunks[2], chunks[3], chunks[4]]
    }

    /// Rectangle (terminal coordinates) occupied by the command input box on
    /// the Chat page, including its top/bottom borders.
    ///
    /// Delegates to [`Self::chat_page_layout`] so mouse hit-testing (clicking
    /// to position the cursor) always agrees with what `render_chat` draws —
    /// even on tiny terminals where the layout solver shrinks the box rather
    /// than placing it at a fixed distance above the status bar.
    pub(crate) fn input_box_rect(&mut self, term_width: u16, term_height: u16) -> Rect {
        self.chat_page_layout(term_width, term_height)[3]
    }

    pub(crate) fn update_viewport_from_terminal_size(&mut self) {
        let size = if self.terminal_resized || self.last_terminal_size.is_none() {
            if let Ok(size) = crossterm::terminal::size() {
                self.last_terminal_size = Some(size);
                self.terminal_resized = false;
                size
            } else {
                return;
            }
        } else {
            match self.last_terminal_size {
                Some(s) => s,
                None => return,
            }
        };
        let (width, height) = size;
        // The history viewport must match what render_chat actually draws:
        // chunk 0 of the shared Chat-page layout, minus the reserved scrollbar
        // column.  Deriving it from the solver output (rather than
        // `height - bottom_height`) keeps the viewport faithful even on
        // terminals too small for the fixed chrome to fit, where the solver
        // shrinks chunks — so the history-box mouse arm can never swallow
        // clicks that the renderer drew as part of the input box.
        let [history_area, _, _, _, _] = self.chat_page_layout(width, height);
        let old_width = self.history_viewport.width;
        let old_height = self.history_viewport.height;
        let new_width = history_area.width.saturating_sub(1);
        let new_height = history_area.height;
        self.history_viewport.update(Rect {
            x: 0,
            y: 0,
            width: new_width,
            height: new_height,
        });
        if old_width != new_width || old_height != self.history_viewport.height {
            // A selection is stored as (content line, viewport column); a
            // terminal resize re-wraps every line, so a stored column no
            // longer points at the same text (and the anchor is deliberately
            // never re-resolved — only the head follows the cursor).  Drop
            // the gesture like suspend/page-switch do.
            self.text_selection = None;
            for display in self.session_displays.values_mut() {
                display.render_cache.fill(None);
                display.markers_dirty = true;
                if old_width != new_width {
                    tracing::debug!(
                        "width changed ({} → {}), clearing content_dirty",
                        old_width,
                        new_width,
                    );
                    display.content_dirty = false;
                }
            }
        }
        // Session-manager list rows: full height minus the status bar (1),
        // the bordered list block (2), and the table header (1).  Must stay
        // in sync with render_session_list_view's layout; navigation uses
        // this cached height to decide when to shift the list window.
        // Computed here (outside the draw closure) because the renderer
        // never mutates app state.
        self.session_mgr.viewport_height = height.saturating_sub(4) as usize;
        // The picker popups (the wizard's provider picker and the model
        // selector) both render their lists in the LIST-popup body; cache
        // that body height so arrow/wheel navigation can pin the highlight at
        // the middle row and scroll the list under it.  Same layout math as
        // the renderers and the mouse hit-testers (`selector_list_layout`),
        // so the cache can never drift from what is drawn.  Only computed
        // while a picker is actually open: the value is consumed solely by
        // open-picker navigation/click handling, and building the layout (a
        // `Block` + `Layout::split`) every frame when no picker is up is
        // pure waste.  Both stay 0 until the first frame (viewport unknown),
        // mirroring `session_mgr.viewport_height` — navigation falls back to
        // focus-only moves then and `picker_window` clamps at render time.
        if self.model_selector.is_open() || self.ai_providers.wizard.is_open() {
            let selector_layout = selector_list_layout(Rect {
                x: 0,
                y: 0,
                width,
                height,
            });
            self.ai_providers.wizard.viewport_height = selector_layout.body.height as usize;
            self.model_selector.viewport_height = selector_layout.body.height as usize;
        }
    }

    pub(crate) fn mark_terminal_resized(&mut self) {
        self.terminal_resized = true;
    }

    pub(crate) fn total_history_height(&self) -> usize {
        self.active_display_ref()
            .map(|d| d.total_history_height())
            .unwrap_or(0)
    }

    /// Whether the vertical scrollbar is currently rendered.  Must stay in
    /// lockstep with the click handling so a hidden scrollbar never swallows
    /// clicks in its (still-reserved) column on sessions whose history fits
    /// the viewport.
    pub(crate) fn scrollbar_visible(&self) -> bool {
        self.total_history_height() > self.history_viewport.height as usize
    }

    #[cfg(test)]
    pub(crate) fn rebuild_height_prefix(&mut self) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.rebuild_height_prefix(&vp);
        }
    }

    pub(crate) fn compute_total_height_and_markers(&mut self) -> usize {
        let vp = self.history_viewport;
        self.active_display()
            .map(|d| d.compute_total_height_and_markers(&vp))
            .unwrap_or(1)
    }

    #[cfg(test)]
    pub(crate) fn mark_streaming_changed(&mut self) {
        if let Some(d) = self.active_display() {
            d.mark_streaming_changed();
        }
    }

    pub(crate) fn max_scroll_offset(&self) -> usize {
        self.active_display_ref()
            .map(|d| d.max_scroll_offset(&self.history_viewport))
            .unwrap_or(0)
    }

    pub(crate) fn clamp_scroll_state(&mut self) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.clamp_scroll_state(&vp);
        }
    }

    pub(crate) fn image_block_height(&self) -> u16 {
        self.active_display_ref()
            .map(|d| d.image_block_height(&self.history_viewport))
            .unwrap_or(1)
    }

    pub(crate) fn ensure_cache_synced(&mut self) {
        if let Some(d) = self.active_display() {
            d.ensure_cache_synced();
        }
    }

    pub(crate) fn sync_turn_images(&mut self, session_id: u64, turn_id: u32, turn: &Turn) {
        let images = self
            .rendered_images
            .entry(session_id)
            .or_default()
            .entry(turn_id)
            .or_default();
        for (idx, record) in turn.displayed_images.iter().enumerate() {
            images.entry(idx).or_insert_with(|| {
                RenderedImage::new_placeholder(
                    record.metadata.clone(),
                    Arc::from(record.data.clone()),
                )
            });
        }
        images.retain(|&idx, _| idx < turn.displayed_images.len());
    }

    pub(crate) fn apply_image_result(&mut self, result: ImageResult) {
        let (session_id, turn_id, img_idx) = match self.pending_job_idx.remove(&result.id) {
            Some(key) => key,
            None => return,
        };
        if let Some(session_images) = self.rendered_images.get_mut(&session_id)
            && let Some(images) = session_images.get_mut(&turn_id)
            && let Some(img) = images.get_mut(&img_idx)
            && img.pending_job == Some(result.id)
        {
            tracing::trace!(
                "[choreo-tui] image job {} completed for session {} turn {} img {}",
                result.id,
                session_id,
                turn_id,
                img_idx,
            );
            img.apply_result(result);
        }
    }

    // All eight parameters are already owned by the caller (an image-ready
    // event handler); grouping them would only add a wrapper struct without
    // reducing the information flow.
    #[expect(clippy::too_many_arguments)]
    pub(crate) fn submit_image_job(
        &mut self,
        session_id: u64,
        turn_id: u32,
        img_idx: usize,
        data: std::sync::Arc<[u8]>,
        metadata: choreo_proto::ImageMetadata,
        cell_size: Size,
        resize: ratatui_image::Resize,
    ) -> Option<ImageId> {
        let tx = self.image_job_tx.as_ref()?;
        let id = next_job_id();

        tracing::trace!(
            "[choreo-tui] submitting image job {} for session {} turn {} img {} ({} {}x{} @ {}x{})",
            id,
            session_id,
            turn_id,
            img_idx,
            metadata.mime_type,
            metadata.width,
            metadata.height,
            cell_size.width,
            cell_size.height,
        );

        self.pending_job_idx
            .insert(id, (session_id, turn_id, img_idx));

        if let Some(session_images) = self.rendered_images.get_mut(&session_id)
            && let Some(images) = session_images.get_mut(&turn_id)
            && let Some(img) = images.get_mut(&img_idx)
        {
            img.pending_job = Some(id);
        }

        let _ = tx.send(ImageJob {
            id,
            data,
            metadata,
            cell_size,
            resize,
        });
        Some(id)
    }

    pub(crate) fn reset_for_session_switch(&mut self, session_id: u64) {
        self.active_session_id = Some(session_id);
        // A selection is keyed to the previous session's rendered content in
        // screen coordinates; it must not linger and highlight the next
        // session's history.
        self.text_selection = None;
        let display = self.display_for(session_id);
        // Keep the session's live state: `view.turns` and `view.request_to_turn`
        // (accumulated via the all-activity subscription while the user was
        // viewing another session), the active-request set, live token
        // estimates, and per-turn reasoning preferences.  Destroying these on
        // switch was the root cause of "switching to a streaming session shows
        // nothing until the next turn": the accumulated content AND the
        // request→turn routing map were wiped exactly when they were needed,
        // and the attach snapshot only holds the empty in-flight placeholder.
        //
        // Only transient *render* state is reset here — it is rebuilt on the
        // next layout pass because `markers_dirty` forces a full rebuild from
        // the preserved `view.turns`.
        display.render_cache.clear();
        display.visible_turn_ids.clear();
        display.history_scroll = HistoryScrollState::new();
        display.markers.clear();
        display.height_prefix.clear();
        display.turn_heights.clear();
        display.turn_layouts.clear();
        display.streaming_turn_index = None;
        display.streaming_dirty = false;
        display.markers_dirty = true;
        display.content_dirty = false;
        display.status = None;
        display.error = None;
        display.progress_dirty = true;
        self.fullscreen_image_target = None;
    }

    pub(crate) fn effective_scroll(&self) -> usize {
        self.active_display_ref()
            .map(|d| d.effective_scroll(&self.history_viewport))
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn scrollbar_notch(&self) -> usize {
        self.active_display_ref()
            .map(|d| d.scrollbar_notch(&self.history_viewport))
            .unwrap_or(1)
    }

    pub(crate) fn scroll_up(&mut self, amount: usize) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scroll_up(amount, &vp);
        }
    }

    pub(crate) fn scroll_down(&mut self, amount: usize) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scroll_down(amount, &vp);
        }
    }

    pub(crate) fn scroll_to(&mut self, row: usize) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scroll_to(row, &vp);
        }
    }

    pub(crate) fn scroll_to_track_row(&mut self, mouse_row: u16, track_height: u16) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scroll_to_track_row(mouse_row, track_height, &vp);
        }
    }

    pub(crate) fn scroll_to_content_line(&mut self, content_line: usize) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scroll_to_content_line(content_line, &vp);
        }
    }

    pub(crate) fn scrollbar_scroll_up(&mut self) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scrollbar_scroll_up(&vp);
        }
    }

    pub(crate) fn scrollbar_scroll_down(&mut self) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scrollbar_scroll_down(&vp);
        }
    }

    pub(crate) fn apply_scroll_delta(&mut self) {
        let delta = self.scroll_accumulator;
        self.scroll_accumulator = 0;
        if delta > 0 {
            self.scroll_up(delta as usize);
        } else if delta < 0 {
            self.scroll_down((-delta) as usize);
        }
    }

    pub(crate) fn user_texts(&self) -> Vec<String> {
        self.active_display_ref()
            .map(|d| {
                d.view
                    .turns
                    .iter()
                    .rev()
                    .filter_map(|(_, turn)| turn.user_text.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn navigate_history_up(&mut self) {
        let texts = self.user_texts();
        if texts.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            // First Up press: stash the user's real draft (text *and* cursor)
            // before loading the newest history entry into the buffer.
            self.saved_draft = self.input.text.clone();
            self.saved_draft_cursor = self.input.cursor;
            self.load_history_entry(0, &texts);
            return;
        }
        if let Some(raw_idx) = self.history_index {
            // history_index was recorded against the turn list as it existed
            // when the user first pressed Up. The list may have shrunk since
            // (turns replaced, session switched), leaving the index past the
            // oldest remaining entry — clamp it and resync the displayed text
            // so Up mirrors Down (which clamps too) instead of silently
            // no-op'ing on a stale index.
            let idx = raw_idx.min(texts.len().saturating_sub(1));
            if idx != raw_idx {
                tracing::debug!(
                    stale = raw_idx,
                    clamped = idx,
                    "[choreo-tui] history index stale on Up; clamped to oldest remaining entry"
                );
                self.load_history_entry(idx, &texts);
                return;
            }
            let next = idx + 1;
            if next < texts.len() {
                self.load_history_entry(next, &texts);
            }
        }
    }

    pub(crate) fn navigate_history_down(&mut self) {
        if let Some(idx) = self.history_index {
            let texts = self.user_texts();
            if texts.is_empty() {
                // The conversation changed out from under us (e.g. the user
                // switched sessions mid-navigation) and no history remains to
                // walk back through — fall straight to the saved draft.
                self.restore_history_draft();
                return;
            }
            if idx == 0 {
                // Already at the newest entry: Down exits back to the draft.
                self.restore_history_draft();
                return;
            }
            // history_index was recorded against the turn list as it existed
            // when the user pressed Up. The list may have shrunk since (turns
            // replaced, session switched), so a step toward the newest entry
            // can land past the end — clamp to the newest remaining entry
            // instead of indexing out of bounds.
            let prev = (idx - 1).min(texts.len() - 1);
            if idx >= texts.len() {
                tracing::debug!(
                    stale = idx,
                    clamped = prev,
                    "[choreo-tui] history index stale on Down; clamped to newest remaining entry"
                );
            }
            self.load_history_entry(prev, &texts);
        }
    }

    /// Load the history entry at `idx` into the input: stash the index, set
    /// the text, move the cursor to the end, and keep it visible.  Shared by
    /// all the "step through history" paths so they can't drift apart.  The
    /// loaded text is recorded too, so `restore_history_draft` can detect when
    /// the user has edited the entry on top of it.
    fn load_history_entry(&mut self, idx: usize, texts: &[String]) {
        self.history_index = Some(idx);
        let entry = texts[idx].clone();
        self.history_entry_text = Some(entry.clone());
        self.input.text = entry;
        self.input.generation += 1;
        self.input.cursor = self.input.text.len();
        self.ensure_input_cursor_visible();
    }

    /// Drop history navigation and put the user's saved draft back in the
    /// input, clearing the stash.  Shared by all the "exit to draft" paths.
    ///
    /// The cursor stashed on the first Up press is restored too, so exiting
    /// mid-editing lands back exactly where the user was typing.  If the
    /// buffer was edited after the history entry loaded, those edits are the
    /// user's real draft — keep the buffer as-is instead of silently
    /// discarding them in favour of the pre-Up stash.
    fn restore_history_draft(&mut self) {
        self.history_index = None;
        if let Some(entry) = self.history_entry_text.take()
            && self.input.text != entry
        {
            // The buffer diverged from the loaded history entry: the user
            // typed over it, so the buffer holds what they actually want.
            tracing::debug!("[choreo-tui] history entry edited; keeping buffer as the draft");
            self.saved_draft.clear();
            self.saved_draft_cursor = 0;
            return;
        }
        // Move the draft out of its stash rather than cloning it: the stash
        // is consumed here and the input buffer takes ownership of the bytes.
        let cursor = self.saved_draft_cursor;
        self.input.text = std::mem::take(&mut self.saved_draft);
        self.saved_draft_cursor = 0;
        self.input.generation += 1;
        self.input.cursor = cursor;
        self.ensure_input_cursor_visible();
    }

    pub(crate) fn commit_to_history(&mut self) {
        self.history_index = None;
        self.history_entry_text = None;
        self.saved_draft.clear();
        self.saved_draft_cursor = 0;
    }

    /// Clear the draft stashed for the currently attached session, mirroring
    /// the input bar being cleared when a prompt is submitted.  Without this
    /// a submitted prompt would resurface the next time the user returns to
    /// the session — the draft must reflect only *unsent* text.
    pub(crate) fn clear_current_draft(&mut self) {
        if let Some(session_id) = self.attached_session_id {
            let display = self.display_for(session_id);
            if !display.draft.is_empty() {
                tracing::trace!(session_id, "cleared per-session draft after submit");
            }
            display.draft.clear();
            display.draft_cursor = 0;
        }
    }

    pub(crate) fn ensure_input_cursor_visible(&mut self) {
        if let Some((term_w, _)) = self.last_terminal_size {
            // inner width must match the renderer's drawing width so the
            // scroll window matches what is actually displayed.
            let inner = input_inner_width(term_w);
            let visible_height = self.input_bar_content_lines(term_w) as usize;
            self.input.ensure_cursor_visible(inner, visible_height);
        }
    }

    pub(crate) fn set_page(&mut self, page: Page) {
        self.page = page;
        // A selection is stored in screen coordinates keyed to the Chat
        // page's rendered history; leaving the page (or re-entering via an
        // attach flow, which changes the underlying session) invalidates
        // that context, so drop the gesture rather than highlight stale rows
        // or swallow the first click on return.
        self.text_selection = None;
        if let Some(d) = self.active_display() {
            d.progress_dirty = true;
        }
    }

    // ── Legacy per-session daemon message handlers ─────────────────────

    pub(crate) fn handle_session_created(
        &mut self,
        session_id: u64,
        parent_session_id: Option<u64>,
        account_name: Option<String>,
        selected_model: Option<String>,
        reasoning_effort: Option<String>,
        client_tx: &std::sync::mpsc::Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        // Agent-spawned sub-sessions (parent_session_id = Some) are transient
        // tool artifacts, not sessions the user opened.  Auto-attaching to one
        // would hijack the Chat view away from the session the user is reading
        // and destroy its scroll position, so treat it like background noise.
        if let Some(parent_id) = parent_session_id {
            tracing::info!(
                session_id,
                parent_session_id = parent_id,
                "sub-session created — not auto-attaching",
            );
        }

        if self.page == Page::SessionManager {
            // Best-effort refresh for both kinds of session: a broken channel
            // here means the whole connection is tearing down, and the reply
            // renders into the session list (never the status line), so there
            // is nothing actionable to propagate.
            if let Some(parent_id) = parent_session_id {
                tracing::debug!(
                    session_id,
                    parent_session_id = parent_id,
                    "refreshing session list so the sub-session is visible on the Session Manager page",
                );
            }
            let _ = client_tx.send(ClientMessage::ListSessions);
            return Ok(());
        }

        // Sub-session created from the Chat page: an unsolicited ListSessions
        // would make the daemon reply with `Sessions`, whose handler writes
        // the global status line and reflows the viewed viewport — the very
        // symptom this path exists to prevent.  Never auto-attach either.
        if parent_session_id.is_some() {
            return Ok(());
        }

        // User-created session from the Chat page: send ListSessions before
        // AttachSession so the session summary list is populated before
        // SessionAttached triggers handle_session_attached.  Unlike the
        // Session Manager refresh above, this send is propagated: the attach
        // below depends on the summary reply arriving in order.
        client_tx
            .send(ClientMessage::ListSessions)
            .map_err(broken_pipe)?;
        // The input bar may hold an unsent prompt belonging to the session
        // the user was viewing before the new session was created — hand it
        // over via `persist_input_draft` so the prompt can't leak into a
        // session it was never meant for.
        self.persist_input_draft(session_id);
        self.reset_for_session_switch(session_id);
        self.attached_session_id = Some(session_id);
        // Set display fields immediately so they're available when
        // SessionAttached arrives — check the session summary first,
        // then fall back to the creation parameters.
        {
            let display = self.display_for(session_id);
            display.account_name = account_name;
            display.selected_model = selected_model;
            display.reasoning_effort = reasoning_effort;
        }
        client_tx
            .send(ClientMessage::AttachSession { session_id })
            .map_err(broken_pipe)?;
        Ok(())
    }

    pub(crate) fn handle_session_attached(&mut self, session_id: u64) {
        self.active_session_id = Some(session_id);
        self.attached_session_id = Some(session_id);
        // Copy session summary fields before borrowing display.
        let (
            token_usage,
            context_window,
            last_prompt_tokens,
            account_name,
            selected_model,
            reasoning_effort,
            working_dir,
            status,
        ) = self
            .session_mgr
            .sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .map(|s| {
                (
                    s.token_usage,
                    s.context_window,
                    s.last_prompt_tokens,
                    s.account_name.clone(),
                    s.selected_model.clone(),
                    s.reasoning_effort.clone(),
                    s.working_dir.clone(),
                    Some(s.status.clone()),
                )
            })
            .unwrap_or((None, None, None, None, None, None, None, None));
        {
            let display = self.display_for(session_id);
            // Fill gaps from the (potentially stale) session summary, but never
            // clobber values already accumulated via the all-activity
            // subscription while this session was in the background: the
            // summary is refreshed on ListSessions, whereas the display may
            // hold fresher per-turn token usage / live counts / model that
            // arrived mid-stream.  Overwriting here would regress the status
            // bar right after switching into a streaming session.
            if display.token_usage.is_none() {
                display.token_usage = token_usage;
            }
            if display.context_window.is_none() {
                display.context_window = context_window;
            }
            if display.last_prompt_tokens.is_none() {
                display.last_prompt_tokens = last_prompt_tokens;
            }
            if display.account_name.is_none() {
                display.account_name = account_name;
            }
            if display.selected_model.is_none() {
                display.selected_model = selected_model;
            }
            if display.reasoning_effort.is_none() {
                display.reasoning_effort = reasoning_effort;
            }
            if display.working_dir.is_none() {
                display.working_dir = working_dir;
            }
            if let Some(ref st) = status {
                display.status = Some(format!("{:?}", st));
            }
        }
        self.attached_status = status;
        self.refresh_attached_account_slug();
        self.show_ctrl_help = true;
        if let Some(d) = self.active_display() {
            d.progress_dirty = true;
        }
    }

    /// The account slug shown in the status bar: the attached session's
    /// account name (the account name is the slug users enter when creating
    /// one). Previously this showed the inference provider slug resolved via
    /// the accounts list, but the account itself is the more useful identity.
    pub(crate) fn refresh_attached_account_slug(&mut self) {
        self.attached_account_slug = self
            .active_display_ref()
            .and_then(|d| d.account_name.clone());
    }

    pub(crate) fn attached_session_mut(&mut self) -> Option<&mut SessionSummary> {
        self.session_mgr
            .sessions
            .iter_mut()
            .find(|s| Some(s.session_id) == self.attached_session_id)
    }

    /// The summary of `session_id`, but only when it is the attached session.
    ///
    /// Per-session display updates mirror into the status bar's summary
    /// exclusively for the attached session — a background session's model,
    /// effort or account change must never rewrite the identity fields of the
    /// session on screen.
    fn mirror_to_attached_summary(&mut self, session_id: u64) -> Option<&mut SessionSummary> {
        if self.attached_session_id == Some(session_id) {
            self.attached_session_mut()
        } else {
            None
        }
    }

    /// A model was selected on the session `session_id`.  Only that session's
    /// display (and, when it is the attached session, the summary used by the
    /// status bar) is updated — a `ModelSelected` broadcast for a background
    /// session must never overwrite the display the user is currently viewing.
    pub(crate) fn handle_model_selected(
        &mut self,
        session_id: u64,
        model: &str,
        reasoning_capability: Option<ReasoningCapability>,
    ) {
        let display = self.display_for(session_id);
        display.selected_model = Some(model.to_owned());
        display.reasoning_capability = reasoning_capability;
        if let Some(s) = self.mirror_to_attached_summary(session_id) {
            s.selected_model = Some(model.to_owned());
        }
    }

    /// A reasoning-effort change was accepted on the session `session_id`.
    /// Routed to that session's own display only — see `handle_model_selected`.
    pub(crate) fn handle_reasoning_effort_set(&mut self, session_id: u64, effort: String) {
        let display = self.display_for(session_id);
        display.reasoning_effort = Some(effort.clone());
        if let Some(s) = self.mirror_to_attached_summary(session_id) {
            s.reasoning_effort = Some(effort);
        }
    }

    pub(crate) fn handle_session_working_dir_set(
        &mut self,
        session_id: u64,
        path: &Option<String>,
    ) {
        if self.attached_session_id == Some(session_id) {
            if let Some(d) = self.active_display() {
                d.working_dir = path.clone();
                d.progress_dirty = true;
            }
            if let Some(s) = self.attached_session_mut() {
                s.working_dir = path.clone();
            }
        }
    }

    pub(crate) fn handle_session_title_set(&mut self, session_id: u64, title: &str) {
        if self.attached_session_id == Some(session_id) {
            self.status = Some(format!("Session title changed to '{title}'"));
            if let Some(s) = self.attached_session_mut() {
                s.title = Some(title.to_owned());
            }
        }
    }

    /// The account for the session `session_id` was set.  Only that session's
    /// display is updated; the status bar's provider slug and the session
    /// summary are refreshed only when the message belongs to the attached
    /// session (a background session's account change must not alter the
    /// attached session's identity fields).
    pub(crate) fn handle_session_account_set(&mut self, session_id: u64, account: &str) {
        let display = self.display_for(session_id);
        display.account_name = Some(account.to_owned());
        if let Some(s) = self.mirror_to_attached_summary(session_id) {
            s.account_name = Some(account.to_owned());
        }
        // Refresh the status-bar account slug only for the attached session
        // (it reads the display account name set above).
        if self.attached_session_id == Some(session_id) {
            self.refresh_attached_account_slug();
        }
    }

    pub(crate) fn handle_session_status_changed(
        &mut self,
        session_id: u64,
        status: &SessionStatus,
        last_modified: i64,
    ) {
        if let Some(session) = self
            .session_mgr
            .sessions
            .iter_mut()
            .find(|s| s.session_id == session_id)
        {
            session.status = status.clone();
            // last_modified is monotonic; guard against duplicate or
            // out-of-order deliveries (per-session + summary paths).
            session.last_modified = session.last_modified.max(last_modified);
        }
        // A status change bumps last_modified on the daemon, so the list may
        // reorder while the user is looking at it — re-sort but keep the
        // cursor on the same session.
        self.session_mgr.resort_after_status_change();
        if let Some(ref mut detail) = self.session_mgr.detail_data
            && detail.session_id == session_id
        {
            detail.status = status.clone();
        }
        if self.attached_session_id == Some(session_id) {
            self.attached_status = Some(status.clone());
        }
    }

    /// Detect when the user is reading an agent-spawned sub-session on the
    /// Chat page and that sub-session just finished running.
    ///
    /// A sub-session "finishes" when its status transitions from an active
    /// state (inference / tool call / retrying) to an idle one (inactive /
    /// sleeping) — the daemon broadcasts exactly one such transition when the
    /// child's request completes.  The check reads the *pre-update* summary
    /// status (the caller invokes this before applying the new status), so
    /// duplicate idle→idle broadcasts — summary refreshes, or re-attaching to
    /// a child that finished earlier — never re-fire the switch.
    ///
    /// Returns the parent session id to switch back to, or `None` when the
    /// user is not viewing a finishing sub-session.  The parent id (and the
    /// titles for the notification) come from the summary list, so a missing
    /// summary — or a parent that no longer exists in it — is a graceful
    /// no-op rather than a misdirected switch.
    pub(crate) fn attached_subsession_finished(
        &self,
        session_id: u64,
        new_status: &SessionStatus,
    ) -> Option<u64> {
        // Only the Chat page: the Session Manager is a browsing view, and
        // auto-jumping away from it would fight the user's navigation.
        if self.page != Page::Chat || self.attached_session_id != Some(session_id) {
            return None;
        }
        // The finishing session must be an agent-spawned sub-session; its
        // parent is only known from the session summary list.
        let summary = self
            .session_mgr
            .sessions
            .iter()
            .find(|s| s.session_id == session_id)?;
        let parent_id = summary.parent_session_id?;
        // Only the active → idle transition counts as "finished".  Idle →
        // idle (e.g. a summary refresh after the child already finished)
        // must not yank the view away while the user is still reading.
        if summary.status.is_active()
            && !new_status.is_active()
            // The parent must still exist in the summary: if it was deleted
            // while the child ran, switching would attach to a dead session
            // id and strand the user on a session the daemon rejects.
            && self
                .session_mgr
                .sessions
                .iter()
                .any(|s| s.session_id == parent_id)
        {
            Some(parent_id)
        } else {
            None
        }
    }

    /// Persist the input bar's current contents as the draft of the session
    /// the user is leaving, then load the target session's saved draft into
    /// the input bar.
    ///
    /// The input bar is a single shared buffer; without this hand-off an
    /// unsent prompt typed in one session would follow the user into the
    /// next.  Drafts live in the per-session display state, so they survive
    /// session switches and are dropped only when the session itself is
    /// deleted (`handle_session_deleted` removes the whole display).
    ///
    /// Callers invoke this *before* rebinding `attached_session_id` so it
    /// still names the session the input bar's current contents belong to.
    /// History navigation interacts with the draft: if the user was stepping
    /// through past prompts (Up), the buffer holds a history entry rather
    /// than their real draft — first drop back to the draft
    /// (`restore_history_draft`) so the stash captures what they actually
    /// typed instead of a history entry.  With nothing attached (the startup
    /// auto-attach), there is no outgoing session to stash into; text already
    /// in the bar is kept rather than clobbered, and only a target with an
    /// empty bar gets its draft loaded.
    pub(crate) fn persist_input_draft(&mut self, target_session_id: u64) {
        if self.history_index.is_some() {
            self.restore_history_draft();
        }
        // Destructure `self` so the input buffer and the per-session display
        // map can be borrowed mutably at the same time (disjoint fields via
        // the pattern) — the hand-off below then moves `String`s around
        // instead of cloning them.
        let Self {
            input,
            session_displays,
            attached_session_id,
            ..
        } = self;
        // Stash the outgoing session's input into its display, overwriting any
        // earlier draft.  The text is moved out of the buffer (not cloned), so
        // switching sessions transfers the bytes without allocating.
        if let Some(prev_id) = attached_session_id {
            let had_draft = !input.text.is_empty();
            let text = std::mem::take(&mut input.text);
            let cursor = input.cursor;
            let display = session_displays.entry(*prev_id).or_default();
            display.draft = text;
            display.draft_cursor = cursor;
            tracing::debug!(
                from_session = *prev_id,
                to_session = target_session_id,
                had_draft,
                "session switch: stashed outgoing input as per-session draft",
            );
        } else if !input.text.is_empty() {
            // Nothing is attached yet (the startup auto-attach path): the
            // input bar holds text typed before any session existed.  There is
            // no session to stash it into, so clobbering it with the target's
            // (empty) draft would silently destroy the user's typing — keep it.
            tracing::debug!(
                to_session = target_session_id,
                "auto-attach: keeping pre-attach input (no outgoing session)",
            );
            return;
        }
        // Move the target session's draft into the input bar.  Swapping
        // rather than cloning transfers the bytes; the target's draft slot is
        // emptied, which is fine — the input bar now owns that content and the
        // slot is overwritten again on the next switch away.
        let display = session_displays.entry(target_session_id).or_default();
        std::mem::swap(&mut input.text, &mut display.draft);
        std::mem::swap(&mut input.cursor, &mut display.draft_cursor);
        input.generation += 1;
        self.ensure_input_cursor_visible();
    }

    /// Attach the Chat view to `session_id` via the shared sequence every
    /// attach path follows (Session Manager list/detail Enter, and the
    /// sub-session finish switch-back).
    ///
    /// The daemon messages are sent *before* the local state is mutated, so a
    /// broken pipe leaves the view on the previous session instead of
    /// stranding the user on a session that was never attached.
    /// `UnsubscribeSessionsSummary` is idempotent on the daemon (removing a
    /// client that was never registered is a no-op), so it is safe to send
    /// unconditionally.  `reset_for_session_switch` runs before `set_page` so
    /// the subsequent `set_page` marks the target's display dirty — the one
    /// that will actually render next — and `attached_status` is refreshed
    /// immediately from the summary instead of waiting for the daemon's
    /// `SessionAttached` reply to arrive.
    pub(crate) fn attach_to_session(
        &mut self,
        session_id: u64,
        client_tx: &std::sync::mpsc::Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        client_tx
            .send(ClientMessage::UnsubscribeSessionsSummary)
            .map_err(broken_pipe)?;
        client_tx
            .send(ClientMessage::AttachSession { session_id })
            .map_err(broken_pipe)?;
        // Hand the input bar over to the target session (stash the outgoing
        // session's input, load the target's draft) before `attached_session_id`
        // is rebound below — it still names the session the input bar's
        // current contents belong to.  See `persist_input_draft`.
        self.persist_input_draft(session_id);
        // reset_for_session_switch first so the subsequent set_page marks the
        // target's display dirty — the one that will actually render next.
        self.reset_for_session_switch(session_id);
        self.set_page(Page::Chat);
        self.attached_session_id = Some(session_id);
        // Refresh the status bar right away from the summary; the daemon's
        // SessionAttached reply re-applies the same (possibly newer) value.
        self.attached_status = self
            .session_mgr
            .sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .map(|s| s.status.clone());
        Ok(())
    }

    /// Switch the Chat view back to the parent session of a sub-session that
    /// just finished, and surface a status notification explaining the jump.
    ///
    /// Delegates to [`attach_to_session`] — the same sequence the Session
    /// Manager Enter handlers use — so the daemon messages are sent *before*
    /// the local state is mutated, and a broken pipe leaves the view on the
    /// finished sub-session instead of stranding the user on a session that
    /// was never attached.
    pub(crate) fn switch_back_to_parent(
        &mut self,
        finished_session_id: u64,
        parent_id: u64,
        client_tx: &std::sync::mpsc::Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        // Titles come from the summary list — the same source that told us
        // the sub-session's parent — falling back to "untitled" exactly like
        // the session list renderer does.
        let title = |id: u64| {
            self.session_mgr
                .sessions
                .iter()
                .find(|s| s.session_id == id)
                .and_then(|s| s.title.clone())
                .unwrap_or_else(|| "untitled".to_string())
        };
        let subsession_title = title(finished_session_id);
        let parent_title = title(parent_id);

        self.attach_to_session(parent_id, client_tx)?;

        self.status = Some(format!(
            "Subsession \"{subsession_title}\" finished. Switched back to parent \"{parent_title}\"."
        ));
        Ok(())
    }

    pub(crate) fn handle_accounts(&mut self, accounts: &[AccountInfo]) {
        self.ai_providers.set_accounts(accounts.to_vec());
        self.refresh_attached_account_slug();
    }

    pub(crate) fn handle_sessions(
        &mut self,
        sessions: &[SessionSummary],
        client_tx: &std::sync::mpsc::Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        self.session_mgr.set_sessions(sessions.to_vec());
        if self.page == Page::Chat {
            if sessions.is_empty() {
                self.status = Some("[daemon] no sessions".to_string());
            } else {
                self.status = Some(format!("[daemon] sessions ({})", sessions.len()));
                for session in sessions {
                    let prefix = if Some(session.session_id) == self.attached_session_id {
                        "*"
                    } else {
                        " "
                    };
                    let title = session.title.as_deref().unwrap_or("untitled");
                    let model = session.selected_model.as_deref().unwrap_or("-");
                    self.status = Some(format!(
                        "{} {}: \"{title}\" ({model}) — {} turns",
                        prefix, session.session_id, session.turn_count,
                    ));
                }
            }
            if self.attached_session_id.is_none() {
                // Prefer the most recently modified *top-level* session.
                // Agent-spawned sub-sessions (parent_session_id = Some) are
                // transient tool artifacts whose last_modified is bumped as
                // they stream, so they'd otherwise top the list and silently
                // hijack the view to a session the user never opened — e.g.
                // its streaming token count would appear on the chat page.
                let target = sessions
                    .iter()
                    .find(|s| s.parent_session_id.is_none())
                    .or_else(|| sessions.first());
                if let Some(first) = target {
                    // Set attachment state immediately (mirroring the session
                    // manager Enter handler) so a second Sessions reply in the
                    // same tick cannot auto-attach again to a different
                    // session, and so the page renders the target session
                    // instead of a blank screen until SessionAttached arrives.
                    // Hand the input bar over like every other attach path;
                    // with nothing attached yet this only loads the target's
                    // draft (see `persist_input_draft`).
                    self.persist_input_draft(first.session_id);
                    self.reset_for_session_switch(first.session_id);
                    self.attached_session_id = Some(first.session_id);
                    client_tx
                        .send(ClientMessage::AttachSession {
                            session_id: first.session_id,
                        })
                        .map_err(broken_pipe)?;
                } else {
                    // Inherit account_name from the first available account,
                    // so the auto-created default session doesn't lose the
                    // account selection that was already configured.
                    let default_account =
                        self.ai_providers.accounts.first().map(|a| a.name.clone());
                    client_tx
                        .send(ClientMessage::CreateSession {
                            title: Some("default".to_string()),
                            parent_session_id: None,
                            working_dir: None,
                            context_config: None,
                            account_name: default_account,
                            selected_model: None,
                            reasoning_effort: None,
                        })
                        .map_err(broken_pipe)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn handle_session_deleted(&mut self, session_id: u64) {
        self.session_mgr.remove_session(session_id);
        self.session_displays.remove(&session_id);
        self.rendered_images.remove(&session_id);
        if self.attached_session_id == Some(session_id) {
            self.attached_session_id = None;
            self.active_session_id = None;
            self.attached_account_slug = None;
            // The deleted session's unsent prompt dies with it — the display
            // (and its draft) are gone above, so drop the input bar too
            // rather than leak the orphaned text into whichever session gets
            // attached next.
            let had_input = !self.input.text.is_empty();
            tracing::debug!(
                session_id,
                had_input,
                "deleted attached session: dropping its input draft",
            );
            self.input.clear();
            self.commit_to_history();
        }
    }

    pub(crate) fn handle_session_delete_failed(&mut self, session_id: u64, error: &str) {
        self.status = Some(format!(
            "failed to delete session {}: {}",
            session_id, error
        ));
    }

    pub(crate) fn display_token_usage(&self) -> Option<TokenUsage> {
        let display = self.active_display_ref()?;
        let usage = display.token_usage.as_ref()?;
        Some(TokenUsage {
            input_tokens: usage.input_tokens + display.live_input_estimate,
            output_tokens: usage.output_tokens + display.live_output_tokens,
            total_tokens: usage.total_tokens
                + display.live_input_estimate
                + display.live_output_tokens,
        })
    }
}

/// Merge a daemon-provided token usage into the display's accumulated value,
/// never regressing it.
///
/// Cumulative token usage only ever increases (the daemon accumulates per-turn
/// usage monotonically), so the merge is a per-field max via
/// [`TokenUsage::merge_max`].  This matters when switching into a session that
/// is mid-turn: the attach `SessionState` snapshot is built from the session
/// thread's config, which can lag the request worker's live accumulation (and,
/// in the worker→main sync window, the value already broadcast to this client).
/// A blind overwrite would regress the status bar's `↑in ↓out` readout until
/// the next `TokenUsageUpdate` — i.e. until the turn ends — while a `None`
/// snapshot must never wipe an accumulated total.
pub(crate) fn merge_token_usage(
    current: &Option<TokenUsage>,
    incoming: &Option<TokenUsage>,
) -> Option<TokenUsage> {
    match (current, incoming) {
        (Some(cur), Some(inc)) => {
            let mut merged = *cur;
            merged.merge_max(*inc);
            Some(merged)
        }
        (Some(cur), None) => Some(*cur),
        (None, Some(inc)) => Some(*inc),
        (None, None) => None,
    }
}

// ── SessionDisplayState methods ─────────────────────────────────────

impl SessionDisplayState {
    pub(crate) fn total_history_height(&self) -> usize {
        self.height_prefix.last().copied().unwrap_or(0)
    }

    /// Current content version for `turn_id` (0 when no mutation has ever
    /// been recorded for it).  Part of the render-cache key so a rebuild
    /// recomputes a turn whose content changed since it was cached.
    pub(crate) fn turn_content_version(&self, turn_id: u32) -> u64 {
        self.turn_versions.get(&turn_id).copied().unwrap_or(0)
    }

    /// Bump the content version for `turn_id` and return the new value.
    ///
    /// Must be called by every event handler that mutates a turn's rendered
    /// content — after `stream_chunk`/`tool_result_chunk`/`tool_call_started`
    /// on the streaming turn, and after `insert_or_replace`/snapshot merges.
    /// Wrapping is deliberate: a version collision after 2^64 mutations is
    /// astronomically improbable and only risks one stale cache hit.
    fn bump_turn_version(&mut self, turn_id: u32) -> u64 {
        // `entry().or_insert(0)` yields `&mut u64`; increment through the
        // reference (auto-deref for the RHS, write-back on the LHS) so the
        // new version is persisted in the map, then return it.
        let version = self.turn_versions.entry(turn_id).or_insert(0);
        *version = version.wrapping_add(1);
        *version
    }

    /// Rebuild height_prefix, markers, visible_turn_ids, and populate render_cache.
    pub(crate) fn rebuild_height_prefix(&mut self, viewport: &HistoryViewport) {
        self.height_prefix.clear();
        self.visible_turn_ids.clear();
        self.markers.clear();
        self.turn_layouts.clear();
        self.turn_heights.clear();
        let mut total = 0usize;
        let virtual_track = self.virtual_track_slots(viewport);
        let fallback_img_height = self.image_block_height(viewport) as usize;
        let turn_count = self.view.turns.len();
        tracing::trace!(turn_count, "rebuild_height_prefix");

        let visible_count = self.view.turns.iter().filter(|(_, t)| !t.undone).count();
        self.render_cache.resize(visible_count, None);

        let mut user_text_start_lines: Vec<usize> = Vec::with_capacity(turn_count);
        let mut visible_idx = 0usize;
        for (&turn_id, turn) in self.view.turns.iter() {
            if turn.undone {
                continue;
            }
            // Must stay in lockstep with render_history (render.rs) so the
            // render-cache key never drifts from what the renderer draws.
            let content_width = viewport.width.saturating_sub(9);
            let tool_content_width = viewport.width.saturating_sub(1);

            // Effective reasoning visibility for this turn: the per-turn
            // user override (from clicking the header), falling back to the
            // streaming-derived default.  The derived default is also stored
            // in the turn layout so the per-frame render path can reuse it
            // in O(1) without re-scanning turn strings.
            let reasoning_default_expanded = reasoning_expanded_default(turn);
            let reasoning_expanded =
                self.effective_reasoning_expanded(turn_id, reasoning_default_expanded);

            // Effective per-result collapse state, aligned with
            // `turn.tool_results`; part of the render-cache key so a stale
            // entry (rendered with a different visibility) is a miss.
            let tool_results_collapsed: Vec<bool> = turn
                .tool_results
                .iter()
                .map(|r| self.effective_tool_result_collapsed(turn_id, r))
                .collect();
            let key = RenderCacheKey {
                turn_id,
                width: content_width,
                viewport_width: viewport.width,
                reasoning_expanded,
                tool_results_collapsed,
                content_version: self.turn_content_version(turn_id),
            };
            let rendered_turn =
                cached_or_compute_lines(&mut self.render_cache, visible_idx, &key, || {
                    render_turn_lines(
                        turn,
                        content_width,
                        tool_content_width,
                        key.reasoning_expanded,
                        &key.tool_results_collapsed,
                    )
                });
            let text_height = rendered_turn.height;
            let text_offsets = rendered_turn.visual_offsets;
            let reasoning_header_idx = rendered_turn.reasoning_header_idx;
            let tool_result_header_idxs = rendered_turn.tool_result_header_idxs;

            // The reasoning header's visual-row range for click hit-testing.
            // The renderer reports the header's semantic-line index directly
            // (no output scanning); the cached offsets convert it to a
            // visual-row range — O(1) in the click handler, same approach
            // as image ranges.
            let reasoning_header_range = reasoning_header_idx.map(|idx| {
                let start = if idx == 0 { 0 } else { text_offsets[idx - 1] };
                let end = text_offsets[idx];
                (start, end)
            });

            // Same conversion for every tool result header, so clicking a
            // triangle toggles exactly that result.  One range per result,
            // aligned with `turn.tool_results`.
            let tool_result_header_ranges = tool_result_header_idxs
                .iter()
                .map(|&idx| {
                    let start = if idx == 0 { 0 } else { text_offsets[idx - 1] };
                    let end = text_offsets[idx];
                    (start, end)
                })
                .collect();

            let mut image_ranges: Vec<(usize, usize)> = Vec::new();
            let mut total_img_height: usize = 0;
            for _ in 0..turn.displayed_images.len() {
                let start = text_height + total_img_height;
                image_ranges.push((start, start + fallback_img_height));
                total_img_height += fallback_img_height;
            }
            self.turn_layouts.push(TurnLayout {
                reasoning_header_range,
                reasoning_default_expanded,
                image_ranges,
                tool_result_header_ranges,
            });
            let turn_height = text_height + total_img_height;
            self.turn_heights.push(turn_height);
            if turn.user_text.is_some() {
                user_text_start_lines.push(total);
            }
            total += turn_height;
            self.height_prefix.push(total);
            self.visible_turn_ids.push(turn_id);
            visible_idx += 1;
        }
        let final_total = total.max(1);
        tracing::trace!(
            marker_count = user_text_start_lines.len(),
            final_total,
            "computed markers"
        );
        self.markers.reserve(user_text_start_lines.len());
        for &start_line in &user_text_start_lines {
            let slot = start_line * virtual_track / final_total;
            self.markers.push(Marker {
                content_line: start_line,
                virtual_slot: slot,
            });
        }
        self.markers_dirty = false;
    }

    pub(crate) fn mark_streaming_changed(&mut self) {
        self.streaming_dirty = true;
        self.content_dirty = true;
    }

    pub(crate) fn mark_content_changed(&mut self) {
        self.markers_dirty = true;
        self.content_dirty = true;
        self.streaming_turn_index = None;
        self.streaming_dirty = false;
    }

    /// Effective reasoning visibility for a turn: an explicit override from
    /// clicking the header wins; otherwise the caller-provided derived
    /// default is used.  Callers compute the default either from the turn
    /// content (`reasoning_expanded_default`) or from the precomputed
    /// `TurnLayout` when one is available (per-frame render path).
    pub(crate) fn effective_reasoning_expanded(&self, turn_id: u32, default: bool) -> bool {
        self.reasoning_override
            .get(&turn_id)
            .copied()
            .unwrap_or(default)
    }

    /// Toggle the reasoning section's visibility for a turn (clicking the
    /// header).  Records the explicit user preference in `reasoning_override`
    /// and invalidates the turn's render cache so the change takes effect on
    /// the next frame.
    pub(crate) fn toggle_reasoning(&mut self, turn_id: u32) {
        let Some(turn) = self.view.turns.get(&turn_id) else {
            return;
        };
        let current = self.effective_reasoning_expanded(turn_id, reasoning_expanded_default(turn));
        self.reasoning_override.insert(turn_id, !current);
        if let Some(idx) = self.visible_turn_ids.iter().position(|id| *id == turn_id)
            && let Some(slot) = self.render_cache.get_mut(idx)
        {
            *slot = None;
        }
        self.mark_content_changed();
    }

    /// Effective collapse state for a tool result: an explicit override from
    /// clicking the header wins; otherwise the derived default (quiet tools
    /// collapsed, everything else expanded — see
    /// [`tool_result_default_collapsed`]) is used.
    pub(crate) fn effective_tool_result_collapsed(
        &self,
        turn_id: u32,
        record: &ToolResultRecord,
    ) -> bool {
        // Nested (turn → call_id → state) lookup borrows the record's
        // call_id — this runs per result per frame, so avoiding a clone
        // here keeps the render path allocation-free for the common case.
        self.tool_collapse_override
            .get(&turn_id)
            .and_then(|by_call| by_call.get(&record.call_id))
            .copied()
            .unwrap_or_else(|| tool_result_default_collapsed(record))
    }

    /// Toggle a tool result's collapse state (clicking its header row).
    /// Records the explicit user preference in `tool_collapse_override` and
    /// invalidates the turn's render cache so the change takes effect on
    /// the next frame.
    pub(crate) fn toggle_tool_result(&mut self, turn_id: u32, call_id: &str) {
        let Some(turn) = self.view.turns.get(&turn_id) else {
            return;
        };
        let Some(record) = turn.tool_results.iter().find(|r| r.call_id == call_id) else {
            return;
        };
        let current = self.effective_tool_result_collapsed(turn_id, record);
        self.tool_collapse_override
            .entry(turn_id)
            .or_default()
            .insert(call_id.to_string(), !current);
        if let Some(idx) = self.visible_turn_ids.iter().position(|id| *id == turn_id)
            && let Some(slot) = self.render_cache.get_mut(idx)
        {
            *slot = None;
        }
        self.mark_content_changed();
    }

    pub(crate) fn resolve_streaming_turn_index(&mut self, request_id: u32) {
        if self.streaming_turn_index.is_none()
            && let Some(&turn_id) = self.view.request_to_turn.get(&request_id)
        {
            self.streaming_turn_index = self.visible_turn_ids.iter().position(|id| *id == turn_id);
        }
    }

    pub(crate) fn compute_total_height_and_markers(&mut self, viewport: &HistoryViewport) -> usize {
        // Streaming content updates run FIRST — even when a separate event
        // also marked markers_dirty — so a mid-stream `Done`/`TurnAppended`/
        // `SessionState` (from this session or, via the all-activity
        // subscription, a busy background session) can never force the per-
        // chunk cost onto the O(n) full rebuild.  The fast path re-renders
        // only the streaming turn and applies its height delta incrementally;
        // the rebuild below (if markers_dirty is still set) then reuses the
        // fresh cache entry, so it only recomputes turns whose content
        // actually changed (content-version key) instead of re-rendering
        // everything.
        if self.streaming_dirty {
            self.apply_streaming_update(viewport);
        }
        if self.markers_dirty {
            let at_bottom = self.effective_scroll(viewport) == 0;
            let preserve_scroll = self.content_dirty && !at_bottom;
            let old_total = if preserve_scroll {
                self.total_history_height()
            } else {
                0
            };

            self.rebuild_height_prefix(viewport);

            if preserve_scroll {
                let new_total = self.total_history_height();
                if new_total > old_total {
                    self.history_scroll.scroll = self
                        .history_scroll
                        .scroll
                        .saturating_add(new_total - old_total);
                } else if old_total > new_total {
                    // Content shrank (e.g. collapsing a reasoning section or
                    // undoing turns).  Pull the scroll offset up by the
                    // removed height so the same content rows stay anchored
                    // in the viewport instead of jumping to the bottom.
                    self.history_scroll.scroll = self
                        .history_scroll
                        .scroll
                        .saturating_sub(old_total - new_total);
                }
            }

            self.content_dirty = false;
            self.streaming_dirty = false;
        }

        self.total_history_height().max(1)
    }

    fn apply_streaming_update(&mut self, viewport: &HistoryViewport) {
        let Some(turn_idx) = self.streaming_turn_index else {
            return self.rebuild_height_prefix_preserving_scroll(viewport);
        };
        if turn_idx >= self.visible_turn_ids.len() {
            return self.rebuild_height_prefix_preserving_scroll(viewport);
        }

        let turn_id = self.visible_turn_ids[turn_idx];
        let Some(turn) = self.view.turns.get(&turn_id) else {
            return self.rebuild_height_prefix_preserving_scroll(viewport);
        };

        // Must stay in lockstep with render_history (render.rs) so the
        // render-cache key never drifts from what the renderer draws.
        let content_width = viewport.width.saturating_sub(9);
        let tool_content_width = viewport.width.saturating_sub(1);

        // Re-render with the effective reasoning visibility so the streaming
        // fast path stays consistent with the collapsed/expanded state.  The
        // derived default is stored back into the turn layout, keeping the
        // per-frame render path O(1).
        let reasoning_default_expanded = reasoning_expanded_default(turn);
        let reasoning_expanded =
            self.effective_reasoning_expanded(turn_id, reasoning_default_expanded);

        // Effective per-result collapse state (aligned with tool_results).
        // This is what makes "stream while toggled visible" work: chunks
        // re-render the streaming turn with the user's visibility choice,
        // so an expanded result grows live and a collapsed one stays flat.
        let tool_results_collapsed: Vec<bool> = turn
            .tool_results
            .iter()
            .map(|r| self.effective_tool_result_collapsed(turn_id, r))
            .collect();

        // Snapshot the turn's current content version before the mutable
        // `render_cache` borrow below — the lookup borrows all of `self`,
        // which would conflict with the `get_mut` held across the cache write.
        let content_version = self.turn_content_version(turn_id);

        if let Some(Some(cached)) = self.render_cache.get_mut(turn_idx)
            && cached.key.turn_id == turn_id
            && cached.key.width == content_width
            && cached.key.viewport_width == viewport.width
        {
            let rendered = render_turn_lines(
                turn,
                content_width,
                tool_content_width,
                reasoning_expanded,
                &tool_results_collapsed,
            );
            // Pin the same parallel-array invariant the rebuild path asserts
            // in `cached_or_compute_lines`: the streaming fast path replaces
            // the cache entry wholesale, so a join/content-range mismatch
            // here would silently slip into the cache and degrade a later
            // selection copy to newline-joined rows.  The asserts catch the
            // drift in debug builds before the Arc conversions hide it.
            debug_assert_eq!(
                rendered.lines.len(),
                rendered.joins.len(),
                "joins must align with the lines"
            );
            debug_assert_eq!(
                rendered.lines.len(),
                rendered.content_ranges.len(),
                "content ranges must align with the lines"
            );
            let text_lines = rendered.lines;
            let text_height = lines_height(&text_lines, viewport.width).max(1);
            let visual_offsets = compute_visual_offsets(&text_lines, viewport.width);
            let content_ranges = Arc::from(rendered.content_ranges);
            let joins = Arc::from(rendered.joins);

            // Keep the reasoning header's click-hit range and the precomputed
            // default in sync as the response streams — the header sits below
            // the growing response, so its position shifts on every chunk.
            // Rebuilds (via `rebuild_height_prefix`) recompute from scratch.
            if let Some(layout) = self.turn_layouts.get_mut(turn_idx) {
                layout.reasoning_header_range = rendered.reasoning_header_idx.map(|idx| {
                    let start = if idx == 0 { 0 } else { visual_offsets[idx - 1] };
                    let end = visual_offsets[idx];
                    (start, end)
                });
                layout.reasoning_default_expanded = reasoning_default_expanded;
                // Same sync for tool result headers: they sit below the
                // growing response too, and their own bodies grow when
                // expanded, so their click ranges shift on every chunk.
                layout.tool_result_header_ranges = rendered
                    .tool_result_header_idxs
                    .iter()
                    .map(|&idx| {
                        let start = if idx == 0 { 0 } else { visual_offsets[idx - 1] };
                        let end = visual_offsets[idx];
                        (start, end)
                    })
                    .collect();
            }

            // Replace the cache entry wholesale with the freshly rendered
            // state so the next frame's lookup is a valid hit.  The key
            // records the turn's current content version, so a later rebuild
            // (which may run while this turn's chunks are still streaming)
            // recomputes instead of reusing these lines once more content
            // arrives.
            *cached = RenderedCache {
                key: RenderCacheKey {
                    turn_id,
                    width: content_width,
                    viewport_width: viewport.width,
                    reasoning_expanded,
                    tool_results_collapsed,
                    content_version,
                },
                rendered: RenderedTurn {
                    lines: Arc::from(text_lines),
                    height: text_height,
                    visual_offsets,
                    joins,
                    content_ranges,
                    reasoning_header_idx: rendered.reasoning_header_idx,
                    tool_result_header_idxs: rendered.tool_result_header_idxs,
                },
            };

            let full_img_height = self.image_block_height(viewport) as usize;
            let img_count = turn.displayed_images.len();
            let turn_height = text_height + img_count * full_img_height;

            let old_height = self.turn_heights[turn_idx];

            if turn_height > old_height {
                let delta = turn_height - old_height;
                self.turn_heights[turn_idx] = turn_height;
                for i in turn_idx..self.height_prefix.len() {
                    self.height_prefix[i] = self.height_prefix[i].saturating_add(delta);
                }
                let at_bottom = self.effective_scroll(viewport) == 0;
                if !at_bottom {
                    self.history_scroll.scroll = self.history_scroll.scroll.saturating_add(delta);
                }
                self.rebuild_markers(viewport);
            } else if old_height > turn_height {
                return self.rebuild_height_prefix_preserving_scroll(viewport);
            }
        } else {
            return self.rebuild_height_prefix_preserving_scroll(viewport);
        }

        self.streaming_dirty = false;
        // A structural rebuild may follow (markers_dirty was already set when
        // this streaming update ran): leave content_dirty set so that
        // rebuild's preserve-scroll logic can still anchor the viewport
        // against any *structural* height change layered on top of the
        // streaming delta (e.g. a turn appended mid-stream).  When no rebuild
        // follows, the fast path is the only consumer and clears it here.
        if !self.markers_dirty {
            self.content_dirty = false;
        }
    }

    fn rebuild_height_prefix_preserving_scroll(&mut self, viewport: &HistoryViewport) {
        let at_bottom = self.effective_scroll(viewport) == 0;
        let preserve_scroll = self.content_dirty && !at_bottom;
        let old_total = if preserve_scroll {
            self.total_history_height()
        } else {
            0
        };

        self.rebuild_height_prefix(viewport);

        if preserve_scroll {
            let new_total = self.total_history_height();
            if new_total > old_total {
                self.history_scroll.scroll = self
                    .history_scroll
                    .scroll
                    .saturating_add(new_total - old_total);
            } else if old_total > new_total {
                // Mirror the anchor-preserving adjustment in
                // `compute_total_height_and_markers`: pull the scroll offset
                // up by the removed height rather than jumping to the bottom.
                self.history_scroll.scroll = self
                    .history_scroll
                    .scroll
                    .saturating_sub(old_total - new_total);
            }
        }

        self.streaming_dirty = false;
        self.content_dirty = false;
    }

    fn rebuild_markers(&mut self, viewport: &HistoryViewport) {
        self.markers.clear();
        let total = self.total_history_height().max(1);
        let virtual_track = self.virtual_track_slots(viewport);
        let mut accum = 0usize;
        for (i, &turn_id) in self.visible_turn_ids.iter().enumerate() {
            let turn_height = self.turn_heights[i];
            if let Some(turn) = self.view.turns.get(&turn_id)
                && turn.user_text.is_some()
            {
                let slot = accum * virtual_track / total;
                self.markers.push(Marker {
                    content_line: accum,
                    virtual_slot: slot,
                });
            }
            accum += turn_height;
        }
    }

    pub(crate) fn max_scroll_offset(&self, viewport: &HistoryViewport) -> usize {
        let viewport_height = viewport.height as usize;
        let total_height = self.total_history_height();
        total_height.saturating_sub(viewport_height)
    }

    pub(crate) fn virtual_track_slots(&self, viewport: &HistoryViewport) -> usize {
        2 * viewport.height as usize
    }

    pub(crate) fn clamp_scroll_state(&mut self, viewport: &HistoryViewport) {
        self.history_scroll.clamp(self.max_scroll_offset(viewport));
    }

    pub(crate) fn effective_scroll(&self, viewport: &HistoryViewport) -> usize {
        self.history_scroll
            .effective_scroll(self.max_scroll_offset(viewport))
    }

    pub(crate) fn image_block_height(&self, viewport: &HistoryViewport) -> u16 {
        (viewport.height / 2).max(1)
    }

    pub(crate) fn ensure_cache_synced(&mut self) {
        let turns_len = self.visible_turn_ids.len();
        let cache_len = self.render_cache.len();
        if cache_len == turns_len {
            return;
        }
        if cache_len > turns_len {
            self.render_cache.truncate(turns_len);
            return;
        }
        self.render_cache.resize(turns_len, None);
    }

    pub(crate) fn scrollbar_notch(&self, viewport: &HistoryViewport) -> usize {
        let max_scroll = self.max_scroll_offset(viewport);
        let virtual_track = self.virtual_track_slots(viewport);
        if virtual_track > 0 {
            ceil_div(max_scroll, virtual_track)
        } else {
            max_scroll
        }
        .max(1)
    }

    pub(crate) fn scroll_up(&mut self, amount: usize, viewport: &HistoryViewport) {
        self.history_scroll
            .scroll_up(amount, self.max_scroll_offset(viewport));
    }

    pub(crate) fn scroll_down(&mut self, amount: usize, viewport: &HistoryViewport) {
        self.history_scroll
            .scroll_down(amount, self.max_scroll_offset(viewport));
    }

    pub(crate) fn scroll_to(&mut self, row: usize, viewport: &HistoryViewport) {
        let max_scroll = self.max_scroll_offset(viewport);
        let amount = row.min(max_scroll);
        self.history_scroll.scroll = amount;
        self.history_scroll.scroll_compensation = 0;
    }

    pub(crate) fn scroll_to_track_row(
        &mut self,
        mouse_row: u16,
        track_height: u16,
        viewport: &HistoryViewport,
    ) {
        let track_height = track_height as usize;
        if track_height > 1 {
            let row = (mouse_row as usize).min(track_height.saturating_sub(1));
            let max_scroll = self.max_scroll_offset(viewport);
            let denom = track_height.saturating_sub(1);
            let target = row.saturating_mul(max_scroll).saturating_add(denom / 2) / denom;
            self.scroll_to(max_scroll.saturating_sub(target.min(max_scroll)), viewport);
        }
    }

    pub(crate) fn scroll_to_content_line(
        &mut self,
        content_line: usize,
        viewport: &HistoryViewport,
    ) {
        let total = self.total_history_height();
        let vh = viewport.height as usize;
        let target = total.saturating_sub(content_line + vh);
        self.scroll_to(target.min(self.max_scroll_offset(viewport)), viewport);
    }

    pub(crate) fn scrollbar_scroll_up(&mut self, viewport: &HistoryViewport) {
        let notch = self.scrollbar_notch(viewport);
        self.scroll_up(notch, viewport);
    }

    pub(crate) fn scrollbar_scroll_down(&mut self, viewport: &HistoryViewport) {
        let notch = self.scrollbar_notch(viewport);
        self.scroll_down(notch, viewport);
    }
}

/// Invalidate the render cache entry for `turn_id`.
fn invalidate_turn_cache(display: &mut SessionDisplayState, turn_id: u32) {
    if let Some(idx) = display
        .visible_turn_ids
        .iter()
        .position(|id| *id == turn_id)
        && let Some(slot) = display.render_cache.get_mut(idx)
    {
        *slot = None;
    }
}

/// Decide whether the locally-accumulated version of a turn should win over
/// the daemon snapshot's version when merging an attach snapshot.
///
/// The snapshot is authoritative for finished turns, but for the in-flight
/// turn it contains only the empty placeholder inserted by `start_turn` — the
/// accumulated version (fed by `OutputChunk`, `ToolCallStarted` and
/// `ToolResultChunk` via the all-activity subscription) holds the real
/// streaming content.  Keep the accumulated turn whenever it carries content
/// the snapshot version lacks; otherwise prefer the snapshot, which is the
/// daemon's canonical state.
fn turn_has_live_content(accumulated: &Turn, snapshot: &Turn) -> bool {
    (accumulated
        .assistant_text
        .as_deref()
        .is_some_and(|s| !s.is_empty())
        && snapshot.assistant_text.as_deref().is_none_or(str::is_empty))
        || (accumulated
            .assistant_reasoning
            .as_deref()
            .is_some_and(|s| !s.is_empty())
            && snapshot
                .assistant_reasoning
                .as_deref()
                .is_none_or(str::is_empty))
        || (!accumulated.tool_calls.is_empty() && snapshot.tool_calls.is_empty())
        || (!accumulated.tool_results.is_empty() && snapshot.tool_results.is_empty())
        || (!accumulated.displayed_images.is_empty() && snapshot.displayed_images.is_empty())
}

// ── TurnEventHandler implementation ──────────────────────────────────

impl TurnEventHandler for App {
    fn handle_turn_appended(&mut self, session_id: u64, turn_id: u32, turn: Turn) {
        tracing::trace!(%turn_id, "handle_turn_appended");
        self.sync_turn_images(session_id, turn_id, &turn);
        let display = self.display_for(session_id);
        invalidate_turn_cache(display, turn_id);
        display.view.insert_or_replace(turn_id, turn);
        // Replacement can change the rendered content even when the cache key's
        // other fields (widths, reasoning/collapse state) stay identical, so
        // bump the version to force a recompute on the next rebuild.
        display.bump_turn_version(turn_id);
        display.mark_content_changed();
    }

    fn handle_turns_undone(&mut self, session_id: u64, turn_ids: &[u32]) {
        tracing::trace!(?turn_ids, "handle_turns_undone");
        let display = self.display_for(session_id);
        for tid in turn_ids {
            invalidate_turn_cache(display, *tid);
            // Drop the content-version entry rather than bumping it: the
            // cache slot was invalidated above and undone turns are skipped
            // by rebuilds, so no cached rendering can survive for this turn;
            // `handle_turns_redone` re-invalidates the slot before
            // re-inserting, so a redone turn (even with byte-identical
            // content) always recomputes fresh.  Pruning keeps the version
            // map bounded by the live (non-undone) turn set instead of the
            // session's whole history.
            display.turn_versions.remove(tid);
            // Drop the user's reasoning-expansion preference for undone turns
            // so the map can't accumulate stale entries; a redo restores the
            // turn fresh with the derived default.
            display.reasoning_override.remove(tid);
            // Same for tool-result collapse preferences: a redo restores the
            // turn fresh, so stale (turn, call_id) overrides must not leak.
            display.tool_collapse_override.remove(tid);
            if let Some(turn) = display.view.turns.get_mut(tid) {
                turn.undone = true;
            }
        }
        display.mark_content_changed();
    }

    fn handle_turns_redone(
        &mut self,
        session_id: u64,
        turns: std::collections::BTreeMap<u32, Turn>,
    ) {
        tracing::trace!(?turns, "handle_turns_redone");
        // Sync images first, then get display to avoid borrow conflict.
        for (tid, turn) in &turns {
            self.sync_turn_images(session_id, *tid, turn);
        }
        let display = self.display_for(session_id);
        for (tid, turn) in turns {
            invalidate_turn_cache(display, tid);
            display.bump_turn_version(tid);
            display.view.insert_or_replace(tid, turn);
        }
        display.mark_content_changed();
    }

    fn handle_request_stream(
        &mut self,
        session_id: u64,
        request_id: u32,
        stream: OutputStream,
        data: Cow<'_, str>,
    ) {
        let display = self.display_for(session_id);
        // Detect the first Answer chunk for this request: the turn has no
        // response text yet, so this chunk begins the response phase.
        let turn_id = display.view.request_to_turn.get(&request_id).copied();
        let first_answer = matches!(stream, OutputStream::Answer)
            && turn_id
                .and_then(|id| display.view.turns.get(&id))
                .is_some_and(|t| t.assistant_text.is_none());

        display.view.stream_chunk(request_id, stream, &data);

        // The appended chunk changed the turn's rendered content: bump its
        // version so any rebuild (e.g. one triggered by an interleaved
        // `Done`/`TurnAppended` from another request or session) recomputes
        // this turn instead of serving the pre-chunk cached lines.
        if let Some(turn_id) = turn_id {
            display.bump_turn_version(turn_id);
        }

        // Auto-collapse reasoning when the response starts — drop any
        // explicit expansion override so the derived default (collapsed once
        // a response exists) takes over.  The user can re-expand it by
        // clicking the header.
        if first_answer && let Some(turn_id) = turn_id {
            display.reasoning_override.remove(&turn_id);
        }

        display.resolve_streaming_turn_index(request_id);
        display.mark_streaming_changed();
    }

    fn handle_started(
        &mut self,
        session_id: u64,
        request_id: u32,
        turn_id: u32,
        estimated_prompt_tokens: u32,
    ) {
        tracing::trace!(%request_id, %turn_id, %estimated_prompt_tokens, "handle_started");
        let display = self.display_for(session_id);
        display.view.request_to_turn.insert(request_id, turn_id);
        display.active.insert(request_id);
        display.live_input_estimate = estimated_prompt_tokens;
        display.live_output_tokens = 0;
        display.streaming_turn_index = display
            .visible_turn_ids
            .iter()
            .position(|id| *id == turn_id);
    }

    fn handle_done(
        &mut self,
        session_id: u64,
        request_id: u32,
        token_usage: Option<TokenUsage>,
        last_prompt_tokens: Option<u32>,
    ) {
        tracing::trace!(%request_id, "handle_done");
        // Done always arrives with `Some` (the session task knows its id), but
        // resolve defensively anyway so this choke point can never write to an
        // unintended display if a connection-level path is ever added.
        let Some(session_id) = self.resolve_daemon_session(Some(session_id)) else {
            return;
        };
        let display = self.display_for(session_id);
        // The final TurnAppended already cleaned description entries via
        // `insert_or_replace`, but if that broadcast was dropped under load
        // the map would keep them — clear for this turn so the map stays
        // bounded by in-flight calls even when the terminal broadcast is
        // lost.  (Looked up before `request_to_turn` is removed.)
        if let Some(&turn_id) = display.view.request_to_turn.get(&request_id) {
            display.view.clear_tool_call_descriptions(turn_id);
        }
        display.view.request_to_turn.remove(&request_id);
        display.active.remove(&request_id);
        if let Some(usage) = token_usage {
            display.token_usage = Some(usage);
            if last_prompt_tokens.is_none() {
                display.last_prompt_tokens = Some(usage.input_tokens);
            }
        }
        if let Some(tokens) = last_prompt_tokens {
            display.last_prompt_tokens = Some(tokens);
        }
        display.live_input_estimate = 0;
        display.live_output_tokens = 0;
        display.streaming_turn_index = None;
        display.mark_content_changed();
    }

    fn handle_failed(&mut self, session_id: Option<u64>, request_id: u32, error: String) {
        tracing::trace!(%request_id, %error, "handle_failed");
        // A connection-level failure (e.g. "no session attached" from
        // RunInput/SetModel/SetReasoningEffort) arrives with `session_id:
        // None` — no origin session — meaning "the attached session".  Resolve
        // it so the failure lands in the session the user is actually attached
        // to rather than a phantom display.
        let is_connection_level = session_id.is_none();
        let Some(session_id) = self.resolve_daemon_session(session_id) else {
            tracing::debug!(%request_id, %error, "dropping failure: no attached session to route the connection-level failure to");
            // No display to update, but a connection-level rejection (e.g.
            // "no session attached") is exactly what the user needs to see
            // on the status line.
            if is_connection_level {
                self.error = Some(error);
            }
            return;
        };
        // A connection-level failure has no turn to render an error block in,
        // so the global status/error bar is its only surface.  A request-level
        // failure (a real session id) already renders the full error as the
        // turn's red block in the transcript — writing it here too would
        // print the same message twice on screen — so it is only recorded on
        // the per-session display.  (Written before the mutable display
        // borrow below so `self.error` is still reachable.)
        if is_connection_level {
            self.error = Some(error.clone());
        }
        let display = self.display_for(session_id);
        // The per-session display records the failure for whichever session it
        // belongs to (rendered once the user views that session).
        display.error = Some(error);
        // A failed request never re-broadcasts its turn, so `insert_or_replace`
        // won't clean the description map — clear it here (before the
        // request→turn mapping is removed) to keep the map bounded by
        // in-flight calls even on the failure path.
        if let Some(&turn_id) = display.view.request_to_turn.get(&request_id) {
            display.view.clear_tool_call_descriptions(turn_id);
        }
        display.view.request_to_turn.remove(&request_id);
        display.active.remove(&request_id);
        display.streaming_turn_index = None;
        display.mark_content_changed();
    }

    fn handle_tool_call_event(&mut self, session_id: u64, request_id: u32, event: ToolCallEvent) {
        let display = self.display_for(session_id);
        match event {
            ToolCallEvent::Started {
                call_id,
                tool_name,
                arguments_json,
                invocation_description,
            } => {
                // Look up the turn before mutating so the version bump below
                // can target the right turn (the start event may backfill the
                // stub's name/description — both visible in the rendered
                // header).
                let turn_id = display.view.request_to_turn.get(&request_id).copied();
                display.view.tool_call_started(
                    request_id,
                    call_id,
                    tool_name,
                    arguments_json,
                    invocation_description,
                );
                if let Some(turn_id) = turn_id {
                    display.bump_turn_version(turn_id);
                }
                display.resolve_streaming_turn_index(request_id);
                display.mark_streaming_changed();
            }
            ToolCallEvent::Finished { .. } => {}
            ToolCallEvent::Failed { .. } => {}
        }
    }

    fn handle_tool_result_chunk(
        &mut self,
        session_id: u64,
        request_id: u32,
        call_id: String,
        data: Vec<u8>,
    ) {
        let text = String::from_utf8_lossy(&data).into_owned();
        let display = self.display_for(session_id);
        // The chunk appends to `turn.tool_results[i].content` (rendered
        // live); bump the turn's content version so a rebuild between chunks
        // recomputes instead of reusing the pre-chunk cached lines — the
        // core fix for "scrollbar moves but results stay stuck".
        let turn_id = display.view.request_to_turn.get(&request_id).copied();
        display.view.tool_result_chunk(request_id, &call_id, &text);
        if let Some(turn_id) = turn_id {
            display.bump_turn_version(turn_id);
        }
        display.resolve_streaming_turn_index(request_id);
        display.mark_streaming_changed();
    }

    fn handle_session_state(&mut self, state: SessionStateData) {
        tracing::debug!(
            turn_count = %state.turns.len(),
            ?state.selected_model,
            ?state.status,
            "handle_session_state"
        );
        let session_id = state.session_id;
        // SessionState snapshots are per-session: the daemon sends one for
        // the attached session on attach, but also broadcasts them for
        // background sessions (e.g. load_tools/unload_tools on that session
        // reach activity subscribers like the TUI).  Route the snapshot to
        // the session it belongs to, and only let the *attached* session's
        // snapshot drive the view switch and the status-bar fields below —
        // otherwise a background session's token usage / status / turns
        // would clobber the display the user is currently looking at.
        let is_attached = self.attached_session_id == Some(session_id);
        if is_attached {
            self.active_session_id = Some(session_id);
        }

        let SessionStateData {
            turns,
            title: _,
            selected_model,
            active_tool_groups,
            token_usage,
            context_window,
            last_prompt_tokens,
            status,
            reasoning_effort,
            reasoning_capability,
            ..
        } = state;

        // Merge the daemon snapshot with turns already accumulated locally
        // via the all-activity subscription (while the user was viewing
        // another session).  The snapshot is authoritative for finished
        // turns, but for an in-flight turn it only holds the empty
        // placeholder inserted by `start_turn` — the worker owns the live
        // content and only syncs back on RequestFinished.  The accumulated
        // turn carries the real streamed content, so it must win; otherwise
        // switching into a streaming session would blank the turn until the
        // next chunk arrived.
        let accumulated = {
            let display = self.display_for(session_id);
            std::mem::take(&mut display.view.turns)
        };
        let mut merged = turns;
        for (turn_id, acc_turn) in &accumulated {
            match merged.get_mut(turn_id) {
                Some(snap_turn) if turn_has_live_content(acc_turn, snap_turn) => {
                    *snap_turn = acc_turn.clone();
                }
                // Turn only known to the client (e.g. a turn created just
                // before this snapshot) — keep the accumulated version.
                None => {
                    merged.insert(*turn_id, acc_turn.clone());
                }
                // Snapshot is at least as complete — keep it.
                Some(_) => {}
            }
        }

        // Sync images before getting display to avoid borrow conflict.
        self.rendered_images.remove(&session_id);
        for (tid, turn) in &merged {
            self.sync_turn_images(session_id, *tid, turn);
        }
        let display = self.display_for(session_id);
        display.view.turns = merged;
        // Content versions must never outlive the turns they fingerprint:
        // drop entries whose turn left the view.  The snapshot merge is a
        // union today (accumulated turns are re-inserted below), so this is
        // defensive — it pins the invariant against any future path that
        // removes turns (undo keeps turns, only marking them undone).
        let live_turn_ids: Vec<u32> = display.view.turns.keys().copied().collect();
        display
            .turn_versions
            .retain(|turn_id, _| live_turn_ids.contains(turn_id));
        // The merge can silently replace a turn's content (the snapshot wins
        // when it is at least as complete, or the accumulated version wins
        // for the in-flight turn) — either way the cached rendering, built
        // from the pre-merge content, may now be stale.  Bump every turn the
        // client already knew about so the next rebuild recomputes rather
        // than reusing those lines.  Turns only present in the snapshot have
        // no cache entry, so they need no bump.
        for turn_id in accumulated.keys() {
            display.bump_turn_version(*turn_id);
        }
        display.selected_model = selected_model;
        // Merge, never overwrite: the attach snapshot can lag the fresher
        // total accumulated via the all-activity subscription for a mid-turn
        // session (see [`merge_token_usage`]), so a blind assignment would
        // regress the status bar's token readout until the next update.
        display.token_usage = merge_token_usage(&display.token_usage, &token_usage);
        if let Some(cw) = context_window {
            display.context_window = Some(cw);
        }
        // Gap-fill, never overwrite: the snapshot's last_prompt_tokens can
        // lag the value already broadcast to this client via the
        // all-activity subscription (the same cross-channel race as
        // token_usage), and unlike cumulative usage it is not monotonic, so
        // a max-merge is wrong.  Never regress a fresher value; the next
        // TokenUsageUpdate / Done refreshes it anyway.
        if display.last_prompt_tokens.is_none()
            && let Some(tokens) = last_prompt_tokens
        {
            display.last_prompt_tokens = Some(tokens);
        }
        if let Some(effort) = reasoning_effort {
            display.reasoning_effort = Some(effort);
        }
        if let Some(cap) = reasoning_capability {
            display.reasoning_capability = Some(cap);
        }
        display.mark_content_changed();
        let _ = display;
        // Only the attached session's snapshot may update the status bar's
        // per-attachment state — a background session's snapshot must not
        // overwrite the status/tool-group display while the user is viewing
        // the attached session.
        if is_attached {
            self.attached_status = Some(status);
            self.attached_tool_groups = active_tool_groups;
        }
    }

    fn handle_token_usage_update(
        &mut self,
        session_id: u64,
        token_usage: TokenUsage,
        last_prompt_tokens: Option<u32>,
    ) {
        tracing::trace!(
            ?token_usage,
            ?last_prompt_tokens,
            "handle_token_usage_update"
        );
        let display = self.display_for(session_id);
        display.token_usage = Some(token_usage);
        if let Some(tokens) = last_prompt_tokens {
            display.last_prompt_tokens = Some(tokens);
        }
        display.live_input_estimate = 0;
        display.live_output_tokens = 0;
    }

    fn handle_status_text(&mut self, text: String) {
        self.status = Some(text);
    }

    fn handle_error(&mut self, error: String) {
        self.error = Some(error);
    }

    fn handle_session_attached(&mut self, session_id: u64) {
        self.active_session_id = Some(session_id);
        self.attached_session_id = Some(session_id);
    }

    fn handle_session_created(
        &mut self,
        _session_id: u64,
        _title: Option<String>,
        _working_dir: Option<String>,
        _account_name: Option<String>,
        _selected_model: Option<String>,
        _reasoning_effort: Option<String>,
    ) {
    }

    fn handle_session_status_changed(
        &mut self,
        session_id: u64,
        status: SessionStatus,
        last_modified: i64,
    ) {
        self.handle_session_status_changed(session_id, &status, last_modified);
    }
}

#[cfg(test)]
pub(crate) fn history_text_height(text: &str, width: u16) -> usize {
    lines_height(
        &crate::markdown_render::plain_text_lines(text, width),
        width,
    )
}

/// Find the visible turn index and the content-line offset within that
/// turn for a given screen row.  Binary search on `height_prefix`.
/// Returns `(turn_idx, offset_within_turn)`.
pub(crate) fn find_turn_at_row(app: &App, screen_row: u16) -> Option<(usize, usize)> {
    let display = app.active_display_ref()?;
    let vh = app.history_viewport.height;
    if screen_row >= vh {
        return None;
    }

    let effective_scroll = display.effective_scroll(&app.history_viewport);
    let total_height = display.total_history_height();

    // Map the screen row to a content line, mirroring `render_history`'s
    // bottom-up draw order: the viewport shows the bottom `vh` rows of the
    // unscrolled content window, i.e. content lines `[total - scroll - vh,
    // total - scroll)`, so screen row `r` maps to content line
    // `r + total - scroll - vh`.  The same formula covers both layouts:
    //  - Tall history (scrollbar present): `scroll + vh <= total`, so the
    //    result is always `>= 0` and every viewport row shows content.
    //  - Short history (no scrollbar, `scroll == 0`): `total < vh` leaves a
    //    blank band of `vh - total` rows at the top — rows whose computed
    //    content line is negative.  Those rows must map to no turn rather
    //    than being clamped into the content (which is what both a naive
    //    `saturating_sub` and the pre-fix code did, breaking header/image
    //    click hit-testing on short sessions).
    let scrolled = effective_scroll.saturating_add(vh as usize);
    let content_line = (screen_row as usize).saturating_add(total_height);
    if content_line < scrolled {
        // Click landed in the blank band above the content.
        return None;
    }
    let content_line = content_line - scrolled;

    if content_line >= total_height {
        return None;
    }

    let i = display
        .height_prefix
        .partition_point(|&p| p <= content_line);
    if i < display.height_prefix.len() {
        let turn_start = i
            .checked_sub(1)
            .and_then(|prev| display.height_prefix.get(prev))
            .copied()
            .unwrap_or(0);
        let offset = content_line.saturating_sub(turn_start);
        Some((i, offset))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_app;

    fn make_session(id: u64, title: &str) -> SessionSummary {
        SessionSummary {
            session_id: id,
            title: Some(title.into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1000,
            last_modified: 1000,
            turn_count: 0,
            status: SessionStatus::Inactive,
            active_tool_groups: vec!["core".into()],
            account_name: None,
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
        }
    }

    fn make_detail_data(session_id: u64) -> SessionDetailData {
        SessionDetailData {
            session_id,
            title: String::new(),
            selected_model: String::new(),
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: String::new(),
            created_at: 0,
            last_modified: 0,
            turn_count: 0,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: None,
            accumulated_usage: None,
            context_window: None,
            last_prompt_tokens: None,
        }
    }

    // ── last_modified ordering ──

    #[test]
    fn input_buffer_drops_nul_and_keeps_newline() {
        // crossterm 0.29 parses kitty-protocol IME "text events"
        // (`CSI 0;;<codepoints>u`) as Char('\0') with the composed text
        // dropped.  The guard in handle_key must refuse to insert the NUL
        // while still accepting a literal newline (legacy Ctrl+J = 0x0A).
        let mut buf = InputBuffer::new();
        assert!(!buf.handle_key(KeyEvent::new(KeyCode::Char('\0'), KeyModifiers::NONE)));
        assert!(buf.handle_key(KeyEvent::new(KeyCode::Char('\n'), KeyModifiers::NONE)));
        assert_eq!(buf.text, "\n");
    }

    #[test]
    fn input_buffer_ctrl_backspace_clears_whole_buffer_from_any_cursor() {
        // Ctrl+Backspace empties the draft prompt outright, independent of
        // the cursor position — unlike Ctrl+U, which keeps the tail after
        // the cursor, and unlike plain Backspace, which deletes one grapheme.
        let mut buf = InputBuffer::new();
        buf.text = "hello world".to_string();
        // Cursor parked mid-text: clearing must not leave the trailing "world".
        buf.cursor = 6;
        buf.generation = 7;
        buf.scroll_offset = 3;

        let consumed = buf.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL));

        assert!(consumed, "ctrl+backspace must be consumed");
        assert!(buf.text.is_empty(), "the whole draft must be cleared");
        assert_eq!(buf.cursor, 0);
        assert_eq!(buf.scroll_offset, 0, "clear resets the visible window");
        assert_ne!(buf.generation, 7, "clear must invalidate the lines cache");
    }

    #[test]
    fn set_sessions_orders_by_last_modified_desc() {
        let mut mgr = SessionManagerState::new();
        let mut old = make_session(1, "old");
        old.last_modified = 1000;
        let mut newest = make_session(2, "newest");
        newest.last_modified = 9000;
        let mut middle = make_session(3, "middle");
        middle.last_modified = 5000;
        // Deliberately unsorted input: the list must come back newest-first.
        mgr.set_sessions(vec![old, middle, newest]);
        let titles: Vec<&str> = mgr
            .sessions
            .iter()
            .map(|s| s.title.as_deref().unwrap())
            .collect();
        assert_eq!(titles, vec!["newest", "middle", "old"]);
    }

    #[test]
    fn set_sessions_stable_for_equal_timestamps() {
        // Equal last_modified values must keep the incoming order (the daemon
        // already applies its id-desc tiebreak before sending).
        let mut mgr = SessionManagerState::new();
        mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
        let ids: Vec<u64> = mgr.sessions.iter().map(|s| s.session_id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn set_sessions_keeps_existing_selection_across_refresh() {
        // A plain refresh (no `select_session`) must keep the current
        // selection pinned to the same session even when it moves index.
        let mut mgr = SessionManagerState::new();
        mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
        mgr.select_down();
        assert_eq!(mgr.selection, Some(1));
        mgr.set_sessions(vec![make_session(2, "b"), make_session(1, "a")]);
        assert_eq!(mgr.selection, Some(0), "session 2 moved to index 0");
        assert_eq!(mgr.sessions[mgr.selection.unwrap()].session_id, 2);
    }

    #[test]
    fn select_session_highlights_existing_session_immediately() {
        let mut mgr = SessionManagerState::new();
        mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
        mgr.select_session(2);
        assert_eq!(mgr.selection, Some(1));
        assert_eq!(mgr.sessions[mgr.selection.unwrap()].session_id, 2);
        // The preference is remembered for the next refresh too.
        assert_eq!(mgr.pending_select, Some(2));
    }

    #[test]
    fn select_session_lands_on_session_once_list_arrives() {
        // First visit: the list hasn't been loaded yet, so the selection
        // stays unset until the ListSessions reply populates the list.
        let mut mgr = SessionManagerState::new();
        mgr.select_session(2);
        assert_eq!(mgr.selection, None);
        mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
        assert_eq!(mgr.selection, Some(1));
        assert_eq!(mgr.sessions[mgr.selection.unwrap()].session_id, 2);
        // The one-shot preference is consumed after the first refresh.
        assert_eq!(mgr.pending_select, None);
    }

    #[test]
    fn select_session_wins_over_previous_selection() {
        // The user viewed session 1, but the session they were just looking
        // at before Ctrl+S is session 2: the pending highlight must win.
        let mut mgr = SessionManagerState::new();
        mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
        assert_eq!(mgr.selection, Some(0)); // default: first row
        mgr.select_session(2);
        mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
        assert_eq!(mgr.selection, Some(1));
        assert_eq!(mgr.sessions[mgr.selection.unwrap()].session_id, 2);
    }

    #[test]
    fn pending_select_does_not_stick_across_later_refreshes() {
        // Once consumed, later refreshes must preserve the user's navigation
        // instead of re-applying an old highlight.
        let mut mgr = SessionManagerState::new();
        mgr.select_session(2);
        mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
        assert_eq!(mgr.selection, Some(1));
        mgr.select_up(); // user navigates back up to session 1
        mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
        assert_eq!(mgr.selection, Some(0));
    }

    #[test]
    fn select_session_falls_back_to_first_row_when_missing() {
        // The attached session is not in the (possibly stale) list: fall
        // back to the first row rather than leaving the selection unset.
        let mut mgr = SessionManagerState::new();
        mgr.select_session(99);
        mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
        assert_eq!(mgr.selection, Some(0));
    }

    #[test]
    fn status_change_reorders_only_when_timestamp_advances() {
        let mut app = test_app();
        app.session_mgr
            .set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
        // Both sessions share a timestamp, so the stable sort keeps input
        // order; move the cursor onto session 2 (index 1).
        app.session_mgr.select_down();
        assert_eq!(app.session_mgr.selection, Some(1));

        // A pure status transition (Inference at request start) carries the
        // session's *current* last_modified — the daemon no longer bumps the
        // timestamp for internal pipeline churn, so the list must NOT re-sort.
        let ts = app.session_mgr.sessions[1].last_modified;
        app.handle_session_status_changed(2, &SessionStatus::Inference, ts);
        assert_eq!(
            app.session_mgr.sessions[0].session_id, 1,
            "no reorder on status-only change"
        );
        assert_eq!(
            app.session_mgr.sessions[1].status,
            SessionStatus::Inference,
            "status still updates without reordering"
        );
        assert_eq!(app.session_mgr.selection, Some(1));

        // Only when the timestamp actually advances (a request completed) does
        // the session jump to the top, with the cursor following it.
        app.handle_session_status_changed(2, &SessionStatus::Inactive, ts + 1000);
        assert_eq!(
            app.session_mgr.sessions[0].session_id, 2,
            "completed session re-sorted to top"
        );
        assert_eq!(app.session_mgr.sessions[0].status, SessionStatus::Inactive);
        assert_eq!(
            app.session_mgr.selection,
            Some(0),
            "cursor follows the session"
        );
    }

    #[test]
    fn status_change_timestamp_is_monotonic() {
        // Duplicate/out-of-order deliveries must never regress last_modified.
        let mut app = test_app();
        app.session_mgr.set_sessions(vec![make_session(1, "a")]);
        app.handle_session_status_changed(1, &SessionStatus::Inference, 9000);
        app.handle_session_status_changed(1, &SessionStatus::Inference, 5000);
        assert_eq!(app.session_mgr.sessions[0].last_modified, 9000);
    }

    // ── remove_session ──

    #[test]
    fn remove_session_removes_from_list() {
        let mut mgr = SessionManagerState::new();
        mgr.sessions = vec![make_session(1, "a"), make_session(2, "b")];
        mgr.selection = Some(0);
        mgr.remove_session(1);
        assert_eq!(mgr.sessions.len(), 1);
        assert_eq!(mgr.sessions[0].session_id, 2);
    }

    #[test]
    fn remove_session_nonexistent_is_noop() {
        let mut mgr = SessionManagerState::new();
        mgr.sessions = vec![make_session(1, "a")];
        mgr.selection = Some(0);
        mgr.remove_session(999);
        assert_eq!(mgr.sessions.len(), 1);
        assert_eq!(mgr.selection, Some(0));
    }

    #[test]
    fn remove_session_last_item_clears_selection() {
        let mut mgr = SessionManagerState::new();
        mgr.sessions = vec![make_session(1, "a")];
        mgr.selection = Some(0);
        mgr.remove_session(1);
        assert!(mgr.sessions.is_empty());
        assert_eq!(mgr.selection, None);
    }

    #[test]
    fn remove_session_clamps_selection_to_new_len() {
        let mut mgr = SessionManagerState::new();
        mgr.sessions = vec![make_session(1, "a"), make_session(2, "b")];
        mgr.selection = Some(1);
        mgr.remove_session(2);
        assert_eq!(mgr.sessions.len(), 1);
        assert_eq!(mgr.selection, Some(0));
    }

    #[test]
    fn remove_session_clears_detail_view_for_deleted_session() {
        let mut mgr = SessionManagerState::new();
        mgr.sessions = vec![make_session(1, "a"), make_session(2, "b")];
        mgr.view = SessionManagerView::Detail;
        mgr.detail_data = Some(make_detail_data(1));
        mgr.remove_session(1);
        assert_eq!(mgr.view, SessionManagerView::List);
        assert!(mgr.detail_data.is_none());
    }

    #[test]
    fn remove_session_clears_confirmation_for_deleted_session() {
        let mut mgr = SessionManagerState::new();
        mgr.sessions = vec![make_session(1, "a")];
        mgr.confirm_delete = Some((1, "a".into()));
        mgr.remove_session(1);
        assert!(mgr.confirm_delete.is_none());
    }

    // ── window (selection-driven scroll) ──

    /// Assert the highlighted row always lies inside the returned window.
    fn assert_selection_in_window(mgr: &SessionManagerState, height: usize) {
        let (start, count) = mgr.window(height);
        if mgr.sessions.is_empty() {
            assert_eq!((start, count), (0, 0));
            return;
        }
        assert!(count > 0, "non-empty list must yield rows");
        assert_eq!(start + count, mgr.sessions.len().min(start + height));
        if let Some(sel) = mgr.selection {
            assert!(
                (start..start + count).contains(&sel),
                "selection {sel} outside window {start}..{}",
                start + count
            );
        }
    }

    #[test]
    fn window_empty_and_zero_height() {
        let mut mgr = SessionManagerState::new();
        assert_eq!(mgr.window(10), (0, 0), "empty list");
        mgr.set_sessions(vec![make_session(1, "a")]);
        assert_eq!(mgr.window(0), (0, 0), "zero height");
    }

    #[test]
    fn window_does_not_scroll_down_until_selection_reaches_bottom_edge() {
        let mut mgr = SessionManagerState::new();
        let sessions: Vec<_> = (1..=30).map(|id| make_session(id, "s")).collect();
        mgr.set_sessions(sessions);
        let height = 10;
        mgr.viewport_height = height;

        // The first `height - 1` presses move the selection through the
        // visible window without scrolling it: the window stays at 0 with
        // the selection pinned to the bottom edge.
        for _ in 0..height - 1 {
            mgr.select_down();
        }
        assert_eq!(mgr.selection, Some(9));
        assert_eq!(mgr.window(height), (0, 10), "window must not move yet");

        // One more press pushes the selection past the bottom edge, so the
        // window scrolls down by exactly one row to keep it visible.
        mgr.select_down();
        assert_eq!(mgr.selection, Some(10));
        assert_eq!(mgr.window(height), (1, 10));
        assert_selection_in_window(&mgr, height);
    }

    #[test]
    fn window_does_not_scroll_up_immediately_after_scrolling_down() {
        // Regression: after scrolling to the bottom, pressing up must move
        // the highlight through the visible rows — the window may only
        // scroll back up once the selection reaches the top edge.
        let mut mgr = SessionManagerState::new();
        let sessions: Vec<_> = (1..=30).map(|id| make_session(id, "s")).collect();
        mgr.set_sessions(sessions);
        let height = 10;
        mgr.viewport_height = height;

        // Scroll to the bottom: selection 29, window rows 20..29.
        for _ in 0..29 {
            mgr.select_down();
        }
        assert_eq!(mgr.selection, Some(29));
        assert_eq!(mgr.window(height), (20, 10));

        // The first nine presses up climb the selection from 29 to 20 (the
        // top edge of the window) without moving the window.
        for _ in 0..9 {
            mgr.select_up();
        }
        assert_eq!(mgr.selection, Some(20));
        assert_eq!(
            mgr.window(height),
            (20, 10),
            "window must stay fixed while the selection climbs"
        );
        assert_selection_in_window(&mgr, height);

        // One more press up leaves the top edge, so the window scrolls up.
        mgr.select_up();
        assert_eq!(mgr.selection, Some(19));
        assert_eq!(mgr.window(height), (19, 10));
        assert_selection_in_window(&mgr, height);
    }

    #[test]
    fn window_scrolls_down_again_from_top_of_scrolled_window() {
        // After scrolling up so the selection sits at the top edge of the
        // window, pressing down must move the highlight down within the
        // window — not scroll it back down immediately.
        let mut mgr = SessionManagerState::new();
        let sessions: Vec<_> = (1..=30).map(|id| make_session(id, "s")).collect();
        mgr.set_sessions(sessions);
        let height = 10;
        mgr.viewport_height = height;

        // Scroll to the bottom then back up one: selection 28, window
        // rows 20..29.
        for _ in 0..29 {
            mgr.select_down();
        }
        mgr.select_up();
        assert_eq!(mgr.selection, Some(28));
        assert_eq!(mgr.window(height), (20, 10));

        // Pressing down moves the selection within the window without
        // scrolling it.
        mgr.select_down();
        assert_eq!(mgr.selection, Some(29));
        assert_eq!(mgr.window(height), (20, 10));
    }

    #[test]
    fn window_reanchors_after_stale_scroll() {
        // A reorder/removal can leave `scroll` pointing below the new
        // selection.  The window must clamp back up so the selection stays
        // visible, and the next navigation step re-anchors from that.
        let mut mgr = SessionManagerState::new();
        mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
        mgr.viewport_height = 2;
        mgr.selection = Some(1);
        mgr.scroll = 5; // stale anchor below the selection
        assert_eq!(mgr.window(2), (0, 2), "window clamps up to keep selection");

        // Navigation re-anchors before shifting, so the next move up works
        // from the displayed window instead of the stale anchor.
        mgr.select_up();
        assert_eq!(mgr.selection, Some(0));
        assert_eq!(mgr.scroll, 0);
        assert_eq!(mgr.window(2), (0, 2));
    }

    #[test]
    fn window_follows_selection_on_reorder_and_removal() {
        // After a removal the selection clamps and the window re-derives
        // from the (new) selection instead of showing stale rows.
        let mut mgr = SessionManagerState::new();
        mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
        mgr.viewport_height = 1;
        mgr.select_down();
        mgr.remove_session(1);
        assert_selection_in_window(&mgr, 1);
        assert_eq!(mgr.selection, Some(0));
        assert_eq!(mgr.window(1), (0, 1));
    }

    // ── paging the selection ──

    #[test]
    fn page_up_down_moves_selection_and_keeps_it_in_window() {
        let mut mgr = SessionManagerState::new();
        let sessions: Vec<_> = (1..=20).map(|id| make_session(id, "s")).collect();
        mgr.set_sessions(sessions);
        mgr.viewport_height = 10;

        mgr.scroll_down_page();
        assert_eq!(mgr.selection, Some(PAGE_SCROLL_LINES));
        assert_selection_in_window(&mgr, 10);

        mgr.scroll_down_page();
        assert_eq!(mgr.selection, Some(PAGE_SCROLL_LINES * 2));
        assert_selection_in_window(&mgr, 10);

        // Paging past the end clamps to the last row.
        for _ in 0..10 {
            mgr.scroll_down_page();
        }
        assert_eq!(mgr.selection, Some(19));
        assert_selection_in_window(&mgr, 10);

        mgr.scroll_up_page();
        assert_eq!(mgr.selection, Some(16));
        assert_selection_in_window(&mgr, 10);

        // Paging up past the top clamps to row 0.
        for _ in 0..10 {
            mgr.scroll_up_page();
        }
        assert_eq!(mgr.selection, Some(0));
        assert_selection_in_window(&mgr, 10);
    }

    #[test]
    fn paging_with_no_selection_is_a_noop() {
        let mut mgr = SessionManagerState::new();
        mgr.scroll_up_page();
        mgr.scroll_down_page();
        assert_eq!(mgr.selection, None);
    }

    // ── scroll_to_content_line ──

    #[test]
    fn scroll_to_content_line_scrolls_to_content_line() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        for i in 0..5u32 {
            let turn = Turn {
                created_at: choreo_proto::TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some(format!("user text {i}")),
                assistant_text: Some(format!("assistant text {i}")),
                assistant_reasoning: None,
                tool_calls: vec![],
                token_usage: None,
                tool_results: vec![],
                displayed_images: vec![],
                reasoning_artifact: None,
                reasoning_producer: None,
            };
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(i, turn);
        }
        app.rebuild_height_prefix();

        app.scroll_to_content_line(0);
        assert_eq!(app.effective_scroll(), app.max_scroll_offset());
    }

    // ── find_turn_at_row ──

    #[test]
    fn find_turn_at_row_returns_none_out_of_bounds() {
        let app = test_app();
        assert!(find_turn_at_row(&app, 999).is_none());
    }

    #[test]
    fn find_turn_at_row_returns_turn_idx_and_offset() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello".into()),
            assistant_text: Some("world".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        app.rebuild_height_prefix();

        // The history is shorter than the viewport, so content is anchored to
        // the bottom: content line 0 sits at screen row `vh - total`.
        let total = app.active_display().unwrap().total_history_height();
        let first_row = (app.history_viewport.height as usize - total) as u16;
        let (turn_idx, offset) = find_turn_at_row(&app, first_row).unwrap();
        assert_eq!(turn_idx, 0);
        assert_eq!(offset, 0);

        // Rows above the content are blank and must not map to a turn.
        assert!(find_turn_at_row(&app, first_row.saturating_sub(1)).is_none());
    }

    #[test]
    fn find_turn_at_row_scrolled_history_maps_rows_correctly() {
        // Tall session with a scrollbar: scroll away from the bottom and
        // verify the mapping agrees with `render_history`'s bottom-up draw
        // order (content line `c` sits at screen row `vh - total + scroll + c`).
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        for i in 0..8 {
            let turn = Turn {
                created_at: choreo_proto::TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some(format!("user {i}")),
                assistant_text: Some(format!("assistant {i}")),
                assistant_reasoning: None,
                tool_calls: vec![],
                token_usage: None,
                tool_results: vec![],
                displayed_images: vec![],
                reasoning_artifact: None,
                reasoning_producer: None,
            };
            app.active_display()
                .unwrap()
                .view
                .insert_or_replace(i, turn);
        }
        app.rebuild_height_prefix();

        let total = app.active_display().unwrap().total_history_height();
        let vh = app.history_viewport.height as usize;
        assert!(
            total > vh,
            "test requires content taller than the viewport (scrollbar present)"
        );

        // Scroll partway up: max_scroll = total - vh.
        let scroll = (total - vh) / 2;
        app.scroll_to(scroll);
        assert_eq!(app.effective_scroll(), scroll);

        // The topmost visible content line is `total - scroll - vh`; the
        // bottom row of the viewport shows content line `total - scroll - 1`.
        let top_line = total - scroll - vh;
        let (idx, offset) = find_turn_at_row(&app, 0).expect("top row must map to a turn");
        assert_eq!(offset, top_line - turn_start(&app, idx));

        let bottom_row = (vh - 1) as u16;
        let (idx_b, offset_b) = find_turn_at_row(&app, bottom_row).expect("bottom row must map");
        assert_eq!(
            offset_b,
            total - scroll - 1 - turn_start(&app, idx_b),
            "bottom row must map to the last visible content line"
        );
    }

    /// Content line where the turn at `turn_idx` starts (height_prefix
    /// prefix-sum entry, 0 for the first turn).
    fn turn_start(app: &App, turn_idx: usize) -> usize {
        app.active_display_ref()
            .and_then(|d| {
                turn_idx
                    .checked_sub(1)
                    .and_then(|prev| d.height_prefix.get(prev))
            })
            .copied()
            .unwrap_or(0)
    }

    #[test]
    fn find_turn_at_row_short_history_anchors_content_at_bottom() {
        // Regression: when the history is shorter than the viewport (no
        // scrollbar shown), the renderer anchors the content at the bottom of
        // the viewport, but the click mapping assumed content always starts at
        // screen row 0.  The reasoning header (and image clicks) therefore
        // couldn't be hit on short sessions.
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("Response text.".into()),
            assistant_reasoning: Some("Hidden thinking.".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(1, turn);
        app.rebuild_height_prefix();

        let (start, total) = {
            let display = app.active_display().unwrap();
            let (start, _end) = display.turn_layouts[0]
                .reasoning_header_range
                .expect("reasoning header range should exist");
            (start, display.total_history_height())
        };
        assert!(
            total < app.history_viewport.height as usize,
            "test requires a session too short to need the scrollbar"
        );

        // The header is drawn at screen row `vh - total + start` (bottom
        // anchored); clicking that row must resolve to the header's content
        // line `start`.
        let screen_row = (app.history_viewport.height as usize - total + start) as u16;
        let (turn_idx, offset) =
            find_turn_at_row(&app, screen_row).expect("row must map to a turn");
        assert_eq!(turn_idx, 0);
        assert_eq!(offset, start);

        // The blank band above the content must not map to any turn.
        let blank_row = (app.history_viewport.height as usize - total - 1) as u16;
        assert!(
            find_turn_at_row(&app, blank_row).is_none(),
            "empty rows above the content must not hit a turn"
        );
    }

    // ── text selection lifecycle ──

    #[test]
    fn reset_for_session_switch_clears_text_selection() {
        // A selection is stored in screen coordinates keyed to the previous
        // session's rendered content; switching sessions must clear it so a
        // stale rectangle can never highlight another session's history.
        let mut app = test_app();
        app.text_selection = Some(crate::selection::TextSelection {
            anchor: (0, 0),
            head: (2, 3),
            cursor: (0, 0),
            active: true,
            head_sync: None,
        });
        app.reset_for_session_switch(1);
        assert!(
            app.text_selection.is_none(),
            "session switch must clear the in-progress selection"
        );
    }

    #[test]
    fn set_page_clears_text_selection() {
        // Leaving the Chat page invalidates the selection's screen-coordinate
        // context (it is keyed to the history it was drawn over); a stale
        // gesture must not survive a page switch and swallow the first click
        // on return.
        let mut app = test_app();
        app.text_selection = Some(crate::selection::TextSelection {
            anchor: (0, 0),
            head: (2, 3),
            cursor: (0, 0),
            active: true,
            head_sync: None,
        });
        app.set_page(Page::SessionManager);
        assert!(
            app.text_selection.is_none(),
            "page switch must clear the in-progress selection"
        );
    }

    #[test]
    fn terminal_resize_clears_text_selection() {
        // A resize re-wraps every rendered line, so a stored (content line,
        // viewport column) anchor would point at different text afterwards.
        // The gesture must be dropped exactly like a suspend/page switch
        // drops it (the anchor is deliberately never re-resolved).
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 30;
        app.text_selection = Some(crate::selection::TextSelection {
            anchor: (0, 0),
            head: (2, 3),
            cursor: (0, 0),
            active: true,
            head_sync: None,
        });
        // A different cached terminal size drives a viewport change without
        // touching a real terminal (crossterm::size() is only queried when
        // `terminal_resized` is set).
        app.last_terminal_size = Some((60, 20));
        app.terminal_resized = false;
        app.update_viewport_from_terminal_size();
        assert!(
            app.text_selection.is_none(),
            "terminal resize must clear the in-progress selection"
        );
    }

    // ── scrollbar_notch ──

    #[test]
    fn scrollbar_notch_no_content() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        assert_eq!(app.scrollbar_notch(), 1);
    }

    #[test]
    fn scrollbar_notch_track_one() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 1;
        let display = app.active_display().unwrap();
        display.height_prefix.push(50);
        // max_scroll = 50 - 1 = 49, virtual_track = 2, notch = ceil(49 / 2) = 25
        assert_eq!(app.scrollbar_notch(), 25);
    }

    #[test]
    fn scrollbar_notch_ceiling_division() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 50;
        let display = app.active_display().unwrap();
        display.height_prefix.push(150);
        // max_scroll = 150 - 50 = 100, virtual_track = 100, notch = ceil(100 / 100) = 1
        assert_eq!(app.scrollbar_notch(), 1);
    }

    #[test]
    fn scrollbar_notch_rounds_up() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 30;
        let display = app.active_display().unwrap();
        display.height_prefix.push(105);
        // max_scroll = 105 - 30 = 75, virtual_track = 60, notch = ceil(75 / 60) = 2
        assert_eq!(app.scrollbar_notch(), 2);
    }

    // ── scrollbar_scroll_up / scrollbar_scroll_down ──

    #[test]
    fn scrollbar_scroll_up_increases_scroll_by_notch() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 100, virtual_track = 20, notch = 5
        display.history_scroll.scroll = 0;
        let before = app.effective_scroll();

        app.scrollbar_scroll_up();

        assert_eq!(app.effective_scroll(), before + 5);
    }

    #[test]
    fn scrollbar_scroll_up_clamps_at_max_scroll() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 100, virtual_track = 20, notch = 5
        display.history_scroll.scroll = 100;

        app.scrollbar_scroll_up();

        assert_eq!(app.effective_scroll(), 100);
    }

    #[test]
    fn scrollbar_scroll_down_decreases_scroll_by_notch() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 100, virtual_track = 20, notch = 5
        display.history_scroll.scroll = 100;
        let before = app.effective_scroll();

        app.scrollbar_scroll_down();

        assert_eq!(app.effective_scroll(), before - 5);
    }

    #[test]
    fn scrollbar_scroll_down_clamps_at_zero() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 100, virtual_track = 20, notch = 5
        display.history_scroll.scroll = 5;

        app.scrollbar_scroll_down();

        assert_eq!(app.effective_scroll(), 0);
    }

    // ── scroll_to_track_row ──

    #[test]
    fn scroll_to_track_row_at_bottom() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 90, denom = 19
        display.history_scroll.scroll = 90;

        app.scroll_to_track_row(0, 20);

        assert_eq!(app.effective_scroll(), 90);
    }

    #[test]
    fn scroll_to_track_row_at_top() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 90, denom = 19
        display.history_scroll.scroll = 0;

        app.scroll_to_track_row(19, 20);

        assert_eq!(app.effective_scroll(), 0);
    }

    #[test]
    fn scroll_to_track_row_midpoint() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 100, denom = 9

        app.scroll_to_track_row(4, 10);

        assert_eq!(app.effective_scroll(), 56);
    }

    #[test]
    fn scroll_to_track_row_zero_viewport() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 0;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        display.history_scroll.scroll = 42;

        app.scroll_to_track_row(0, 0);

        assert_eq!(app.effective_scroll(), 42);
    }

    #[test]
    fn scroll_to_track_row_track_one() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        display.history_scroll.scroll = 42;

        app.scroll_to_track_row(0, 1);

        assert_eq!(app.effective_scroll(), 42);
    }

    #[test]
    fn scroll_to_track_row_mouse_row_clamped() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 90, denom = 19
        display.history_scroll.scroll = 0;

        app.scroll_to_track_row(30, 20);

        assert_eq!(app.effective_scroll(), 0);
    }

    // ── scroll_to_content_line ──

    #[test]
    fn scroll_to_content_line_idempotent_when_already_visible() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello".into()),
            assistant_text: Some("world".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(0, turn);
        app.rebuild_height_prefix();

        let before = app.effective_scroll();
        app.scroll_to_content_line(0);
        assert_eq!(app.effective_scroll(), before);
    }

    #[test]
    fn scroll_to_content_line_large_content_line_saturates() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        for i in 0..5u32 {
            let turn = Turn {
                created_at: choreo_proto::TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some(format!("user text {i}")),
                assistant_text: Some(format!("assistant text {i}")),
                assistant_reasoning: None,
                tool_calls: vec![],
                token_usage: None,
                tool_results: vec![],
                displayed_images: vec![],
                reasoning_artifact: None,
                reasoning_producer: None,
            };
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(i, turn);
        }
        app.rebuild_height_prefix();

        app.scroll_to_content_line(9999);
        assert_eq!(app.effective_scroll(), 0);
    }

    // ── status_error_height ──

    #[test]
    fn status_error_height_neither_set_returns_zero() {
        let app = test_app();
        assert_eq!(app.status_error_height(80), 0);
    }

    #[test]
    fn status_error_height_short_error_returns_one() {
        let mut app = test_app();
        app.error = Some("oops".into());
        assert_eq!(app.status_error_height(80), 1);
    }

    #[test]
    fn status_error_height_short_status_returns_one() {
        let mut app = test_app();
        app.status = Some("all good".into());
        assert_eq!(app.status_error_height(80), 1);
    }

    #[test]
    fn status_error_height_error_preferred_over_status() {
        let mut app = test_app();
        app.error = Some("error".into());
        app.status = Some("status".into());
        // Should use error text, not status text
        assert_eq!(app.status_error_height(80), 1);
    }

    #[test]
    fn status_error_height_wrapping() {
        let mut app = test_app();
        // The status Paragraph wraps at width-2 (the inset `notify_area`), so
        // at width 5 the inner width is 3: "12345 7890" hard-splits to
        // ["123", "45 ", "789", "0"] → 4 rows (matches what ratatui draws).
        app.error = Some("12345 7890".into());
        assert_eq!(app.status_error_height(5), 4);
    }

    #[test]
    fn status_error_height_multi_line() {
        let mut app = test_app();
        // Three explicit lines via \n
        app.status = Some("line a\nline b\nline c".into());
        // Each line fits in width 80, so total = 3
        assert_eq!(app.status_error_height(80), 3);
    }

    #[test]
    fn status_error_height_multi_line_with_wrapping() {
        let mut app = test_app();
        // Two lines; at width 5 the inner wrap width is 3: "hello" hard-splits
        // to ["hel", "lo"] (2 rows) and "12345 7890" to 4 rows — 6 total,
        // matching the rows the inset status Paragraph actually draws.
        app.error = Some("hello\n12345 7890".into());
        assert_eq!(app.status_error_height(5), 6);
    }

    #[test]
    fn status_error_height_empty_after_clearing() {
        let mut app = test_app();
        app.error = Some("error".into());
        app.error = None;
        assert_eq!(app.status_error_height(80), 0);
    }

    #[test]
    fn status_error_height_status_takes_over_when_error_cleared() {
        let mut app = test_app();
        app.status = Some("status".into());
        assert_eq!(app.status_error_height(80), 1);
    }

    #[test]
    fn sync_turn_images_populates_rendered_images() {
        let mut app = test_app();
        let metadata = choreo_proto::ImageMetadata {
            mime_type: "image/svg+xml".to_string(),
            width: 100,
            height: 200,
            byte_len: 50,
            alt: None,
        };
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![
                choreo_proto::DisplayedImageRecord {
                    metadata: metadata.clone(),
                    data: b"svg-data".to_vec(),
                    tool_call_id: Some("call-1".into()),
                },
                choreo_proto::DisplayedImageRecord {
                    metadata: metadata.clone(),
                    data: b"more-svg".to_vec(),
                    tool_call_id: None,
                },
            ],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.sync_turn_images(0, 42, &turn);

        let images = app.rendered_images.get(&0).unwrap().get(&42).unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[&0].data.as_ref(), b"svg-data");
        assert_eq!(images[&1].data.as_ref(), b"more-svg");
        // Second call is idempotent — preserves existing entries
        app.sync_turn_images(0, 42, &turn);
        assert_eq!(
            app.rendered_images.get(&0).unwrap().get(&42).unwrap().len(),
            2
        );
    }

    // ── TurnImageLayout image_ranges ──

    #[test]
    fn turn_layout_empty_when_no_images() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello".into()),
            assistant_text: Some("world".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(1, turn);
        app.rebuild_height_prefix();

        assert_eq!(app.active_display().unwrap().turn_layouts.len(), 1);
        assert!(
            app.active_display().unwrap().turn_layouts[0]
                .image_ranges
                .is_empty()
        );
    }

    #[test]
    fn turn_layout_populates_image_ranges_with_fallback_height() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let metadata = choreo_proto::ImageMetadata {
            mime_type: "image/png".to_string(),
            width: 100,
            height: 100,
            byte_len: 500,
            alt: None,
        };
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("short".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![
                choreo_proto::DisplayedImageRecord {
                    metadata: metadata.clone(),
                    data: vec![0u8; 10],
                    tool_call_id: None,
                },
                choreo_proto::DisplayedImageRecord {
                    metadata: metadata.clone(),
                    data: vec![1u8; 10],
                    tool_call_id: None,
                },
            ],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        let turn_clone = turn.clone();
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(2, turn);
        app.sync_turn_images(0, 2, &turn_clone);
        app.rebuild_height_prefix();

        assert_eq!(app.active_display().unwrap().turn_layouts.len(), 1);
        // Mutable borrow dropped.

        // Capture needed values before taking another mutable borrow for layout.
        let fallback_h = app.image_block_height() as usize;
        let vp_width = app.history_viewport.width;
        let text_h = {
            let display = app.active_display().unwrap();
            let turn = &display.view.turns[&2];
            lines_height(
                &render_turn_lines(turn, 71, vp_width, false, &[]).lines,
                vp_width,
            )
            .max(1)
        };

        let layout = &app.active_display().unwrap().turn_layouts[0];
        assert_eq!(layout.image_ranges.len(), 2);

        let (s0, e0) = layout.image_ranges[0];
        assert_eq!(s0, text_h);
        assert_eq!(e0, text_h + fallback_h);

        let (s1, e1) = layout.image_ranges[1];
        assert_eq!(s1, text_h + fallback_h);
        assert_eq!(e1, text_h + 2 * fallback_h);
    }

    // ── TurnLayout reasoning_header_range ──

    #[test]
    fn turn_layout_reasoning_header_range_present() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello".into()),
            assistant_text: Some("world".into()),
            assistant_reasoning: Some("think".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(1, turn);
        app.rebuild_height_prefix();

        let layout = &app.active_display().unwrap().turn_layouts[0];
        let Some((start, end)) = layout.reasoning_header_range else {
            panic!("reasoning header range should be present");
        };
        assert!(
            start < end,
            "header range must be non-empty ({start}..{end})"
        );
        // No images on this turn, so the full turn height is its text block;
        // the header must lie inside it.
        let turn_h = app.active_display().unwrap().turn_heights[0];
        assert!(end <= turn_h, "header must lie within the turn text");
    }

    #[test]
    fn turn_layout_reasoning_default_expanded_reflects_turn_content() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;

        // Response present → default collapsed.
        let responded = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("world".into()),
            assistant_reasoning: Some("think".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(1, responded);

        // Streaming (no response yet) → default expanded.
        let streaming = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: Some("thinking".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(2, streaming);

        app.rebuild_height_prefix();

        let display = app.active_display().unwrap();
        assert!(
            !display.turn_layouts[0].reasoning_default_expanded,
            "response present → collapsed default"
        );
        assert!(
            display.turn_layouts[1].reasoning_default_expanded,
            "no response yet → expanded default"
        );
    }

    #[test]
    fn turn_layout_reasoning_header_range_none_without_reasoning() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello".into()),
            assistant_text: Some("world".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(1, turn);
        app.rebuild_height_prefix();

        let layout = &app.active_display().unwrap().turn_layouts[0];
        assert!(
            layout.reasoning_header_range.is_none(),
            "no reasoning → no header range"
        );
    }

    // ── toggle_reasoning ──

    #[test]
    fn toggle_reasoning_flips_override_and_invalidates_cache() {
        let mut app = test_app();
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("response".into()),
            assistant_reasoning: Some("thinking".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.visible_turn_ids.push(1);
        display.render_cache = vec![Some(RenderedCache {
            key: RenderCacheKey {
                turn_id: 1,
                width: 71,
                viewport_width: 80,
                reasoning_expanded: false, // response present → collapsed default
                tool_results_collapsed: vec![],
                content_version: 0,
            },
            rendered: RenderedTurn {
                lines: Arc::from(vec![Line::from("stale")]),
                height: 1,
                visual_offsets: Arc::from([1]),
                joins: Arc::from([LineJoin::Break]),
                content_ranges: Arc::from([Some((0, 5))]),
                reasoning_header_idx: None,
                tool_result_header_idxs: vec![],
            },
        })];

        // Default is collapsed (response present) → first click expands.
        display.toggle_reasoning(1);
        assert_eq!(
            display.reasoning_override.get(&1),
            Some(&true),
            "first click should expand"
        );
        assert!(
            display.render_cache[0].is_none(),
            "toggle must invalidate the render cache"
        );

        // Second click collapses again.
        display.toggle_reasoning(1);
        assert_eq!(
            display.reasoning_override.get(&1),
            Some(&false),
            "second click should collapse"
        );
    }

    #[test]
    fn toggle_reasoning_missing_turn_is_noop() {
        let mut app = test_app();
        let display = app.active_display().unwrap();
        display.toggle_reasoning(999);
        assert!(
            display.reasoning_override.is_empty(),
            "unknown turn should not record an override"
        );
    }

    #[test]
    fn toggle_reasoning_default_expanded_without_response() {
        let mut app = test_app();
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: Some("thinking".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        // No response yet → default expanded → first click collapses.
        display.toggle_reasoning(1);
        assert_eq!(
            display.reasoning_override.get(&1),
            Some(&false),
            "first click on streaming reasoning should collapse"
        );
    }

    // ── effective_reasoning_expanded ──

    #[test]
    fn effective_reasoning_expanded_prefers_override() {
        let mut app = test_app();
        let display = app.active_display().unwrap();
        // No override → the derived default wins.
        assert!(!display.effective_reasoning_expanded(1, false));
        assert!(display.effective_reasoning_expanded(1, true));
        // An explicit override wins over the derived default.
        display.reasoning_override.insert(1, true);
        assert!(
            display.effective_reasoning_expanded(1, false),
            "override should beat a collapsed default"
        );
        display.reasoning_override.insert(1, false);
        assert!(
            !display.effective_reasoning_expanded(1, true),
            "override should beat an expanded default"
        );
    }

    // ── reasoning_override pruning on undo ──

    #[test]
    fn turns_undone_prunes_reasoning_override() {
        let mut app = test_app();
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("response".into()),
            assistant_reasoning: Some("thinking".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        {
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(1, turn);
            // Simulate the user having expanded the reasoning section.
            display.reasoning_override.insert(1, true);
        }

        app.handle_turns_undone(0, &[1]);

        let display = app.active_display_ref().unwrap();
        assert!(
            !display.reasoning_override.contains_key(&1),
            "undo should prune the reasoning override"
        );
        assert!(
            display.view.turns[&1].undone,
            "the turn should be marked undone"
        );
    }

    #[test]
    fn turns_undone_prunes_content_version() {
        // The content-version map must stay bounded by the live (non-undone)
        // turn set, mirroring the reasoning/collapse override pruning: a
        // redone turn re-invalidates its cache slot, so dropping the version
        // here can never serve a stale rendering.
        let mut app = test_app();
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("response".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        {
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(1, turn);
            // A chunk-like mutation records a version for the turn.
            display.bump_turn_version(1);
            assert_eq!(display.turn_content_version(1), 1);
        }

        app.handle_turns_undone(0, &[1]);

        let display = app.active_display_ref().unwrap();
        assert!(
            !display.turn_versions.contains_key(&1),
            "undo should prune the turn's content version"
        );
        assert_eq!(
            display.turn_content_version(1),
            0,
            "an undone turn reports version 0 (no recorded mutations)"
        );
    }

    // ── tool result collapse ──

    #[test]
    fn effective_tool_result_collapsed_prefers_override() {
        let mut app = test_app();
        let display = app.active_display().unwrap();
        let quiet = ToolResultRecord {
            call_id: "c".into(),
            name: "read_file".into(),
            content: "x".into(),
            is_error: false,
            invocation_description: String::new(),
            image: None,
        };
        let loud = ToolResultRecord {
            call_id: "c2".into(),
            name: "sh".into(),
            content: "y".into(),
            is_error: false,
            invocation_description: String::new(),
            image: None,
        };
        // No override → the derived default wins (quiet collapsed, others
        // expanded).
        assert!(display.effective_tool_result_collapsed(1, &quiet));
        assert!(!display.effective_tool_result_collapsed(1, &loud));
        // An explicit override wins over the derived default.
        display
            .tool_collapse_override
            .entry(1)
            .or_default()
            .insert("c".into(), false);
        assert!(!display.effective_tool_result_collapsed(1, &quiet));
        display
            .tool_collapse_override
            .entry(1)
            .or_default()
            .insert("c2".into(), true);
        assert!(display.effective_tool_result_collapsed(1, &loud));
    }

    #[test]
    fn toggle_tool_result_flips_override_and_invalidates_cache() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![ToolResultRecord {
                call_id: "call-1".into(),
                name: "read_file".into(),
                content: "file contents".into(),
                is_error: false,
                invocation_description: "Reading file `src/main.rs`.".into(),
                image: None,
            }],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.visible_turn_ids.push(1);
        display.render_cache = vec![Some(RenderedCache {
            key: RenderCacheKey {
                turn_id: 1,
                width: 71,
                viewport_width: 80,
                reasoning_expanded: false,
                tool_results_collapsed: vec![true], // quiet default → collapsed
                content_version: 0,
            },
            rendered: RenderedTurn {
                lines: Arc::from(vec![Line::from("stale")]),
                height: 1,
                visual_offsets: Arc::from([1]),
                joins: Arc::from([LineJoin::Break]),
                content_ranges: Arc::from([Some((0, 5))]),
                reasoning_header_idx: None,
                tool_result_header_idxs: vec![0],
            },
        })];

        // Quiet default is collapsed → the first click expands.
        display.toggle_tool_result(1, "call-1");
        assert_eq!(
            display
                .tool_collapse_override
                .get(&1)
                .and_then(|m| m.get("call-1")),
            Some(&false),
            "first click should expand a collapsed quiet result"
        );
        assert!(
            display.render_cache[0].is_none(),
            "toggle must invalidate the render cache"
        );

        // Second click collapses again.
        display.toggle_tool_result(1, "call-1");
        assert_eq!(
            display
                .tool_collapse_override
                .get(&1)
                .and_then(|m| m.get("call-1")),
            Some(&true),
            "second click should collapse the result again"
        );
    }

    #[test]
    fn toggle_tool_result_missing_turn_or_call_is_noop() {
        let mut app = test_app();
        let display = app.active_display().unwrap();
        // No such turn → no-op.
        display.toggle_tool_result(99, "call-1");
        assert!(display.tool_collapse_override.is_empty());
        // Turn exists but no matching call_id → no-op.
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![ToolResultRecord {
                call_id: "other".into(),
                name: "sh".into(),
                content: "y".into(),
                is_error: false,
                invocation_description: String::new(),
                image: None,
            }],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        display.view.insert_or_replace(1, turn);
        display.toggle_tool_result(1, "call-1");
        assert!(display.tool_collapse_override.is_empty());
    }

    #[test]
    fn turns_undone_prunes_tool_collapse_override() {
        let mut app = test_app();
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![ToolResultRecord {
                call_id: "call-1".into(),
                name: "read_file".into(),
                content: "x".into(),
                is_error: false,
                invocation_description: String::new(),
                image: None,
            }],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        {
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(1, turn);
            // Simulate the user having expanded the quiet result.
            display
                .tool_collapse_override
                .entry(1)
                .or_default()
                .insert("call-1".into(), false);
        }

        app.handle_turns_undone(0, &[1]);

        let display = app.active_display_ref().unwrap();
        assert!(
            display.tool_collapse_override.is_empty(),
            "undo should prune the tool collapse override"
        );
        assert!(
            display.view.turns[&1].undone,
            "the turn should be marked undone"
        );
    }

    #[test]
    fn turn_layout_populates_tool_result_header_ranges() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![
                ToolResultRecord {
                    call_id: "c1".into(),
                    name: "read_file".into(),
                    content: "x".into(),
                    is_error: false,
                    invocation_description: "Reading `a`.".into(),
                    image: None,
                },
                ToolResultRecord {
                    call_id: "c2".into(),
                    name: "sh".into(),
                    content: "y".into(),
                    is_error: false,
                    invocation_description: "Running `b`.".into(),
                    image: None,
                },
            ],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(1, turn);
        app.rebuild_height_prefix();

        // Capture the ranges and turn height in one borrow scope to avoid
        // overlapping borrows of the display.
        let (ranges, turn_h) = {
            let display = app.active_display().unwrap();
            let layout = &display.turn_layouts[0];
            (
                layout.tool_result_header_ranges.clone(),
                display.turn_heights[0],
            )
        };
        assert_eq!(ranges.len(), 2, "one header range per tool result");
        // No other sections on this turn: both headers are the first two
        // lines (the quiet read_file is collapsed, the sh result expanded).
        assert_eq!(ranges[0], (0, 1));
        assert_eq!(ranges[1], (1, 2));
        assert!(
            ranges[1].1 <= turn_h,
            "headers must lie within the turn text"
        );
    }

    // ── auto-collapse on first answer chunk ──

    #[test]
    fn first_answer_chunk_auto_collapses_reasoning() {
        let mut app = test_app();
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: Some("thinking".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.view.request_to_turn.insert(7, 1);
        // The user expanded reasoning during streaming.
        display.reasoning_override.insert(1, true);

        app.handle_request_stream(0, 7, OutputStream::Answer, Cow::Borrowed("Hi"));

        let display = app.active_display().unwrap();
        assert!(
            !display.reasoning_override.contains_key(&1),
            "first answer chunk should auto-collapse reasoning"
        );
        assert_eq!(display.view.turns[&1].assistant_text.as_deref(), Some("Hi"));
        assert!(
            display.view.turns[&1].assistant_reasoning.is_some(),
            "reasoning content must be retained after the response streams"
        );
    }

    #[test]
    fn reasoning_chunk_keeps_expansion_override() {
        let mut app = test_app();
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: Some("thinking".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.view.request_to_turn.insert(7, 1);
        display.reasoning_override.insert(1, true);

        app.handle_request_stream(0, 7, OutputStream::Reasoning, Cow::Borrowed(" more"));

        let display = app.active_display().unwrap();
        assert_eq!(
            display.reasoning_override.get(&1),
            Some(&true),
            "reasoning chunks must not collapse the section"
        );
        assert_eq!(
            display.view.turns[&1].assistant_reasoning.as_deref(),
            Some("thinking more"),
            "reasoning chunk should append to the reasoning text"
        );
    }

    // ── apply_image_result ──

    #[test]
    fn apply_image_result_clears_pending_job_and_records_failure() {
        use crate::image_worker::next_job_id;

        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let metadata = choreo_proto::ImageMetadata {
            mime_type: "image/png".to_string(),
            width: 100,
            height: 100,
            byte_len: 500,
            alt: None,
        };
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![choreo_proto::DisplayedImageRecord {
                metadata: metadata.clone(),
                data: vec![3u8; 30],
                tool_call_id: None,
            }],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        let turn_clone = turn.clone();
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(4, turn);
        app.sync_turn_images(0, 4, &turn_clone);

        let img_id = next_job_id();
        app.pending_job_idx.insert(img_id, (0, 4, 0));
        let img = app
            .rendered_images
            .get_mut(&0)
            .unwrap()
            .get_mut(&4)
            .unwrap()
            .get_mut(&0)
            .unwrap();
        img.pending_job = Some(img_id);

        let inline_size = Size::new(app.history_viewport.width, app.image_block_height());
        let result = crate::image_worker::ImageResult {
            id: img_id,
            protocol: None,
            cell_size: inline_size,
        };
        app.apply_image_result(result);

        let img = app
            .rendered_images
            .get(&0)
            .unwrap()
            .get(&4)
            .unwrap()
            .get(&0)
            .unwrap();
        assert!(img.failed_sizes.contains(&inline_size));
        assert!(img.pending_job.is_none());
    }

    #[test]
    fn apply_image_result_records_failure_at_any_size() {
        use crate::image_worker::next_job_id;

        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let metadata = choreo_proto::ImageMetadata {
            mime_type: "image/png".to_string(),
            width: 100,
            height: 100,
            byte_len: 500,
            alt: None,
        };
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![choreo_proto::DisplayedImageRecord {
                metadata: metadata.clone(),
                data: vec![4u8; 40],
                tool_call_id: None,
            }],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        let turn_clone = turn.clone();
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(5, turn);
        app.sync_turn_images(0, 5, &turn_clone);

        let img_id = next_job_id();
        app.pending_job_idx.insert(img_id, (0, 5, 0));
        let img = app
            .rendered_images
            .get_mut(&0)
            .unwrap()
            .get_mut(&5)
            .unwrap()
            .get_mut(&0)
            .unwrap();
        img.pending_job = Some(img_id);

        // Use a cell_size that is NOT the inline size.
        let non_inline_size = Size::new(80, app.image_block_height() + 1);
        let result = crate::image_worker::ImageResult {
            id: img_id,
            protocol: None,
            cell_size: non_inline_size,
        };
        app.apply_image_result(result);

        let img = app
            .rendered_images
            .get(&0)
            .unwrap()
            .get(&5)
            .unwrap()
            .get(&0)
            .unwrap();
        assert!(img.failed_sizes.contains(&non_inline_size));
    }

    // ── compute_total_height_and_markers scroll preservation ──

    /// Helper: insert a minimal turn into `app`.
    fn insert_turn(app: &mut App, id: u32, user_text: &str, assistant_text: &str) {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some(user_text.into()),
            assistant_text: Some(assistant_text.into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.display_for(0).view.insert_or_replace(id, turn);
    }

    #[test]
    fn scroll_preserved_when_scrolled_up_and_content_changes() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        insert_turn(&mut app, 0, "a", "a");
        insert_turn(&mut app, 1, "b", "b");
        app.rebuild_height_prefix();

        // Capture viewport height before taking a mutable borrow.
        let viewport_height = app.history_viewport.height;
        {
            let display = app.active_display().unwrap();
            let initial_total = display.total_history_height();

            display.history_scroll.scroll =
                initial_total.saturating_sub(viewport_height as usize) / 2;
        }
        assert!(app.effective_scroll() > 0, "should be scrolled up");

        insert_turn(&mut app, 2, "new content", "new content");
        let old_total = app.total_history_height();
        let old_scroll;
        {
            let display = app.active_display().unwrap();
            old_scroll = display.history_scroll.scroll;

            display.mark_content_changed();
        }

        app.compute_total_height_and_markers();

        let display = app.active_display_ref().unwrap();
        let new_total = display.total_history_height();
        let delta = new_total.saturating_sub(old_total);
        assert!(
            delta > 0,
            "total height should increase after adding content"
        );
        assert_eq!(
            display.history_scroll.scroll,
            old_scroll + delta,
            "scroll should be adjusted by the content delta"
        );
        assert!(
            !display.content_dirty,
            "content_dirty should be cleared after computation"
        );
    }

    #[test]
    fn scroll_not_preserved_when_at_bottom_and_content_changes() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        insert_turn(&mut app, 0, "a", "a");
        insert_turn(&mut app, 1, "b", "b");
        app.rebuild_height_prefix();

        {
            let display = app.active_display().unwrap();
            display.history_scroll.scroll = 0;
        }
        assert_eq!(app.effective_scroll(), 0, "should be at bottom");

        insert_turn(&mut app, 2, "more", "more");
        let old_scroll;
        {
            let display = app.active_display().unwrap();
            old_scroll = display.history_scroll.scroll;
            display.mark_content_changed();
        }

        app.compute_total_height_and_markers();

        let display = app.active_display_ref().unwrap();
        assert_eq!(
            display.history_scroll.scroll, old_scroll,
            "scroll should stay at 0 when user is at bottom"
        );
    }

    // ── marker computation ──

    #[test]
    fn markers_empty_when_no_user_text_turns() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;

        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("hello".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        {
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(0, turn);
        }
        app.rebuild_height_prefix();

        let display = app.active_display_ref().unwrap();
        assert!(
            display.markers.is_empty(),
            "no markers should be created when no turn has user_text"
        );
    }

    #[test]
    fn markers_created_for_each_user_text_turn() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;

        insert_turn(&mut app, 0, "user a", "assistant a");
        let turn_no_user = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("assistant only".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        {
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(1, turn_no_user);
        }
        insert_turn(&mut app, 2, "user c", "assistant c");

        app.rebuild_height_prefix();

        let display = app.active_display_ref().unwrap();
        assert_eq!(
            display.markers.len(),
            2,
            "expected 2 markers for 2 user-text turns"
        );
        assert!(
            display.markers[0].content_line < display.markers[1].content_line,
            "first user-text turn should appear before the second"
        );

        let total = display.total_history_height();
        for marker in &display.markers {
            assert!(
                marker.content_line < total,
                "marker content_line {0} should be < total history {total}",
                marker.content_line
            );
        }
    }

    #[test]
    fn marker_virtual_slot_uses_final_total_height() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let virtual_track = 2 * app.history_viewport.height as usize;

        insert_turn(&mut app, 0, "x", "y");
        insert_turn(&mut app, 1, "x", "y");
        app.rebuild_height_prefix();

        let display = app.active_display_ref().unwrap();
        let total = display.total_history_height();
        assert!(total > 0, "total history should be positive");

        let mut prev_end = 0usize;
        for (i, marker) in display.markers.iter().enumerate() {
            assert_eq!(
                marker.content_line, prev_end,
                "marker {i} content_line should equal the start of the turn"
            );
            if let Some(&end) = display.height_prefix.get(i) {
                prev_end = end;
            }

            let expected_slot = marker.content_line * virtual_track / total;
            assert_eq!(
                marker.virtual_slot, expected_slot,
                "marker {i} virtual_slot should use final total={total} as denominator"
            );
        }
    }

    #[test]
    fn marker_virtual_slot_proportional_to_position() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let virtual_track = 2 * app.history_viewport.height as usize;

        insert_turn(&mut app, 0, "a", "a");
        insert_turn(&mut app, 1, "b", "b");
        insert_turn(&mut app, 2, "c", "c");
        app.rebuild_height_prefix();

        let display = app.active_display_ref().unwrap();
        assert!(
            display.markers[0].virtual_slot <= display.markers[1].virtual_slot,
            "second marker slot should be >= first marker slot"
        );
        assert!(
            display.markers[1].virtual_slot <= display.markers[2].virtual_slot,
            "third marker slot should be >= second marker slot"
        );
        assert!(
            display.markers[2].virtual_slot < virtual_track,
            "last marker slot should be less than virtual_track={virtual_track}"
        );
    }

    #[test]
    fn scroll_not_preserved_when_content_dirty_is_false() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        insert_turn(&mut app, 0, "a", "a");
        app.rebuild_height_prefix();

        let old_scroll;
        {
            let display = app.active_display().unwrap();
            display.history_scroll.scroll = 10;
            old_scroll = display.history_scroll.scroll;

            display.markers_dirty = true;
            assert!(!display.content_dirty, "content should not be dirty");
        }
        app.compute_total_height_and_markers();

        let display = app.active_display_ref().unwrap();
        assert_eq!(
            display.history_scroll.scroll, old_scroll,
            "scroll should not change when content_dirty is false"
        );
    }

    // ── update_viewport_from_terminal_size ──

    #[test]
    fn help_overlay_reduces_viewport_height() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 26;

        app.last_terminal_size = Some((80, 30));
        app.terminal_resized = false;

        app.show_ctrl_help = false;
        app.update_viewport_from_terminal_size();
        let height_without_help = app.history_viewport.height;

        app.last_terminal_size = Some((80, 30));
        app.terminal_resized = false;
        app.show_ctrl_help = true;
        app.update_viewport_from_terminal_size();
        let height_with_help = app.history_viewport.height;

        assert_eq!(height_without_help - height_with_help, 2,);

        let total = app.total_history_height();
        let max_scroll = app.max_scroll_offset();
        if total > height_with_help as usize {
            assert_eq!(max_scroll + height_with_help as usize, total,);
        }
    }

    #[test]
    fn width_change_clears_content_dirty() {
        let mut app = test_app();

        app.history_viewport.width = 80;
        app.history_viewport.height = 26;

        app.last_terminal_size = Some((100, 30));
        app.terminal_resized = false;

        {
            let display = app.active_display().unwrap();
            display.content_dirty = true;
            display.markers_dirty = true;
            display.render_cache = vec![Some(RenderedCache {
                key: RenderCacheKey {
                    turn_id: 0,
                    width: 0,
                    viewport_width: 0,
                    reasoning_expanded: false,
                    tool_results_collapsed: vec![],
                    content_version: 0,
                },
                rendered: RenderedTurn {
                    lines: Arc::from(Vec::<Line<'static>>::new()),
                    height: 0,
                    visual_offsets: Arc::from([]),
                    joins: Arc::from([]),
                    content_ranges: Arc::from([]),
                    reasoning_header_idx: None,
                    tool_result_header_idxs: vec![],
                },
            })];
        }

        app.update_viewport_from_terminal_size();

        let display = app.active_display_ref().unwrap();
        assert!(
            !display.content_dirty,
            "content_dirty should be cleared on width change"
        );
        assert!(display.markers_dirty, "markers_dirty should remain true");
        assert!(
            display.render_cache.iter().all(|c| c.is_none()),
            "render_cache should be cleared"
        );
        assert_eq!(app.history_viewport.width, 99);
    }

    #[test]
    fn height_only_change_does_not_clear_content_dirty() {
        let mut app = test_app();

        app.history_viewport.width = 79;
        app.history_viewport.height = 20;

        app.last_terminal_size = Some((80, 30));
        app.terminal_resized = false;

        {
            let display = app.active_display().unwrap();
            display.content_dirty = true;
            display.markers_dirty = true;
            display.render_cache = vec![Some(RenderedCache {
                key: RenderCacheKey {
                    turn_id: 0,
                    width: 0,
                    viewport_width: 0,
                    reasoning_expanded: false,
                    tool_results_collapsed: vec![],
                    content_version: 0,
                },
                rendered: RenderedTurn {
                    lines: Arc::from(Vec::<Line<'static>>::new()),
                    height: 0,
                    visual_offsets: Arc::from([]),
                    joins: Arc::from([]),
                    content_ranges: Arc::from([]),
                    reasoning_header_idx: None,
                    tool_result_header_idxs: vec![],
                },
            })];
        }

        app.update_viewport_from_terminal_size();

        let display = app.active_display_ref().unwrap();
        assert!(
            display.content_dirty,
            "content_dirty should NOT be cleared on height-only change"
        );
        assert!(display.markers_dirty, "markers_dirty should remain true");
        assert!(
            display.render_cache.iter().all(|c| c.is_none()),
            "render_cache should be cleared"
        );
    }

    // ── compute_total_height_and_markers: anchor preservation on content removal ──

    #[test]
    fn content_removed_preserves_scroll_anchor() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        insert_turn(&mut app, 0, "user text", "assistant text");
        insert_turn(&mut app, 1, "more user", "more assistant");
        app.rebuild_height_prefix();

        let old_total = app.total_history_height();
        assert!(old_total > 0, "should have content");

        let viewport_height = app.history_viewport.height;
        let old_scroll;
        {
            let display = app.active_display().unwrap();
            // Scroll to the top of the history so the removed turn (the
            // last one) lies entirely below the viewport — the scenario
            // where anchor preservation keeps the viewport still.
            display.history_scroll.scroll = old_total.saturating_sub(viewport_height as usize);
            old_scroll = display.history_scroll.scroll;
        }
        assert!(app.effective_scroll() > 0, "should be scrolled up");

        {
            let display = app.active_display().unwrap();
            display.view.turns.remove(&1);
            assert_eq!(display.view.turns.len(), 1);

            display.mark_content_changed();
        }

        app.compute_total_height_and_markers();

        let display = app.active_display_ref().unwrap();
        let new_total = display.total_history_height();
        let new_scroll = display.history_scroll.scroll;
        assert!(
            new_total < old_total,
            "removing a turn should shrink the total height"
        );
        // The content row at the viewport's bottom edge stays anchored
        // instead of the viewport jumping to the bottom.
        assert_eq!(
            new_total.saturating_sub(new_scroll),
            old_total.saturating_sub(old_scroll),
            "the anchored content row should not move"
        );
    }

    #[test]
    fn content_added_shifts_scroll_down() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        insert_turn(&mut app, 0, "a", "b");
        app.rebuild_height_prefix();

        let viewport_height = app.history_viewport.height;
        let old_total;
        let old_scroll;
        {
            let display = app.active_display().unwrap();
            old_total = display.total_history_height();
            display.history_scroll.scroll = old_total.saturating_sub(viewport_height as usize) / 2;
            old_scroll = display.history_scroll.scroll;
        }

        insert_turn(&mut app, 1, "c", "d");
        {
            let display = app.active_display().unwrap();
            display.mark_content_changed();
        }

        app.compute_total_height_and_markers();

        let display = app.active_display_ref().unwrap();
        let new_total = display.total_history_height();
        let delta = new_total.saturating_sub(old_total);
        assert!(delta > 0, "total height should increase");
        assert_eq!(
            display.history_scroll.scroll,
            old_scroll + delta,
            "scroll should be shifted down by the content delta"
        );
    }

    // ── streaming (incremental update) ──

    #[test]
    fn mark_streaming_changed_sets_flags() {
        let mut app = test_app();
        {
            let display = app.active_display_ref().unwrap();
            assert!(!display.streaming_dirty);
            assert!(!display.content_dirty);
        }

        app.mark_streaming_changed();

        let display = app.active_display_ref().unwrap();
        assert!(display.streaming_dirty, "streaming_dirty should be set");
        assert!(display.content_dirty, "content_dirty should be set");
    }

    #[test]
    fn mark_content_changed_resets_streaming_turn_index() {
        let mut app = test_app();
        let display = app.active_display().unwrap();
        display.markers_dirty = false;
        display.streaming_turn_index = Some(0);

        display.mark_content_changed();

        assert!(display.markers_dirty, "markers_dirty should be set");
        assert!(display.content_dirty, "content_dirty should be set");
        assert!(
            display.streaming_turn_index.is_none(),
            "streaming_turn_index should be cleared"
        );
    }

    // ── turn_has_live_content (attach snapshot merge) ──

    fn empty_placeholder() -> Turn {
        Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("q".into()),
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        }
    }

    fn with_text(turn: &Turn, text: &str) -> Turn {
        let mut t = turn.clone();
        t.assistant_text = Some(text.into());
        t
    }

    #[test]
    fn accumulated_live_content_beats_snapshot_placeholder() {
        let placeholder = empty_placeholder();
        let live = with_text(&placeholder, "streamed so far");
        // The accumulated turn has content the snapshot placeholder lacks.
        assert!(turn_has_live_content(&live, &placeholder));
        // But the placeholder never "wins" over a live turn.
        assert!(!turn_has_live_content(&placeholder, &live));
    }

    #[test]
    fn snapshot_with_content_wins_over_accumulated() {
        let placeholder = empty_placeholder();
        let snapshot_final = with_text(&placeholder, "final answer from daemon");
        let accumulated = with_text(&placeholder, "earlier accumulated");
        // Both have content — the snapshot (daemon-canonical) wins.
        assert!(!turn_has_live_content(&accumulated, &snapshot_final));
        // Identical content: snapshot wins too (no clause triggers).
        let same = with_text(&placeholder, "same");
        assert!(!turn_has_live_content(&same, &same));
    }

    #[test]
    fn reasoning_and_tool_content_also_count_as_live() {
        let placeholder = empty_placeholder();
        let mut reasoning = placeholder.clone();
        reasoning.assistant_reasoning = Some("thinking…".into());
        assert!(turn_has_live_content(&reasoning, &placeholder));

        let mut tool = placeholder.clone();
        tool.tool_calls.push(choreo_proto::AssistantToolCallRecord {
            call_id: "call_1".into(),
            name: "read_file".into(),
            arguments_json: "{}".into(),
        });
        assert!(turn_has_live_content(&tool, &placeholder));
    }

    #[test]
    fn streaming_update_without_turn_index_falls_back() {
        let mut app = test_app();
        insert_turn(&mut app, 0, "hello", "world");
        app.rebuild_height_prefix();

        let display = app.active_display_ref().unwrap();
        let old_total = display.total_history_height();
        assert!(old_total > 0);

        // Simulate streaming without a streaming_turn_index.
        // Capture viewport before mutable borrow.
        let viewport = app.history_viewport;
        let display = app.active_display().unwrap();
        display.streaming_turn_index = None;
        display.streaming_dirty = true;
        display.content_dirty = true;

        let total = display.compute_total_height_and_markers(&viewport);

        assert!(!display.streaming_dirty, "streaming_dirty cleared");
        assert!(!display.content_dirty, "content_dirty cleared");
        assert_eq!(total, old_total, "full rebuild produces same total");
    }

    #[test]
    fn streaming_update_recalculates_turn_height() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        insert_turn(&mut app, 0, "hello", "world");
        app.rebuild_height_prefix();

        let display = app.active_display_ref().unwrap();
        let before_height = display.turn_heights[0];
        let before_total = display.total_history_height();

        // Simulate streaming: append to assistant_text.
        let viewport = app.history_viewport;
        let display = app.active_display().unwrap();
        let turn = display.view.turns.get_mut(&0).unwrap();
        turn.assistant_text
            .as_mut()
            .unwrap()
            .push_str("\n\nnew streaming content");
        display.streaming_turn_index = Some(0);
        display.streaming_dirty = true;
        display.content_dirty = true;

        let total = display.compute_total_height_and_markers(&viewport);

        assert!(
            display.turn_heights[0] > before_height,
            "turn height should increase after content added"
        );
        assert!(
            total >= before_total,
            "total height should increase or stay same"
        );
        assert!(!display.streaming_dirty, "streaming_dirty cleared");
        assert!(!display.content_dirty, "content_dirty cleared");
    }

    #[test]
    fn streaming_answer_moves_reasoning_header_range() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        // A turn with reasoning only (no response yet), actively streaming.
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: Some("thinking".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        {
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(1, turn);
            display.view.request_to_turn.insert(7, 1);
        }
        app.rebuild_height_prefix();

        // Before the answer: reasoning is the only content, so the header
        // sits at the top of the assistant block.
        let initial_start = app.active_display_ref().unwrap().turn_layouts[0]
            .reasoning_header_range
            .expect("header range should exist")
            .0;

        // First Answer chunk auto-collapses the reasoning and places the
        // response above the header.
        app.handle_request_stream(0, 7, OutputStream::Answer, Cow::Borrowed("Response text."));
        app.compute_total_height_and_markers();

        let (start, end) = app.active_display_ref().unwrap().turn_layouts[0]
            .reasoning_header_range
            .expect("header range should remain after auto-collapse");
        assert!(
            start > initial_start,
            "header should move below the streaming response ({initial_start} -> {start})"
        );
        assert!(start < end, "header range must be non-empty");
    }

    #[test]
    fn streaming_tool_result_expanded_grows_collapsed_stays_flat() {
        // The streaming fast path re-renders the in-flight turn with the
        // effective per-result visibility every chunk: an expanded result's
        // body (and turn height) grows live, while a collapsed quiet result
        // stays a single header row no matter how much content streams in.
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![
                ToolResultRecord {
                    call_id: "quiet".into(),
                    name: "read_file".into(), // quiet → collapsed by default
                    content: String::new(),
                    is_error: false,
                    invocation_description: "Reading `a`.".into(),
                    image: None,
                },
                ToolResultRecord {
                    call_id: "loud".into(),
                    name: "sh".into(), // not quiet → expanded by default
                    content: String::new(),
                    is_error: false,
                    invocation_description: "Running `b`.".into(),
                    image: None,
                },
            ],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        {
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(1, turn);
            display.view.request_to_turn.insert(7, 1);
        }
        app.rebuild_height_prefix();

        // Capture the header ranges and turn height in one borrow scope.
        let snapshot = |app: &mut App| {
            let display = app.active_display_ref().unwrap();
            let layout = &display.turn_layouts[0];
            (
                layout.tool_result_header_ranges[0],
                layout.tool_result_header_ranges[1],
                display.turn_heights[0],
            )
        };
        let (quiet_range, _loud_range, height_before) = snapshot(&mut app);
        assert_eq!(quiet_range, (0, 1), "collapsed result: single header row");

        // Stream a chunk into the *expanded* result: the turn must grow and
        // the collapsed result must keep its single-row header range.
        app.handle_tool_result_chunk(
            0,
            7,
            "loud".into(),
            b"line one\nline two\nline three\n".to_vec(),
        );
        app.compute_total_height_and_markers();

        let (quiet_range, loud_range, height_after) = snapshot(&mut app);
        assert!(
            height_after > height_before,
            "expanded result grows as content streams ({height_before} -> {height_after})"
        );
        assert_eq!(quiet_range, (0, 1), "collapsed result stays a single row");
        assert_eq!(loud_range, (1, 2), "expanded header still on its own row");

        // Now stream an even bigger chunk into the *collapsed* quiet result:
        // nothing visible changes — the body is hidden behind the triangle.
        let height_before_collapsed = snapshot(&mut app).2;
        app.handle_tool_result_chunk(
            0,
            7,
            "quiet".into(),
            b"hidden\nhidden\nhidden\nhidden\n".to_vec(),
        );
        app.compute_total_height_and_markers();
        let height_after_collapsed = snapshot(&mut app).2;
        assert_eq!(
            height_after_collapsed, height_before_collapsed,
            "collapsed result stays flat while its content streams"
        );
    }

    #[test]
    fn streaming_chunk_after_mark_content_changed_stays_fresh_and_incremental() {
        // Regression for "scrollbar moves but the results stay stuck": a
        // mid-stream `mark_content_changed` (here simulated with a `Done` for
        // an unrelated request — the same shape as a `TurnAppended` or
        // `SessionState` interleaving between chunks, which happens more
        // often when another session is active) disarms the streaming fast
        // path (`streaming_dirty=false`, `streaming_turn_index=None`).
        //
        // Before the fix the next chunk was processed by the O(n) full
        // rebuild, whose content-blind cache key served the *pre-chunk* lines
        // — the visible results froze until the final `TurnAppended`
        // invalidated the slot.  The content-version key forces the rebuild
        // to recompute, and running the fast path first keeps chunk
        // processing incremental.
        let mut app = test_app();
        app.attached_session_id = Some(0);
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![ToolResultRecord {
                call_id: "call-1".into(),
                name: "sh".into(), // not quiet → expanded by default
                content: String::new(),
                is_error: false,
                invocation_description: "Running `b`.".into(),
                image: None,
            }],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        {
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(1, turn);
            display.view.request_to_turn.insert(7, 1);
        }
        app.rebuild_height_prefix();

        let version_before = app.active_display_ref().unwrap().turn_content_version(1);
        let height_before = app.active_display_ref().unwrap().turn_heights[0];

        // First chunk: fast path renders it live.
        app.handle_tool_result_chunk(0, 7, "call-1".into(), b"line one\n".to_vec());
        app.compute_total_height_and_markers();
        let version_after_chunk1 = app.active_display_ref().unwrap().turn_content_version(1);
        assert!(
            version_after_chunk1 > version_before,
            "chunk must bump the turn's content version"
        );

        // Mid-stream disarming event (unrelated Done): clears the streaming
        // flags and forces markers_dirty.
        app.handle_done(0, 99, None, None);
        assert!(
            app.active_display_ref().unwrap().markers_dirty,
            "Done must mark the display for a rebuild"
        );

        // Second chunk arrives while markers_dirty is still set.
        app.handle_tool_result_chunk(0, 7, "call-1".into(), b"line two\n".to_vec());
        app.compute_total_height_and_markers();

        let display = app.active_display_ref().unwrap();
        // The rebuild (or the fast path) must serve the *latest* content, not
        // the pre-second-chunk lines the content-blind key would have reused.
        let cached = display.render_cache[0].as_ref().expect("cache slot filled");
        let text: String = cached
            .rendered
            .lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("line two"),
            "rebuild must not serve stale pre-chunk lines:\n{text}"
        );
        assert!(
            text.contains("line one"),
            "earlier chunk must still be present"
        );
        assert!(
            display.turn_heights[0] > height_before,
            "turn height must reflect the streamed content"
        );
        assert!(!display.streaming_dirty, "streaming flag consumed");
        assert!(!display.markers_dirty, "markers flag consumed");
        assert!(!display.content_dirty, "content flag consumed");
    }

    #[test]
    fn rebuild_after_disarmed_chunk_serves_fresh_content() {
        // Regression for the content-version cache key: a chunk arrives, then
        // a `mark_content_changed` event (a `Done`/`TurnAppended`/`SessionState`
        // interleaving between the chunk and its render) disarms the fast
        // path *before* it can re-render — so the rebuild runs against a cache
        // entry rendered from pre-chunk content.  Without the content version
        // in the key the rebuild would reuse those stale lines; with it, the
        // mismatch forces a recompute.
        let mut app = test_app();
        app.attached_session_id = Some(0);
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![ToolResultRecord {
                call_id: "call-1".into(),
                name: "sh".into(), // not quiet → expanded by default
                content: String::new(),
                is_error: false,
                invocation_description: "Running `b`.".into(),
                image: None,
            }],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        {
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(1, turn);
            display.view.request_to_turn.insert(7, 1);
        }
        app.rebuild_height_prefix();

        // The cache now holds the pre-chunk rendering (empty result body).
        // A chunk appends content and bumps the version, but the fast path
        // has NOT run yet.
        app.handle_tool_result_chunk(0, 7, "call-1".into(), b"line one\n".to_vec());

        // The disarming event lands before the next render: streaming flags
        // are cleared, markers_dirty set — the rebuild path will run.
        app.handle_done(0, 99, None, None);

        app.compute_total_height_and_markers();

        let display = app.active_display_ref().unwrap();
        let cached = display.render_cache[0].as_ref().expect("cache slot filled");
        let text: String = cached
            .rendered
            .lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("line one"),
            "rebuild must recompute the chunk's content (content-version key):\n{text}"
        );
        assert!(
            cached.key.content_version > 0,
            "cache entry must record the post-chunk version"
        );
    }

    #[test]
    fn content_version_bumps_on_every_mutating_handler() {
        // The version is the cache key's content fingerprint: every handler
        // that changes a turn's rendered text must bump it, so a rebuild can
        // tell stale entries apart from current ones.
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![ToolResultRecord {
                call_id: "call-1".into(),
                name: "sh".into(),
                content: String::new(),
                is_error: false,
                invocation_description: String::new(),
                image: None,
            }],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        {
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(1, turn);
            display.view.request_to_turn.insert(7, 1);
        }
        app.rebuild_height_prefix();

        let v0 = app.active_display_ref().unwrap().turn_content_version(1);
        assert_eq!(v0, 0, "fresh turn starts at version 0");

        app.handle_tool_result_chunk(0, 7, "call-1".into(), b"one\n".to_vec());
        let v1 = app.active_display_ref().unwrap().turn_content_version(1);
        assert_eq!(v1, v0 + 1, "chunk bumps by one");

        app.handle_tool_result_chunk(0, 7, "call-1".into(), b"two\n".to_vec());
        let v2 = app.active_display_ref().unwrap().turn_content_version(1);
        assert_eq!(v2, v1 + 1, "every chunk bumps the version");

        // A replacement turn (the daemon's final TurnAppended) bumps too, so
        // a cached rendering of the accumulated version is never reused.
        let mut replacement = app.active_display_ref().unwrap().view.turns[&1].clone();
        replacement.tool_results[0].content.push_str("final\n");
        app.handle_turn_appended(0, 1, replacement);
        let v3 = app.active_display_ref().unwrap().turn_content_version(1);
        assert_eq!(v3, v2 + 1, "TurnAppended bumps the version");
    }

    #[test]
    fn streaming_update_preserves_height_prefix_invariant() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        insert_turn(&mut app, 0, "a", "b");
        insert_turn(&mut app, 1, "c", "d");
        insert_turn(&mut app, 2, "e", "f");
        app.rebuild_height_prefix();

        let viewport = app.history_viewport;
        let display = app.active_display().unwrap();
        let old_prefix = display.height_prefix.clone();
        let old_heights = display.turn_heights.clone();

        // Stream content into turn 1.
        let turn = display.view.turns.get_mut(&1).unwrap();
        turn.assistant_text
            .as_mut()
            .unwrap()
            .push_str("\n\nlots of new content that should increase height");
        display.streaming_turn_index = Some(1);
        display.streaming_dirty = true;
        display.content_dirty = true;

        display.compute_total_height_and_markers(&viewport);

        // Verify invariant: height_prefix[i] == sum(turn_heights[0..=i]).
        let mut accum = 0usize;
        for i in 0..display.turn_heights.len() {
            accum += display.turn_heights[i];
            assert_eq!(
                display.height_prefix[i], accum,
                "invariant failed at index {i}: height_prefix[i] should equal cumulative turn_heights"
            );
        }

        // Turn 0 height unchanged.
        assert_eq!(
            display.turn_heights[0], old_heights[0],
            "turn 0 height should not change"
        );
        assert_eq!(
            display.height_prefix[0], old_prefix[0],
            "height_prefix[0] should not change"
        );
        // Markers must also be correct after the streaming update.
        assert_eq!(
            display.markers[0].content_line, 0,
            "marker[0] content_line should be 0"
        );
        assert_eq!(
            display.markers[1].content_line, display.turn_heights[0],
            "marker[1] content_line should equal turn 0 height"
        );
        assert_eq!(
            display.markers[2].content_line,
            display.turn_heights[0] + display.turn_heights[1],
            "marker[2] content_line should reflect updated turn 1 height"
        );
    }

    #[test]
    fn handle_started_sets_streaming_turn_index() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        // Pre-populate turns so visible_turn_ids exist.
        insert_turn(&mut app, 10, "user", "assistant");
        insert_turn(&mut app, 20, "another user", "another assistant");
        app.rebuild_height_prefix();

        {
            let display = app.active_display_ref().unwrap();
            assert_eq!(display.visible_turn_ids.len(), 2);
            assert_eq!(display.visible_turn_ids[0], 10);
            assert_eq!(display.visible_turn_ids[1], 20);
            assert!(display.streaming_turn_index.is_none());
        }

        // handle_started now requires session_id
        app.handle_started(0, 1, 10, 100);

        let display = app.active_display_ref().unwrap();
        assert_eq!(
            display.streaming_turn_index,
            Some(0),
            "should find turn 10 at index 0"
        );
    }

    #[test]
    fn handle_done_fires_full_rebuild() {
        let mut app = test_app();
        // test_app's default display lives on session 0; treat it as the
        // attached session so `handle_done` routes to it.
        app.attached_session_id = Some(0);
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        insert_turn(&mut app, 10, "user", "assistant");
        app.rebuild_height_prefix();
        {
            let display = app.active_display().unwrap();
            display.markers_dirty = false;
            display.streaming_turn_index = Some(0);
            display.streaming_dirty = false;
            display.content_dirty = false;
        }

        app.handle_done(0, 1, None, None);

        let display = app.active_display_ref().unwrap();
        assert!(
            display.streaming_turn_index.is_none(),
            "streaming_turn_index should be cleared"
        );
        assert!(
            display.markers_dirty,
            "markers_dirty should be set (full rebuild)"
        );
        assert!(display.content_dirty, "content_dirty should be set");
    }

    #[test]
    fn handle_failed_clears_streaming() {
        let mut app = test_app();
        // test_app's default display lives on session 0; treat it as attached
        // so the connection-level (`None`) resolution keeps routing to it.
        app.attached_session_id = Some(0);
        {
            let display = app.active_display().unwrap();
            display.streaming_turn_index = Some(0);
            display.streaming_dirty = false;
            display.content_dirty = false;
            display.markers_dirty = false;
        }

        app.handle_failed(None, 1, "oops".into());

        let display = app.active_display_ref().unwrap();
        assert!(display.streaming_turn_index.is_none());
        assert!(display.error.is_some());
        assert!(display.markers_dirty, "markers_dirty should be set");
        assert!(display.content_dirty, "content_dirty should be set");
    }

    #[test]
    fn handle_failed_connection_level_resolves_to_attached_session_without_phantom_display() {
        // A connection-level "no session attached" failure arrives with
        // `session_id: None`.  It must land in the attached session's display
        // and must NOT create a phantom display entry.
        let mut app = App::new();
        app.attached_session_id = Some(42);
        app.active_session_id = Some(42);

        app.handle_failed(None, 7, "no session attached".into());

        let display = app.display_for(42);
        assert_eq!(display.error.as_deref(), Some("no session attached"));
        assert!(
            !app.session_displays.contains_key(&0),
            "a connection-level failure must not create a phantom session-0 display"
        );
    }

    #[test]
    fn handle_failed_for_request_failure_does_not_write_global_error() {
        // A request-level failure (a real session id) renders its full error
        // as the turn's red block in the transcript; the global status/error
        // bar must not print it a second time.  The per-session display still
        // records it.
        let mut app = test_app();
        app.attached_session_id = Some(42);
        app.active_session_id = Some(42);
        assert!(app.error.is_none());

        app.handle_failed(
            Some(42),
            1,
            "client error (402): Insufficient Balance".into(),
        );

        assert_eq!(
            app.error, None,
            "a request failure's transcript block must not be duplicated on the status bar"
        );
        assert_eq!(
            app.display_for(42).error.as_deref(),
            Some("client error (402): Insufficient Balance"),
            "the per-session display records the failure"
        );
    }

    #[test]
    fn handle_failed_connection_level_writes_global_error_for_attached_session() {
        // A `session_id: None` envelope marks a connection-level failure (e.g.
        // "no session attached"), which has no turn to render an error block
        // in: the global status/error bar is its only surface.
        let mut app = App::new();
        app.attached_session_id = Some(42);
        app.active_session_id = Some(42);
        assert!(app.error.is_none());

        app.handle_failed(None, 7, "no session attached".into());

        assert_eq!(app.error.as_deref(), Some("no session attached"));
        assert_eq!(
            app.display_for(42).error.as_deref(),
            Some("no session attached")
        );
        assert!(
            !app.session_displays.contains_key(&0),
            "a connection-level failure must not create a phantom session-0 display"
        );
    }

    #[test]
    fn handle_failed_connection_level_without_attached_session_still_writes_global_error() {
        // A connection-level rejection with no attached session to resolve to
        // has no display to update, but the user must still see it on the
        // status line — there is no transcript block for it.
        let mut app = test_app();
        app.attached_session_id = None;
        assert!(app.error.is_none());

        app.handle_failed(None, 9, "no session attached".into());

        assert_eq!(app.error.as_deref(), Some("no session attached"));
    }

    #[test]
    fn handle_failed_for_background_session_does_not_write_global_error() {
        // The TUI subscribes to all activity, so a background session's
        // request failure arrives too.  It must be recorded on that session's
        // display but must not clobber the global status/error bar the user
        // is looking at (same gating as the ModelSelected / ReasoningEffortSet
        // arms).
        let mut app = test_app();
        app.attached_session_id = Some(42);
        app.active_session_id = Some(42);
        assert!(app.error.is_none());

        app.handle_failed(Some(99), 3, "background failure".into());

        assert_eq!(
            app.error, None,
            "background failure must not write the global error bar"
        );
        assert_eq!(
            app.display_for(99).error.as_deref(),
            Some("background failure")
        );
    }

    // ── ModelSelectorState ──

    fn selector_with_models(models: &[&str]) -> ModelSelectorState {
        let mut sel = ModelSelectorState::new();
        sel.open();
        sel.apply_models(models.iter().map(|s| s.to_string()).collect(), None);
        sel
    }

    #[test]
    fn model_selector_open_resets_state_and_marks_loading() {
        let mut sel = ModelSelectorState::new();
        sel.all_models = vec!["a".into()];
        sel.selected = Some("a".into());
        sel.filter.text = "stale".to_string();
        sel.filter.cursor = 5;
        sel.focused = 3;
        sel.scroll = 2;
        sel.error = Some("old error".into());

        sel.open();

        assert!(sel.is_open());
        assert!(sel.loading);
        assert!(sel.filter.text.is_empty());
        assert_eq!(sel.focused, 0);
        assert_eq!(sel.scroll, 0);
        assert!(sel.error.is_none());
    }

    #[test]
    fn model_selector_close_keeps_model_list() {
        let mut sel = selector_with_models(&["a", "b"]);
        sel.close();

        assert!(!sel.is_open());
        assert_eq!(sel.all_models.len(), 2, "cached list survives close");
    }

    #[test]
    fn model_selector_apply_models_preselects_current() {
        let mut sel = ModelSelectorState::new();
        sel.open();
        sel.apply_models(
            vec![
                "gpt-4o".into(),
                "gpt-4o-mini".into(),
                "gpt-3.5-turbo".into(),
            ],
            Some("gpt-4o-mini".into()),
        );

        assert!(!sel.loading);
        assert_eq!(sel.focused, 1, "highlight lands on the active model");
        assert_eq!(sel.highlighted().as_deref(), Some("gpt-4o-mini"));
    }

    #[test]
    fn model_selector_apply_models_falls_back_to_top_when_selected_missing() {
        let mut sel = ModelSelectorState::new();
        sel.open();
        sel.apply_models(vec!["a".into(), "b".into()], Some("missing".into()));

        assert_eq!(sel.focused, 0);
        assert_eq!(sel.highlighted().as_deref(), Some("a"));
    }

    #[test]
    fn model_selector_filter_matches_case_insensitive_substring() {
        let mut sel = selector_with_models(&["gpt-4o", "GPT-4O-MINI", "claude-3"]);
        sel.filter.text = "gpt".to_string();
        sel.filter.cursor = 3;

        let filtered = sel.filtered();
        assert_eq!(filtered, vec!["gpt-4o", "GPT-4O-MINI"]);
    }

    #[test]
    fn model_selector_empty_filter_returns_all() {
        let sel = selector_with_models(&["a", "b", "c"]);
        assert_eq!(sel.filtered(), vec!["a", "b", "c"]);
    }

    #[test]
    fn model_selector_no_match_returns_empty() {
        let mut sel = selector_with_models(&["a", "b"]);
        sel.filter.text = "zzz".to_string();
        sel.filter.cursor = 3;
        assert!(sel.filtered().is_empty());
    }

    #[test]
    fn model_selector_focus_clamps_when_filter_narrows_list() {
        let mut sel = selector_with_models(&["a", "b", "c"]);
        sel.focused = 2;
        // Narrow to a single row: the highlight must not point past the end.
        sel.filter.text = "a".to_string();
        sel.filter.cursor = 1;
        sel.clamp_focus();
        assert_eq!(sel.focused, 0);
    }

    #[test]
    fn model_selector_move_up_down_clamped() {
        let mut sel = selector_with_models(&["a", "b", "c"]);
        sel.move_down();
        assert_eq!(sel.focused, 1);
        sel.move_down();
        sel.move_down();
        assert_eq!(sel.focused, 2, "move_down clamps at the last row");
        sel.move_up();
        assert_eq!(sel.focused, 1);
        sel.move_up();
        sel.move_up();
        assert_eq!(sel.focused, 0, "move_up clamps at the first row");
    }

    #[test]
    fn model_selector_window_keeps_focus_visible() {
        let mut sel = selector_with_models(&["a", "b", "c", "d", "e"]);
        sel.focused = 4;
        let filtered = sel.filtered();
        let (start, count) = sel.window(&filtered, 3);
        assert_eq!((start, count), (2, 3), "window slides down to reveal focus");
        assert!(sel.focused >= start && sel.focused < start + count);
    }

    #[test]
    fn model_selector_window_pulls_up_when_focus_above() {
        let mut sel = selector_with_models(&["a", "b", "c", "d", "e"]);
        sel.scroll = 4;
        sel.focused = 1;
        let filtered = sel.filtered();
        let (start, _) = sel.window(&filtered, 3);
        assert_eq!(start, 1, "window pulls up so focus is visible");
    }

    #[test]
    fn model_selector_window_empty_list_returns_zero() {
        let sel = selector_with_models(&[]);
        assert_eq!(sel.window(&sel.filtered(), 5), (0, 0));
        assert!(sel.highlighted().is_none());
    }

    #[test]
    fn model_selector_window_is_pure_and_idempotent() {
        // The renderer calls `window` during terminal.draw(), which must
        // never mutate scroll/focus state.  Verify repeated calls return
        // identical results and leave the fields untouched.
        let mut sel = selector_with_models(&["a", "b", "c", "d", "e"]);
        sel.scroll = 3;
        sel.focused = 4;
        let before_scroll = sel.scroll;
        let before_focused = sel.focused;

        let filtered = sel.filtered();
        let first = sel.window(&filtered, 3);
        let second = sel.window(&filtered, 3);

        assert_eq!(first, second, "window must be deterministic");
        assert_eq!(sel.scroll, before_scroll, "window must not mutate scroll");
        assert_eq!(sel.focused, before_focused, "window must not mutate focus");
    }

    #[test]
    fn model_selector_submit_returns_highlighted_and_closes() {
        let mut sel = selector_with_models(&["a", "b"]);
        sel.move_down();
        let model = sel.submit();
        assert_eq!(model.as_deref(), Some("b"));
        assert!(!sel.is_open(), "submit closes the selector");
    }

    #[test]
    fn model_selector_submit_empty_returns_none() {
        let mut sel = selector_with_models(&[]);
        assert!(sel.submit().is_none());
        assert!(!sel.is_open());
    }

    #[test]
    fn model_selector_filter_key_consumes_chars_and_backspace() {
        let mut sel = selector_with_models(&["gpt-4o", "claude-3"]);
        sel.filter_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        assert_eq!(sel.filter.text, "g");
        assert_eq!(sel.filtered(), vec!["gpt-4o"]);

        sel.filter_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(sel.filter.text.is_empty());
        assert_eq!(sel.filtered().len(), 2);
    }

    #[test]
    fn model_selector_filter_key_ignores_enter() {
        let mut sel = selector_with_models(&["a"]);
        sel.filter_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(sel.is_open());
    }

    #[test]
    fn model_selector_apply_error_records_and_clears_loading() {
        let mut sel = ModelSelectorState::new();
        sel.open();
        sel.apply_error("no credential".to_string());
        assert!(!sel.loading);
        assert_eq!(sel.error.as_deref(), Some("no credential"));
    }
}
