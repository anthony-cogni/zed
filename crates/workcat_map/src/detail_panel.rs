//! The Workcat Detail dock panel: the reading-and-acting surface for
//! the map's focused item. It renders the brief as the mockup's layout
//! — title, a clickable status pill, a primary "next action" split
//! button, the Next callout, a Context handoff chip, Hazards, and an
//! autosaving Notes field — plus a footer with the dependency count.
//!
//! The map panel stays the gesture surface; this panel is where a brief
//! is read and its status is advanced. They communicate through a
//! `WorkcatMapHandle` global: the map view registers itself there, and
//! this panel observes the map entity, re-rendering on every
//! focus/mutation notify. Status changes and notes writes flow back
//! through the map view, keeping the event-append path and the
//! pending-events counter in one place.

use std::time::Duration;

use anyhow::Result;
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, AsyncWindowContext, ClickEvent, Context, DismissEvent, Div, Entity,
    EventEmitter, FocusHandle, Focusable, Hsla, Pixels, Point, SharedString, Subscription, Task,
    WeakEntity, Window, actions, anchored, deferred, px,
};
use ui::{ContextMenu, IconButton, IconPosition, Tooltip, prelude::*};
use workspace::{
    OpenOptions, OpenVisible, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::model::{ALL_STATUSES, ItemMeta, Status};
use crate::panel::{SetStatus, WorkcatMapHandle, WorkcatMapView, ZoomIn, ZoomOut, ZoomReset};

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
/// Debounce before an edit to the Notes field autosaves.
const AUTOSAVE_DELAY: Duration = Duration::from_millis(700);

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
    /// For opening the focused item's handoff reference in the editor.
    workspace: WeakEntity<Workspace>,
    map: Option<WeakEntity<WorkcatMapView>>,
    notes_editor: Entity<Editor>,
    /// Which item id the notes editor currently holds text for.
    notes_item: Option<String>,
    /// True while `sync_notes_editor` is programmatically replacing the
    /// notes text, so the resulting edit event does not autosave.
    suppress_autosave: bool,
    autosave_task: Option<Task<()>>,
    /// The status/autosave line under the Notes field.
    status: SharedString,
    /// The open "Set status" popover (menu, anchor, dismiss sub).
    status_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    /// Text zoom for the reading surface (title, meta, sections).
    zoom: f32,
    _map_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl WorkcatDetailView {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let notes_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 8, window, cx);
            editor.set_placeholder_text("Add a note\u{2026}", window, cx);
            editor
        });
        // The map panel may register its handle before or after this
        // panel loads (panels load concurrently); observe the global
        // so a late-arriving map still gets picked up.
        let mut subscriptions = vec![cx.observe_global::<WorkcatMapHandle>(|_, cx| {
            cx.notify();
        })];
        // Notes autosave: each edit arms a debounced save; a fresh edit
        // replaces the pending task (dropping cancels it). Programmatic
        // text swaps on item change are suppressed.
        subscriptions.push(cx.subscribe_in(
            &notes_editor,
            window,
            |this: &mut Self, _editor, event, _window, cx| {
                if this.suppress_autosave {
                    return;
                }
                if let EditorEvent::BufferEdited = event {
                    this.status = "Saving\u{2026}".into();
                    cx.notify();
                    this.autosave_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(AUTOSAVE_DELAY).await;
                        this.update(cx, |this, cx| this.save_notes(cx)).ok();
                    }));
                }
            },
        ));
        // Blur is a natural commit point: flush any pending edit when
        // focus leaves the notes field.
        subscriptions.push(cx.on_blur(
            &notes_editor.focus_handle(cx),
            window,
            |this, _, cx| {
                this.save_notes(cx);
            },
        ));
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            workspace,
            map: None,
            notes_editor,
            notes_item: None,
            suppress_autosave: false,
            autosave_task: None,
            status: SharedString::default(),
            status_menu: None,
            zoom: 1.0,
            _map_subscription: None,
            _subscriptions: subscriptions,
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
        self.suppress_autosave = true;
        self.notes_editor.update(cx, |editor, cx| {
            editor.set_text(notes, window, cx);
        });
        self.suppress_autosave = false;
        self.status = SharedString::default();
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

    // === Status transitions ===

    /// Set the focused item's status through the map view (the detail
    /// item is the map's focused item), reusing the map's append +
    /// checkpoint path.
    fn apply_status(&mut self, status: Status, window: &mut Window, cx: &mut Context<Self>) {
        let Some(map) = self.map.as_ref().and_then(|weak| weak.upgrade()) else {
            self.status = "map panel not loaded".into();
            cx.notify();
            return;
        };
        map.update(cx, |map, cx| {
            map.set_status(
                &SetStatus {
                    status: status.as_str().to_string(),
                },
                window,
                cx,
            );
        });
    }

    /// The primary split button: apply the status's forward transition,
    /// or (for prompt-only transitions like Unknown -> triage) open the
    /// full status menu.
    fn primary_action(&mut self, anchor: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.focused_item(cx) else {
            return;
        };
        let Some((_, to)) = item.status.next_transition() else {
            return;
        };
        match to {
            Some(to) => self.apply_status(to, window, cx),
            None => self.open_status_menu(anchor, window, cx),
        }
    }

    fn open_status_menu(
        &mut self,
        anchor: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.focused_item(cx).map(|item| item.status);
        let map = self.map.clone();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
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
                    IconPosition::Start,
                    Some(Box::new(SetStatus {
                        status: status.as_str().to_string(),
                    })),
                    move |window, cx| {
                        if let Some(map) = map.as_ref().and_then(|weak| weak.upgrade()) {
                            map.update(cx, |map, cx| {
                                map.set_status(
                                    &SetStatus {
                                        status: status.as_str().to_string(),
                                    },
                                    window,
                                    cx,
                                );
                            });
                        }
                    },
                );
            }
            menu
        });
        window.focus(&menu.focus_handle(cx), cx);
        let subscription =
            cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, window, cx| {
                if this.status_menu.as_ref().is_some_and(|(menu, _, _)| {
                    menu.focus_handle(cx).contains_focused(window, cx)
                }) {
                    cx.focus_self(window);
                }
                this.status_menu.take();
                cx.notify();
            });
        self.status_menu = Some((menu, anchor, subscription));
        cx.notify();
    }

    // === Locate & handoff ===

    /// Scroll the map to the focused node (it is already focused there).
    fn locate(&mut self, cx: &mut Context<Self>) {
        let Some(map) = self.map.as_ref().and_then(|weak| weak.upgrade()) else {
            return;
        };
        map.update(cx, |map, cx| map.center_on_focused(cx));
    }

    /// Open the focused item's `ref` (its handoff doc) in the editor,
    /// resolving it against each visible worktree root. Falls back to a
    /// status message when the reference is not inside the project.
    fn open_reference(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.focused_item(cx) else {
            return;
        };
        let reference = item.reference;
        let Some(workspace) = self.workspace.upgrade() else {
            self.status = "workspace unavailable".into();
            cx.notify();
            return;
        };
        let existing = workspace
            .read(cx)
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().join(&reference))
            .find(|path| path.is_file());
        match existing {
            Some(path) => {
                let task = workspace.update(cx, |workspace, cx| {
                    workspace.open_abs_path(
                        path,
                        OpenOptions {
                            visible: Some(OpenVisible::OnlyFiles),
                            ..Default::default()
                        },
                        window,
                        cx,
                    )
                });
                task.detach_and_log_err(cx);
                self.status = format!("opening {reference}").into();
            }
            None => {
                self.status = format!("handoff not in project: {reference}").into();
            }
        }
        cx.notify();
    }

    // === Rendering ===

    fn render_status_pill(
        &self,
        status: Status,
        status_color: Hsla,
        zoom: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().colors().text_muted;
        h_flex()
            .id("workcat-status-pill")
            .items_center()
            .gap_1p5()
            .px_2()
            .py_0p5()
            .rounded_md()
            .border_1()
            .border_color(status_color.alpha(0.5))
            .bg(status_color.alpha(0.15))
            .cursor_pointer()
            .tooltip(Tooltip::text("Change status"))
            .child(div().flex_none().w_2().h_2().rounded_full().bg(status_color))
            .child(
                div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(12.5 * zoom))
                    .child(SharedString::from(status.as_str())),
            )
            .child(
                div()
                    .text_size(px(9.0 * zoom))
                    .text_color(muted)
                    .child("\u{25be}"),
            )
            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                this.open_status_menu(event.position(), window, cx);
            }))
    }

    fn render_actions(&self, status: Status, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = h_flex().mt_1().gap_2().flex_wrap();
        if let Some((verb, _)) = status.next_transition() {
            row = row
                .child(
                    Button::new("workcat-primary-action", verb)
                        .style(ButtonStyle::Filled)
                        .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                            this.primary_action(event.position(), window, cx);
                        })),
                )
                .child(
                    IconButton::new("workcat-status-more", IconName::ChevronDown)
                        .tooltip(Tooltip::text("More status options"))
                        .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                            this.open_status_menu(event.position(), window, cx);
                        })),
                );
        }
        row.child(
            Button::new("workcat-open-handoff", "Open handoff").on_click(cx.listener(
                |this, _: &ClickEvent, window, cx| {
                    this.open_reference(window, cx);
                },
            )),
        )
        .child(
            Button::new("workcat-locate", "Locate").on_click(cx.listener(
                |this, _: &ClickEvent, _window, cx| {
                    this.locate(cx);
                },
            )),
        )
    }

    fn render_context_chip(&self, reference: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        h_flex()
            .id("workcat-context-chip")
            .w_full()
            .gap_2()
            .items_center()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.editor_background)
            .cursor_pointer()
            .tooltip(Tooltip::text(reference.to_string()))
            .child(div().child("\u{1f4c4}"))
            .child(
                div()
                    .flex_1()
                    .text_size(px(12.0))
                    .text_color(colors.text_accent)
                    .overflow_hidden()
                    .child(shorten_reference(reference)),
            )
            .child(div().text_color(colors.text_muted).child("\u{2197}"))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_reference(window, cx);
            }))
    }

    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let item = self.focused_item(cx);
        self.sync_notes_editor(item.as_ref(), window, cx);
        let colors = cx.theme().colors().clone();
        let muted = colors.text_muted;
        let accent = colors.text_accent;
        let Some(item) = item else {
            return v_flex()
                .p_4()
                .child(
                    Label::new("No focused item \u{2014} click a node in the Workcat Map.")
                        .color(Color::Muted),
                )
                .into_any_element();
        };
        let zoom = self.zoom;
        let status = item.status;
        let status_color = WorkcatMapView::status_color(status, cx);

        let section = |name: &str| -> Option<String> {
            item.sections
                .iter()
                .find(|(heading, _)| heading.eq_ignore_ascii_case(name))
                .map(|(_, body)| body.clone())
                .filter(|body| !body.trim().is_empty())
        };
        let next_body = section("Next");
        let context_body = section("Context");
        let hazards_body = section("Hazards");

        let mut body = v_flex()
            .id("workcat-detail-body")
            .size_full()
            .p_4()
            .gap_2()
            .overflow_y_scroll();

        // Title.
        body = body.child(
            div()
                .text_size(px(18.0 * zoom))
                .line_height(px(24.0 * zoom))
                .font_weight(gpui::FontWeight::BOLD)
                .child(item.subject.clone()),
        );

        // Meta row: the status pill (opens the menu) and the item id8,
        // standing in for the mockup's commit hash.
        body = body.child(
            h_flex()
                .items_center()
                .gap_2()
                .flex_wrap()
                .child(self.render_status_pill(status, status_color, zoom, cx))
                .child(
                    div()
                        .id("workcat-hash")
                        .text_size(px(12.0 * zoom))
                        .text_color(accent)
                        .tooltip(Tooltip::text(item.id.clone()))
                        .child(item.id8().to_string()),
                ),
        );

        // Actions: the primary transition split button plus handoff and
        // locate.
        body = body.child(self.render_actions(status, cx));

        // Next callout.
        body = body.child(
            v_flex()
                .mt_1()
                .p_3()
                .rounded_md()
                .bg(accent.alpha(0.1))
                .child(section_label("Next", accent, px(10.5 * zoom)))
                .child(
                    div()
                        .text_size(px(13.5 * zoom))
                        .text_color(accent)
                        .child(next_body.unwrap_or_else(|| "No next step recorded.".to_string())),
                ),
        );

        // Context: the handoff reference chip, plus any Context body.
        let mut context = v_flex()
            .child(section_label("Context", muted, px(10.5 * zoom)))
            .child(self.render_context_chip(&item.reference, cx));
        if let Some(context_body) = context_body {
            for line in context_body.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                context = context.child(
                    div()
                        .pt_1()
                        .text_size(px(13.5 * zoom))
                        .child(line.to_string()),
                );
            }
        }
        body = body.child(context);

        // Any remaining brief sections (e.g. State) in file order.
        for (heading, section_body) in &item.sections {
            if matches!(
                heading.to_ascii_lowercase().as_str(),
                "next" | "context" | "hazards" | "notes"
            ) {
                continue;
            }
            body = body.child(section_label(heading, accent, px(10.5 * zoom)));
            for line in section_body.lines() {
                if line.trim().is_empty() {
                    body = body.child(div().h_2());
                    continue;
                }
                body = body.child(
                    div()
                        .py_0p5()
                        .text_size(px(13.5 * zoom))
                        .child(line.to_string()),
                );
            }
        }

        // Hazards.
        body = body.child(section_label("Hazards", muted, px(10.5 * zoom)));
        let hazards: Vec<String> = hazards_body
            .as_deref()
            .unwrap_or_default()
            .lines()
            .map(|line| line.trim().trim_start_matches("- ").to_string())
            .filter(|line| !line.is_empty())
            .collect();
        if hazards.is_empty() {
            body = body.child(
                div()
                    .text_size(px(12.5 * zoom))
                    .text_color(muted)
                    .child("None recorded"),
            );
        } else {
            let error = cx.theme().status().error;
            for hazard in hazards {
                body = body.child(
                    h_flex()
                        .mt_1()
                        .gap_2()
                        .items_start()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(error.alpha(0.35))
                        .bg(error.alpha(0.1))
                        .child(div().flex_none().text_color(error).child("\u{26a0}"))
                        .child(div().text_size(px(13.0 * zoom)).child(hazard)),
                );
            }
        }

        // Notes: an autosaving field with a save/status line.
        body = body
            .child(section_label("Notes", muted, px(10.5 * zoom)))
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.editor_background)
                    .child(self.notes_editor.clone()),
            )
            .child(
                div()
                    .min_h(px(15.0))
                    .text_size(px(11.0))
                    .text_color(muted)
                    .child(self.status.clone()),
            );

        // Footer: the dependency count.
        let deps = item.depends_on.len();
        body = body.child(
            h_flex()
                .mt_3()
                .pt_2()
                .gap_4()
                .border_t_1()
                .border_color(colors.border)
                .text_size(px(12.0))
                .text_color(muted)
                .child(format!(
                    "{deps} {}",
                    if deps == 1 { "dependency" } else { "dependencies" }
                )),
        );

        body.into_any_element()
    }
}

/// A quiet uppercase "eyebrow" label above a section.
fn section_label(text: &str, color: Hsla, size: Pixels) -> Div {
    div()
        .pt_4()
        .pb_1()
        .text_size(size)
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(color)
        .child(text.to_uppercase())
}

/// Elide the middle of a path so a long `ref` fits on one line
/// (`a/b/c/d.md` -> `a/\u{2026}/d.md`), matching the mockup's chip.
fn shorten_reference(reference: &str) -> String {
    let parts: Vec<&str> = reference.split('/').collect();
    if parts.len() <= 2 {
        return reference.to_string();
    }
    format!("{}/\u{2026}/{}", parts[0], parts[parts.len() - 1])
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
            .children(self.status_menu.as_ref().map(|(menu, position, _)| {
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
pub struct WorkcatDetailPanel {
    view: Entity<WorkcatDetailView>,
    position: DockPosition,
}

impl WorkcatDetailPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = cx.new(|cx| WorkcatDetailView::new(workspace, window, cx));
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
        let handle = workspace.clone();
        handle.update_in(&mut cx, move |_workspace, window, cx| {
            cx.new(|cx| Self::new(workspace.clone(), window, cx))
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
