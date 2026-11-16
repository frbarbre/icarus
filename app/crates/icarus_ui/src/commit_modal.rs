use editor::Editor;
use gpui::{
    App, ClickEvent, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    WeakEntity, Window, actions,
};
use ui::{Checkbox, KeyBinding, ToggleState, prelude::*};
use util::ResultExt as _;
use workspace::ModalView;

use crate::changes_tab::SourceControlPanel;

actions!(
    icarus,
    [
        /// Commits with the dialog's message (⌘Enter in the commit dialog).
        ConfirmCommit,
    ]
);

/// The commit dialog: a message and an amend toggle. With nothing staged every change is
/// committed, as the server's `git.commit` does.
pub struct CommitModal {
    panel: WeakEntity<SourceControlPanel>,
    message: Entity<Editor>,
    amend: bool,
}

impl CommitModal {
    pub fn new(
        panel: WeakEntity<SourceControlPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let message = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 12, window, cx);
            editor.set_placeholder_text("Commit message (⌘Enter to commit)", window, cx);
            editor
        });
        window.focus(&message.focus_handle(cx), cx);
        Self {
            panel,
            message,
            amend: false,
        }
    }

    pub fn message_text(&self, cx: &App) -> String {
        self.message.read(cx).text(cx)
    }

    pub fn set_amend(&mut self, amend: bool, cx: &mut Context<Self>) {
        self.amend = amend;
        cx.notify();
    }

    fn confirm(&mut self, _: &ConfirmCommit, _window: &mut Window, cx: &mut Context<Self>) {
        let message = self.message_text(cx);
        // Amending may keep the old message; a new commit needs one (the server says so too).
        if message.trim().is_empty() && !self.amend {
            return;
        }
        let amend = self.amend;
        self.panel
            .update(cx, |panel, cx| panel.commit(message, amend, cx))
            .log_err();
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl ModalView for CommitModal {}

impl EventEmitter<DismissEvent> for CommitModal {}

impl Focusable for CommitModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.message.focus_handle(cx)
    }
}

impl Render for CommitModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.message.focus_handle(cx);
        let can_commit = self.amend || !self.message_text(cx).trim().is_empty();
        v_flex()
            .key_context("IcarusCommit")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .w(rems(30.))
            .elevation_3(cx)
            .p_2()
            .gap_2()
            .child(
                Label::new("Commit")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .p_1()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(self.message.clone()),
            )
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Checkbox::new(
                            "commit-amend",
                            if self.amend {
                                ToggleState::Selected
                            } else {
                                ToggleState::Unselected
                            },
                        )
                        .label("Amend the last commit")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(
                            |this, state: &ToggleState, _, cx| {
                                this.set_amend(*state == ToggleState::Selected, cx);
                            },
                        )),
                    )
                    .child(
                        Button::new(
                            "commit-confirm",
                            if self.amend { "Amend" } else { "Commit" },
                        )
                        .style(ButtonStyle::Filled)
                        .disabled(!can_commit)
                        .key_binding(KeyBinding::for_action_in(&ConfirmCommit, &focus, cx))
                        .on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| {
                                this.confirm(&ConfirmCommit, window, cx);
                            },
                        )),
                    ),
            )
    }
}
