//! The Workcat Map dock panel: read, focus, drag, set status (DR-005
//! ruling 17) against the workcat-db repo-as-database — plus the
//! SPA-parity pass: filters + search, saved lenses, rect-select
//! multi-node move, session undo/redo, auto-arrange, a detail pane
//! with the brief body, and notes editing.
//!
//! Dragging is direct manipulation: mouse down on a node starts a
//! gesture, window-level mouse events move it, and mouse up commits: a
//! `node_moved` event is appended to the db's event log immediately
//! (the fine grain), while git commit/push happens at checkpoints (the
//! coarse grain, DR-006): on every status change and on the manual
//! Checkpoint action.
//!
//! Undo is event-sourced and pane-focus scoped (DR-005 ruling 16):
//! undoing appends compensating events restoring the prior value; the
//! log is never rewritten, and Zed's editor history is untouched.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use editor::{Editor, EditorEvent};
use futures::StreamExt as _;
use gpui::{
    App, Context, DismissEvent, DispatchPhase, Entity, FocusHandle, Focusable, Hsla, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, PinchEvent, Pixels, Point,
    ScrollHandle, SharedString, Subscription, Task, WeakEntity, Window, actions, anchored, canvas,
    deferred, point, px,
};
use ui::{ContextMenu, prelude::*};

use crate::geometry;
use crate::model::{ALL_STATUSES, FilterState, ItemMeta, Lens, Op, Status, UndoStack};
use crate::store;

actions!(
    workcat_map,
    [
        /// Toggles focus on the workcat map panel.
        ToggleFocus,
        /// Commits and pushes the session's pending events (write lock,
        /// fold, commit, push).
        Checkpoint,
        /// Clears the focused node, selection, and any pending lens
        /// naming.
        ClearFocus,
        /// Undoes the last map operation (move or status change) by
        /// appending compensating events.
        Undo,
        /// Redoes the last undone map operation.
        Redo,
        /// Opens the focused item in the Workcat Detail panel (or
        /// confirms the lens name while naming a lens).
        ToggleDetail,
        /// Re-lays out the visible nodes on the deterministic grid.
        AutoArrange,
        /// Packs connected components into a compact, viewport-shaped
        /// layout (the SPA's "squeeze"), persisted as node moves.
        Squeeze,
        /// Scrolls the field so the visible nodes are in view.
        Fit,
        /// Zooms the view in (map: the whole field; detail: the text).
        ZoomIn,
        /// Zooms the view out.
        ZoomOut,
        /// Resets the zoom to 100%.
        ZoomReset,
        /// Resets filters and search to the default scope.
        ClearFilter,
        /// Starts naming a new lens (the "+" affordance) to save the
        /// current filter and layout as.
        SaveLensPrompt,
        /// Saves the current filter and layout into the active lens,
        /// overwriting it in place (no name prompt).
        SaveActiveLens,
        /// Confirms the lens name being typed.
        ConfirmLensName,
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

/// Applies a saved lens by name.
#[derive(Clone, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = workcat_map)]
pub struct ApplyLens {
    pub name: String,
}

/// Deletes a saved lens by name.
#[derive(Clone, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = workcat_map)]
pub struct DeleteLens {
    pub name: String,
}

/// Global registry connecting the map panel to the detail panel: the
/// map view publishes a weak handle to itself here on creation, and
/// the detail panel observes it (panels load concurrently, in either
/// order).
#[derive(Default)]
pub struct WorkcatMapHandle(pub Option<WeakEntity<WorkcatMapView>>);

impl gpui::Global for WorkcatMapHandle {}

/// ~16 words: four wrapped lines of ~30 characters.
const MAX_LABEL_CHARS: usize = 118;
/// How long the map waits after the last mutation before auto-checkpointing.
const AUTO_CHECKPOINT_DELAY: Duration = Duration::from_secs(5);
/// A lens with this name is applied automatically on load, so the last
/// saved arrangement is the startup view.
const DEFAULT_LENS: &str = "default";

/// The dock panel wrapper (and its `ToggleFocus`/`ToggleDetailPanelFocus`
/// registrations) lives in `workcat_panel.rs`, which combines this view
/// with `WorkcatDetailView` into one panel.
pub fn init(_cx: &mut App) {}

/// One rendered node: an index into `items` plus owned geometry.
struct MapNode {
    item_ix: usize,
    x: f32,
    y: f32,
    label: SharedString,
}

/// An in-flight drag gesture: the pressed node plus (for a group
/// drag over a selection) every other node moving with it.
struct DragState {
    /// The node the gesture started on.
    pressed_ix: usize,
    pointer_start: Point<Pixels>,
    /// Every node moving in this gesture with its start position.
    starts: Vec<(usize, (f32, f32))>,
}

/// An in-flight rectangle-selection gesture, in field coordinates.
struct RectSelect {
    start: (f32, f32),
    current: (f32, f32),
}

/// An in-flight background pan (drag-to-scroll) gesture, in window
/// coordinates plus the scroll offset at gesture start.
struct Pan {
    pointer_start: Point<Pixels>,
    offset_start: Point<Pixels>,
}

/// The map view. Standalone `Render`-able, needs no `Workspace`.
pub struct WorkcatMapView {
    focus_handle: FocusHandle,
    /// Every item parsed from briefs (all statuses, including hidden).
    items: Vec<ItemMeta>,
    /// Owned geometry, keyed by full item id. Persisted `node_moved`
    /// positions plus pinned initial-grid slots.
    positions: HashMap<String, (f32, f32)>,
    /// Visible nodes (per the current filter).
    nodes: Vec<MapNode>,
    /// Dependency edges between visible nodes (dependent -> dependency).
    edges: Vec<(usize, usize)>,
    focused_node: Option<usize>,
    /// Multi-selection, keyed by item id (stable across rebuilds).
    selected: HashSet<String>,
    drag: Option<DragState>,
    rect_select: Option<RectSelect>,
    pan: Option<Pan>,
    /// The field content's window-space origin, captured at paint time
    /// so background gestures can be mapped into field coordinates.
    field_origin: Rc<Cell<Point<Pixels>>>,
    /// Scroll state of the field viewport (drives Fit and gives
    /// Squeeze its viewport aspect).
    scroll_handle: ScrollHandle,
    /// Center the view on the content once, after load, as soon as
    /// the viewport has been measured (its bounds are only known
    /// after the first paint).
    needs_initial_fit: bool,
    /// View zoom: scales the rendered field (positions, node size,
    /// labels, edges). Node geometry stays in unzoomed field
    /// coordinates everywhere else.
    zoom: f32,
    /// The visibility filter (statuses + free-text query).
    filter: FilterState,
    search_editor: Entity<Editor>,
    /// Saved lenses folded from the event log.
    lenses: BTreeMap<String, Lens>,
    active_lens: Option<String>,
    /// When true, the header shows the lens-name input.
    naming_lens: bool,
    lens_name_editor: Entity<Editor>,
    /// Session undo/redo (200 steps, event-sourced compensations).
    undo_stack: UndoStack,
    panel_focused: bool,
    status: SharedString,
    loading: bool,
    /// Events appended to the log but not yet committed.
    pending_events: usize,
    checkpoint_running: bool,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    append_task: Option<Task<()>>,
    checkpoint_task: Option<Task<()>>,
    /// Debounced auto-checkpoint timer, reset by every mutation.
    auto_checkpoint_task: Option<Task<()>>,
    /// Apply the "default" lens once, after the first load, to restore
    /// the saved startup arrangement.
    needs_default_lens: bool,
    _load_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl WorkcatMapView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search subject or ref\u{2026}", window, cx);
            editor
        });
        let lens_name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Lens name\u{2026}", window, cx);
            editor
        });
        cx.set_global(WorkcatMapHandle(Some(cx.weak_entity())));
        let subscriptions = vec![
            cx.on_focus(&focus_handle, window, |this, _, cx| {
                this.panel_focused = true;
                cx.notify();
            }),
            cx.on_blur(&focus_handle, window, |this, _, cx| {
                this.panel_focused = false;
                cx.notify();
            }),
            cx.subscribe_in(
                &search_editor,
                window,
                |this: &mut Self, editor, event, _window, cx| {
                    if let EditorEvent::BufferEdited = event {
                        let query = editor.read(cx).text(cx);
                        if this.filter.query != query {
                            this.filter.query = query;
                            this.active_lens = None;
                            this.rebuild_scene();
                            cx.notify();
                        }
                    }
                },
            ),
        ];

        let load_task = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let db = store::resolve_db_dir();
                    let items = store::load_items(&db)?;
                    let positions = store::load_positions(&db)?;
                    let lenses = store::load_lenses(&db)?;
                    anyhow::Ok((items, positions, lenses))
                })
                .await;
            this.update(cx, |this, cx| match loaded {
                Ok((items, positions, lenses)) => {
                    this.lenses = lenses;
                    this.apply_loaded(items, positions, cx)
                }
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
            selected: HashSet::new(),
            drag: None,
            rect_select: None,
            pan: None,
            field_origin: Rc::new(Cell::new(Point::default())),
            scroll_handle: ScrollHandle::new(),
            needs_initial_fit: true,
            zoom: 1.0,
            filter: FilterState::default(),
            search_editor,
            lenses: BTreeMap::new(),
            active_lens: None,
            naming_lens: false,
            lens_name_editor,
            undo_stack: UndoStack::default(),
            panel_focused: false,
            status: "loading workcat-db...".into(),
            loading: true,
            pending_events: 0,
            checkpoint_running: false,
            context_menu: None,
            append_task: None,
            checkpoint_task: None,
            auto_checkpoint_task: None,
            needs_default_lens: true,
            _load_task: load_task,
            _subscriptions: subscriptions,
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

    /// Recompute visible nodes and edges from `items` + `positions`,
    /// gated by the current filter. Nodes without a position get a
    /// deterministic grid slot, which is then pinned into `positions`
    /// so later rebuilds keep it.
    fn rebuild_scene(&mut self) {
        let mut visible: Vec<usize> = (0..self.items.len())
            .filter(|&ix| self.filter.matches(&self.items[ix]))
            .collect();
        visible.sort_by(|&a, &b| {
            let (a, b) = (&self.items[a], &self.items[b]);
            (a.status.layout_rank(), &a.subject).cmp(&(b.status.layout_rank(), &b.subject))
        });
        let visible_count = visible.len();
        self.nodes = visible
            .iter()
            .enumerate()
            .map(|(slot, &item_ix)| {
                let item = &self.items[item_ix];
                let (x, y) = self
                    .positions
                    .get(&item.id)
                    .copied()
                    .unwrap_or_else(|| geometry::grid_position(slot, visible_count));
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
        // Selection survives rebuilds by id; drop ids that are no
        // longer visible so gestures never touch hidden nodes.
        let visible_ids: HashSet<&str> = self
            .nodes
            .iter()
            .map(|node| self.items[node.item_ix].id.as_str())
            .collect();
        self.selected.retain(|id| visible_ids.contains(id.as_str()));
    }

    fn node_ix_by_id(&self, id: &str) -> Option<usize> {
        self.nodes
            .iter()
            .position(|node| self.items[node.item_ix].id == id)
    }

    pub fn focused_item(&self) -> Option<&ItemMeta> {
        let node = self.nodes.get(self.focused_node?)?;
        self.items.get(node.item_ix)
    }

    // === Drag, selection & focus ===

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
        let pressed_id = self.items[node.item_ix].id.clone();
        // Dragging a selected node moves the whole selection; dragging
        // an unselected node moves just it — the selection is kept
        // (only an explicit background click or escape deselects).
        let starts: Vec<(usize, (f32, f32))> =
            if self.selected.contains(&pressed_id) && self.selected.len() > 1 {
                self.nodes
                    .iter()
                    .enumerate()
                    .filter(|(_, node)| self.selected.contains(&self.items[node.item_ix].id))
                    .map(|(ix, node)| (ix, (node.x, node.y)))
                    .collect()
            } else {
                vec![(node_ix, (node.x, node.y))]
            };
        self.focused_node = Some(node_ix);
        self.drag = Some(DragState {
            pressed_ix: node_ix,
            pointer_start: pointer,
            starts,
        });
        cx.notify();
    }

    fn update_drag(&mut self, pointer: Point<Pixels>, cx: &mut Context<Self>) {
        let zoom = self.zoom;
        if let Some(pan) = &self.pan {
            // Drag-to-scroll: the content follows the hand.
            let delta = pointer - pan.pointer_start;
            self.scroll_handle.set_offset(pan.offset_start + delta);
            cx.notify();
            return;
        }
        if let Some(rect) = &mut self.rect_select {
            let origin = self.field_origin.get();
            rect.current = (
                f32::from(pointer.x - origin.x) / zoom,
                f32::from(pointer.y - origin.y) / zoom,
            );
            cx.notify();
            return;
        }
        let Some(drag) = &self.drag else {
            return;
        };
        let dx = f32::from(pointer.x - drag.pointer_start.x) / zoom;
        let dy = f32::from(pointer.y - drag.pointer_start.y) / zoom;
        let moves: Vec<(usize, (f32, f32))> = drag
            .starts
            .iter()
            .map(|&(node_ix, start)| {
                (
                    node_ix,
                    geometry::clamp_position(start.0 + dx, start.1 + dy),
                )
            })
            .collect();
        for (node_ix, (x, y)) in moves {
            if let Some(node) = self.nodes.get_mut(node_ix) {
                node.x = x;
                node.y = y;
            }
        }
        cx.notify();
    }

    /// Gesture rest: a real drag appends one `node_moved` event per
    /// moved node to the log immediately (commit happens at the next
    /// checkpoint) and records one undo step for the whole gesture. A
    /// mouse-up within the click slop is a click, which only focuses.
    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(pan) = self.pan.take() {
            // A pan that never moved past the click slop is a plain
            // background click: clear the selection.
            let delta = self.scroll_handle.offset() - pan.offset_start;
            if geometry::is_click(f32::from(delta.x), f32::from(delta.y)) {
                self.selected.clear();
            }
            cx.notify();
            return;
        }
        if let Some(rect) = self.rect_select.take() {
            self.finish_rect_select(rect, cx);
            return;
        }
        let Some(drag) = self.drag.take() else {
            return;
        };
        let Some(pressed) = self.nodes.get(drag.pressed_ix) else {
            return;
        };
        let pressed_start = drag
            .starts
            .iter()
            .find(|(ix, _)| *ix == drag.pressed_ix)
            .map(|&(_, start)| start)
            .unwrap_or((pressed.x, pressed.y));
        if geometry::is_click(pressed.x - pressed_start.0, pressed.y - pressed_start.1) {
            cx.notify();
            return;
        }
        let mut op_moves = Vec::new();
        for &(node_ix, start) in &drag.starts {
            let Some(node) = self.nodes.get(node_ix) else {
                continue;
            };
            if node.x == start.0 && node.y == start.1 {
                continue;
            }
            let id = self.items[node.item_ix].id.clone();
            let at = (node.x, node.y);
            self.positions.insert(id.clone(), at);
            op_moves.push((id, start, at));
        }
        for (id, _, (x, y)) in &op_moves {
            let event = store::node_moved_event(id, *x, *y);
            self.append_to_log(event, cx);
        }
        if !op_moves.is_empty() {
            self.undo_stack.push(Op::Move(op_moves));
        }
        cx.notify();
    }

    /// Background mouse-down: shift starts a rectangle selection;
    /// plain grab starts a pan (drag-to-scroll). Both clear focus.
    fn begin_background_gesture(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.focused_node = None;
        if event.modifiers.shift {
            let origin = self.field_origin.get();
            let at = (
                f32::from(event.position.x - origin.x) / self.zoom,
                f32::from(event.position.y - origin.y) / self.zoom,
            );
            self.rect_select = Some(RectSelect {
                start: at,
                current: at,
            });
        } else {
            self.pan = Some(Pan {
                pointer_start: event.position,
                offset_start: self.scroll_handle.offset(),
            });
        }
        cx.notify();
    }

    fn finish_rect_select(&mut self, rect: RectSelect, cx: &mut Context<Self>) {
        let (dx, dy) = (rect.current.0 - rect.start.0, rect.current.1 - rect.start.1);
        if geometry::is_click(dx, dy) {
            // A shift-click that never dragged: clear the selection.
            self.selected.clear();
            cx.notify();
            return;
        }
        let bounds = geometry::normalize_rect(rect.start, rect.current);
        self.selected = self
            .nodes
            .iter()
            .filter(|node| geometry::node_in_rect((node.x, node.y), bounds))
            .map(|node| self.items[node.item_ix].id.clone())
            .collect();
        self.status = format!("{} selected", self.selected.len()).into();
        cx.notify();
    }

    fn clear_focus(&mut self, _: &ClearFocus, window: &mut Window, cx: &mut Context<Self>) {
        self.focused_node = None;
        self.selected.clear();
        self.rect_select = None;
        self.pan = None;
        self.naming_lens = false;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    // === Undo/redo (event-sourced compensations, DR-005 ruling 16) ===

    fn undo(&mut self, _: &Undo, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(op) = self.undo_stack.undo() else {
            self.status = "nothing to undo".into();
            cx.notify();
            return;
        };
        self.apply_op(&op, true, cx);
        self.status = format!("undid ({} left)", self.undo_stack.undo_len()).into();
        cx.notify();
    }

    fn redo(&mut self, _: &Redo, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(op) = self.undo_stack.redo() else {
            self.status = "nothing to redo".into();
            cx.notify();
            return;
        };
        self.apply_op(&op, false, cx);
        self.status = format!("redid ({} redoable)", self.undo_stack.redo_len()).into();
        cx.notify();
    }

    /// Apply one op in the undo (`backward = true`) or redo direction
    /// by updating local state and appending compensating events.
    /// Unlike a fresh status change, compensations do not checkpoint;
    /// they ride along with the next one.
    fn apply_op(&mut self, op: &Op, backward: bool, cx: &mut Context<Self>) {
        match op {
            Op::Move(moves) => {
                for (id, from, to) in moves {
                    let (x, y) = if backward { *from } else { *to };
                    self.positions.insert(id.clone(), (x, y));
                    if let Some(node_ix) = self.node_ix_by_id(id)
                        && let Some(node) = self.nodes.get_mut(node_ix)
                    {
                        node.x = x;
                        node.y = y;
                    }
                    self.append_to_log(store::node_moved_event(id, x, y), cx);
                }
            }
            Op::SetStatus(id, from, to) => {
                let status = if backward { *from } else { *to };
                if let Some(item) = self.items.iter_mut().find(|item| item.id == *id) {
                    item.status = status;
                }
                self.append_to_log(store::status_set_event(id, status.as_str()), cx);
                self.rebuild_scene();
            }
        }
    }

    // === Layout verbs: auto-arrange, squeeze, fit ===

    /// Re-lay out the currently visible nodes on the deterministic
    /// grid (their current sorted order), recording one undo step.
    fn auto_arrange(&mut self, _: &AutoArrange, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.nodes.len();
        let mut targets = Vec::with_capacity(count);
        for slot in 0..count {
            targets.push((slot, geometry::grid_position(slot, count)));
        }
        let moved = self.apply_layout(&targets, cx);
        self.status = if moved == 0 {
            "already arranged".into()
        } else {
            format!("arranged {moved} nodes").into()
        };
        self.fit(&Fit, window, cx);
    }

    /// The SPA's "squeeze", upgraded: each multi-node connected
    /// component gets a fresh compact layered layout (dependents
    /// above dependencies, barycenter-ordered — no more giant sparse
    /// blocks from old drag positions), all isolated nodes pool into
    /// one dense grid block, and the blocks shelf-pack into a layout
    /// whose aspect roughly matches the viewport. Persisted as
    /// ordinary node moves, so it composes with manual nudges and is
    /// one undo step.
    fn squeeze(&mut self, _: &Squeeze, window: &mut Window, cx: &mut Context<Self>) {
        if self.nodes.is_empty() {
            self.status = "nothing to squeeze".into();
            cx.notify();
            return;
        }
        let components = geometry::connected_components(self.nodes.len(), &self.edges);
        let mut blocks: Vec<geometry::Block> = Vec::new();
        let mut singles: Vec<usize> = Vec::new();
        for members in &components {
            if members.len() == 1 {
                singles.push(members[0]);
                continue;
            }
            let local_ix: HashMap<usize, usize> = members
                .iter()
                .enumerate()
                .map(|(local, &global)| (global, local))
                .collect();
            let local_edges: Vec<(usize, usize)> = self
                .edges
                .iter()
                .filter_map(|&(a, b)| Some((*local_ix.get(&a)?, *local_ix.get(&b)?)))
                .collect();
            let layout = geometry::layered_layout(members.len(), &local_edges);
            let (min_x, min_y, max_x, max_y) =
                geometry::nodes_bbox(&layout).expect("non-empty component");
            blocks.push(geometry::Block {
                w: max_x - min_x,
                h: max_y - min_y,
                members: members
                    .iter()
                    .zip(&layout)
                    .map(|(&global, &(x, y))| (global, x - min_x, y - min_y))
                    .collect(),
            });
        }
        if !singles.is_empty() {
            // One dense, roughly square grid block for all isolated
            // nodes, in their current sorted (status, subject) order.
            let cols = (singles.len() as f32).sqrt().ceil().max(1.0) as usize;
            let members: Vec<(usize, f32, f32)> = singles
                .iter()
                .enumerate()
                .map(|(ix, &global)| {
                    let col = ix % cols;
                    let row = ix / cols;
                    (
                        global,
                        col as f32 * geometry::CELL_WIDTH,
                        row as f32 * geometry::CELL_HEIGHT,
                    )
                })
                .collect();
            let rows = singles.len().div_ceil(cols);
            blocks.push(geometry::Block {
                w: (cols - 1) as f32 * geometry::CELL_WIDTH + geometry::NODE_WIDTH,
                h: (rows - 1) as f32 * geometry::CELL_HEIGHT + geometry::NODE_HEIGHT,
                members,
            });
        }
        let viewport = self.scroll_handle.bounds().size;
        let aspect = if viewport.height > px(0.) {
            f32::from(viewport.width) / f32::from(viewport.height)
        } else {
            1.0
        };
        let packed = geometry::pack_blocks(&blocks, aspect);
        let mut targets = Vec::new();
        for (block, &(bx, by)) in blocks.iter().zip(&packed) {
            for &(node_ix, dx, dy) in &block.members {
                targets.push((node_ix, geometry::clamp_position(bx + dx, by + dy)));
            }
        }
        let moved = self.apply_layout(&targets, cx);
        self.status = if moved == 0 {
            "already packed".into()
        } else {
            format!("squeezed {} blocks ({moved} nodes)", blocks.len()).into()
        };
        self.fit(&Fit, window, cx);
    }

    /// Scroll the field so the focused node sits at the viewport
    /// center. Drives the detail panel's "Locate" action: the item is
    /// already focused there, this just brings it into view on the map.
    pub(crate) fn center_on_focused(&mut self, cx: &mut Context<Self>) {
        let Some(node) = self.focused_node.and_then(|ix| self.nodes.get(ix)) else {
            self.status = "no focused item to locate".into();
            cx.notify();
            return;
        };
        let viewport = self.scroll_handle.bounds().size;
        let center_x = (node.x + geometry::NODE_WIDTH / 2.0) * self.zoom;
        let center_y = (node.y + geometry::NODE_HEIGHT / 2.0) * self.zoom;
        self.scroll_handle.set_offset(point(
            px(-(center_x - f32::from(viewport.width) / 2.0).max(0.0)),
            px(-(center_y - f32::from(viewport.height) / 2.0).max(0.0)),
        ));
        cx.notify();
    }

    /// Scroll the field so the visible nodes' bounding box is
    /// centered in the viewport (squeeze is the "make it all fit"
    /// half, fit is the "take me there" half).
    fn fit(&mut self, _: &Fit, _window: &mut Window, cx: &mut Context<Self>) {
        let positions: Vec<(f32, f32)> = self.nodes.iter().map(|node| (node.x, node.y)).collect();
        let Some((min_x, min_y, max_x, max_y)) = geometry::nodes_bbox(&positions) else {
            self.status = "nothing to fit".into();
            cx.notify();
            return;
        };
        let viewport = self.scroll_handle.bounds().size;
        let center = (
            (min_x + max_x) / 2.0 * self.zoom,
            (min_y + max_y) / 2.0 * self.zoom,
        );
        // Offsets grow negative as content scrolls up/left, and live
        // in zoomed (rendered) coordinates.
        self.scroll_handle.set_offset(point(
            px(-(center.0 - f32::from(viewport.width) / 2.0).max(0.0)),
            px(-(center.1 - f32::from(viewport.height) / 2.0).max(0.0)),
        ));
        cx.notify();
    }

    // === Zoom ===

    const MIN_ZOOM: f32 = 0.3;
    const MAX_ZOOM: f32 = 2.5;

    fn zoom_in(&mut self, _: &ZoomIn, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom * 1.2, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom / 1.2, cx);
    }

    fn zoom_reset(&mut self, _: &ZoomReset, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(1.0, cx);
    }

    /// Change zoom keeping the viewport center anchored on the same
    /// field point, so zooming feels like moving toward/away from
    /// what you're looking at.
    fn set_zoom(&mut self, new_zoom: f32, cx: &mut Context<Self>) {
        self.set_zoom_anchored(new_zoom, None, cx);
    }

    /// Change zoom keeping the field point under `anchor` (a window-space
    /// point, e.g. the pinch center) fixed on screen. With `None` the
    /// viewport center is held instead — used by the zoom in/out actions,
    /// so those feel like moving toward/away from what you're looking at.
    fn set_zoom_anchored(
        &mut self,
        new_zoom: f32,
        anchor: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let new_zoom = new_zoom.clamp(Self::MIN_ZOOM, Self::MAX_ZOOM);
        if (new_zoom - self.zoom).abs() < f32::EPSILON {
            return;
        }
        let bounds = self.scroll_handle.bounds();
        let viewport = bounds.size;
        let offset = self.scroll_handle.offset();
        // Anchor in viewport-local coordinates: the pinch center converted
        // out of window space, or the viewport center when no anchor given.
        let anchor = anchor
            .map(|p| p - bounds.origin)
            .unwrap_or_else(|| point(viewport.width / 2.0, viewport.height / 2.0));
        // Field-space point currently under the anchor. Offsets are <= 0 and
        // grow negative as content scrolls down/right, so subtracting the
        // offset shifts the anchor into content space before dividing out
        // the old zoom.
        let field = (
            (f32::from(anchor.x) - f32::from(offset.x)) / self.zoom,
            (f32::from(anchor.y) - f32::from(offset.y)) / self.zoom,
        );
        self.zoom = new_zoom;
        // Re-derive the scroll offset so that same field point lands back
        // under the anchor. Clamp to <= 0 (can't scroll past the top-left
        // origin), matching the invariant the rest of the panel relies on.
        self.scroll_handle.set_offset(point(
            px(-(field.0 * new_zoom - f32::from(anchor.x)).max(0.0)),
            px(-(field.1 * new_zoom - f32::from(anchor.y)).max(0.0)),
        ));
        self.status = format!("zoom {:.0}%", self.zoom * 100.0).into();
        cx.notify();
    }

    /// Trackpad pinch-to-zoom. `delta` is the incremental magnification for
    /// this event (0.1 == +10%), so the running gesture compounds naturally.
    /// Anchored at the pinch center so the field point under the fingers
    /// stays put. Pinch events dispatch by hitbox, so this fires whenever
    /// the pointer is over the map — the panel need not hold focus.
    fn handle_pinch(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        if event.delta == 0.0 {
            return;
        }
        let new_zoom = self.zoom * (1.0 + event.delta);
        self.set_zoom_anchored(new_zoom, Some(event.position), cx);
    }

    /// Apply `(node_ix, target)` moves as one undoable operation,
    /// appending a `node_moved` event per changed node. Returns how
    /// many nodes actually moved.
    fn apply_layout(&mut self, targets: &[(usize, (f32, f32))], cx: &mut Context<Self>) -> usize {
        let mut op_moves = Vec::new();
        for &(node_ix, (x, y)) in targets {
            let Some(node) = self.nodes.get_mut(node_ix) else {
                continue;
            };
            if node.x == x && node.y == y {
                continue;
            }
            let from = (node.x, node.y);
            node.x = x;
            node.y = y;
            let item = &self.items[node.item_ix];
            self.positions.insert(item.id.clone(), (x, y));
            op_moves.push((item.id.clone(), from, (x, y)));
        }
        let moved = op_moves.len();
        for (id, _, (x, y)) in &op_moves {
            self.append_to_log(store::node_moved_event(id, *x, *y), cx);
        }
        if !op_moves.is_empty() {
            self.undo_stack.push(Op::Move(op_moves));
        }
        moved
    }

    // === Filters & lenses ===

    fn toggle_status_filter(&mut self, status: Status, cx: &mut Context<Self>) {
        self.filter.toggle_status(status);
        self.active_lens = None;
        self.rebuild_scene();
        cx.notify();
    }

    fn clear_filter(&mut self, _: &ClearFilter, window: &mut Window, cx: &mut Context<Self>) {
        self.filter = FilterState::default();
        self.active_lens = None;
        self.search_editor.update(cx, |editor, cx| {
            editor.set_text("", window, cx);
        });
        self.rebuild_scene();
        cx.notify();
    }

    fn apply_lens(&mut self, action: &ApplyLens, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lens) = self.lenses.get(&action.name) else {
            self.status = format!("no lens named {}", action.name).into();
            cx.notify();
            return;
        };
        let lens = lens.clone();
        self.filter = lens.to_filter();
        let query = self.filter.query.clone();
        self.active_lens = Some(action.name.clone());
        self.search_editor.update(cx, |editor, cx| {
            editor.set_text(query, window, cx);
        });
        // Setting editor text re-fires BufferEdited, which clears
        // active_lens; restore it after.
        self.active_lens = Some(action.name.clone());
        // Restore the lens's saved geometry: a lens organizes a
        // workstream by layout as well as filter. Older lenses saved no
        // positions, so this is a no-op for them.
        for (id, x, y) in &lens.positions {
            self.positions.insert(id.clone(), (*x, *y));
        }
        self.rebuild_scene();
        self.status = format!("lens: {}", action.name).into();
        cx.notify();
    }

    /// All currently-visible nodes' geometry, snapshotted into a lens.
    fn current_positions(&self) -> Vec<(String, f32, f32)> {
        self.nodes
            .iter()
            .map(|node| (self.items[node.item_ix].id.clone(), node.x, node.y))
            .collect()
    }

    /// Save the current filter + query + layout under `name`, appending
    /// a `lens_saved` event and folding it in memory (last-write-wins).
    fn save_lens(&mut self, name: String, cx: &mut Context<Self>) {
        let visible: Vec<&str> = ALL_STATUSES
            .iter()
            .filter(|status| self.filter.visible_statuses.contains(status))
            .map(|status| status.as_str())
            .collect();
        let query = self.filter.query.trim().to_string();
        let positions = self.current_positions();
        self.append_to_log(
            store::lens_saved_event(&name, &visible, &query, &positions),
            cx,
        );
        self.lenses.insert(
            name.clone(),
            Lens {
                name: name.clone(),
                visible_statuses: visible.iter().map(|s| s.to_string()).collect(),
                query,
                positions,
            },
        );
        self.active_lens = Some(name.clone());
        self.status = format!("saved lens {name}").into();
    }

    /// Overwrite the active lens in place (the "Save" affordance). With
    /// no active lens, nothing to update — use "+" to create one.
    fn save_active_lens(&mut self, _: &SaveActiveLens, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.active_lens.clone() else {
            self.status = "no active lens \u{2014} use + to create one".into();
            cx.notify();
            return;
        };
        self.save_lens(name, cx);
        cx.notify();
    }

    fn delete_lens(&mut self, action: &DeleteLens, _window: &mut Window, cx: &mut Context<Self>) {
        if self.lenses.remove(&action.name).is_none() {
            return;
        }
        if self.active_lens.as_deref() == Some(action.name.as_str()) {
            self.active_lens = None;
        }
        self.append_to_log(store::lens_deleted_event(&action.name), cx);
        self.status = format!("deleted lens {}", action.name).into();
        cx.notify();
    }

    fn save_lens_prompt(
        &mut self,
        _: &SaveLensPrompt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.naming_lens = true;
        let focus = self.lens_name_editor.focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn confirm_lens_name(
        &mut self,
        _: &ConfirmLensName,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = self.lens_name_editor.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            self.status = "lens needs a name".into();
            cx.notify();
            return;
        }
        self.save_lens(name, cx);
        self.naming_lens = false;
        self.lens_name_editor.update(cx, |editor, cx| {
            editor.set_text("", window, cx);
        });
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    // === Detail panel & notes ===

    /// `enter`: open the detail panel on the focused item — or, while
    /// naming a lens, confirm the name instead (the single-line editor
    /// lets `enter` bubble up to this context).
    fn toggle_detail(&mut self, _: &ToggleDetail, window: &mut Window, cx: &mut Context<Self>) {
        if self.naming_lens {
            self.confirm_lens_name(&ConfirmLensName, window, cx);
            return;
        }
        if self.focused_item().is_none() {
            self.status = "no focused item".into();
            cx.notify();
            return;
        }
        window.dispatch_action(Box::new(crate::detail_panel::ToggleDetailPanelFocus), cx);
    }

    /// Whole-section replace of an item's `## Notes` (`brief_edited`,
    /// workcat-db commit 87fb2b8), called by the detail panel. The
    /// brief file re-materializes at the next checkpoint's fold.
    /// Returns a human-readable status line.
    pub fn save_notes_for(&mut self, id: &str, text: String, cx: &mut Context<Self>) -> String {
        self.save_section_for(id, "Notes", text, cx)
    }

    /// Whole-section replace of any brief section (`brief_edited`,
    /// whole-section, workcat-db commit 87fb2b8). Backs both the Notes
    /// editor and the editable Hazards list. Returns a human-readable
    /// status line; no-ops (and says so) when the text is unchanged.
    pub(crate) fn save_section_for(
        &mut self,
        id: &str,
        heading: &str,
        text: String,
        cx: &mut Context<Self>,
    ) -> String {
        let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
            return format!("no item {id}");
        };
        let existing = item
            .sections
            .iter()
            .find(|(section_heading, _)| section_heading.eq_ignore_ascii_case(heading))
            .map(|(_, body)| body.as_str())
            .unwrap_or_default();
        if existing == text {
            return format!("{} unchanged", heading.to_lowercase());
        }
        if let Some(section) = item
            .sections
            .iter_mut()
            .find(|(section_heading, _)| section_heading.eq_ignore_ascii_case(heading))
        {
            section.1 = text.clone();
        } else {
            item.sections.push((heading.to_string(), text.clone()));
        }
        let id = id.to_string();
        self.append_to_log(store::brief_edited_event(&id, heading, &text), cx);
        let message = format!("{} saved (uncommitted)", heading.to_lowercase());
        self.status = message.clone().into();
        cx.notify();
        message
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
        self.arm_auto_checkpoint(cx);
    }

    /// Debounced auto-checkpoint: every mutation (arriving through
    /// `append_to_log`) restarts a timer, so a burst of edits settles
    /// into a single checkpoint ~`AUTO_CHECKPOINT_DELAY` after the last
    /// one. Dropping the prior task cancels the earlier timer. The
    /// checkpoint's own progress ("acquiring lock", "folding", ...)
    /// streams into the status line as before.
    fn arm_auto_checkpoint(&mut self, cx: &mut Context<Self>) {
        self.auto_checkpoint_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(AUTO_CHECKPOINT_DELAY)
                .await;
            this.update(cx, |this, cx| {
                if this.pending_events > 0 && !this.checkpoint_running {
                    this.spawn_checkpoint(None, "auto checkpoint".into(), cx);
                }
            })
            .ok();
        }));
    }

    /// Set the focused item's status by value (the detail panel's undo
    /// path). Thin wrapper over [`Self::set_status`].
    pub(crate) fn set_focused_status(
        &mut self,
        status: Status,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_status(
            &SetStatus {
                status: status.as_str().to_string(),
            },
            window,
            cx,
        );
    }

    pub(crate) fn set_status(
        &mut self,
        action: &SetStatus,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        let previous = item.status;
        item.status = status;
        self.undo_stack
            .push(Op::SetStatus(id.clone(), previous, status));
        if !self.filter.visible_statuses.contains(&status) {
            self.rebuild_scene();
        }
        // Append the event and let the debounced auto-checkpoint commit
        // it (with any pending drag/note events) once activity settles.
        self.status = format!("status: {} -> {}", id8, status.label()).into();
        self.append_to_log(store::status_set_event(&id, status.as_str()), cx);
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
        let map = cx.weak_entity();
        let focus = self.focus_handle.clone();
        let context_menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let menu = Self::populate_status_menu(menu.context(focus), current, map);
            menu.separator()
                .action("Open detail panel", Box::new(ToggleDetail))
                .action("Checkpoint now", Box::new(Checkpoint))
        });
        self.show_context_menu(context_menu, position, window, cx);
    }

    /// Right-click on the background: map-wide operations. The menu
    /// teaches the gestures (DR-003 ruling 8) by showing bindings.
    fn deploy_background_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lens_names: Vec<String> = self.lenses.keys().cloned().collect();
        let active_lens = self.active_lens.clone();
        let filter_is_default = self.filter.is_default();
        let context_menu = ContextMenu::build(window, cx, |mut menu, _, _| {
            menu = menu
                .context(self.focus_handle.clone())
                .action("Fit", Box::new(Fit))
                .action("Squeeze", Box::new(Squeeze))
                .action("Auto-arrange", Box::new(AutoArrange))
                .action("Zoom In", Box::new(ZoomIn))
                .action("Zoom Out", Box::new(ZoomOut))
                .action("Zoom 100%", Box::new(ZoomReset))
                .action("Undo", Box::new(Undo))
                .action("Redo", Box::new(Redo))
                .separator()
                .header("Lenses");
            for name in &lens_names {
                let label = if active_lens.as_deref() == Some(name.as_str()) {
                    format!("{name} (active)")
                } else {
                    name.clone()
                };
                menu = menu.action(label, Box::new(ApplyLens { name: name.clone() }));
            }
            // "Save" overwrites the active lens in place; "New lens..."
            // (the "+") creates one. With no active lens, only New.
            if let Some(active) = &active_lens {
                menu = menu.action(format!("Save {active}"), Box::new(SaveActiveLens));
            }
            menu = menu.action("New lens\u{2026}", Box::new(SaveLensPrompt));
            if let Some(active) = &active_lens {
                menu = menu.action(
                    format!("Delete Lens {active}"),
                    Box::new(DeleteLens {
                        name: active.clone(),
                    }),
                );
            }
            if !filter_is_default {
                menu = menu.action("Clear Filter", Box::new(ClearFilter));
            }
            menu.separator()
                .action("Checkpoint Now", Box::new(Checkpoint))
        });
        self.show_context_menu(context_menu, position, window, cx);
    }

    fn show_context_menu(
        &mut self,
        context_menu: Entity<ContextMenu>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    /// The single status->color source of truth: one saturated "dot"
    /// color per status. Pill tints, the status menu, lens chips, and
    /// node fills all derive from these via alpha/blend, so light and
    /// dark mode fall out of blending the dot into the theme surface.
    pub fn status_color(status: Status) -> Hsla {
        Hsla::from(match status {
            Status::NotStarted => gpui::rgb(0x888780),
            Status::Started => gpui::rgb(0x378add),
            Status::Paused => gpui::rgb(0xef9f27),
            Status::Blocked => gpui::rgb(0xe24b4a),
            Status::Implemented => gpui::rgb(0x97c459),
            Status::Merged => gpui::rgb(0x639922),
            Status::Complete => gpui::rgb(0x3b6d11),
            Status::Canceled => gpui::rgb(0xb4b2a9),
            Status::Unknown => gpui::rgb(0xc9c7be),
        })
    }

    /// Shared "Set status" menu section (DR-003 ruling 8: one component
    /// so the map node menu and the detail-panel pill never drift in
    /// order, labels, shortcuts, or the current-status check). Statuses
    /// run in lifecycle order with a divider before the terminal
    /// choices; each entry carries the `SetStatus` action so the 1-9
    /// keybindings show as badges and fire while the menu is open, and
    /// picking one advances the map's focused item.
    pub(crate) fn populate_status_menu(
        mut menu: ContextMenu,
        current: Option<Status>,
        map: WeakEntity<WorkcatMapView>,
    ) -> ContextMenu {
        menu = menu.header("Set status");
        let mut divided = false;
        for status in ALL_STATUSES {
            if status.is_terminal_choice() && !divided {
                menu = menu.separator();
                divided = true;
            }
            let map = map.clone();
            menu = menu.toggleable_entry(
                status.label(),
                current == Some(status),
                ui::IconPosition::Start,
                Some(Box::new(SetStatus {
                    status: status.as_str().to_string(),
                })),
                move |window, cx| {
                    map.update(cx, |map, cx| {
                        map.set_status(
                            &SetStatus {
                                status: status.as_str().to_string(),
                            },
                            window,
                            cx,
                        );
                    })
                    .ok();
                },
            );
        }
        menu
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .border_b_1()
            .border_color(colors.border)
            .child(Label::new("Workcat Map").weight(gpui::FontWeight::BOLD))
            .when_some(self.active_lens.clone(), |this, lens| {
                this.child(
                    Label::new(format!("lens: {lens}"))
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
            })
            .when((self.zoom - 1.0).abs() > 0.01, |this| {
                this.child(
                    Label::new(format!("{:.0}%", self.zoom * 100.0))
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
            })
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
                    .color(Color::Muted)
                    .truncate(),
            )
    }

    /// The filter row: the search editor, one toggle chip per status
    /// (dot + visible-count), and the lens-name input while naming.
    fn render_filter_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let mut counts: HashMap<Status, usize> = HashMap::new();
        for item in &self.items {
            *counts.entry(item.status).or_default() += 1;
        }
        // A single fixed-height row: many status chips no longer fit one
        // line at every panel width, so this scrolls horizontally rather
        // than wrapping (wrapping made the whole top bar grow tall).
        let mut row = h_flex()
            .id("workcat-filter-row")
            .w_full()
            .px_2()
            .py_1p5()
            .gap_2()
            .flex_none()
            .overflow_x_scroll()
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex_none()
                    .w(px(200.))
                    .px_1p5()
                    .py_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.editor_background)
                    .child(self.search_editor.clone()),
            )
            .child(
                Button::new("lenses", "Lenses")
                    .label_size(LabelSize::Small)
                    .tooltip(ui::Tooltip::text(
                        "Saved lenses (also: right-click the map)",
                    ))
                    .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
                        this.deploy_background_menu(event.position(), window, cx);
                    })),
            )
            // "Save" updates the active lens (filter + layout); the "+"
            // creates a new one. Save only shows with an active lens.
            .when_some(self.active_lens.clone(), |row, active| {
                row.child(
                    Button::new("save-active-lens", "Save")
                        .label_size(LabelSize::Small)
                        .tooltip(ui::Tooltip::text(format!("Update lens \u{201c}{active}\u{201d}")))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save_active_lens(&SaveActiveLens, window, cx);
                        })),
                )
            })
            .child(
                Button::new("new-lens", "+")
                    .label_size(LabelSize::Small)
                    .tooltip(ui::Tooltip::text("New lens from the current filter and layout"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.save_lens_prompt(&SaveLensPrompt, window, cx);
                    })),
            );
        for status in ALL_STATUSES {
            let visible = self.filter.visible_statuses.contains(&status);
            let count = counts.get(&status).copied().unwrap_or(0);
            let status_color = Self::status_color(status);
            row = row.child(
                h_flex()
                    .id(SharedString::from(format!("chip-{}", status.as_str())))
                    .px_1p5()
                    .py_0p5()
                    .gap_1p5()
                    .rounded_md()
                    .border_1()
                    .cursor_pointer()
                    // Zero-count lenses read as dimmed but stay clickable.
                    .opacity(if count == 0 { 0.45 } else { 1.0 })
                    .border_color(if visible {
                        status_color.alpha(0.9)
                    } else {
                        colors.border_variant
                    })
                    .bg(if visible {
                        status_color.alpha(0.15)
                    } else {
                        colors.element_background
                    })
                    .tooltip(ui::Tooltip::text(format!(
                        "{} \u{2014} click to {}",
                        status.label(),
                        if visible { "hide" } else { "show" }
                    )))
                    .child(
                        div()
                            .flex_none()
                            .w_2()
                            .h_2()
                            .rounded_full()
                            .bg(status_color),
                    )
                    // Dot + humanized name + count; the row doubles as
                    // the map's status legend.
                    .child(
                        Label::new(status.label())
                            .size(LabelSize::Small)
                            .color(if visible { Color::Default } else { Color::Muted }),
                    )
                    .child(
                        Label::new(format!("{count}"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                            this.toggle_status_filter(status, cx);
                            cx.stop_propagation();
                        }),
                    ),
            );
        }
        if self.naming_lens {
            row = row
                .child(
                    div()
                        .min_w(px(120.))
                        .px_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(colors.border_focused)
                        .child(self.lens_name_editor.clone()),
                )
                .child(
                    Button::new("save-lens", "Save")
                        .label_size(LabelSize::XSmall)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.confirm_lens_name(&ConfirmLensName, window, cx);
                        })),
                );
        }
        row
    }

    /// Dependency edges, drawn under the nodes: a line from each
    /// dependent to its dependency with a small arrowhead at the
    /// dependency end.
    fn render_edges(&self, cx: &Context<Self>) -> impl IntoElement {
        let color = cx.theme().colors().text_muted.alpha(0.5);
        let focused_color = cx.theme().colors().text_accent;
        let focused = self.focused_node;
        let zoom = self.zoom;
        let segments: Vec<((f32, f32), (f32, f32), bool)> = self
            .edges
            .iter()
            .map(|&(from_ix, to_ix)| {
                let (from, to) = (&self.nodes[from_ix], &self.nodes[to_ix]);
                let center = |n: &MapNode| {
                    (
                        (n.x + geometry::NODE_WIDTH / 2.0) * zoom,
                        (n.y + geometry::NODE_HEIGHT / 2.0) * zoom,
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
                            x2 - ux * geometry::NODE_HEIGHT * 0.6 * zoom,
                            y2 - uy * geometry::NODE_HEIGHT * 0.6 * zoom,
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
        let status_color = Self::status_color(item.status);
        let dragging = self.drag.as_ref().is_some_and(|drag| {
            drag.pressed_ix == node_ix || drag.starts.iter().any(|(ix, _)| *ix == node_ix)
        });
        let focused = self.focused_node == Some(node_ix);
        let selected = self.selected.contains(&item.id);
        let zoom = self.zoom;
        div()
            .absolute()
            .left(px(node.x * zoom))
            .top(px(node.y * zoom))
            .w(px(geometry::NODE_WIDTH * zoom))
            .h(px(geometry::NODE_HEIGHT * zoom))
            .px(px(6.0 * zoom))
            .py(px(4.0 * zoom))
            .gap(px(5.0 * zoom))
            .flex()
            .items_start()
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
                } else if selected {
                    this.border_2().border_color(colors.text_accent)
                } else {
                    this.border_1().border_color(status_color.alpha(0.8))
                }
            })
            // Status-tinted fill in the same light language as the filter
            // pills. We blend the tint into the opaque panel background
            // rather than laying it on as a translucent overlay, so the
            // fill is fully opaque: it clips the dependency edges that run
            // beneath a node (a translucent fill let them bleed through)
            // while staying as light as the pills. Focused/selected nodes
            // get a slightly stronger tint.
            .bg(if focused || selected {
                colors.panel_background.blend(status_color.alpha(0.32))
            } else {
                colors.panel_background.blend(status_color.alpha(0.16))
            })
            .cursor_grab()
            .child(
                div()
                    .flex_none()
                    .mt(px(4.0 * zoom))
                    .w(px(8.0 * zoom))
                    .h(px(8.0 * zoom))
                    .rounded_full()
                    .bg(status_color),
            )
            .child(
                div()
                    .text_size(px(12.5 * zoom))
                    .line_height(px(15.5 * zoom))
                    .overflow_hidden()
                    .child(node.label.clone()),
            )
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

    /// The in-flight selection rectangle (stored in field
    /// coordinates, rendered zoomed).
    fn render_rect_select(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let rect = self.rect_select.as_ref()?;
        let (min_x, min_y, max_x, max_y) = geometry::normalize_rect(rect.start, rect.current);
        let zoom = self.zoom;
        let accent = cx.theme().colors().text_accent;
        Some(
            div()
                .absolute()
                .left(px(min_x * zoom))
                .top(px(min_y * zoom))
                .w(px((max_x - min_x) * zoom))
                .h(px((max_y - min_y) * zoom))
                .border_1()
                .border_color(accent)
                .bg(accent.alpha(0.08)),
        )
    }

    /// Captures the field content's window-space origin at paint time
    /// so background gestures can map pointer positions into field
    /// coordinates (the field scrolls, so this shifts per frame).
    fn render_origin_probe(&self) -> impl IntoElement {
        let origin = self.field_origin.clone();
        canvas(
            |_, _, _| (),
            move |bounds, _, _, _| {
                origin.set(bounds.origin);
            },
        )
        .absolute()
        .size_full()
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
            .w(px(geometry::FIELD_WIDTH * self.zoom))
            .h(px(geometry::FIELD_HEIGHT * self.zoom))
            .map(|this| {
                if self.pan.is_some() {
                    this.cursor_grabbing()
                } else {
                    this.cursor_grab()
                }
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.begin_background_gesture(event, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.deploy_background_menu(event.position, window, cx);
                }),
            )
            .child(self.render_origin_probe())
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
        content = content.children(self.render_rect_select(cx));
        if self.drag.is_some() || self.rect_select.is_some() || self.pan.is_some() {
            content = content.child(self.render_drag_listener(cx));
        }
        div()
            .id("workcat-map-field")
            .flex_grow(1.)
            .overflow_scroll()
            .track_scroll(&self.scroll_handle)
            .on_pinch(cx.listener(|this, event: &PinchEvent, _window, cx| {
                this.handle_pinch(event, cx);
            }))
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Once loaded, restore the saved startup arrangement by applying
        // the "default" lens (filter + layout), so the map opens the way
        // it was last left rather than on a mixed/degenerate layout.
        if self.needs_default_lens && !self.loading {
            self.needs_default_lens = false;
            if self.lenses.contains_key(DEFAULT_LENS) {
                self.apply_lens(
                    &ApplyLens {
                        name: DEFAULT_LENS.to_string(),
                    },
                    window,
                    cx,
                );
            }
        }
        if self.needs_initial_fit
            && !self.loading
            && self.scroll_handle.bounds().size.height > px(0.)
        {
            self.needs_initial_fit = false;
            self.fit(&Fit, window, cx);
        }
        // When a text input (the search box or the lens-name box) is
        // focused, drop the bare-letter/digit bindings by switching the
        // key context. Otherwise typing a lens name like "eda" would
        // fire Fit/AutoArrange/Squeeze/... instead of inserting the
        // characters. `enter`/`escape` are still bound in the input
        // context so confirm/cancel keep working while typing.
        let input_focused = self
            .search_editor
            .focus_handle(cx)
            .contains_focused(window, cx)
            || self
                .lens_name_editor
                .focus_handle(cx)
                .contains_focused(window, cx);
        let key_context = if input_focused {
            "WorkcatMapInput"
        } else {
            "WorkcatMap"
        };
        v_flex()
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::set_status))
            .on_action(cx.listener(Self::checkpoint))
            .on_action(cx.listener(Self::clear_focus))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::toggle_detail))
            .on_action(cx.listener(Self::auto_arrange))
            .on_action(cx.listener(Self::squeeze))
            .on_action(cx.listener(Self::fit))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::zoom_reset))
            .on_action(cx.listener(Self::clear_filter))
            .on_action(cx.listener(Self::apply_lens))
            .on_action(cx.listener(Self::delete_lens))
            .on_action(cx.listener(Self::save_lens_prompt))
            .on_action(cx.listener(Self::save_active_lens))
            .on_action(cx.listener(Self::confirm_lens_name))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .child(self.render_filter_row(cx))
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

// The dock panel wrapper combining this view with WorkcatDetailView
// lives in workcat_panel.rs.
