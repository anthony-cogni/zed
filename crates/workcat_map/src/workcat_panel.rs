//! The Workcat dock panel: `WorkcatMapView` (the gesture surface) and
//! `WorkcatDetailView` (the reading/acting surface) side by side inside
//! one `workspace::Panel`, matching the design mockup's overall layout
//! (a resizable detail sidebar beside the map's flex-grow field).
//!
//! Combining them into one panel — rather than registering each view
//! as its own dock panel — keeps them simultaneously visible without
//! competing for a dock's one active-panel slot. That frees the *other*
//! dock for the real agent panel (which defaults to the right dock),
//! so opening a work item's originating conversation shows the agent
//! panel and the workcat panel side by side rather than three panels
//! fighting over two docks. The two views still talk to each other
//! exactly as before, through the `WorkcatMapHandle` global; this panel
//! only changes how they're laid out and registered.

use anyhow::Result;
use gpui::{
    App, AsyncWindowContext, Context, DispatchPhase, Entity, EventEmitter, FocusHandle, Focusable,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, WeakEntity, Window,
    canvas, px,
};
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::detail_panel::{ToggleDetailPanelFocus, WorkcatDetailView};
use crate::panel::{ToggleFocus, WorkcatMapView};

const WORKCAT_PANEL_KEY: &str = "WorkcatPanel";
/// Initial width of the detail sidebar within the combined panel; the
/// user can drag it wider/narrower via the splitter.
const DEFAULT_DETAIL_WIDTH: f32 = 380.;
const MIN_DETAIL_WIDTH: f32 = 260.;
const MAX_DETAIL_WIDTH: f32 = 720.;
/// Default width of the whole combined panel (detail sidebar + a
/// reasonably-sized map field).
const DEFAULT_WIDTH: f32 = 1040.;
/// Width of the draggable splitter between detail and map.
const SPLITTER_WIDTH: f32 = 6.;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|workspace, _: &ToggleFocus, window, cx| {
                workspace.toggle_panel_focus::<WorkcatPanel>(window, cx);
            })
            .register_action(|workspace, _: &ToggleDetailPanelFocus, window, cx| {
                if let Some(panel) = workspace.panel::<WorkcatPanel>(cx) {
                    panel.update(cx, |panel, cx| panel.focus_detail(window, cx));
                }
                workspace.focus_panel::<WorkcatPanel>(window, cx);
            });
    })
    .detach();
}

/// The dock panel wrapper: the map and detail views side by side.
pub struct WorkcatPanel {
    map: Entity<WorkcatMapView>,
    detail: Entity<WorkcatDetailView>,
    position: DockPosition,
    detail_width: Pixels,
    /// `(pointer x at drag start, detail_width at drag start)`, while
    /// the splitter between detail and map is being dragged.
    dragging_split: Option<(Pixels, Pixels)>,
}

impl WorkcatPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let map = cx.new(|cx| WorkcatMapView::new(window, cx));
        let detail = cx.new(|cx| WorkcatDetailView::new(workspace, window, cx));
        Self {
            map,
            detail,
            // The agent panel defaults to the right dock; putting the
            // combined workcat panel on the left keeps both visible at
            // once with no dock-activation race between them.
            position: DockPosition::Left,
            detail_width: px(DEFAULT_DETAIL_WIDTH),
            dragging_split: None,
        }
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let handle = workspace.clone();
        handle.update_in(&mut cx, move |_workspace, window, cx| {
            cx.new(|cx| Self::new(workspace.clone(), window, cx))
        })
    }

    fn focus_detail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.detail.focus_handle(cx);
        window.focus(&handle, cx);
    }

    // === Detail/map splitter drag ===

    fn begin_split_drag(&mut self, pointer: Point<Pixels>, cx: &mut Context<Self>) {
        self.dragging_split = Some((pointer.x, self.detail_width));
        cx.notify();
    }

    fn update_split_drag(&mut self, pointer: Point<Pixels>, cx: &mut Context<Self>) {
        let Some((start_x, start_width)) = self.dragging_split else {
            return;
        };
        let width = (start_width + (pointer.x - start_x))
            .max(px(MIN_DETAIL_WIDTH))
            .min(px(MAX_DETAIL_WIDTH));
        self.detail_width = width;
        cx.notify();
    }

    fn end_split_drag(&mut self, cx: &mut Context<Self>) {
        if self.dragging_split.take().is_some() {
            cx.notify();
        }
    }

    /// While the splitter drag is live, a window-level listener overlay
    /// keeps tracking the gesture even if the pointer leaves the thin
    /// splitter hitbox — the same technique the map view uses for node
    /// drags (`render_drag_listener`).
    fn render_split_drag_listener(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity: WeakEntity<Self> = cx.entity().downgrade();
        canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                window.on_mouse_event({
                    let entity = entity.clone();
                    move |event: &MouseMoveEvent, phase: DispatchPhase, _, cx| {
                        if phase.bubble() {
                            entity
                                .update(cx, |this, cx| this.update_split_drag(event.position, cx))
                                .ok();
                        }
                    }
                });
                window.on_mouse_event({
                    move |_: &MouseUpEvent, phase: DispatchPhase, _, cx| {
                        if phase.bubble() {
                            entity.update(cx, |this, cx| this.end_split_drag(cx)).ok();
                        }
                    }
                });
            },
        )
        .absolute()
        .size_full()
    }
}

impl EventEmitter<PanelEvent> for WorkcatPanel {}

impl Focusable for WorkcatPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // The map is the primary gesture surface (drag, 1-9 status keys,
        // undo/redo, fit/squeeze); focusing it is what makes those
        // shortcuts fire when the dock activates this panel. The detail
        // view is still independently focusable by clicking into it
        // (e.g. the notes editor), or via `ToggleDetailPanelFocus`.
        self.map.focus_handle(cx)
    }
}

impl Render for WorkcatPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let dragging = self.dragging_split.is_some();
        h_flex()
            .size_full()
            .relative()
            .child(
                div()
                    .flex_none()
                    .w(self.detail_width)
                    .h_full()
                    .overflow_hidden()
                    .child(self.detail.clone()),
            )
            .child(
                div()
                    .id("workcat-split")
                    .flex_none()
                    .w(px(SPLITTER_WIDTH))
                    .h_full()
                    .cursor_col_resize()
                    .border_r_1()
                    .border_color(colors.border)
                    .hover(|this| this.bg(colors.border_focused))
                    .when(dragging, |this| this.bg(colors.border_focused))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                            this.begin_split_drag(event.position, cx);
                            cx.stop_propagation();
                        }),
                    ),
            )
            .child(
                div()
                    .id("workcat-map-slot")
                    .flex_grow(1.)
                    // Without an explicit min-width, a flex-grow child
                    // defaults to sizing itself around its intrinsic
                    // content — and the map's field is 4800px wide.
                    // That distorted the whole panel's width and broke
                    // its own scroll clipping. min_w(0) + overflow_hidden
                    // clip it to whatever space is actually available.
                    .min_w(px(0.))
                    .h_full()
                    .overflow_hidden()
                    .child(self.map.clone()),
            )
            .when(dragging, |this| {
                this.child(self.render_split_drag_listener(cx))
            })
    }
}

impl Panel for WorkcatPanel {
    fn persistent_name() -> &'static str {
        "WorkcatPanel"
    }

    fn panel_key() -> &'static str {
        WORKCAT_PANEL_KEY
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

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        // Default layout shows the map and the detail pane together.
        true
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Blocks)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Workcat")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        11
    }
}
