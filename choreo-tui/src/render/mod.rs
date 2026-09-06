use crate::RenderedImage;
use crate::markdown_render::{display_width, reasoning_expanded_default, render_turn_lines};
use crate::scrollbar::{SmoothScrollbar, SmoothScrollbarState};
use crate::selection;
use crate::state::{
    App, CTRL_HELP_LINE1, CTRL_HELP_LINE1_LEGACY, CTRL_HELP_LINE2, INPUT_PAD, Page, RenderCacheKey,
    cached_or_compute_lines, cached_visual_lines, input_inner_width,
};
use choreo_proto::{SessionStatus, TokenUsage};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect, Size},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Padding, Paragraph, Wrap},
};
use ratatui_image::StatefulImage;
use std::sync::Arc;

// The former monolithic render.rs was split into a render/ module.  This file
// keeps the chat/fullscreen/history drawing plus the helpers shared by several
// pages (cursor positioning, status/timestamp formatting, the scrollbar
// widget), while each page's dedicated rendering moved to its own submodule so
// every frame draws exactly what it did before the split.  The submodules reach
// back into this file via `super::` for those shared helpers, and the top-level
// `render()` dispatches into them through the `use self::…` imports below.
// Popup sizing/centering and the LIST-popup 3-band layout now live in
// `state/pages.rs` (`centered_popup`, `selector_list_layout`) so the renderers,
// the connection-layer mouse handlers, and the viewport cache share one
// geometry.
mod ai_providers;
mod model_selector;
mod session_manager;

use self::ai_providers::render_ai_providers;
use self::model_selector::render_model_selector;
use self::session_manager::render_session_manager;

pub(crate) const BG_SHADE: Color = Color::Rgb(53, 53, 53);

/// The persistent status-bar banner shown while the daemon's credential
/// keystore is locked (see `App::keystore_locked`). Small and non-intrusive:
/// a lock marker that leads the status bar until the daemon reports unlocked
/// (or bound-and-unlocked). A fresh daemon starts unbound — it reports
/// `KeystoreUnbound` and the client auto-binds it (the marker clears on the
/// `Bound` reply); a bound-but-locked daemon is unlocked with `/unlock` or
/// `/unlock <base64 unlock-key>` (the fuller guidance also appears in the
/// startup status and the submit-time prompt rejection).
pub(crate) const KEYS_LOCKED_MARKER: &str = " 🔒 keystore locked";

pub(crate) fn mouse_in_history_box(column: u16, row: u16, vp_width: u16, vp_height: u16) -> bool {
    column < vp_width && row < vp_height
}

pub(crate) fn mouse_in_scrollbar_column(
    column: u16,
    row: u16,
    vp_width: u16,
    vp_height: u16,
) -> bool {
    column == vp_width && row < vp_height
}

fn vertical_scrollbar() -> SmoothScrollbar {
    SmoothScrollbar::new()
        .thumb_fg(Color::DarkGray)
        .track_bg(BG_SHADE)
        .marker_fg(Color::Green)
}

pub(crate) fn render(frame: &mut Frame<'_>, app: &mut App) {
    if render_fullscreen_only(frame, app) {
        return;
    }

    match app.page {
        Page::Chat => render_chat(frame, app),
        Page::SessionManager => render_session_manager(frame, app),
        Page::AIProviders => render_ai_providers(frame, app),
    }

    // The model selector overlay draws on top of the Chat page content.
    if app.model_selector.is_open() {
        render_model_selector(frame, app);
    }
}

/// Look up the fullscreen image by (turn_id, img_idx) and render it.
pub(crate) fn render_fullscreen_only(frame: &mut Frame<'_>, app: &mut App) -> bool {
    let Some((session_id, turn_id, img_idx)) = app.fullscreen_image_target else {
        return false;
    };
    if !app
        .rendered_images
        .get(&session_id)
        .is_some_and(|m| m.contains_key(&turn_id))
        && !app
            .display_for(session_id)
            .view
            .turns
            .get(&turn_id)
            .is_some_and(|t| !t.displayed_images.is_empty())
    {
        app.fullscreen_image_target = None;
        return false;
    }
    render_fullscreen_image(frame, session_id, turn_id, img_idx, app);
    true
}

fn render_fullscreen_placeholder(frame: &mut Frame<'_>) {
    let area = frame.area();
    let block = Block::bordered()
        .title(" Loading image … ")
        .title_alignment(Alignment::Center);
    frame.render_widget(block, area);
}

fn render_fullscreen_image(
    frame: &mut Frame<'_>,
    session_id: u64,
    turn_id: u32,
    img_idx: usize,
    app: &mut App,
) {
    let area = frame.area();
    let full = Size::new(area.width, area.height);

    // Ensure the rendered_images entry exists — create from turn data if missing.
    if !app.rendered_images.contains_key(&session_id) {
        let Some(turn) = app.display_for(session_id).view.turns.get(&turn_id) else {
            return;
        };
        let Some(record) = turn.displayed_images.get(img_idx) else {
            return;
        };
        let placeholder =
            RenderedImage::new_placeholder(record.metadata.clone(), Arc::from(record.data.clone()));
        app.rendered_images
            .entry(session_id)
            .or_default()
            .entry(turn_id)
            .or_default()
            .insert(img_idx, placeholder);
    }

    // Fast path — already encoded at full size.
    let should_submit = match app
        .rendered_images
        .get_mut(&session_id)
        .and_then(|imgs| imgs.get_mut(&turn_id))
        .and_then(|images| images.get_mut(&img_idx))
    {
        Some(img) => {
            if let Some(protocol) = img.protocols.get_mut(&full) {
                let target = protocol.size_for(crate::IMAGE_RESIZE, full);
                let centered = Rect {
                    x: area.x + (area.width.saturating_sub(target.width)) / 2,
                    y: area.y + (area.height.saturating_sub(target.height)) / 2,
                    width: target.width.min(area.width),
                    height: target.height.min(area.height),
                };
                frame.render_stateful_widget(
                    StatefulImage::new().resize(crate::IMAGE_RESIZE),
                    centered,
                    protocol,
                );
                return;
            }
            // Submit job if not pending/failed/cached.
            img.pending_job.is_none()
                && !img.failed_sizes.contains(&full)
                && !img.protocols.contains_key(&full)
        }
        None => false,
    };

    if should_submit
        && let Some(images) = app.rendered_images.get(&session_id)
        && let Some(imgs) = images.get(&turn_id)
        && let Some(img) = imgs.get(&img_idx)
    {
        app.submit_image_job(
            session_id,
            turn_id,
            img_idx,
            img.data.clone(),
            img.metadata.clone(),
            full,
            crate::IMAGE_RESIZE,
        );
    }

    render_fullscreen_placeholder(frame);
}

fn render_chat(frame: &mut Frame<'_>, app: &mut App) {
    // Compute the Chat page's vertical layout via the shared helper so that
    // rendering, mouse hit-testing (connection.rs click-to-position), and the
    // history viewport (update_viewport_from_terminal_size) all use identical
    // math — they can never drift apart, even on tiny terminals where the
    // layout solver shrinks the fixed-height chunks.
    let [
        history_area,
        status_error_area,
        help_area,
        input_area,
        status_bar_area,
    ] = app.chat_page_layout(frame.area().width, frame.area().height);

    // Reserve 1 column on the right for the scrollbar
    let history_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(history_area);

    // Build height_prefix and visible_turn_ids BEFORE rendering history,
    // so render_history iterates the correct set of visible turns rather
    // than an empty visible_turn_ids on the first frame after session data arrives.
    app.compute_total_height_and_markers();
    // The rebuild above settles content-induced viewport movement (streaming
    // growth, appended turns, undo/redo); re-anchor an in-progress selection's
    // live head to the content now under the cursor so the highlight (and the
    // copy on release) tracks the pointer even when no mouse event arrived —
    // the anchor stays pinned to its text.  See `selection::follow_cursor`.
    selection::follow_cursor(app);
    render_history(frame, history_chunks[0], app);

    // ── Scrollbar ────────────────────────────────────────────
    let viewport_height = app.history_viewport.height as usize;
    let total_height = app.total_history_height();
    if app.scrollbar_visible() {
        let position = app
            .max_scroll_offset()
            .saturating_sub(app.effective_scroll());
        let marker_slots: Vec<usize> = app
            .active_display_ref()
            .map(|d| d.markers.iter().map(|m| m.virtual_slot).collect())
            .unwrap_or_default();
        frame.render_stateful_widget(
            vertical_scrollbar().with_markers(&marker_slots),
            history_chunks[1],
            &mut SmoothScrollbarState::new(total_height)
                .position(position)
                .viewport_content_length(viewport_height),
        );
    }

    // ── Status/error bar (above command box) ──────────────────
    let notify_area = Rect {
        x: status_error_area.x + 1,
        width: status_error_area.width.saturating_sub(2),
        ..status_error_area
    };
    if let Some(ref err) = app.error {
        let err_para = Paragraph::new(Text::from(err.clone()))
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: false });
        frame.render_widget(err_para, notify_area);
    } else if let Some(ref status) = app.status {
        let status_para = Paragraph::new(Text::from(status.clone()))
            .style(Style::default().fg(Color::Green))
            .wrap(Wrap { trim: false });
        frame.render_widget(status_para, notify_area);
    }

    // ── Help overlay (2 lines, conditional) ───────────────────
    if app.show_ctrl_help {
        let help_inner = Rect {
            x: help_area.x + 1,
            width: help_area.width.saturating_sub(2),
            ..help_area
        };
        let help = Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                // The model-selector key differs on legacy terminals (see
                // `App::keyboard_enhanced`) — always advertise the one that
                // works on the terminal the user is actually sitting at.
                if app.keyboard_enhanced {
                    CTRL_HELP_LINE1
                } else {
                    CTRL_HELP_LINE1_LEGACY
                },
                Style::default().fg(Color::Cyan),
            )),
            Line::from(Span::styled(
                CTRL_HELP_LINE2,
                Style::default().fg(Color::Cyan),
            )),
        ]));
        frame.render_widget(help, help_inner);
    }

    // ── Command input box ──────────────────────────────────────
    // Inner width = box width minus INPUT_PAD padding on both sides.
    // The box draws no left/right borders, so padding is the only loss.
    // This must match input_inner_width() used by the height estimation
    // (input_bar_content_lines) or wrapped lines won't grow the box.
    let inner_width = input_inner_width(input_area.width);
    let visible_height = (input_area.height.saturating_sub(2)) as usize;

    // Compute cursor position first (populates the lines cache) so we
    // can then borrow separate fields of app.input for the cached lines.
    let (vrow, vcol) = app.input.cursor_visual_pos(inner_width);

    let all_visual_lines = cached_visual_lines(
        &app.input.text,
        inner_width,
        app.input.generation,
        &mut app.input.lines_cache,
    );

    // Apply scroll offset — only show the visible window.
    let visible_count = visible_height.max(1).min(all_visual_lines.len());
    let offset = app
        .input
        .scroll_offset
        .min(all_visual_lines.len().saturating_sub(visible_count));
    let visible_lines = all_visual_lines
        .get(offset..offset + visible_count)
        .unwrap_or(&[]);
    let text_lines: Vec<Line> = visible_lines
        .iter()
        .map(|vl| {
            Line::from(
                app.input
                    .text
                    .get(vl.start_byte..vl.end_byte)
                    .unwrap_or_default(),
            )
        })
        .collect();

    let input = Paragraph::new(Text::from(text_lines)).block(
        Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .padding(Padding::new(INPUT_PAD, INPUT_PAD, 0, 0)),
    );
    frame.render_widget(input, input_area);
    // Clamp to visible area so the cursor is always inside the box,
    // even when scroll_offset hasn't been adjusted yet (e.g. after
    // loading a long history entry that ends at scroll_offset = 0).
    let max_display_row = (visible_count as u16).saturating_sub(1);
    let display_vrow = vrow.saturating_sub(offset as u16).min(max_display_row);
    let cursor_x = input_area.x.saturating_add(INPUT_PAD).saturating_add(vcol);
    let cursor_y = input_area.y.saturating_add(1).saturating_add(display_vrow);
    frame.set_cursor_position((cursor_x, cursor_y));

    // ── Status bar (single line) ───────────────────────────────
    let has_session = app.attached_session_id.is_some();

    let mut spans: Vec<Span> = Vec::new();
    // PERSISTENT lock banner: when the daemon's keystore is locked, lead the
    // status bar with a lock marker. Driven directly by the latched
    // `keystore_locked` flag (set by the daemon's lock-state broadcasts and
    // the subscribe-time push in `handle_daemon_message`), so it is NOT
    // cleared by the per-keypress transient status/error clear — the lock
    // indication survives every keystroke until the daemon reports unlocked.
    if app.keystore_locked {
        spans.push(Span::styled(
            KEYS_LOCKED_MARKER,
            Style::default().fg(Color::Yellow),
        ));
        spans.push(Span::raw(" | "));
    }

    if has_session {
        // Session-identity values (wd, account, model, reasoning) — stable
        // across the session — go first (left side) so the bar's leading edge
        // stays fixed.  Runtime metrics (tokens, context fill) follow on the
        // right where their per-turn updates don't shift the identity fields.
        let wd = app
            .active_display_ref()
            .and_then(|d| d.working_dir.as_deref())
            .unwrap_or("-");
        let account = app.attached_account_slug.as_deref().unwrap_or("-");
        let model = app
            .active_display_ref()
            .and_then(|d| d.selected_model.as_deref())
            .unwrap_or("-");
        let reasoning = app
            .active_display_ref()
            .and_then(|d| d.reasoning_effort.as_deref())
            .unwrap_or("-");

        // Runtime metrics: tokens flow and context-window fill.
        let tokens = match &app.display_token_usage() {
            Some(usage) => status_token_readout(usage),
            None => String::new(),
        };
        // `?` cleanup: while the keystore is locked the context window is not
        // loaded into memory (credentials are undecrypted), so `last_prompt_tokens`
        // being `Some` alongside a `None` context_window would render a
        // misleading `X / ?`. Render NO context readout at all when locked — the
        // lock banner already reports the state, and there is no sensibly
        // interpretable fill percentage.
        let context = if app.keystore_locked {
            String::new()
        } else {
            match (
                app.active_display_ref().and_then(|d| d.context_window),
                app.active_display_ref().and_then(|d| d.last_prompt_tokens),
            ) {
                (Some(limit), Some(current)) => {
                    let ratio = if limit > 0 {
                        current as f64 / limit as f64
                    } else {
                        0.0
                    };
                    format!(
                        "{} / {} ({})",
                        humfmt::number(current),
                        humfmt::number(limit),
                        humfmt::percent(ratio),
                    )
                }
                (Some(limit), None) => {
                    format!("0 / {} ({})", humfmt::number(limit), humfmt::percent(0.0))
                }
                (None, Some(current)) => format!("{} / ?", humfmt::number(current)),
                (None, None) => String::new(),
            }
        };

        // Tool groups can change at runtime via load_tools/unload_tools.
        let tool_groups = app.attached_tool_groups.join(", ");

        // Order: stable identity first (wd, account, model, reasoning) so
        // the bar doesn't visually jitter when per-turn metrics appear or
        // disappear; tools in the middle; runtime metrics (tokens, context
        // window fill, active status) on the right.
        // ── Session identity (always present, default to "-") ──
        spans.push(Span::raw(" "));
        spans.push(Span::styled(wd, Style::default().fg(Color::White)));
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(account, Style::default().fg(Color::White)));
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(model, Style::default().fg(Color::White)));
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(reasoning, Style::default().fg(Color::White)));

        // ── Tool groups (conditionally present) ──
        if !tool_groups.is_empty() {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(
                tool_groups,
                Style::default().fg(Color::DarkGray),
            ));
        }

        // ── Runtime metrics (tokens → context → status) ──
        if !tokens.is_empty() {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(tokens, Style::default().fg(Color::White)));
        }
        if !context.is_empty() {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(context, Style::default().fg(Color::White)));
        }
        if let Some(status) = &app.attached_status {
            let (label, color) = status_display(status);
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(label, Style::default().fg(color)));
        }
    }

    let status_bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Reset));
    frame.render_widget(status_bar, status_bar_area);
}

fn render_history(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Text-wrap budgets: message blocks lose 9 columns of chrome (a 2-col
    // margin + `┃` gutter + 2-col shading on the left; 2 shaded + 2 blank on
    // the right) and tool rows lose 1 (their single right-margin column).
    // These must stay in lockstep with add_margin_lines / the tool-result
    // row loop in markdown_render.rs so rows fill exactly the viewport width.
    let content_width = area.width.saturating_sub(9);
    let tool_content_width = area.width.saturating_sub(1);

    app.ensure_cache_synced();

    let mut rows_remaining = area.height as usize;
    let mut y = area.y + area.height;
    let mut rows_to_skip = app.effective_scroll();

    // Clone visible turn IDs upfront to avoid borrow conflicts when
    // accessing display state via app.display_for inside the loop.
    let session_id = match app.active_session_id {
        Some(sid) => sid,
        None => return,
    };
    let visible_turn_ids: Vec<u32> = app.display_for(session_id).visible_turn_ids.clone();
    let len = visible_turn_ids.len();

    // Iterate visible turns from newest to oldest.  clipped_area consumes
    // rows_to_skip from the bottom (newest end) so that turns fully below
    // the viewport are skipped before any content is rendered.
    for raw_i in 0..len {
        let i = len - 1 - raw_i;
        let turn_id = visible_turn_ids[i];

        if rows_remaining == 0 {
            break;
        }

        // Get cached lines (Arc clone is O(1)), the pre-computed height,
        // cumulative visual-row offsets for O(log n) row→line lookups, and
        // the per-line content column ranges (for selection clamping).
        let (text_lines_arc, text_height, text_offsets, content_ranges, img_count) = {
            let display = app.display_for(session_id);
            let Some(turn) = display.view.turns.get(&turn_id) else {
                continue;
            };
            if turn.undone {
                continue;
            }
            let count = turn.displayed_images.len();
            // Effective reasoning visibility: explicit override (header click)
            // wins, else the streaming-derived default.  The default is read
            // from the precomputed turn layout — rebuilt in lockstep with
            // `visible_turn_ids` before every render — keeping this per-frame
            // path free of string scanning; the trim-based derivation is only
            // a defensive fallback for a missing layout.
            let reasoning_expanded = {
                let default = display
                    .turn_layouts
                    .get(i)
                    .map(|l| l.reasoning_default_expanded)
                    .unwrap_or_else(|| reasoning_expanded_default(turn));
                display.effective_reasoning_expanded(turn_id, default)
            };
            // Effective per-result collapse state (aligned with
            // `turn.tool_results`), part of the render-cache key like
            // `reasoning_expanded`.  Built only for turns that actually
            // have tool results — the common case allocates nothing and the
            // key comparison short-circuits on the empty slice.
            let tool_results_collapsed: Vec<bool> = if turn.tool_results.is_empty() {
                Vec::new()
            } else {
                turn.tool_results
                    .iter()
                    .map(|r| display.effective_tool_result_collapsed(turn_id, r))
                    .collect()
            };
            let key = RenderCacheKey {
                turn_id,
                width: content_width,
                viewport_width: area.width,
                reasoning_expanded,
                tool_results_collapsed,
                content_version: display.turn_content_version(turn_id),
            };
            let rendered = cached_or_compute_lines(&mut display.render_cache, i, &key, || {
                render_turn_lines(
                    turn,
                    content_width,
                    tool_content_width,
                    key.reasoning_expanded,
                    &key.tool_results_collapsed,
                )
            });
            (
                rendered.lines,
                rendered.height,
                rendered.visual_offsets,
                rendered.content_ranges,
                count,
            )
        };

        // ── Images (rendered first so they sit below text) ──
        let full_img_height = app.image_block_height() as usize;
        for img_idx in (0..img_count).rev() {
            if let Some((_top_line, visible_height)) = clipped_area(
                full_img_height,
                &mut rows_to_skip,
                &mut rows_remaining,
                &mut y,
            ) {
                let fully_visible = visible_height >= full_img_height;
                let img_rect = Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: visible_height as u16,
                };
                render_turn_image(
                    frame,
                    img_rect,
                    session_id,
                    turn_id,
                    img_idx,
                    app,
                    fully_visible,
                );
            }
        }

        // ── Text content (render above images) ──
        if let Some((top_line, visible_height)) =
            clipped_area(text_height, &mut rows_to_skip, &mut rows_remaining, &mut y)
        {
            // Clone only the visible slice from the Arc — O(visible_lines)
            // instead of O(total_lines_in_turn).
            // Binary-search the precomputed cumulative offsets to find which
            // semantic lines the visible visual rows span — O(log n).
            let row_start = top_line;
            let row_end = top_line + visible_height;
            let line_start = text_offsets.partition_point(|&o| o <= row_start);
            let line_end = text_offsets.partition_point(|&o| o <= row_end);
            let mut visible_lines = text_lines_arc[line_start..line_end].to_vec();
            // Apply the in-progress text-selection highlight to the visible
            // slice at draw time — the render cache stays pure, and the same
            // cached lines drive both the highlight and the copy, so what is
            // highlighted is exactly what gets copied.  `i` is the
            // visible-turn index; the turn's first content row is
            // `height_prefix[i-1]` (0 for the first turn).
            let turn_start = i
                .checked_sub(1)
                .and_then(|prev| {
                    app.active_display_ref()
                        .and_then(|d| d.height_prefix.get(prev))
                        .copied()
                })
                .unwrap_or(0);
            selection::apply_selection_to_lines(
                app,
                turn_start,
                &text_offsets[..],
                &content_ranges[..],
                line_start,
                &mut visible_lines,
            );
            render_text_block(
                frame,
                area,
                visible_lines,
                visible_height,
                &mut y,
                Style::default(),
            );
        }
    }
}

/// Return the cached rendered lines (as an `Arc` slice for O(1) sharing) and
/// their pre-computed height for a turn at the given cache index, or compute,
/// cache, and return them.
///
/// On cache hit the height is returned from the cache (avoids re-computing
/// `lines_height`, which iterates every line).  On cache miss the height is
/// computed inline and stored alongside the lines.
///
fn clipped_area(
    full_height: usize,
    rows_to_skip: &mut usize,
    rows_remaining: &mut usize,
    y: &mut u16,
) -> Option<(usize, usize)> {
    if *rows_to_skip >= full_height {
        *rows_to_skip -= full_height;
        return None;
    }

    let visible_height = (full_height.saturating_sub(*rows_to_skip)).min(*rows_remaining);
    if visible_height == 0 {
        return None;
    }

    let bottom_line = full_height.saturating_sub(*rows_to_skip);
    let top_line = bottom_line.saturating_sub(visible_height);

    *y = (*y).saturating_sub(visible_height as u16);
    *rows_remaining -= visible_height;
    *rows_to_skip = 0;

    Some((top_line, visible_height))
}

/// Render a text block into the given area with wrapping.
///
/// `lines` must already be clipped to the visible portion (no `scroll` offset
/// is applied since the slice starts at the correct position).
fn render_text_block(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: Vec<Line<'static>>,
    visible_height: usize,
    y: &mut u16,
    paragraph_style: Style,
) {
    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height: visible_height as u16,
    };

    frame.render_widget(
        Paragraph::new(lines).style(paragraph_style).scroll((0, 0)),
        rect,
    );
}

/// Render a single turn-displayed image block.
///
/// Height is always `image_block_height()` regardless of encoding state,
/// so scroll positions remain stable.
///
/// When `fully_visible` is true the image is centered within its block
/// using the protocol's actual rendered dimensions (via `size_for`). When
/// only a slice of the block is visible it is rendered without centering
/// to prevent visual reflow during scrolling.
fn render_turn_image(
    frame: &mut Frame<'_>,
    area: Rect,
    session_id: u64,
    turn_id: u32,
    img_idx: usize,
    app: &mut App,
    fully_visible: bool,
) {
    let inline_size = Size::new(area.width, app.image_block_height());

    // Extract data we need while the borrow is active.
    let (needs_job, data, meta) = match app
        .rendered_images
        .get_mut(&session_id)
        .and_then(|imgs| imgs.get_mut(&turn_id))
        .and_then(|images| images.get_mut(&img_idx))
    {
        Some(img) => {
            if let Some(protocol) = img.protocols.get_mut(&inline_size) {
                let title = format!(
                    "image {} ({} {}x{})",
                    turn_id, img.metadata.mime_type, img.metadata.width, img.metadata.height,
                );
                let block = Block::default().title(title);
                let inner = block.inner(area);
                frame.render_widget(block, area);
                if fully_visible {
                    // Center the image within the block using the protocol's actual
                    // rendered dimensions, preventing visual reflow when only part
                    // of the block is visible.
                    let rendered_at = protocol.size_for(crate::IMAGE_RESIZE, inline_size);
                    let centered = Rect {
                        x: inner.x + (inner.width.saturating_sub(rendered_at.width)) / 2,
                        y: inner.y + (inner.height.saturating_sub(rendered_at.height)) / 2,
                        width: rendered_at.width.min(inner.width),
                        height: rendered_at.height.min(inner.height),
                    };
                    frame.render_stateful_widget(
                        StatefulImage::new().resize(crate::IMAGE_RESIZE),
                        centered,
                        protocol,
                    );
                }
                return;
            }
            (
                img.pending_job.is_none()
                    && !img.failed_sizes.contains(&inline_size)
                    && !img.protocols.contains_key(&inline_size),
                img.data.clone(),
                img.metadata.clone(),
            )
        }
        None => {
            let block = Block::default().title(format!("image {turn_id}[{img_idx}] (pending)"));
            frame.render_widget(block, area);
            return;
        }
    };

    if needs_job {
        app.submit_image_job(
            session_id,
            turn_id,
            img_idx,
            data,
            meta.clone(),
            inline_size,
            crate::IMAGE_RESIZE,
        );
    }

    // Render placeholder frame while encoding is pending.
    // Use metadata from the RenderedImage entry (already populated by
    // sync_turn_images), not from the session turns.
    let placeholder_title = format!(
        "image {} ({} {}x{})",
        turn_id, meta.mime_type, meta.width, meta.height,
    );
    let block = Block::default().title(placeholder_title);
    frame.render_widget(block, area);
}

/// Format a Unix-epoch-milliseconds timestamp as simplified absolute time.
///
/// - today → "14:32" (time only)
/// - this calendar year → "Mar 5"
/// - older → "Mar 5 2024"
pub(crate) fn format_timestamp(ts_ms: i64) -> String {
    if ts_ms <= 0 {
        return "-".to_string();
    }

    use chrono::{Datelike, Local, TimeZone};

    let dt = match Local.timestamp_millis_opt(ts_ms) {
        chrono::LocalResult::Single(dt) => dt,
        _ => return "-".to_string(),
    };

    let now = Local::now();
    if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else if dt.year() == now.year() {
        dt.format("%b %d").to_string()
    } else {
        dt.format("%b %d %Y").to_string()
    }
}

fn set_input_cursor(
    frame: &mut Frame<'_>,
    area: Rect,
    line: u16,
    prefix_width: u16,
    text_before_cursor: &str,
) {
    let x = area.x + prefix_width + display_width(text_before_cursor) as u16;
    let y = area.y + line;
    frame.set_cursor_position((x, y));
}

pub(crate) fn status_display(status: &SessionStatus) -> (String, Color) {
    match status {
        SessionStatus::Sleeping => ("sleeping".into(), Color::DarkGray),
        SessionStatus::Inactive => ("idle".into(), Color::Green),
        SessionStatus::Inference => ("inferring".into(), Color::Yellow),
        SessionStatus::ToolCall(name) => (format!("tool call: {name}"), Color::Cyan),
        SessionStatus::Retrying {
            attempt,
            max_attempts,
            delay_ms,
        } => (
            format!(
                "retrying ({attempt}/{max_attempts}, {})",
                humfmt::duration(std::time::Duration::from_millis(*delay_ms)),
            ),
            Color::Magenta,
        ),
        _ => ("unknown".into(), Color::White),
    }
}

pub(crate) fn format_status(status: &SessionStatus) -> String {
    status_display(status).0
}

/// The status bar's cumulative token readout, e.g. `↑15.3K ↓1.2K`.
///
/// Both counters pass through humfmt's compact number formatter so the
/// readout stays consistent with the context-window fill rendered beside it
/// (which already uses `humfmt::number`/`humfmt::percent`): small sessions
/// (< 1_000 tokens) render verbatim, large ones get K/M suffixes.
pub(crate) fn status_token_readout(usage: &TokenUsage) -> String {
    format!(
        "↑{} ↓{}",
        humfmt::number(usage.input_tokens),
        humfmt::number(usage.output_tokens),
    )
}

/// The session-detail "Tokens:" line, e.g.
/// `Tokens:        15.3K in / 1.2K out (16.5K total)`.
///
/// Same humfmt treatment as the status bar's readout so the two token
/// surfaces agree; the `Tokens:        ` label keeps the column aligned with
/// its neighbours (`Working Dir:`, `Turn Count:`, …).
pub(crate) fn session_detail_tokens_line(usage: &TokenUsage) -> String {
    format!(
        "Tokens:        {} in / {} out ({} total)",
        humfmt::number(usage.input_tokens),
        humfmt::number(usage.output_tokens),
        humfmt::number(usage.total_tokens),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown_render::{
        LineJoin, RenderedTurnLines, compute_visual_offsets, lines_height,
    };
    use crate::state::{RenderCacheKey, RenderedCache, RenderedTurn};

    /// Wrap a line vector as a rendered turn with no headers.
    fn rendered(lines: Vec<Line<'static>>) -> RenderedTurnLines {
        // Every line gets a full-width content range so the cache-alignment
        // invariant asserted in `cached_or_compute_lines` holds for test
        // fixtures too (each entry must align with `lines`).
        let joins = lines.iter().map(|_| LineJoin::Break).collect();
        let content_ranges = lines.iter().map(|l| Some((0, l.width()))).collect();
        RenderedTurnLines {
            lines,
            joins,
            content_ranges,
            reasoning_header_idx: None,
            tool_result_header_idxs: Vec::new(),
        }
    }

    /// Build a cache entry with the given key/output pieces, defaulting the
    /// rest, so tests can focus on the field under test.
    fn cache_entry(
        key: RenderCacheKey,
        lines: Arc<[Line<'static>]>,
        height: usize,
        visual_offsets: Arc<[usize]>,
    ) -> RenderedCache {
        // Derive the copy-join and content-range metadata from the lines so
        // fixtures built through this helper keep the parallel-array
        // alignment invariant the selection machinery relies on (every
        // renderer-produced entry and the debug_asserts in
        // `cached_or_compute_lines` expect lines/joins/content_ranges to
        // have identical lengths).
        let joins = lines.iter().map(|_| LineJoin::Break).collect::<Vec<_>>();
        let content_ranges = lines
            .iter()
            .map(|l| Some((0, l.width())))
            .collect::<Vec<_>>();
        RenderedCache {
            key,
            rendered: RenderedTurn {
                lines,
                height,
                visual_offsets,
                joins: Arc::from(joins),
                content_ranges: Arc::from(content_ranges),
                reasoning_header_idx: None,
                tool_result_header_idxs: Vec::new(),
            },
        }
    }

    /// A key for a one-line entry rendered at content width 80 in a
    /// viewport of width 100, reasoning collapsed, no tool results, at
    /// content version 0 (no mutations recorded).
    fn base_key() -> RenderCacheKey {
        RenderCacheKey {
            turn_id: 0,
            width: 80,
            viewport_width: 100,
            reasoning_expanded: false,
            tool_results_collapsed: Vec::new(),
            content_version: 0,
        }
    }

    // ── mouse_in_history_box ──

    #[test]
    fn mouse_in_history_box_inside() {
        assert!(mouse_in_history_box(5, 10, 80, 24));
    }

    #[test]
    fn mouse_in_history_box_column_too_large() {
        assert!(!mouse_in_history_box(80, 10, 80, 24));
    }

    #[test]
    fn mouse_in_history_box_row_too_large() {
        assert!(!mouse_in_history_box(5, 24, 80, 24));
    }

    #[test]
    fn mouse_in_history_box_both_out_of_bounds() {
        assert!(!mouse_in_history_box(99, 99, 80, 24));
    }

    #[test]
    fn mouse_in_history_box_zero_height_viewport() {
        assert!(!mouse_in_history_box(0, 0, 80, 0));
    }

    #[test]
    fn mouse_in_history_box_zero_width_viewport() {
        assert!(!mouse_in_history_box(0, 0, 0, 24));
    }

    // ── mouse_in_scrollbar_column ──

    #[test]
    fn mouse_in_scrollbar_column_on_scrollbar() {
        assert!(mouse_in_scrollbar_column(80, 10, 80, 24));
    }

    #[test]
    fn mouse_in_scrollbar_column_before_scrollbar() {
        assert!(!mouse_in_scrollbar_column(79, 10, 80, 24));
    }

    #[test]
    fn mouse_in_scrollbar_column_after_scrollbar() {
        assert!(!mouse_in_scrollbar_column(81, 10, 80, 24));
    }

    #[test]
    fn mouse_in_scrollbar_column_row_too_large() {
        assert!(!mouse_in_scrollbar_column(80, 24, 80, 24));
    }

    #[test]
    fn mouse_in_scrollbar_column_zero_height() {
        assert!(!mouse_in_scrollbar_column(80, 0, 80, 0));
    }

    // ── clipped_area ──

    #[test]
    fn clipped_area_skip_when_rows_to_skip_equals_full_height() {
        let mut skip = 10usize;
        let mut remain = 20usize;
        let mut y = 30u16;
        let result = clipped_area(10, &mut skip, &mut remain, &mut y);
        assert!(
            result.is_none(),
            "should skip when rows_to_skip >= full_height"
        );
        assert_eq!(skip, 0, "rows_to_skip should be decremented by full_height");
        assert_eq!(remain, 20, "rows_remaining should be unchanged");
        assert_eq!(y, 30, "y should be unchanged");
    }

    #[test]
    fn clipped_area_skip_when_rows_to_skip_exceeds_full_height() {
        let mut skip = 15usize;
        let mut remain = 20usize;
        let mut y = 30u16;
        let result = clipped_area(10, &mut skip, &mut remain, &mut y);
        assert!(
            result.is_none(),
            "should skip when rows_to_skip > full_height"
        );
        assert_eq!(skip, 5, "rows_to_skip should be decremented by full_height");
    }

    #[test]
    fn clipped_area_partial_visibility_at_boundary() {
        let mut skip = 7usize;
        let mut remain = 20usize;
        let mut y = 30u16;
        let result = clipped_area(10, &mut skip, &mut remain, &mut y);
        let (top_line, visible_height) = result.expect("should be visible");
        // bottom_line = 10 - 7 = 3, top_line = 3 - 3 = 0, visible = 3
        assert_eq!(
            top_line, 0,
            "top_line should be at start of non-skipped region"
        );
        assert_eq!(visible_height, 3, "should show remaining 3 lines");
        assert_eq!(skip, 0, "rows_to_skip should be reset to 0");
        assert_eq!(
            remain, 17,
            "rows_remaining should decrease by visible_height"
        );
        assert_eq!(y, 27, "y should decrease by visible_height");
    }

    #[test]
    fn clipped_area_partial_visibility_skip_some_rows_within_turn() {
        let mut skip = 3usize;
        let mut remain = 10usize;
        let mut y = 50u16;
        let result = clipped_area(10, &mut skip, &mut remain, &mut y);
        let (top_line, visible_height) = result.expect("should be visible");
        // bottom_line = 10 - 3 = 7, top_line = 7 - 7 = 0, visible = 7...
        // Wait: visible_height = min(10-3, 10) = 7, bottom_line = 10-3 = 7, top_line = 7-7 = 0
        assert_eq!(top_line, 0);
        assert_eq!(visible_height, 7);
        assert_eq!(skip, 0);
        assert_eq!(remain, 3);
        assert_eq!(y, 43);
    }

    #[test]
    fn clipped_area_full_turn_within_viewport() {
        let mut skip = 0usize;
        let mut remain = 20usize;
        let mut y = 40u16;
        let result = clipped_area(10, &mut skip, &mut remain, &mut y);
        let (top_line, visible_height) = result.expect("should show all");
        assert_eq!(top_line, 0, "top_line should be 0 when nothing is skipped");
        assert_eq!(visible_height, 10, "should show full height");
        assert_eq!(y, 30, "y should decrease by full height");
    }

    #[test]
    fn clipped_area_clamps_to_rows_remaining() {
        let mut skip = 0usize;
        let mut remain = 3usize;
        let mut y = 20u16;
        let result = clipped_area(10, &mut skip, &mut remain, &mut y);
        let (top_line, visible_height) = result.expect("should clip to remaining");
        // visible_height = min(10, 3) = 3
        // bottom_line = 10 - 0 = 10, top_line = 10 - 3 = 7
        assert_eq!(
            top_line, 7,
            "top_line should be offset from bottom by visible_height"
        );
        assert_eq!(visible_height, 3, "should be clamped by rows_remaining");
        assert_eq!(remain, 0);
        assert_eq!(y, 17);
    }

    #[test]
    fn clipped_area_zero_rows_remaining_returns_none() {
        let mut skip = 0usize;
        let mut remain = 0usize;
        let mut y = 10u16;
        let result = clipped_area(10, &mut skip, &mut remain, &mut y);
        assert!(result.is_none(), "should return None when no rows remain");
        assert_eq!(y, 10, "y should be unchanged");
    }

    #[test]
    fn clipped_area_skip_exactly_full_height_then_show_next() {
        let mut skip = 6usize;
        let mut remain = 10usize;
        let mut y = 30u16;
        // First turn: full height = 6, rows_to_skip = 6 → skip entirely
        let result1 = clipped_area(6, &mut skip, &mut remain, &mut y);
        assert!(result1.is_none());
        assert_eq!(skip, 0);
        // Second turn: full height = 4, rows_to_skip = 0 → show fully
        let result2 = clipped_area(4, &mut skip, &mut remain, &mut y);
        let (_, visible) = result2.unwrap();
        assert_eq!(visible, 4);
        assert_eq!(y, 26);
    }

    // ── cached_or_compute_lines ──

    /// Helper: a simple compute function that returns a single short line.
    fn compute_one_line() -> RenderedTurnLines {
        rendered(vec![Line::from("hello")])
    }

    #[test]
    fn cached_or_compute_lines_cache_miss_stores_result() {
        let mut cache = vec![None];
        let rendered = cached_or_compute_lines(&mut cache, 0, &base_key(), compute_one_line);
        assert_eq!(rendered.lines.len(), 1, "should return computed lines");
        assert_eq!(
            rendered.height, 1,
            "single line at any viewport width has height 1"
        );
        assert_eq!(
            &*rendered.visual_offsets,
            &[1],
            "single short line should occupy one visual row"
        );
        assert_eq!(
            rendered.reasoning_header_idx, None,
            "no reasoning → no header index"
        );
        // Cache should be filled
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(cached.key.width, 80);
        assert_eq!(cached.key.viewport_width, 100);
        assert_eq!(cached.rendered.height, 1);
        assert_eq!(cached.rendered.lines.len(), 1);
        assert_eq!(&*cached.rendered.visual_offsets, &[1]);
        assert!(
            !cached.key.reasoning_expanded,
            "cache should record the reasoning state it was rendered with"
        );
    }

    #[test]
    fn cached_or_compute_lines_cache_hit_returns_stored_height() {
        let mut cache = vec![Some(cache_entry(
            base_key(),
            Arc::from(vec![Line::from("cached")]),
            42,
            Arc::from([99]),
        ))];
        // Cache hit — should return stored height without recomputing
        let rendered = cached_or_compute_lines(&mut cache, 0, &base_key(), || {
            panic!("should not be called on cache hit")
        });
        assert_eq!(rendered.height, 42, "should return cached height");
        assert_eq!(rendered.lines.len(), 1, "should return cached lines");
        assert_eq!(rendered.lines[0], Line::from("cached"));
        assert_eq!(
            &*rendered.visual_offsets,
            &[99],
            "should return cached offsets"
        );
        assert_eq!(
            rendered.reasoning_header_idx, None,
            "should return the cached header index"
        );
    }

    #[test]
    fn cached_or_compute_lines_cache_hit_arc_shares_allocation() {
        let stored = Arc::from(vec![Line::from("shared")]);
        let stored_ptr = Arc::as_ptr(&stored);
        let mut cache = vec![Some(cache_entry(base_key(), stored, 7, Arc::from([1])))];
        let returned = cached_or_compute_lines(&mut cache, 0, &base_key(), || {
            panic!("should not recompute")
        });
        assert_eq!(
            Arc::as_ptr(&returned.lines),
            stored_ptr,
            "returned Arc should point to the same allocation as cache entry"
        );
    }

    #[test]
    fn cached_or_compute_lines_width_mismatch_recomputes() {
        let mut cache = vec![Some(cache_entry(
            RenderCacheKey {
                width: 40, // different from requested width 80
                ..base_key()
            },
            Arc::from(vec![Line::from("stale")]),
            99,
            Arc::from([1]),
        ))];
        let compute_called = std::cell::Cell::new(false);
        let rendered = cached_or_compute_lines(&mut cache, 0, &base_key(), || {
            compute_called.set(true);
            rendered(vec![Line::from("fresh")])
        });
        assert!(compute_called.get(), "should recompute on width mismatch");
        assert_eq!(rendered.lines[0], Line::from("fresh"));
        // Height of a single "fresh" line at viewport width 100 is 1
        assert_eq!(rendered.height, 1);
        assert_eq!(
            &*rendered.visual_offsets,
            &[1],
            "offsets recomputed for fresh lines"
        );
        // Cache should be updated
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(cached.key.width, 80);
        assert_eq!(cached.key.viewport_width, 100);
        assert_eq!(cached.rendered.lines[0], Line::from("fresh"));
        assert_eq!(&*cached.rendered.visual_offsets, &[1]);
    }

    #[test]
    fn cached_or_compute_lines_viewport_width_mismatch_recomputes() {
        let mut cache = vec![Some(cache_entry(
            RenderCacheKey {
                viewport_width: 40, // different from requested viewport_width 100
                ..base_key()
            },
            Arc::from(vec![Line::from("stale")]),
            99,
            Arc::from([1]),
        ))];
        let compute_called = std::cell::Cell::new(false);
        let rendered = cached_or_compute_lines(&mut cache, 0, &base_key(), || {
            compute_called.set(true);
            rendered(vec![Line::from("fresh")])
        });
        assert!(
            compute_called.get(),
            "should recompute on viewport_width mismatch"
        );
        assert_eq!(rendered.lines[0], Line::from("fresh"));
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(cached.key.viewport_width, 100);
    }

    #[test]
    fn cached_or_compute_lines_turn_id_mismatch_recomputes() {
        let mut cache = vec![Some(cache_entry(
            RenderCacheKey {
                turn_id: 7, // cached entry is for turn 7
                ..base_key()
            },
            Arc::from(vec![Line::from("stale")]),
            99,
            Arc::from([1]),
        ))];
        // Request turn_id 42 at the same index — should be a miss.
        let compute_called = std::cell::Cell::new(false);
        let rendered = cached_or_compute_lines(
            &mut cache,
            0,
            &RenderCacheKey {
                turn_id: 42,
                ..base_key()
            },
            || {
                compute_called.set(true);
                rendered(vec![Line::from("fresh")])
            },
        );
        assert!(compute_called.get(), "should recompute on turn_id mismatch");
        assert_eq!(rendered.lines[0], Line::from("fresh"));
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(
            cached.key.turn_id, 42,
            "cache entry should be updated to new turn_id"
        );
    }

    #[test]
    fn cached_or_compute_lines_out_of_range_index_does_not_store() {
        let mut cache = vec![None]; // length 1
        // Request index 5 which is out of range
        let rendered = cached_or_compute_lines(&mut cache, 5, &base_key(), compute_one_line);
        assert_eq!(
            rendered.lines.len(),
            1,
            "should still return computed result"
        );
        assert_eq!(rendered.height, 1);
        assert_eq!(&*rendered.visual_offsets, &[1]);
        // Cache should remain unchanged (all entries still None since index 5 doesn't exist)
        assert!(
            cache[0].is_none(),
            "original cache entry should be untouched"
        );
    }

    #[test]
    fn cached_or_compute_lines_height_matches_lines_height() {
        let mut cache = vec![None];
        let lines = vec![
            Line::from("line one"),
            Line::from("line two"),
            Line::from("line three"),
        ];
        let expected_h = lines_height(&lines, 80);
        let rendered = cached_or_compute_lines(
            &mut cache,
            0,
            &RenderCacheKey {
                width: 70, // content_width
                viewport_width: 80,
                ..base_key()
            },
            || rendered(lines.clone()),
        );
        assert_eq!(
            rendered.height, expected_h,
            "returned height should match lines_height"
        );
        assert_eq!(
            *rendered.visual_offsets.last().unwrap(),
            expected_h,
            "last offset should equal total visual height"
        );
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(
            cached.rendered.height, expected_h,
            "stored height should match"
        );
        assert_eq!(
            *cached.rendered.visual_offsets.last().unwrap(),
            expected_h,
            "stored last offset should equal total height"
        );
    }

    #[test]
    fn cached_or_compute_lines_none_slot_treated_as_miss() {
        let mut cache: Vec<Option<RenderedCache>> = vec![None];
        let compute_called = std::cell::Cell::new(false);
        let rendered = cached_or_compute_lines(&mut cache, 0, &base_key(), || {
            compute_called.set(true);
            compute_one_line()
        });
        assert!(compute_called.get(), "should compute when slot is None");
        assert!(cache[0].is_some(), "should fill the slot");
        drop(rendered.lines);
    }

    #[test]
    fn cached_or_compute_lines_reasoning_expanded_mismatch_recomputes() {
        let mut cache = vec![Some(cache_entry(
            RenderCacheKey {
                reasoning_expanded: false, // cached as collapsed
                ..base_key()
            },
            Arc::from(vec![Line::from("stale")]),
            99,
            Arc::from([1]),
        ))];
        // Request with reasoning expanded — should be a miss.
        let compute_called = std::cell::Cell::new(false);
        let rendered = cached_or_compute_lines(
            &mut cache,
            0,
            &RenderCacheKey {
                reasoning_expanded: true,
                ..base_key()
            },
            || {
                compute_called.set(true);
                rendered(vec![Line::from("fresh")])
            },
        );
        assert!(
            compute_called.get(),
            "should recompute when reasoning_expanded differs"
        );
        assert_eq!(rendered.lines[0], Line::from("fresh"));
        let cached = cache[0].as_ref().unwrap();
        assert!(
            cached.key.reasoning_expanded,
            "cache entry should record the new reasoning state"
        );
    }

    #[test]
    fn cached_or_compute_lines_tool_results_collapsed_mismatch_recomputes() {
        let mut cache = vec![Some(RenderedCache {
            key: RenderCacheKey {
                tool_results_collapsed: vec![true], // cached as collapsed
                ..base_key()
            },
            rendered: RenderedTurn {
                lines: Arc::from(vec![Line::from("stale")]),
                height: 99,
                visual_offsets: Arc::from([1]),
                joins: Arc::from([LineJoin::Break]),
                content_ranges: Arc::from([Some((0, 5))]),
                reasoning_header_idx: None,
                tool_result_header_idxs: vec![0],
            },
        })];
        // Request with the result expanded — should be a miss.
        let compute_called = std::cell::Cell::new(false);
        let rendered = cached_or_compute_lines(
            &mut cache,
            0,
            &RenderCacheKey {
                tool_results_collapsed: vec![false],
                ..base_key()
            },
            || {
                compute_called.set(true);
                rendered(vec![Line::from("fresh")])
            },
        );
        assert!(
            compute_called.get(),
            "should recompute when tool_results_collapsed differs"
        );
        assert_eq!(rendered.lines[0], Line::from("fresh"));
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(
            cached.key.tool_results_collapsed,
            vec![false],
            "cache entry should record the new collapse state"
        );
    }

    #[test]
    fn cached_or_compute_lines_content_version_mismatch_recomputes() {
        // The cache key carries a per-turn content version so a rebuild can
        // never reuse a rendering of a turn whose content grew behind the
        // key's other fields (a tool-result chunk appended between a rebuild
        // and the streaming fast path).  Same turn/widths/collapse state, but
        // a newer content version — must be a miss.
        let mut cache = vec![Some(cache_entry(
            RenderCacheKey {
                content_version: 1, // cached from an earlier chunk
                ..base_key()
            },
            Arc::from(vec![Line::from("stale")]),
            99,
            Arc::from([1]),
        ))];
        // Request with a bumped content version — should be a miss.
        let compute_called = std::cell::Cell::new(false);
        let rendered = cached_or_compute_lines(
            &mut cache,
            0,
            &RenderCacheKey {
                content_version: 2,
                ..base_key()
            },
            || {
                compute_called.set(true);
                rendered(vec![Line::from("fresh")])
            },
        );
        assert!(
            compute_called.get(),
            "should recompute when content_version differs"
        );
        assert_eq!(rendered.lines[0], Line::from("fresh"));
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(
            cached.key.content_version, 2,
            "cache entry should record the new content version"
        );
    }

    // ── compute_visual_offsets ─────────────────────────────────

    #[test]
    fn compute_visual_offsets_single_line_fits() {
        let lines = vec![Line::from("hello")];
        let offsets = compute_visual_offsets(&lines, 80);
        assert_eq!(&*offsets, &[1], "short line at wide viewport = 1 row");
    }

    #[test]
    fn compute_visual_offsets_single_line_wraps() {
        let long = "a".repeat(200);
        let lines = vec![Line::from(long)];
        let offsets = compute_visual_offsets(&lines, 80);
        assert_eq!(&*offsets, &[3], "200 chars at 80-wide wraps to 3 rows");
    }

    #[test]
    fn compute_visual_offsets_empty_lines_count_as_one_row_each() {
        let lines = vec![Line::from(""), Line::from(""), Line::from("")];
        let offsets = compute_visual_offsets(&lines, 80);
        assert_eq!(&*offsets, &[1, 2, 3], "each empty line = 1 visual row");
    }

    #[test]
    fn compute_visual_offsets_mixed_lines() {
        let lines = vec![
            Line::from("short"),
            Line::from(""),              // 1 row
            Line::from("x".repeat(150)), // 2 rows at 80
        ];
        let offsets = compute_visual_offsets(&lines, 80);
        assert_eq!(&*offsets, &[1, 2, 4]);
    }

    #[test]
    fn compute_visual_offsets_empty_slice() {
        let lines: Vec<Line<'static>> = vec![];
        let offsets = compute_visual_offsets(&lines, 80);
        assert!(offsets.is_empty(), "no lines → no offsets");
    }

    #[test]
    fn compute_visual_offsets_zero_width_each_line_zero() {
        let lines = vec![Line::from("hello"), Line::from("world")];
        let offsets = compute_visual_offsets(&lines, 0);
        // At width 0 every line contributes 0 visual rows, so each
        // cumulative entry stays 0 (same length as lines).
        assert_eq!(&*offsets, &[0, 0], "zero width → each entry = 0");
    }

    // ── partition_point mapping (visual row → line index) ──────

    #[test]
    fn partition_point_finds_line_at_row_zero() {
        let offsets = [2, 5, 7];
        assert_eq!(offsets.partition_point(|&o| o == 0), 0);
    }

    #[test]
    fn partition_point_finds_line_in_middle() {
        let offsets = [2, 5, 7];
        // row 3 falls in the second line (offset 2 < 3, offset 5 > 3)
        assert_eq!(offsets.partition_point(|&o| o <= 3), 1);
    }

    #[test]
    fn partition_point_finds_line_at_exact_boundary() {
        let offsets = [2, 5, 7];
        // row 2 is the last visual row of line 0 — still maps to line 0
        assert_eq!(offsets.partition_point(|&o| o <= 2), 1);
        // row 5 maps to line 2
        assert_eq!(offsets.partition_point(|&o| o <= 5), 2);
    }

    #[test]
    fn partition_point_past_end_returns_len() {
        let offsets = [2, 5, 7];
        assert_eq!(offsets.partition_point(|&o| o <= 7), 3);
        assert_eq!(offsets.partition_point(|&o| o <= 99), 3);
    }

    #[test]
    fn partition_point_empty_offsets_returns_zero() {
        let offsets: [usize; 0] = [];
        assert_eq!(offsets.partition_point(|&o| o == 0), 0);
        assert_eq!(offsets.partition_point(|&o| o <= 99), 0);
    }
}
