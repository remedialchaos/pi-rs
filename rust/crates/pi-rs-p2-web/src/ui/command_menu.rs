//! Command menu UI: header, list and parts (upstream `command-menu/`).

use crate::command_menu::highlight::Highlight;
use crate::command_menu::r#match::{match_score, rank_by};
use leptos::prelude::*;

/// A command shown in the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandItem {
    pub id: String,
    pub title: String,
    pub keywords: Vec<String>,
    pub section: String,
}

impl CommandItem {
    fn score(&self, query: &str) -> f64 {
        std::iter::once(match_score(query, &self.title))
            .chain(self.keywords.iter().map(|word| match_score(query, word)))
            .fold(0.0f64, f64::max)
    }
}

/// The menu's search header (upstream `Header.tsx`).
#[component]
pub fn Header(query: RwSignal<String>, on_escape: Callback<()>) -> impl IntoView {
    view! {
        <div class="command-menu-head">
            <input
                type="text"
                class="command-menu-input"
                role="combobox"
                aria-label="Search commands"
                aria-expanded="true"
                autocomplete="off"
                prop:value=move || query.get()
                on:input=move |event| query.set(event_target_value(&event))
                on:keydown=move |event| {
                    if event.key() == "Escape" { on_escape.run(()); }
                }
            />
        </div>
    }
}

/// Commands matching the open page's query, best first (upstream `match.ts`).
pub fn matching_commands<'a>(
    commands: &'a [CommandItem],
    query: &str,
    recents: &[String],
) -> Vec<&'a CommandItem> {
    if query.trim().is_empty() {
        let mut ordered: Vec<&CommandItem> = recents
            .iter()
            .filter_map(|id| commands.iter().find(|command| &command.id == id))
            .collect();
        for command in commands {
            if !recents.contains(&command.id) {
                ordered.push(command);
            }
        }
        ordered
    } else {
        rank_by(commands, |command| command.score(query))
    }
}

/// The menu's listbox (upstream `List.tsx`, `Parts.tsx`).
#[component]
pub fn List(
    commands: Vec<CommandItem>,
    query: RwSignal<String>,
    recents: Vec<String>,
    on_run: Callback<String>,
) -> impl IntoView {
    let items = Signal::derive({
        let commands = commands.clone();
        let recents = recents.clone();
        move || {
            matching_commands(&commands, &query.get(), &recents)
                .into_iter()
                .cloned()
                .collect::<Vec<CommandItem>>()
        }
    });
    view! {
        <div class="command-menu-list" role="listbox" aria-label="Commands" id="command-menu-list">
            <For
                each=move || items.get()
                key=|command| command.id.clone()
                children={move |command: CommandItem| {
                    let id = command.id.clone();
                    view! {
                        <div
                            class="command-menu-option"
                            role="option"
                            data-section=command.section.clone()
                            tabindex="-1"
                            on:click=move |_| on_run.run(id.clone())
                        >
                            <span class="command-title">{command.title.clone()}</span>
                        </div>
                    }
                }}
            />
        </div>
    }
}

/// The command menu dialog.
#[component]
pub fn CommandMenu(
    commands: Vec<CommandItem>,
    query: RwSignal<String>,
    recents: Vec<String>,
    on_run: Callback<String>,
    on_close: Callback<()>,
) -> impl IntoView {
    let commands_for_list = commands.clone();
    view! {
        <div class="command-menu" role="dialog" aria-modal="true" aria-label="Command menu">
            <Header query=query on_escape=on_close />
            <List commands=commands_for_list query=query recents=recents on_run=on_run />
        </div>
    }
}

/// Highlights the first option of a listbox from a signal of option ids.
pub fn sync_highlight(highlight: &mut Highlight, ids: &[String]) {
    let options = ids
        .iter()
        .map(|id| crate::command_menu::highlight::ListOption {
            id: id.clone(),
            preferred: false,
        })
        .collect();
    highlight.set_options(options, "", 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(id: &str, title: &str) -> CommandItem {
        CommandItem {
            id: id.into(),
            title: title.into(),
            keywords: vec![],
            section: "Commands".into(),
        }
    }

    #[test]
    fn empty_query_shows_recents_first() {
        let commands = vec![command("new", "New session"), command("theme", "Theme")];
        let matched = matching_commands(&commands, "", &["theme".into()]);
        assert_eq!(
            matched.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["theme", "new"]
        );
    }

    #[test]
    fn query_filters_and_ranks() {
        let commands = vec![
            command("theme", "Change theme"),
            command("new", "New session"),
        ];
        let matched = matching_commands(&commands, "theme", &[]);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].id, "theme");
    }
}
