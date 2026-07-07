//! The Workcat Map dock panel: ~100 draggable rectangles in GPUI.
//!
//! Dragging is direct manipulation: mouse down on a rect starts a gesture,
//! window-level mouse events (registered from a canvas overlay, the same
//! pattern pane resize handles use) move it, and mouse up commits the
//! layout to disk on a background executor.

use anyhow::Result;
use gpui::{
    App, AsyncWindowContext, Context, DispatchPhase, Entity, EventEmitter, FocusHandle, Focusable,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString,
    Subscription, Task, WeakEntity, Window, actions, canvas,
};
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::geometry::{self, Layout, NodePosition};
use crate::{store, threads};

actions!(
    workcat_map,
    [
        /// Toggles focus on the workcat map panel.
        ToggleFocus
    ]
);

const WORKCAT_MAP_PANEL_KEY: &str = "WorkcatMapPanel";
const DEFAULT_WIDTH: f32 = 560.;
/// How many nodes display real thread titles from the threads database.
const THREAD_NODE_COUNT: usize = 3;
const MAX_LABEL_CHARS: usize = 24;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<WorkcatMapPanel>(window, cx);
        });
    })
    .detach();
}

/// One rendered node: persisted geometry plus a display label.
struct MapNode {
    id: usize,
    x: f32,
    y: f32,
    label: SharedString,
    is_thread: bool,
}

/// An in-flight drag gesture.
struct DragState {
    ix: usize,
    pointer_start: Point<Pixels>,
    node_start: (f32, f32),
}

/// The map canvas. Standalone `Render`-able, needs no `Workspace`.
pub struct WorkcatMapView {
    focus_handle: FocusHandle,
    nodes: Vec<MapNode>,
    drag: Option<DragState>,
    focused: bool,
    status: SharedString,
    loading: bool,
    save_task: Option<Task<()>>,
    _load_task: Task<()>,
    _focus_subscriptions: Vec<Subscription>,
}

impl WorkcatMapView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let focus_subscriptions = vec![
            cx.on_focus(&focus_handle, window, |this, _, cx| {
                this.focused = true;
                log::info!("workcat_map: focus in");
                cx.notify();
            }),
            cx.on_blur(&focus_handle, window, |this, _, cx| {
                this.focused = false;
                log::info!("workcat_map: focus out");
                cx.notify();
            }),
        ];

        let load_task = cx.spawn(async move |this, cx| {
            let (layout, titles) = cx
                .background_spawn(async move {
                    let dir = store::resolve_dir();
                    let layout = match store::load_layout(&dir) {
                        Ok(layout) => layout,
                        Err(error) => {
                            log::warn!("workcat_map: failed to load layout: {error:#}");
                            None
                        }
                    };
                    let titles = match threads::load_recent_titles(THREAD_NODE_COUNT) {
                        Ok(titles) => titles,
                        Err(error) => {
                            log::warn!("workcat_map: failed to load thread titles: {error:#}");
                            Vec::new()
                        }
                    };
                    (layout, titles)
                })
                .await;
            this.update(cx, |this, cx| this.apply_loaded(layout, titles, cx))
                .ok();
        });

        Self {
            focus_handle,
            nodes: Vec::new(),
            drag: None,
            focused: false,
            status: "loading layout...".into(),
            loading: true,
            save_task: None,
            _load_task: load_task,
            _focus_subscriptions: focus_subscriptions,
        }
    }

    fn apply_loaded(&mut self, saved: Option<Layout>, titles: Vec<String>, cx: &mut Context<Self>) {
        let restored = saved.is_some();
        let layout = geometry::reconcile(saved, geometry::NODE_COUNT);
        self.nodes = layout
            .nodes
            .into_iter()
            .map(|node| {
                let title = titles.get(node.id).map(|title| truncate(title));
                MapNode {
                    is_thread: title.is_some(),
                    label: title.unwrap_or_else(|| format!("node {}", node.id)).into(),
                    id: node.id,
                    x: node.x,
                    y: node.y,
                }
            })
            .collect();
        let node0 = self.nodes.first();
        log::info!(
            "workcat_map: {} layout; {} nodes; node0 at ({:.1},{:.1}); thread-title nodes: {:?}",
            if restored { "restored" } else { "generated" },
            self.nodes.len(),
            node0.map_or(0.0, |n| n.x),
            node0.map_or(0.0, |n| n.y),
            self.nodes
                .iter()
                .filter(|node| node.is_thread)
                .map(|node| node.label.as_ref())
                .collect::<Vec<_>>(),
        );
        self.loading = false;
        self.status = if restored {
            format!("restored {} nodes from disk", self.nodes.len()).into()
        } else {
            format!("generated fresh layout ({} nodes)", self.nodes.len()).into()
        };
        if !restored {
            // Save the fresh layout so it survives restart even before the
            // first drag.
            self.persist("generated", None, cx);
        }
        cx.notify();
    }

    fn begin_drag(
        &mut self,
        ix: usize,
        pointer: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        let Some(node) = self.nodes.get(ix) else {
            return;
        };
        self.drag = Some(DragState {
            ix,
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
        let ix = drag.ix;
        if let Some(node) = self.nodes.get_mut(ix) {
            node.x = x;
            node.y = y;
        }
        cx.notify();
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        let moved_id = self.nodes.get(drag.ix).map(|node| node.id);
        self.persist("drag_end", moved_id, cx);
        cx.notify();
    }

    /// Persist the whole layout at gesture rest, off the main thread.
    fn persist(&mut self, reason: &'static str, moved_id: Option<usize>, cx: &mut Context<Self>) {
        let layout = Layout {
            version: 1,
            nodes: self
                .nodes
                .iter()
                .map(|node| NodePosition {
                    id: node.id,
                    x: node.x,
                    y: node.y,
                })
                .collect(),
        };
        let node_count = layout.nodes.len();
        self.save_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let dir = store::resolve_dir();
                    store::save_layout(&dir, &layout)?;
                    store::append_event(
                        &dir,
                        &serde_json::json!({
                            "event": reason,
                            "node": moved_id,
                            "at": chrono::Utc::now().to_rfc3339(),
                        }),
                    )?;
                    anyhow::Ok(dir)
                })
                .await;
            this.update(cx, |this, cx| {
                this.status = match result {
                    Ok(dir) => format!("saved {} nodes to {}", node_count, dir.display()).into(),
                    Err(error) => format!("save failed: {error:#}").into(),
                };
                cx.notify();
            })
            .ok();
        }));
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
            .child(
                Label::new(if self.focused {
                    "focus: in"
                } else {
                    "focus: out"
                })
                .color(if self.focused {
                    Color::Accent
                } else {
                    Color::Muted
                }),
            )
            .child(
                Label::new(self.status.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn render_node(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let node = &self.nodes[ix];
        let colors = cx.theme().colors().clone();
        let dragging = self.drag.as_ref().is_some_and(|drag| drag.ix == ix);
        div()
            .absolute()
            .left(px(node.x))
            .top(px(node.y))
            .w(px(geometry::NODE_WIDTH))
            .h(px(geometry::NODE_HEIGHT))
            .px_1()
            .flex()
            .items_center()
            .overflow_hidden()
            .rounded_sm()
            .border_1()
            .border_color(if dragging {
                colors.border_selected
            } else if node.is_thread {
                colors.text_accent
            } else {
                colors.border
            })
            .bg(if node.is_thread {
                colors.element_selected
            } else {
                colors.element_background
            })
            .cursor_grab()
            .child(
                Label::new(node.label.clone())
                    .size(LabelSize::XSmall)
                    .color(if node.is_thread {
                        Color::Accent
                    } else {
                        Color::Default
                    }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.begin_drag(ix, event.position, window, cx);
                    cx.stop_propagation();
                }),
            )
    }

    /// While a drag is live, register window-level move/up handlers via a
    /// canvas overlay so the gesture keeps tracking even when the pointer
    /// leaves the panel bounds.
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
        let mut field = div()
            .relative()
            .flex_grow(1.)
            .overflow_hidden()
            .border_2()
            .border_color(if self.focused {
                colors.border_focused
            } else {
                colors.border_variant
            });
        if self.loading {
            field = field.child(
                div()
                    .p_2()
                    .child(Label::new("loading...").color(Color::Muted)),
            );
        }
        for ix in 0..self.nodes.len() {
            let node = self.render_node(ix, cx);
            field = field.child(node);
        }
        if self.drag.is_some() {
            field = field.child(self.render_drag_listener(cx));
        }
        field
    }
}

fn truncate(title: &str) -> String {
    if title.chars().count() <= MAX_LABEL_CHARS {
        title.to_string()
    } else {
        let mut out: String = title.chars().take(MAX_LABEL_CHARS).collect();
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
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .child(self.render_field(cx))
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
