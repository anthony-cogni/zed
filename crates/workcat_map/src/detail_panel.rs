//! The Workcat Detail dock panel: the single-status-control reading
//! surface for the map's focused item. It renders the brief as the
//! design mockup: title, a status pill that is the *only* status
//! control (it opens the shared "Set status" menu), Open handoff /
//! Locate actions, one handoff context chip (a copy-able id), and an
//! autosaving Notes field, over a footer with the dependency count and
//! last-updated time.
//!
//! The map panel stays the gesture surface; this panel is where a brief
//! is read and its status is advanced. They communicate through a
//! `WorkcatMapHandle` global: the map view registers itself there, and
//! this panel observes the map entity, re-rendering on every
//! focus/mutation notify. Status changes and notes flow back through
//! the map view, keeping the event-append path and the pending-events
//! counter in one place. Status *side effects* (the Canceled undo, the
//! Unknown triage note) are driven off the observed status transition,
//! so they fire no matter which surface changed the status.

use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, ClickEvent, Context, DismissEvent, Div, Entity, FocusHandle, Focusable, Hsla,
    Pixels, Point, SharedString, Subscription, Task, WeakEntity, Window, actions, anchored,
    deferred, px,
};
use ui::{ContextMenu, Tooltip, prelude::*};
use workspace::{OpenOptions, OpenVisible, Workspace};

use crate::model::{ItemMeta, Status};
use crate::panel::{WorkcatMapHandle, WorkcatMapView, ZoomIn, ZoomOut, ZoomReset};

actions!(
    workcat_map,
    [
        /// Toggles focus on the workcat detail panel.
        ToggleDetailPanelFocus,
    ]
);

/// Requests opening a work item's originating Zed conversation (its
/// `ref`, when that ref is a bare conversation id rather than a file
/// path) in the real agent panel. The workcat_map crate has no
/// dependency on agent internals, so this is just a signal; `zed.rs`
/// (which already depends on both crates) registers the handler that
/// resolves the id to an `acp::SessionId` and calls
/// `AgentPanel::open_thread`.
#[derive(Clone, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = workcat_map)]
pub struct OpenConversation {
    pub thread_id: String,
}

/// Debounce before an edit to the Notes field autosaves.
const AUTOSAVE_DELAY: Duration = Duration::from_millis(700);
/// How long the "Marked as canceled" undo affordance stays offered.
const CANCEL_UNDO_WINDOW: Duration = Duration::from_secs(6);

/// The dock panel wrapper (and its `ToggleDetailPanelFocus`
/// registration) lives in `workcat_panel.rs`, which combines this view
/// with `WorkcatMapView` into one panel.
pub fn init(_cx: &mut App) {}

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
    /// The item id / status the panel last reconciled to, so a status
    /// transition (from any surface) can trigger its side effects once.
    tracked_item: Option<String>,
    tracked_status: Option<Status>,
    /// The status to restore while the Canceled undo window is open.
    undo_prev: Option<Status>,
    undo_task: Option<Task<()>>,
    /// The status/autosave/notice line under the Notes field.
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
            tracked_item: None,
            tracked_status: None,
            undo_prev: None,
            undo_task: None,
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

    fn map(&self) -> Option<Entity<WorkcatMapView>> {
        self.map.as_ref().and_then(|weak| weak.upgrade())
    }

    fn focused_item(&self, cx: &App) -> Option<ItemMeta> {
        self.map()?.read(cx).focused_item().cloned()
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

    /// Reconcile transient UI to the focused item, and fire the side
    /// effects of a status transition once (Canceled undo, Unknown
    /// triage note).
    fn sync_focused(&mut self, item: Option<&ItemMeta>, cx: &mut Context<Self>) {
        let new_id = item.map(|item| item.id.as_str());
        if self.tracked_item.as_deref() != new_id {
            self.tracked_item = new_id.map(str::to_string);
            self.tracked_status = item.map(|item| item.status);
            self.undo_prev = None;
            self.status = SharedString::default();
            return;
        }
        let new_status = item.map(|item| item.status);
        if self.tracked_status == new_status {
            return;
        }
        let prev = self.tracked_status;
        self.tracked_status = new_status;
        match new_status {
            Some(Status::Canceled) => self.show_undo_banner(prev, cx),
            Some(Status::Unknown) => self.status = "Flagged for triage".into(),
            _ => {}
        }
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
        let Some(map) = self.map() else {
            self.status = "map panel not loaded".into();
            cx.notify();
            return;
        };
        let text = self.notes_editor.read(cx).text(cx).trim().to_string();
        map.update(cx, |map, cx| map.save_notes_for(&id, text, cx));
        self.status = "\u{2713} Saved just now".into();
        cx.notify();
    }

    // === Status menu (the sole status control) ===

    fn open_status_menu(
        &mut self,
        anchor: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(map) = self.map() else {
            self.status = "map panel not loaded".into();
            cx.notify();
            return;
        };
        let current = map.read(cx).focused_item().map(|item| item.status);
        // Dispatch in the map's key context so the 1-9 SetStatus
        // bindings show as badges and fire while the menu is open.
        let map_focus = map.read(cx).focus_handle(cx);
        let map_weak = map.downgrade();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            WorkcatMapView::populate_status_menu(menu.context(map_focus), current, map_weak)
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
        if let Some(map) = self.map() {
            map.update(cx, |map, cx| map.center_on_focused(cx));
        }
    }

    /// Open the focused item's `ref`. Most refs are a file path (a
    /// handoff doc) resolved against the visible worktrees and opened in
    /// the editor; the rest are a Zed conversation id (a bare uuid),
    /// opened in the real agent panel (see `OpenConversation`).
    fn open_reference(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.focused_item(cx) else {
            return;
        };
        let reference = item.reference;
        if is_conversation_ref(&reference) {
            window.dispatch_action(
                Box::new(OpenConversation {
                    thread_id: reference.clone(),
                }),
                cx,
            );
            self.status = format!("opening conversation {}", short_id(&reference)).into();
            cx.notify();
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            self.status = "workspace unavailable".into();
            cx.notify();
            return;
        };
        // Most handoff refs live outside whatever project this map's
        // own workspace happens to have open, so check the open
        // worktrees first, then fall back to the conventional handoff
        // root (agent_notes/... is rooted there, not in any one project).
        let in_project = workspace
            .read(cx)
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().join(&reference))
            .find(|path| path.is_file());
        let existing = in_project.or_else(|| {
            let candidate = crate::store::resolve_handoff_root().join(&reference);
            candidate.is_file().then_some(candidate)
        });
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
                self.status = format!("handoff not found: {reference}").into();
            }
        }
        cx.notify();
    }

    /// Copy the focused item's full id to the clipboard.
    fn copy_id(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.focused_item(cx) else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(item.id.clone()));
        // Confirm by reading it back, so a failed copy says so.
        let copied = cx
            .read_from_clipboard()
            .and_then(|entry| entry.text())
            .is_some_and(|text| text == item.id);
        self.status = if copied {
            format!("\u{2713} Copied {}", item.id8()).into()
        } else {
            "Copy failed".into()
        };
        cx.notify();
    }

    // === Canceled undo ===

    fn show_undo_banner(&mut self, prev: Option<Status>, cx: &mut Context<Self>) {
        self.undo_prev = prev;
        self.status = "Marked as canceled".into();
        self.undo_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CANCEL_UNDO_WINDOW).await;
            this.update(cx, |this, cx| {
                this.undo_prev = None;
                cx.notify();
            })
            .ok();
        }));
    }

    fn undo_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prev) = self.undo_prev.take() else {
            return;
        };
        self.undo_task = None;
        if let Some(map) = self.map() {
            map.update(cx, |map, cx| map.set_focused_status(prev, window, cx));
        }
        self.status = SharedString::default();
        cx.notify();
    }

    // === Rendering ===

    fn render_status_pill(
        &self,
        status: Status,
        zoom: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status_color = WorkcatMapView::status_color(status);
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
            .hover(|this| this.border_color(status_color.alpha(0.9)))
            .tooltip(Tooltip::text("Change status"))
            .child(div().flex_none().w_2().h_2().rounded_full().bg(status_color))
            .child(
                div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(12.5 * zoom))
                    .child(SharedString::from(status.label())),
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

    fn render_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .mt_1()
            .gap_2()
            .flex_wrap()
            .child(
                Button::new("workcat-open-handoff", "Open handoff")
                    .style(ButtonStyle::Outlined)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_reference(window, cx);
                    })),
            )
            .child(
                Button::new("workcat-locate", "Locate")
                    .style(ButtonStyle::Outlined)
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.locate(cx);
                    })),
            )
    }

    fn render_context_chip(&self, reference: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let conversation = is_conversation_ref(reference);
        let (glyph, label) = if conversation {
            ("\u{1f4ac}", format!("conversation {}", short_id(reference)))
        } else {
            ("\u{1f4c4}", shorten_reference(reference))
        };
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
            .hover(|this| this.border_color(colors.border_focused))
            .tooltip(Tooltip::text(reference.to_string()))
            .child(div().child(glyph))
            .child(
                div()
                    .flex_1()
                    .text_size(px(12.0))
                    .text_color(colors.text_accent)
                    .overflow_hidden()
                    .child(label),
            )
            .child(div().text_color(colors.text_muted).child("\u{2197}"))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_reference(window, cx);
            }))
    }

    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let item = self.focused_item(cx);
        self.sync_focused(item.as_ref(), cx);
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

        // Meta row: the status pill (the sole status control) and the
        // item id8, standing in for the mockup's commit hash.
        body = body.child(
            h_flex()
                .items_center()
                .gap_2()
                .flex_wrap()
                .child(self.render_status_pill(status, zoom, cx))
                .child(
                    div()
                        .id("workcat-hash")
                        .text_size(px(12.0 * zoom))
                        .text_color(accent)
                        .cursor_pointer()
                        .hover(|this| this.text_color(accent.opacity(0.7)))
                        .tooltip(Tooltip::text("Click to copy full id"))
                        .child(item.id8().to_string())
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.copy_id(cx);
                        })),
                ),
        );

        // Actions.
        body = body.child(self.render_actions(cx));

        // Canceled undo affordance (offered for a few seconds).
        if self.undo_prev.is_some() {
            body = body.child(
                h_flex()
                    .mt_1()
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(colors.element_background)
                    .border_1()
                    .border_color(colors.border)
                    .child(div().flex_1().text_size(px(13.0 * zoom)).child("Marked as canceled"))
                    .child(
                        Button::new("workcat-undo-cancel", "Undo")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.undo_cancel(window, cx);
                            })),
                    ),
            );
        }

        // Context: the handoff reference chip, rendered exactly once.
        body = body.child(
            v_flex()
                .child(section_label("Context", muted, px(10.5 * zoom)))
                .child(self.render_context_chip(&item.reference, cx)),
        );

        // Notes: an autosaving field with a status/save line.
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

        // Footer: dependency count (dimmed when zero) and last update.
        let deps = item.depends_on.len();
        let mut footer = h_flex()
            .mt_3()
            .pt_2()
            .gap_2()
            .flex_wrap()
            .border_t_1()
            .border_color(colors.border)
            .text_size(px(12.0))
            .text_color(muted)
            .child(
                div()
                    .opacity(if deps == 0 { 0.6 } else { 1.0 })
                    .child(format!(
                        "{deps} {}",
                        if deps == 1 { "dependency" } else { "dependencies" }
                    )),
            );
        if let Some(updated) = item.updated_at.as_deref().and_then(humanize_timestamp) {
            footer = footer
                .child(div().child("\u{00b7}"))
                .child(div().child(format!("updated {updated}")));
        }
        body = body.child(footer);

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

/// A `ref` that is a bare Zed conversation id (a uuid) rather than a
/// file path: no path separator and not a markdown doc.
fn is_conversation_ref(reference: &str) -> bool {
    !reference.contains('/') && !reference.ends_with(".md")
}

/// First 8 chars of an id, for compact display.
fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
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

/// Humanize an ISO-ish `updated_at` (`2026-07-07T15:40:...` or
/// `2026-07-07 15:40:00`) to the footer form `Jul 7, 3:40 PM`. Returns
/// `None` if it can't be parsed, so the footer just omits it.
fn humanize_timestamp(raw: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (date, rest) = raw.trim().split_once(['T', ' '])?;
    let mut date_parts = date.split('-');
    let _year = date_parts.next()?;
    let month: usize = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    let month_name = MONTHS.get(month.checked_sub(1)?)?;
    let mut time_parts = rest.split(':');
    let hour: u32 = time_parts.next()?.parse().ok()?;
    let minute: u32 = time_parts.next()?.parse().ok()?;
    let (hour12, meridiem) = match hour {
        0 => (12, "AM"),
        1..=11 => (hour, "AM"),
        12 => (12, "PM"),
        _ => (hour - 12, "PM"),
    };
    Some(format!("{month_name} {day}, {hour12}:{minute:02} {meridiem}"))
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

// The dock panel wrapper combining this view with WorkcatMapView lives
// in workcat_panel.rs.
