use std::{
    cmp::{self, Reverse},
    sync::Arc,
};

use client::telemetry::Telemetry;
use collections::HashMap;
use copilot::CommandPaletteFilter;
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    actions, Action, AppContext, DismissEvent, EventEmitter, FocusHandle, FocusableView, Global,
    ParentElement, Render, Styled, View, ViewContext, VisualContext, WeakView,
};
use picker::{Picker, PickerDelegate};

use release_channel::{parse_zed_link, ReleaseChannel};
use ui::{h_flex, prelude::*, v_flex, HighlightedLabel, KeyBinding, ListItem, ListItemSpacing};
use util::ResultExt;
use workspace::{ModalView, Workspace};
use zed_actions::OpenZedUrl;

actions!(command_palette, [Toggle]);

pub fn init(cx: &mut AppContext) {
    cx.set_global(HitCounts::default());
    cx.set_global(CommandPaletteFilter::default());
    cx.observe_new_views(CommandPalette::register).detach();
}

impl ModalView for CommandPalette {}

pub struct CommandPalette {
    picker: View<Picker<CommandPaletteDelegate>>,
}

impl CommandPalette {
    fn register(workspace: &mut Workspace, _: &mut ViewContext<Workspace>) {
        workspace.register_action(|workspace, _: &Toggle, cx| {
            let Some(previous_focus_handle) = cx.focused() else {
                return;
            };
            let telemetry = workspace.client().telemetry().clone();
            workspace.toggle_modal(cx, move |cx| {
                CommandPalette::new(previous_focus_handle, telemetry, cx)
            });
        });
    }

    fn new(
        previous_focus_handle: FocusHandle,
        telemetry: Arc<Telemetry>,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        let filter = cx.try_global::<CommandPaletteFilter>();

        let commands = cx
            .available_actions()
            .into_iter()
            .filter_map(|action| {
                let name = action.name();
                let namespace = name.split("::").next().unwrap_or("malformed action name");
                if filter.is_some_and(|f| {
                    f.hidden_namespaces.contains(namespace)
                        || f.hidden_action_types.contains(&action.type_id())
                }) {
                    return None;
                }

                Some(Command {
                    name: humanize_action_name(&name),
                    action,
                })
            })
            .collect();

        let delegate = CommandPaletteDelegate::new(
            cx.view().downgrade(),
            commands,
            telemetry,
            previous_focus_handle,
        );

        let picker = cx.new_view(|cx| Picker::uniform_list(delegate, cx));
        Self { picker }
    }
}

impl EventEmitter<DismissEvent> for CommandPalette {}

impl FocusableView for CommandPalette {
    fn focus_handle(&self, cx: &AppContext) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _cx: &mut ViewContext<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

pub struct CommandPaletteInterceptor(
    pub Box<dyn Fn(&str, &AppContext) -> Option<CommandInterceptResult>>,
);

impl Global for CommandPaletteInterceptor {}

pub struct CommandInterceptResult {
    pub action: Box<dyn Action>,
    pub string: String,
    pub positions: Vec<usize>,
}

pub struct CommandPaletteDelegate {
    command_palette: WeakView<CommandPalette>,
    all_commands: Vec<Command>,
    commands: Vec<Command>,
    matches: Vec<StringMatch>,
    selected_ix: usize,
    telemetry: Arc<Telemetry>,
    previous_focus_handle: FocusHandle,
}

struct Command {
    name: String,
    action: Box<dyn Action>,
}

impl Clone for Command {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            action: self.action.boxed_clone(),
        }
    }
}

/// Hit count for each command in the palette.
/// We only account for commands triggered directly via command palette and not by e.g. keystrokes because
/// if a user already knows a keystroke for a command, they are unlikely to use a command palette to look for it.
#[derive(Default)]
struct HitCounts(HashMap<String, usize>);

impl Global for HitCounts {}

impl CommandPaletteDelegate {
    fn new(
        command_palette: WeakView<CommandPalette>,
        commands: Vec<Command>,
        telemetry: Arc<Telemetry>,
        previous_focus_handle: FocusHandle,
    ) -> Self {
        Self {
            command_palette,
            all_commands: commands.clone(),
            matches: vec![],
            commands,
            selected_ix: 0,
            telemetry,
            previous_focus_handle,
        }
    }
}

impl PickerDelegate for CommandPaletteDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self) -> Arc<str> {
        "Execute a command...".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_ix
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut ViewContext<Picker<Self>>) {
        self.selected_ix = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        cx: &mut ViewContext<Picker<Self>>,
    ) -> gpui::Task<()> {
        let mut commands = self.all_commands.clone();

        cx.spawn(move |picker, mut cx| async move {
            cx.read_global::<HitCounts, _>(|hit_counts, _| {
                commands.sort_by_key(|action| {
                    (
                        Reverse(hit_counts.0.get(&action.name).cloned()),
                        action.name.clone(),
                    )
                });
            })
            .ok();

            let candidates = commands
                .iter()
                .enumerate()
                .map(|(ix, command)| StringMatchCandidate {
                    id: ix,
                    string: command.name.to_string(),
                    char_bag: command.name.chars().collect(),
                })
                .collect::<Vec<_>>();
            let mut matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .enumerate()
                    .map(|(index, candidate)| StringMatch {
                        candidate_id: index,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                fuzzy::match_strings(
                    &candidates,
                    &query,
                    true,
                    10000,
                    &Default::default(),
                    cx.background_executor().clone(),
                )
                .await
            };

            let mut intercept_result = cx
                .try_read_global(|interceptor: &CommandPaletteInterceptor, cx| {
                    (interceptor.0)(&query, cx)
                })
                .flatten();
            let release_channel = cx
                .update(|cx| ReleaseChannel::try_global(cx))
                .ok()
                .flatten();
            if release_channel == Some(ReleaseChannel::Dev) {
                if parse_zed_link(&query).is_some() {
                    intercept_result = Some(CommandInterceptResult {
                        action: OpenZedUrl { url: query.clone() }.boxed_clone(),
                        string: query.clone(),
                        positions: vec![],
                    })
                }
            }

            if let Some(CommandInterceptResult {
                action,
                string,
                positions,
            }) = intercept_result
            {
                if let Some(idx) = matches
                    .iter()
                    .position(|m| commands[m.candidate_id].action.type_id() == action.type_id())
                {
                    matches.remove(idx);
                }
                commands.push(Command {
                    name: string.clone(),
                    action,
                });
                matches.insert(
                    0,
                    StringMatch {
                        candidate_id: commands.len() - 1,
                        string,
                        positions,
                        score: 0.0,
                    },
                )
            }

            picker
                .update(&mut cx, |picker, _| {
                    let delegate = &mut picker.delegate;
                    delegate.commands = commands;
                    delegate.matches = matches;
                    if delegate.matches.is_empty() {
                        delegate.selected_ix = 0;
                    } else {
                        delegate.selected_ix =
                            cmp::min(delegate.selected_ix, delegate.matches.len() - 1);
                    }
                })
                .log_err();
        })
    }

    fn dismissed(&mut self, cx: &mut ViewContext<Picker<Self>>) {
        self.command_palette
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn confirm(&mut self, _: bool, cx: &mut ViewContext<Picker<Self>>) {
        if self.matches.is_empty() {
            self.dismissed(cx);
            return;
        }
        let action_ix = self.matches[self.selected_ix].candidate_id;
        let command = self.commands.swap_remove(action_ix);

        self.telemetry
            .report_action_event("command palette", command.name.clone());

        self.matches.clear();
        self.commands.clear();
        cx.update_global(|hit_counts: &mut HitCounts, _| {
            *hit_counts.0.entry(command.name).or_default() += 1;
        });
        let action = command.action;
        cx.focus(&self.previous_focus_handle);
        self.dismissed(cx);
        cx.dispatch_action(action);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut ViewContext<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let r#match = self.matches.get(ix)?;
        let command = self.commands.get(r#match.candidate_id)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .selected(selected)
                .child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .child(HighlightedLabel::new(
                            command.name.clone(),
                            r#match.positions.clone(),
                        ))
                        .children(KeyBinding::for_action_in(
                            &*command.action,
                            &self.previous_focus_handle,
                            cx,
                        )),
                ),
        )
    }
}

fn humanize_action_name(name: &str) -> String {
    let capacity = name.len() + name.chars().filter(|c| c.is_uppercase()).count();
    let mut result = String::with_capacity(capacity);
    for char in name.chars() {
        if char == ':' {
            if result.ends_with(':') {
                result.push(' ');
            } else {
                result.push(':');
            }
        } else if char == '_' {
            result.push(' ');
        } else if char.is_uppercase() {
            if !result.ends_with(' ') {
                result.push(' ');
            }
            result.extend(char.to_lowercase());
        } else {
            result.push(char);
        }
    }
    result
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use editor::Editor;
    use go_to_line::GoToLine;
    use gpui::TestAppContext;
    use language::Point;
    use project::Project;
    use settings::KeymapFile;
    use workspace::{AppState, Workspace};

    #[test]
    fn test_humanize_action_name() {
        assert_eq!(
            humanize_action_name("editor::GoToDefinition"),
            "editor: go to definition"
        );
        assert_eq!(
            humanize_action_name("editor::Backspace"),
            "editor: backspace"
        );
        assert_eq!(
            humanize_action_name("go_to_line::Deploy"),
            "go to line: deploy"
        );
    }

    #[gpui::test]
    async fn test_command_palette(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, cx) = cx.add_window_view(|cx| Workspace::test_new(project.clone(), cx));

        let editor = cx.new_view(|cx| {
            let mut editor = Editor::single_line(cx);
            editor.set_text("abc", cx);
            editor
        });

        workspace.update(cx, |workspace, cx| {
            workspace.add_item(Box::new(editor.clone()), cx);
            editor.update(cx, |editor, cx| editor.focus(cx))
        });

        cx.simulate_keystrokes("cmd-shift-p");

        let palette = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });

        palette.update(cx, |palette, _| {
            assert!(palette.delegate.commands.len() > 5);
            let is_sorted =
                |actions: &[Command]| actions.windows(2).all(|pair| pair[0].name <= pair[1].name);
            assert!(is_sorted(&palette.delegate.commands));
        });

        cx.simulate_input("bcksp");

        palette.update(cx, |palette, _| {
            assert_eq!(palette.delegate.matches[0].string, "editor: backspace");
        });

        cx.simulate_keystrokes("enter");

        workspace.update(cx, |workspace, cx| {
            assert!(workspace.active_modal::<CommandPalette>(cx).is_none());
            assert_eq!(editor.read(cx).text(cx), "ab")
        });

        // Add namespace filter, and redeploy the palette
        cx.update(|cx| {
            cx.set_global(CommandPaletteFilter::default());
            cx.update_global::<CommandPaletteFilter, _>(|filter, _| {
                filter.hidden_namespaces.insert("editor");
            })
        });

        cx.simulate_keystrokes("cmd-shift-p");
        cx.simulate_input("bcksp");

        let palette = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });
        palette.update(cx, |palette, _| {
            assert!(palette.delegate.matches.is_empty())
        });
    }

    #[gpui::test]
    async fn test_go_to_line(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, cx) = cx.add_window_view(|cx| Workspace::test_new(project.clone(), cx));

        cx.simulate_keystrokes("cmd-n");

        let editor = workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<Editor>(cx).unwrap()
        });
        editor.update(cx, |editor, cx| editor.set_text("1\n2\n3\n4\n5\n6\n", cx));

        cx.simulate_keystrokes("cmd-shift-p");
        cx.simulate_input("go to line: Toggle");
        cx.simulate_keystrokes("enter");

        workspace.update(cx, |workspace, cx| {
            assert!(workspace.active_modal::<GoToLine>(cx).is_some())
        });

        cx.simulate_keystrokes("3 enter");

        editor.update(cx, |editor, cx| {
            assert!(editor.focus_handle(cx).is_focused(cx));
            assert_eq!(
                editor.selections.last::<Point>(cx).range().start,
                Point::new(2, 0)
            );
        });
    }

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme::init(theme::LoadThemes::JustBase, cx);
            language::init(cx);
            editor::init(cx);
            menu::init();
            go_to_line::init(cx);
            workspace::init(app_state.clone(), cx);
            init(cx);
            Project::init_settings(cx);
            KeymapFile::parse(
                r#"[
                    {
                        "bindings": {
                            "cmd-n": "workspace::NewFile",
                            "enter": "menu::Confirm",
                            "cmd-shift-p": "command_palette::Toggle"
                        }
                    }
                ]"#,
            )
            .unwrap()
            .add_to_cx(cx)
            .unwrap();
            app_state
        })
    }
}
