//! The Workcat Detail dock panel: a full, agent-panel-level pane for
//! the focused map item's brief — subject, status, ref, dependencies,
//! every brief section (State/Next/Context/Hazards), and an editable
//! Notes field.
//!
//! The map panel stays the gesture surface; this panel is the reading
//! surface. They communicate through a `WorkcatMapHandle` global: the
//! map view registers itself there, and this panel observes the map
//! entity, re-rendering on every focus/mutation notify. Writes flow
//! back through the map view (`save_notes_for`), keeping the event
//! append path and the pending-events counter in one place.

use anyhow::Result;
use editor::Editor;
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels,
    SharedString, Subscription, WeakEntity, Window, actions, px,
};
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::model::ItemMeta;
use crate::panel::{WorkcatMapHandle, WorkcatMapView, ZoomIn, ZoomOut, ZoomReset};

actions!(
    workcat_map,
    [
        /// Toggles focus on the workcat detail panel.
        ToggleDetailPanelFocus,
    ]
);

const WORKCAT_DETAIL_PANEL_KEY: &str = "WorkcatDetailPanel";
/// Minimum default width (px) for the reading pane on narrow windows.
const DEFAULT_WIDTH: f32 = 420.;
/// Fraction of the window width the reading pane defaults to (~1/5-1/4).
const DETAIL_WIDTH_FRACTION: f32 = 0.22;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleDetailPanelFocus, window, cx| {
            workspace.toggle_panel_focus::<WorkcatDetailPanel>(window, cx);
        });
    })
    .detach();
}

/// The detail view: renders whatever the map has focused.
pub struct WorkcatDetailView {
    focus_handle: FocusHandle,
    map: Option<WeakEntity<WorkcatMapView>>,
    notes_editor: Entity<Editor>,
    /// Which item id the notes editor currently holds text for.
    notes_item: Option<String>,
    /// Text zoom for the reading surface (title, meta, sections).
    zoom: f32,
    status: SharedString,
    _map_subscription: Option<Subscription>,
    _global_subscription: Subscription,
}

impl WorkcatDetailView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let notes_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 8, window, cx);
            editor.set_placeholder_text("Notes\u{2026}", window, cx);
            editor
        });
        // The map panel may register its handle before or after this
        // panel loads (panels load concurrently); observe the global
        // so a late-arriving map still gets picked up.
        let global_subscription = cx.observe_global::<WorkcatMapHandle>(|_, cx| {
            cx.notify();
        });
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            map: None,
            notes_editor,
            notes_item: None,
            zoom: 1.0,
            status: SharedString::default(),
            _map_subscription: None,
            _global_subscription: global_subscription,
        };
        this.ensure_subscribed(cx);
        this
    }

    /// Lazily attach to the map view once its handle exists; the map
    /// entity's own `notify` then drives this panel's re-renders.
    fn ensure_subscribed(&mut self, cx: &mut Context<Self>) {
        if self
            .map
            .as_ref()
            .is_some_and(|weak| weak.upgrade().is_some())
        {
            return;
        }
        let Some(map) = cx
            .try_global::<WorkcatMapHandle>()
            .and_then(|handle| handle.0.clone())
            .and_then(|weak| weak.upgrade())
        else {
            return;
        };
        self._map_subscription = Some(cx.observe(&map, |_, _, cx| cx.notify()));
        self.map = Some(map.downgrade());
    }

    fn focused_item(&self, cx: &App) -> Option<ItemMeta> {
        let map = self.map.as_ref()?.upgrade()?;
        map.read(cx).focused_item().cloned()
    }

    /// Keep the notes editor's text in step with the focused item.
    fn sync_notes_editor(
        &mut self,
        item: Option<&ItemMeta>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = item else {
            self.notes_item = None;
            return;
        };
        if self.notes_item.as_deref() == Some(item.id.as_str()) {
            return;
        }
        let notes = item.notes().unwrap_or_default().to_string();
        self.notes_item = Some(item.id.clone());
        self.notes_editor.update(cx, |editor, cx| {
            editor.set_text(notes, window, cx);
        });
    }

    const MIN_ZOOM: f32 = 0.7;
    const MAX_ZOOM: f32 = 2.0;

    fn zoom_in(&mut self, _: &ZoomIn, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom = (self.zoom * 1.1).clamp(Self::MIN_ZOOM, Self::MAX_ZOOM);
        cx.notify();
    }

    fn zoom_out(&mut self, _: &ZoomOut, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom = (self.zoom / 1.1).clamp(Self::MIN_ZOOM, Self::MAX_ZOOM);
        cx.notify();
    }

    fn zoom_reset(&mut self, _: &ZoomReset, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom = 1.0;
        cx.notify();
    }

    fn save_notes(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.notes_item.clone() else {
            return;
        };
        let Some(map) = self.map.as_ref().and_then(|weak| weak.upgrade()) else {
            self.status = "map panel not loaded".into();
            cx.notify();
            return;
        };
        let text = self.notes_editor.read(cx).text(cx).trim().to_string();
        let message = map.update(cx, |map, cx| map.save_notes_for(&id, text, cx));
        self.status = message.into();
        cx.notify();
    }

    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let item = self.focused_item(cx);
        self.sync_notes_editor(item.as_ref(), window, cx);
        let colors = cx.theme().colors().clone();
        let Some(item) = item else {
            return v_flex()
                .p_2()
                .child(
                    Label::new("No focused item \u{2014} click a node in the Workcat Map.")
                        .color(Color::Muted),
                )
                .into_any_element();
        };
        let status_color = WorkcatMapView::status_color(item.status, cx);
        let accent = cx.theme().colors().text_accent;
        let muted = cx.theme().colors().text_muted;
        // Readability: body copy at the default UI size (never below
        // at 100% zoom), a bounded measure (~70ch), 1.5-ish line
        // rhythm, and clear space above each section heading so the
        // layer-cake scans. All reading type scales with zoom.
        let zoom = self.zoom;
        let measure = px(560. * zoom);
        let title_size = px(18.0 * zoom);
        let meta_size = px(14.0 * zoom);
        let heading_size = px(12.0 * zoom);
        let body_size = px(15.0 * zoom);
        let mut body = v_flex()
            .id("workcat-detail-body")
            .size_full()
            .p_3()
            .gap_1()
            .overflow_y_scroll()
            .child(
                div()
                    .max_w(measure)
                    .text_size(title_size)
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(item.subject.clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .pt_1()
                    .text_size(meta_size)
                    .child(
                        div()
                            .flex_none()
                            .w_2p5()
                            .h_2p5()
                            .rounded_full()
                            .bg(status_color),
                    )
                    .child(
                        div()
                            .text_color(accent)
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(SharedString::from(item.status.as_str())),
                    )
                    .child(
                        div()
                            .text_color(muted)
                            .child(format!("{} deps", item.depends_on.len())),
                    )
                    .child(div().text_color(muted).child(item.id8().to_string())),
            )
            .child(
                div()
                    .text_size(px(13.0 * zoom))
                    .text_color(muted)
                    .child(item.reference.clone()),
            );
        for (heading, section_body) in &item.sections {
            if heading.eq_ignore_ascii_case("notes") {
                continue; // Rendered as the editable field below.
            }
            body = body.child(
                div()
                    .pt_4()
                    .pb_1()
                    .text_size(heading_size)
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(accent)
                    .child(heading.to_uppercase()),
            );
            for line in section_body.lines() {
                if line.trim().is_empty() {
                    body = body.child(div().h_2());
                    continue;
                }
                body = body.child(
                    div()
                        .max_w(measure)
                        .py_0p5()
                        .text_size(body_size)
                        .child(line.to_string()),
                );
            }
        }
        body = body
            .child(
                div()
                    .pt_4()
                    .pb_1()
                    .text_size(heading_size)
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(accent)
                    .child("NOTES"),
            )
            .child(
                div()
                    .w_full()
                    .max_w(measure)
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.editor_background)
                    .child(self.notes_editor.clone()),
            )
            .child(
                h_flex()
                    .w_full()
                    .max_w(measure)
                    .pt_1()
                    .justify_between()
                    .child(
                        Label::new(self.status.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Button::new("save-notes", "Save Notes")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.save_notes(cx);
                            })),
                    ),
            );
        body.into_any_element()
    }
}

impl Focusable for WorkcatDetailView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkcatDetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_subscribed(cx);
        v_flex()
            .key_context("WorkcatDetail")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::zoom_reset))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_body(window, cx))
    }
}

/// The dock panel wrapper.
pub struct WorkcatDetailPanel {
    view: Entity<WorkcatDetailView>,
    position: DockPosition,
}

impl WorkcatDetailPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let view = cx.new(|cx| WorkcatDetailView::new(window, cx));
        Self {
            view,
            // The map lives in the right dock; defaulting detail to
            // the left lets both panels be open at once (one active
            // panel per dock).
            position: DockPosition::Left,
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

impl EventEmitter<PanelEvent> for WorkcatDetailPanel {}

impl Focusable for WorkcatDetailPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }
}

impl Render for WorkcatDetailPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.view.clone())
    }
}

impl Panel for WorkcatDetailPanel {
    fn persistent_name() -> &'static str {
        "WorkcatDetailPanel"
    }

    fn panel_key() -> &'static str {
        WORKCAT_DETAIL_PANEL_KEY
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

    fn default_size(&self, window: &Window, _cx: &App) -> Pixels {
        // Default the reading pane to ~22% of the window width (between
        // 1/5 and 1/4), with a floor so it stays usable on narrow
        // windows. A previously dragged size is restored from serialized
        // state and takes precedence over this default.
        let viewport_width = window.viewport_size().width;
        (viewport_width * DETAIL_WIDTH_FRACTION).max(px(DEFAULT_WIDTH))
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        // Default layout shows the map and the detail pane together.
        true
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Reader)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Workcat Detail")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleDetailPanelFocus)
    }

    fn activation_priority(&self) -> u32 {
        12
    }
}
