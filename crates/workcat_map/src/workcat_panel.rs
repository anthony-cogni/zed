//! The Workcat dock panel: `WorkcatMapView` (the gesture surface) and
//! `WorkcatDetailView` (the reading/acting surface) side by side inside
//! one `workspace::Panel`, matching the design mockup's overall layout
//! (a fixed-width detail sidebar beside the map's flex-grow field).
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
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels,
    Window, px,
};
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::detail_panel::{ToggleDetailPanelFocus, WorkcatDetailView};
use crate::panel::{ToggleFocus, WorkcatMapView};

const WORKCAT_PANEL_KEY: &str = "WorkcatPanel";
/// Fixed width of the detail sidebar within the combined panel.
const DETAIL_WIDTH: f32 = 380.;
/// Default width of the whole combined panel (detail sidebar + a
/// reasonably-sized map field).
const DEFAULT_WIDTH: f32 = 1040.;

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
}

impl WorkcatPanel {
    pub fn new(
        workspace: gpui::WeakEntity<Workspace>,
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
        }
    }

    pub async fn load(
        workspace: gpui::WeakEntity<Workspace>,
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
        h_flex()
            .size_full()
            .child(
                div()
                    .flex_none()
                    .w(px(DETAIL_WIDTH))
                    .h_full()
                    .border_r_1()
                    .border_color(colors.border)
                    .child(self.detail.clone()),
            )
            .child(div().flex_grow(1.).h_full().child(self.map.clone()))
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
