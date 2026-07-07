//! The Workcat Map dock panel, v1: read, focus, drag, set status
//! (DR-005 ruling 17) against the workcat-db repo-as-database.
//!
//! Dragging is direct manipulation: mouse down on a node starts a
//! gesture, window-level mouse events move it, and mouse up commits: a
//! `node_moved` event is appended to the db's event log immediately
//! (the fine grain), while git commit/push happens at checkpoints (the
//! coarse grain, DR-006): on every status change and on the manual
//! Checkpoint action.

use std::collections::HashMap;

use anyhow::Result;
use futures::StreamExt as _;
use gpui::{
    App, AsyncWindowContext, Context, DismissEvent, DispatchPhase, Entity, EventEmitter,
    FocusHandle, Focusable, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PathBuilder, Pixels, Point, SharedString, Subscription, Task, WeakEntity, Window, actions,
    anchored, canvas, deferred, point, px,
};
use ui::{ContextMenu, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::geometry;
use crate::model::{ALL_STATUSES, ItemMeta, Status};
use crate::store;

actions!(
    workcat_map,
    [
        /// Toggles focus on the workcat map panel.
        ToggleFocus,
        /// Commits and pushes the session's pending events (write lock,
        /// fold, commit, push).
        Checkpoint,
        /// Clears the focused node.
        ClearFocus,
    ]
);

/// Sets the focused item's status. Bound to keys 1-9 in the
/// `WorkcatMap` context, mirrored by the node context menu (the menu
/// teaches the gesture, DR-003 ruling 8).
#[derive(Clone, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = workcat_map)]
pub struct SetStatus {
    pub status: String,
}

const WORKCAT_MAP_PANEL_KEY: &str = "WorkcatMapPanel";
const DEFAULT_WIDTH: f32 = 640.;
const MAX_LABEL_CHARS: usize = 21;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<WorkcatMapPanel>(window, cx);
        });
    })
    .detach();
}

/// One rendered node: an index into `items` plus owned geometry.
struct MapNode {
    item_ix: usize,
    x: f32,
    y: f32,
    label: SharedString,
}

/// An in-flight drag gesture.
struct DragState {
    node_ix: usize,
    pointer_start: Point<Pixels>,
    node_start: (f32, f32),
}

/// The map view. Standalone `Render`-able, needs no `Workspace`.
pub struct WorkcatMapView {
    focus_handle: FocusHandle,
    /// Every item parsed from briefs (all statuses, including hidden).
    items: Vec<ItemMeta>,
    /// Owned geometry, keyed by full item id. Persisted `node_moved`
    /// positions plus pinned initial-grid slots.
    positions: HashMap<String, (f32, f32)>,
    /// Visible nodes (default scope, DR-003 ruling 7).
    nodes: Vec<MapNode>,
    /// Dependency edges between visible nodes (dependent -> dependency).
    edges: Vec<(usize, usize)>,
    focused_node: Option<usize>,
    drag: Option<DragState>,
    panel_focused: bool,
    status: SharedString,
    loading: bool,
    /// Events appended to the log but not yet committed.
    pending_events: usize,
    checkpoint_running: bool,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    append_task: Option<Task<()>>,
    checkpoint_task: Option<Task<()>>,
    _load_task: Task<()>,
    _focus_subscriptions: Vec<Subscription>,
}

impl WorkcatMapView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let focus_subscriptions = vec![
            cx.on_focus(&focus_handle, window, |this, _, cx| {
                this.panel_focused = true;
                cx.notify();
            }),
            cx.on_blur(&focus_handle, window, |this, _, cx| {
                this.panel_focused = false;
                cx.notify();
            }),
        ];

        let load_task = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let db = store::resolve_db_dir();
                    let items = store::load_items(&db)?;
                    let positions = store::load_positions(&db)?;
                    anyhow::Ok((items, positions))
                })
                .await;
            this.update(cx, |this, cx| match loaded {
                Ok((items, positions)) => this.apply_loaded(items, positions, cx),
                Err(error) => {
                    log::error!("workcat_map: load failed: {error:#}");
                    this.loading = false;
                    this.status = format!("load failed: {error:#}").into();
                    cx.notify();
                }
            })
            .ok();
        });

        Self {
            focus_handle,
            items: Vec::new(),
            positions: HashMap::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
            focused_node: None,
            drag: None,
            panel_focused: false,
            status: "loading workcat-db...".into(),
            loading: true,
            pending_events: 0,
            checkpoint_running: false,
            context_menu: None,
            append_task: None,
            checkpoint_task: None,
            _load_task: load_task,
            _focus_subscriptions: focus_subscriptions,
        }
    }

    fn apply_loaded(
        &mut self,
        items: Vec<ItemMeta>,
        positions: HashMap<String, (f32, f32)>,
        cx: &mut Context<Self>,
    ) {
        let total_items = items.len();
        let total_edges: usize = items.iter().map(|item| item.depends_on.len()).sum();
        let persisted = positions.len();
        self.items = items;
        self.positions = positions;
        self.loading = false;
        self.rebuild_scene();
        log::info!(
            "workcat_map: loaded {} items ({} in scope), {} edges ({} between visible nodes), \
             {} persisted positions",
            total_items,
            self.nodes.len(),
            total_edges,
            self.edges.len(),
            persisted,
        );
        self.status = format!(
            "{} of {} items in scope, {} edges",
            self.nodes.len(),
            total_items,
            self.edges.len()
        )
        .into();
        cx.notify();
    }

    /// Recompute visible nodes and edges from `items` + `positions`.
    /// Nodes without a position get a deterministic grid slot, which is
    /// then pinned into `positions` so later rebuilds keep it.
    fn rebuild_scene(&mut self) {
        let mut visible: Vec<usize> = (0..self.items.len())
            .filter(|&ix| self.items[ix].status.in_default_scope())
            .collect();
        visible.sort_by(|&a, &b| {
            let (a, b) = (&self.items[a], &self.items[b]);
            (a.status.layout_rank(), &a.subject).cmp(&(b.status.layout_rank(), &b.subject))
        });
        self.nodes = visible
            .iter()
            .enumerate()
            .map(|(slot, &item_ix)| {
                let item = &self.items[item_ix];
                let (x, y) = self
                    .positions
                    .get(&item.id)
                    .copied()
                    .unwrap_or_else(|| geometry::grid_position(slot));
                self.positions.insert(item.id.clone(), (x, y));
                MapNode {
                    item_ix,
                    x,
                    y,
                    label: truncate(&item.subject).into(),
                }
            })
            .collect();
        let by_id8: HashMap<&str, usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(node_ix, node)| (self.items[node.item_ix].id8(), node_ix))
            .collect();
        let mut edges = Vec::new();
        for (node_ix, node) in self.nodes.iter().enumerate() {
            for dep in &self.items[node.item_ix].depends_on {
                if let Some(&dep_ix) = by_id8.get(dep.as_str()) {
                    edges.push((node_ix, dep_ix));
                }
            }
        }
        self.edges = edges;
        if self
            .focused_node
            .is_some_and(|node_ix| node_ix >= self.nodes.len())
        {
            self.focused_node = None;
        }
    }

    fn focused_item(&self) -> Option<&ItemMeta> {
        let node = self.nodes.get(self.focused_node?)?;
        self.items.get(node.item_ix)
    }

    // === Drag & focus ===

    fn begin_drag(
        &mut self,
        node_ix: usize,
        pointer: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        let Some(node) = self.nodes.get(node_ix) else {
            return;
        };
        self.focused_node = Some(node_ix);
        self.drag = Some(DragState {
            node_ix,
            pointer_start: pointer,
            node_start: (node.x, node.y),
        });
        cx.notify();
    }

    fn update_drag(&mut self, pointer: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = &self.drag else {
            return;
        };
        let dx = f32::from(pointer.x - drag.pointer_start.x);
        let dy = f32::from(pointer.y - drag.pointer_start.y);
        let (x, y) = geometry::clamp_position(drag.node_start.0 + dx, drag.node_start.1 + dy);
        let node_ix = drag.node_ix;
        if let Some(node) = self.nodes.get_mut(node_ix) {
            node.x = x;
            node.y = y;
        }
        cx.notify();
    }

    /// Gesture rest: a real drag appends one `node_moved` event to the
    /// log immediately (commit happens at the next checkpoint). A
    /// mouse-up within the click slop is a click, which only focuses.
    fn end_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        let Some(node) = self.nodes.get(drag.node_ix) else {
            return;
        };
        if geometry::is_click(node.x - drag.node_start.0, node.y - drag.node_start.1) {
            cx.notify();
            return;
        }
        let item = &self.items[node.item_ix];
        self.positions.insert(item.id.clone(), (node.x, node.y));
        let event = store::node_moved_event(&item.id, node.x, node.y);
        self.append_to_log(event, cx);
        cx.notify();
    }

    fn clear_focus(&mut self, _: &ClearFocus, _window: &mut Window, cx: &mut Context<Self>) {
        self.focused_node = None;
        cx.notify();
    }

    // === Writes ===

    fn append_to_log(&mut self, event: serde_json::Value, cx: &mut Context<Self>) {
        self.pending_events += 1;
        self.append_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let db = store::resolve_db_dir();
                    store::append_event(&db, &event)
                })
                .await;
            if let Err(error) = result {
                log::error!("workcat_map: append failed: {error:#}");
                this.update(cx, |this, cx| {
                    this.status = format!("event append failed: {error:#}").into();
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    fn set_status(&mut self, action: &SetStatus, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = Status::parse(&action.status) else {
            self.status = format!("unknown status: {}", action.status).into();
            cx.notify();
            return;
        };
        let Some(node_ix) = self.focused_node else {
            self.status = "no focused item to set status on".into();
            cx.notify();
            return;
        };
        let item_ix = self.nodes[node_ix].item_ix;
        let item = &mut self.items[item_ix];
        if item.status == status {
            return;
        }
        let id = item.id.clone();
        let id8 = item.id8().to_string();
        item.status = status;
        if !status.in_default_scope() {
            self.rebuild_scene();
        }
        // Status changes checkpoint immediately (DR-006 two-grain: the
        // event appends now; the checkpoint commits it plus any pending
        // drag events).
        self.pending_events += 1;
        let message = format!("status: {} -> {}", id8, status.as_str());
        let event = store::status_set_event(&id, status.as_str());
        self.spawn_checkpoint(Some(event), message, cx);
        cx.notify();
    }

    fn checkpoint(&mut self, _: &Checkpoint, _window: &mut Window, cx: &mut Context<Self>) {
        self.spawn_checkpoint(None, "map: session checkpoint".into(), cx);
    }

    /// Run the write protocol on the background executor: optionally
    /// append one more event, then lock -> fold -> commit -> push ->
    /// release. Progress (including "waiting for lock") streams into
    /// the status line; the UI thread never blocks on the lock.
    fn spawn_checkpoint(
        &mut self,
        event: Option<serde_json::Value>,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if self.checkpoint_running {
            self.status = "checkpoint already running; event queued for the next one".into();
            if let Some(event) = event {
                self.append_to_log(event, cx);
                self.pending_events -= 1; // append_to_log counted it again
            }
            cx.notify();
            return;
        }
        self.checkpoint_running = true;
        self.status = "checkpoint starting...".into();
        let (progress_tx, mut progress_rx) = futures::channel::mpsc::unbounded::<String>();
        cx.spawn(async move |this, cx| {
            while let Some(message) = progress_rx.next().await {
                this.update(cx, |this, cx| {
                    this.status = message.into();
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        self.checkpoint_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let db = store::resolve_db_dir();
                    if let Some(event) = event {
                        store::append_event(&db, &event)?;
                    }
                    store::checkpoint(&db, &message, &progress_tx).await
                })
                .await;
            this.update(cx, |this, cx| {
                this.checkpoint_running = false;
                match result {
                    Ok(Some(sha)) => {
                        this.pending_events = 0;
                        this.status = format!("checkpointed @ {sha}").into();
                    }
                    Ok(None) => {
                        this.pending_events = 0;
                        this.status = "nothing to checkpoint".into();
                    }
                    Err(error) => {
                        log::error!("workcat_map: checkpoint failed: {error:#}");
                        this.status = format!("checkpoint failed: {error:#}").into();
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    // === Context menu (the menu teaches the gesture: entries show
    // their key bindings via the WorkcatMap context) ===

    fn deploy_context_menu(
        &mut self,
        node_ix: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focused_node = Some(node_ix);
        let current = self.focused_item().map(|item| item.status);
        let context_menu = ContextMenu::build(window, cx, |mut menu, _, _| {
            menu = menu.context(self.focus_handle.clone()).header("Set Status");
            for status in ALL_STATUSES {
                let label = if current == Some(status) {
                    format!("{} (current)", status.label())
                } else {
                    status.label().to_string()
                };
                menu = menu.action(
                    label,
                    Box::new(SetStatus {
                        status: status.as_str().to_string(),
                    }),
                );
            }
            menu.separator()
                .action("Checkpoint Now", Box::new(Checkpoint))
        });
        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe_in(
            &context_menu,
            window,
            |this, _, _: &DismissEvent, window, cx| {
                if this.context_menu.as_ref().is_some_and(|context_menu| {
                    context_menu.0.focus_handle(cx).contains_focused(window, cx)
                }) {
                    cx.focus_self(window);
                }
                this.context_menu.take();
                cx.notify();
            },
        );
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    // === Rendering ===

    fn status_color(status: Status, cx: &App) -> Hsla {
        let colors = cx.theme().status();
        match status {
            Status::NotStarted => colors.ignored,
            Status::Started => colors.info,
            Status::Paused => colors.modified,
            Status::Blocked => colors.error,
            Status::Implemented => colors.created,
            Status::Merged => colors.renamed,
            Status::Complete => colors.success,
            Status::Canceled => colors.hidden,
            Status::Unknown => colors.conflict,
        }
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .border_b_1()
            .border_color(colors.border)
            .child(Label::new("Workcat Map").weight(gpui::FontWeight::BOLD))
            .when(self.pending_events > 0, |this| {
                this.child(
                    Label::new(format!("{} uncommitted", self.pending_events))
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .child(
                Label::new(self.status.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn render_detail_strip(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let item = self.focused_item()?;
        let colors = cx.theme().colors().clone();
        let status_color = Self::status_color(item.status, cx);
        Some(
            h_flex()
                .w_full()
                .px_2()
                .py_1()
                .gap_2()
                .border_b_1()
                .border_color(colors.border)
                .bg(colors.element_background)
                .child(div().w_2().h_2().rounded_full().bg(status_color))
                .child(
                    Label::new(truncate_to(&item.subject, 52))
                        .size(LabelSize::Small)
                        .weight(gpui::FontWeight::BOLD),
                )
                .child(
                    Label::new(item.status.as_str())
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
                .child(
                    Label::new(format!("{} deps", item.depends_on.len()))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(truncate_to(&item.reference, 44))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        )
    }

    /// Dependency edges, drawn under the nodes: a line from each
    /// dependent to its dependency with a small arrowhead at the
    /// dependency end.
    fn render_edges(&self, cx: &Context<Self>) -> impl IntoElement {
        let color = cx.theme().colors().text_muted.alpha(0.5);
        let focused_color = cx.theme().colors().text_accent;
        let focused = self.focused_node;
        let segments: Vec<((f32, f32), (f32, f32), bool)> = self
            .edges
            .iter()
            .map(|&(from_ix, to_ix)| {
                let (from, to) = (&self.nodes[from_ix], &self.nodes[to_ix]);
                let center = |n: &MapNode| {
                    (
                        n.x + geometry::NODE_WIDTH / 2.0,
                        n.y + geometry::NODE_HEIGHT / 2.0,
                    )
                };
                let lit = focused == Some(from_ix) || focused == Some(to_ix);
                (center(from), center(to), lit)
            })
            .collect();
        canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                for lit in [false, true] {
                    let mut builder = PathBuilder::stroke(px(if lit { 1.5 } else { 1.0 }));
                    let mut any = false;
                    for &((x1, y1), (x2, y2), seg_lit) in &segments {
                        if seg_lit != lit {
                            continue;
                        }
                        any = true;
                        let from = bounds.origin + point(px(x1), px(y1));
                        let to = bounds.origin + point(px(x2), px(y2));
                        builder.move_to(from);
                        builder.line_to(to);
                        // Arrowhead pointing at the dependency, pulled
                        // back from the node center.
                        let (dx, dy) = (x2 - x1, y2 - y1);
                        let len = (dx * dx + dy * dy).sqrt().max(1.0);
                        let (ux, uy) = (dx / len, dy / len);
                        let tip = (
                            x2 - ux * geometry::NODE_HEIGHT,
                            y2 - uy * geometry::NODE_HEIGHT,
                        );
                        for angle in [2.6f32, -2.6] {
                            let (sin, cos) = angle.sin_cos();
                            let wing = (
                                tip.0 + (ux * cos - uy * sin) * 7.0,
                                tip.1 + (ux * sin + uy * cos) * 7.0,
                            );
                            builder.move_to(bounds.origin + point(px(tip.0), px(tip.1)));
                            builder.line_to(bounds.origin + point(px(wing.0), px(wing.1)));
                        }
                    }
                    if any && let Ok(path) = builder.build() {
                        window.paint_path(path, if lit { focused_color } else { color });
                    }
                }
            },
        )
        .absolute()
        .size_full()
    }

    fn render_node(&self, node_ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let node = &self.nodes[node_ix];
        let item = &self.items[node.item_ix];
        let colors = cx.theme().colors().clone();
        let status_color = Self::status_color(item.status, cx);
        let dragging = self
            .drag
            .as_ref()
            .is_some_and(|drag| drag.node_ix == node_ix);
        let focused = self.focused_node == Some(node_ix);
        div()
            .absolute()
            .left(px(node.x))
            .top(px(node.y))
            .w(px(geometry::NODE_WIDTH))
            .h(px(geometry::NODE_HEIGHT))
            .px_1()
            .gap_1()
            .flex()
            .items_center()
            .overflow_hidden()
            .rounded_sm()
            .map(|this| {
                // Focus halo: a bright, thicker border. Unknown items
                // are visually distinct via a dashed border.
                let this = if item.status == Status::Unknown {
                    this.border_dashed()
                } else {
                    this
                };
                if focused || dragging {
                    this.border_2().border_color(colors.border_focused)
                } else {
                    this.border_1().border_color(status_color.alpha(0.8))
                }
            })
            .bg(if focused {
                colors.element_selected
            } else {
                colors.element_background
            })
            .cursor_grab()
            .child(
                div()
                    .flex_none()
                    .w_2()
                    .h_2()
                    .rounded_full()
                    .bg(status_color),
            )
            .child(Label::new(node.label.clone()).size(LabelSize::XSmall))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.begin_drag(node_ix, event.position, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.deploy_context_menu(node_ix, event.position, window, cx);
                    cx.stop_propagation();
                }),
            )
    }

    /// While a drag is live, register window-level move/up handlers via
    /// a canvas overlay so the gesture keeps tracking even when the
    /// pointer leaves the panel bounds.
    fn render_drag_listener(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity: WeakEntity<Self> = cx.entity().downgrade();
        canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                window.on_mouse_event({
                    let entity = entity.clone();
                    move |event: &MouseMoveEvent, phase: DispatchPhase, _, cx| {
                        if phase.bubble() {
                            entity
                                .update(cx, |this, cx| this.update_drag(event.position, cx))
                                .ok();
                        }
                    }
                });
                window.on_mouse_event({
                    move |_: &MouseUpEvent, phase: DispatchPhase, _, cx| {
                        if phase.bubble() {
                            entity.update(cx, |this, cx| this.end_drag(cx)).ok();
                        }
                    }
                });
            },
        )
        .absolute()
        .size_full()
    }

    fn render_field(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let mut content = div()
            .relative()
            .w(px(geometry::FIELD_WIDTH))
            .h(px(geometry::FIELD_HEIGHT))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.focused_node = None;
                    window.focus(&this.focus_handle, cx);
                    cx.notify();
                }),
            )
            .child(self.render_edges(cx));
        if self.loading {
            content = content.child(
                div()
                    .p_2()
                    .child(Label::new("loading workcat-db...").color(Color::Muted)),
            );
        }
        for node_ix in 0..self.nodes.len() {
            let node = self.render_node(node_ix, cx);
            content = content.child(node);
        }
        if self.drag.is_some() {
            content = content.child(self.render_drag_listener(cx));
        }
        div()
            .id("workcat-map-field")
            .flex_grow(1.)
            .overflow_scroll()
            .border_2()
            .border_color(if self.panel_focused {
                colors.border_focused
            } else {
                colors.border_variant
            })
            .child(content)
    }
}

fn truncate(text: &str) -> String {
    truncate_to(text, MAX_LABEL_CHARS)
}

fn truncate_to(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max_chars).collect();
        out.push('\u{2026}');
        out
    }
}

impl Focusable for WorkcatMapView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkcatMapView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("WorkcatMap")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::set_status))
            .on_action(cx.listener(Self::checkpoint))
            .on_action(cx.listener(Self::clear_focus))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .children(self.render_detail_strip(cx))
            .child(self.render_field(cx))
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

/// The dock panel wrapper.
pub struct WorkcatMapPanel {
    view: Entity<WorkcatMapView>,
    position: DockPosition,
}

impl WorkcatMapPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let view = cx.new(|cx| WorkcatMapView::new(window, cx));
        Self {
            view,
            position: DockPosition::Right,
        }
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |_workspace, window, cx| {
            cx.new(|cx| Self::new(window, cx))
        })
    }
}

impl EventEmitter<PanelEvent> for WorkcatMapPanel {}

impl Focusable for WorkcatMapPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }
}

impl Render for WorkcatMapPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.view.clone())
    }
}

impl Panel for WorkcatMapPanel {
    fn persistent_name() -> &'static str {
        "WorkcatMapPanel"
    }

    fn panel_key() -> &'static str {
        WORKCAT_MAP_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(DEFAULT_WIDTH)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Blocks)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Workcat Map")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        11
    }
}
