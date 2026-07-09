//! The Mission Control dock panel and its standalone content view.
//!
//! `MissionControlView` renders the triage list and has no dependency on
//! `Workspace`, so the headless visual binary can render it directly.
//! `MissionControlPanel` is a thin wrapper implementing `workspace::Panel`.

use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Hsla, SharedString, Task, WeakEntity, Window, actions,
};
use ui::{Button, IconButton, Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{ThreadDigest, TriageState, store, triage::relative_age};

actions!(
    mission_control,
    [
        /// Toggles focus on the mission control panel.
        ToggleFocus
    ]
);

const MISSION_CONTROL_PANEL_KEY: &str = "MissionControlPanel";
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_WIDTH: f32 = 360.;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<MissionControlPanel>(window, cx);
        });
    })
    .detach();
}

/// The triage list. Standalone `Render`-able, needs no `Workspace`.
pub struct MissionControlView {
    focus_handle: FocusHandle,
    digests: Vec<ThreadDigest>,
    filter_needs_me: bool,
    expanded: Option<String>,
    loading: bool,
    error: Option<String>,
    _periodic_refresh: Option<Task<()>>,
    manual_refresh: Option<Task<()>>,
}

impl MissionControlView {
    fn base(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            digests: Vec::new(),
            filter_needs_me: false,
            expanded: None,
            loading: false,
            error: None,
            _periodic_refresh: None,
            manual_refresh: None,
        }
    }

    /// A view that loads from the threads database and refreshes every 30s.
    pub fn live(cx: &mut Context<Self>) -> Self {
        let mut this = Self::base(cx);
        this.loading = true;
        this._periodic_refresh = Some(cx.spawn(async move |this, cx| {
            loop {
                let result = cx
                    .background_spawn(async move {
                        let path = store::resolve_db_path(None);
                        store::load_digests(&path, store::DEFAULT_LIMIT)
                    })
                    .await;
                if this
                    .update(cx, |this, cx| this.apply_load_result(result, cx))
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(REFRESH_INTERVAL).await;
            }
        }));
        this
    }

    /// A view rendering injected digests; no database, no timer.
    pub fn with_digests(digests: Vec<ThreadDigest>, cx: &mut Context<Self>) -> Self {
        let mut this = Self::base(cx);
        this.digests = digests;
        this
    }

    pub fn set_filter_needs_me(&mut self, filter: bool, cx: &mut Context<Self>) {
        self.filter_needs_me = filter;
        cx.notify();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.manual_refresh = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let path = store::resolve_db_path(None);
                    store::load_digests(&path, store::DEFAULT_LIMIT)
                })
                .await;
            this.update(cx, |this, cx| this.apply_load_result(result, cx))
                .ok();
        }));
        cx.notify();
    }

    fn apply_load_result(&mut self, result: Result<Vec<ThreadDigest>>, cx: &mut Context<Self>) {
        self.loading = false;
        match result {
            Ok(digests) => {
                self.digests = digests;
                self.error = None;
            }
            Err(error) => {
                self.error = Some(format!("{error:#}"));
            }
        }
        cx.notify();
    }

    fn visible_states(&self) -> &'static [TriageState] {
        if self.filter_needs_me {
            &[TriageState::NeedsYou, TriageState::AwaitingMerge]
        } else {
            &TriageState::ALL
        }
    }

    fn count_summary(&self) -> String {
        let needs_you = self
            .digests
            .iter()
            .filter(|digest| {
                matches!(
                    digest.state,
                    TriageState::NeedsYou | TriageState::AwaitingMerge
                )
            })
            .count();
        let running = self
            .digests
            .iter()
            .filter(|digest| digest.state == TriageState::Running)
            .count();
        format!("{needs_you} need you · {running} running")
    }

    fn state_color(state: TriageState, cx: &App) -> Hsla {
        let status = cx.theme().status();
        match state {
            TriageState::NeedsYou => status.warning,
            TriageState::AwaitingMerge => status.info,
            TriageState::Running => status.created,
            TriageState::Done => status.ignored,
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .px_2()
            .py_1p5()
            .gap_2()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(
                        Label::new("Mission Control")
                            .size(LabelSize::Small)
                            .weight(FontWeight::BOLD),
                    )
                    .child(
                        Label::new(self.count_summary())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_none()
                    .child(
                        Button::new("filter-needs-me", "Needs me")
                            .toggle_state(self.filter_needs_me)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_filter_needs_me(!this.filter_needs_me, cx);
                            })),
                    )
                    .child(
                        IconButton::new("refresh", IconName::ArrowCircle)
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh(cx);
                            })),
                    ),
            )
    }

    fn render_group_header(&self, state: TriageState, count: usize, cx: &App) -> impl IntoElement {
        h_flex()
            .w_full()
            .px_2()
            .pt_2()
            .pb_0p5()
            .gap_1p5()
            .child(
                div()
                    .size(px(8.))
                    .rounded_full()
                    .flex_none()
                    .bg(Self::state_color(state, cx)),
            )
            .child(
                Label::new(format!("{} · {}", state.label(), count))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .weight(FontWeight::SEMIBOLD),
            )
    }

    fn secondary_line(digest: &ThreadDigest) -> Option<String> {
        if let Some(ask) = &digest.ask {
            return Some(ask.clone());
        }
        match digest.state {
            TriageState::Running => Some(if digest.last_role == Some(crate::Role::User) {
                "Waiting for the agent to reply".to_string()
            } else {
                "Agent is mid-work".to_string()
            }),
            _ => {
                let line = digest
                    .final_agent_text
                    .lines()
                    .find(|line| !line.trim().is_empty())?
                    .trim();
                if line.is_empty() {
                    None
                } else {
                    Some(line.to_string())
                }
            }
        }
    }

    /// The last few lines of the final agent message, for the expanded row.
    fn expanded_text(digest: &ThreadDigest) -> String {
        let text = digest.final_agent_text.trim();
        if text.is_empty() {
            return "(no agent message yet)".to_string();
        }
        let lines: Vec<&str> = text.lines().collect();
        let tail = lines[lines.len().saturating_sub(6)..].join("\n");
        let chars: Vec<char> = tail.chars().collect();
        if chars.len() > 600 {
            let truncated: String = chars[chars.len() - 600..].iter().collect();
            format!("…{truncated}")
        } else {
            tail
        }
    }

    fn render_row(&self, digest: &ThreadDigest, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Utc::now();
        let is_expanded = self.expanded.as_deref() == Some(digest.id.as_str());
        let is_done = digest.state == TriageState::Done;
        let id: SharedString = digest.id.clone().into();
        let toggle_id = digest.id.clone();
        let age = digest
            .updated_at
            .map(|updated_at| relative_age(updated_at, now))
            .unwrap_or_else(|| "—".to_string());
        let title_color = if is_done {
            Color::Muted
        } else {
            Color::Default
        };
        let secondary = Self::secondary_line(digest);
        let expanded_text = is_expanded.then(|| Self::expanded_text(digest));

        v_flex()
            .id(id)
            .w_full()
            .px_2()
            .py_1()
            .gap_0p5()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .when(is_done, |el| el.opacity(0.6))
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.expanded.as_deref() == Some(toggle_id.as_str()) {
                    this.expanded = None;
                } else {
                    this.expanded = Some(toggle_id.clone());
                }
                cx.notify();
            }))
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .items_start()
                    .child(
                        div()
                            .size(px(8.))
                            .mt_1()
                            .rounded_full()
                            .flex_none()
                            .bg(Self::state_color(digest.state, cx)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_1()
                                    .justify_between()
                                    .child(
                                        Label::new(digest.title.clone())
                                            .size(LabelSize::Small)
                                            .weight(FontWeight::MEDIUM)
                                            .color(title_color)
                                            .truncate(),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .flex_none()
                                            .child(
                                                Label::new(age)
                                                    .size(LabelSize::XSmall)
                                                    .color(Color::Muted),
                                            )
                                            .child(
                                                Label::new(format!(
                                                    "{} msgs",
                                                    digest.message_count
                                                ))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                            ),
                                    ),
                            )
                            .when_some(secondary, |el, secondary| {
                                el.child(
                                    Label::new(secondary)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                            }),
                    ),
            )
            .when_some(expanded_text, |el, expanded_text| {
                el.child(
                    div().w_full().pl_4().pt_0p5().child(
                        Label::new(expanded_text)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex()
            .id("mission-control-list")
            .flex_1()
            .overflow_y_scroll();

        if let Some(error) = &self.error {
            list = list.child(
                div().p_2().child(
                    Label::new(format!("Failed to load threads: {error}"))
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                ),
            );
        }

        if self.loading && self.digests.is_empty() {
            return list.child(
                div()
                    .p_2()
                    .child(Label::new("Loading threads…").color(Color::Muted)),
            );
        }

        let mut any = false;
        for &state in self.visible_states() {
            let group: Vec<&ThreadDigest> = self
                .digests
                .iter()
                .filter(|digest| digest.state == state)
                .collect();
            if group.is_empty() {
                continue;
            }
            any = true;
            list = list.child(self.render_group_header(state, group.len(), cx));
            for digest in group {
                list = list.child(self.render_row(digest, cx));
            }
        }

        if !any && self.error.is_none() {
            list = list.child(
                div()
                    .p_2()
                    .child(Label::new("No threads found").color(Color::Muted)),
            );
        }

        list
    }
}

impl Focusable for MissionControlView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MissionControlView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("MissionControl")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .child(self.render_list(cx))
    }
}

/// The dock panel wrapper.
pub struct MissionControlPanel {
    view: Entity<MissionControlView>,
    position: DockPosition,
}

impl MissionControlPanel {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let view = cx.new(MissionControlView::live);
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

impl EventEmitter<PanelEvent> for MissionControlPanel {}

impl Focusable for MissionControlPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }
}

impl Render for MissionControlPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.view.clone())
    }
}

impl Panel for MissionControlPanel {
    fn persistent_name() -> &'static str {
        "MissionControlPanel"
    }

    fn panel_key() -> &'static str {
        MISSION_CONTROL_PANEL_KEY
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
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Agent Mission Control")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}
