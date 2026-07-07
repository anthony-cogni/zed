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
use crate::panel::{WorkcatMapHandle, WorkcatMapView};

actions!(
    workcat_map,
    [
        /// Toggles focus on the workcat detail panel.
        ToggleDetailPanelFocus,
    ]
);

const WORKCAT_DETAIL_PANEL_KEY: &str = "WorkcatDetailPanel";
const DEFAULT_WIDTH: f32 = 420.;

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
        let mut body = v_flex()
            .id("workcat-detail-body")
            .size_full()
            .p_2()
            .gap_1()
            .overflow_y_scroll()
            .child(
                Label::new(item.subject.clone())
                    .weight(gpui::FontWeight::BOLD)
                    .size(LabelSize::Default),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .w_2()
                            .h_2()
                            .rounded_full()
                            .bg(status_color),
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
                        Label::new(item.id8().to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                Label::new(item.reference.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(div().h_1());
        for (heading, section_body) in &item.sections {
            if heading.eq_ignore_ascii_case("notes") {
                continue; // Rendered as the editable field below.
            }
            body = body.child(
                Label::new(heading.clone())
                    .size(LabelSize::Small)
                    .weight(gpui::FontWeight::BOLD)
                    .color(Color::Accent),
            );
            for line in section_body.lines() {
                body = body.child(Label::new(line.to_string()).size(LabelSize::XSmall));
            }
            body = body.child(div().h_1());
        }
        body = body
            .child(
                Label::new("Notes")
                    .size(LabelSize::Small)
                    .weight(gpui::FontWeight::BOLD)
                    .color(Color::Accent),
            )
            .child(
                div()
                    .w_full()
                    .px_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(colors.border_variant)
                    .child(self.notes_editor.clone()),
            )
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .child(
                        Label::new(self.status.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Button::new("save-notes", "Save Notes")
                            .label_size(LabelSize::XSmall)
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

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(DEFAULT_WIDTH)
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
