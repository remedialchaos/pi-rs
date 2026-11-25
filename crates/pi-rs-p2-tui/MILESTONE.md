# Milestone 3.2-tui

The crate's mounted TUI exercises five behaviors through TuiClient::mount and TuiClient::step. The tests use an in-memory Terminal for input and captured ANSI output.

| Behavior | Result | Evidence |
| --- | --- | --- |
| Incremental transcript | Pass for keyed entry retention | Prepending history preserves the contributed component. Changing its entry data invokes the renderer again and the replacement reaches the terminal. Tool cards persist by call ID and update when their data changes. |
| Panel scrolling | Pass for keyboard paging | Page Down received before the first render advances a three-row viewport to items 03–05 and keeps the title visible. The caller supplies the panel height. |
| Tree layout | Pass for active branches | A mounted selector displays the linear chain without indentation and moves the active child ahead of its sibling with the corresponding branch connectors. Navigation uses upstream's 16-row page. |
| Plugin dispatch | Pass for registered native contributions | A terminal shortcut invokes the contribution's host action once and consumes the input before the focused editor receives it. |
| Live theme reload | Pass | Replacing a watched JSON file by atomic rename changes the mounted component's ANSI accent from RGB 1/2/3 to 4/5/6. |

Crate formatting, offline all-target Clippy with warnings denied, and all 20 tests passed. This change adds five tests and removes zero. Five deliberate behavior mutations broke all five new tests in one run. The four prior headless tests still passed under mutation. Restoring the behaviors returned the suite to green.

The 15-test baseline command took 13m40.989s including dependency compilation. The final 20-test command took 2m23.915s including a wait for Clippy's build-directory lock. Its unit, headless and rendering binaries reported 0.00s, 0.67s and 0.03s. Build and lock times prevent a performance comparison.

Those library checks covered the mounted client and a registered Rust contribution. The standalone launcher checks below extend that coverage. The crate has no cases.jsonl or rust-anything.json, so rust-anything diff and gate could not run a crate differential. The coordinator reported 6/6 for the separate p2 HTTP oracle and owns root replay.

## Standalone launcher

The Rust `p2 tui` executable connected to a disposable upstream p2 server over its authenticated Chord WebSocket. The server held a session with 32 labeled transcript entries. The launcher used the canonical Sessions and Chat declarations for hold versions, fingerprints, and the bound variant’s state and method indexes.

| Gate | Result | Evidence |
| --- | --- | --- |
| Upstream wheel oracle | Pass | Upstream `apps/p2/test/p2/tui-mouse.test.ts` real-worker test sent SGR wheel-up at column 10, row 4 until an earlier echo returned. Node reported one pass in 15.021 s (20.700 s including setup). |
| Standalone wheel | Pass | A 60-column, eight-row pseudoterminal initially showed `terminal-line-31`. SGR wheel-up at column 10, row 4 revealed `terminal-line-0`. The Rust panel regression also checks that the heading and editor stay on screen. |
| Standalone theme reload | Pass | In the same process, the selected custom palette initially emitted RGB 18/52/86. Atomic replacement changed the emitted foreground to RGB 101/67/33 without keyboard input or a restart. |

The upstream panel places its header outside the body scroll view in `apps/p2/src/tui/components/panel.ts`. The launcher uses that layout for its transcript and session list. These checks cover the standalone launcher panel and the upstream transcript oracle. They do not exercise dynamically loaded plugin side panels.

One regression test was added and zero removed. Disabling mouse handling made its viewport-offset assertion fail. The existing nine headless tests took 0.04 s on the candidate with the new test filtered out. With mouse handling enabled, all ten took 0.06 s, and all three launcher admission tests took 0.09 s. These timings exclude compilation. The upstream custom-theme hot-reload test command also passed.

After restoration, both complete crate suites passed all 30 tests (9 in p2, 21 in p2-tui). Cargo reported 8m09s for the build. The headless binary took 0.20 s and the launcher admission binary took 0.41 s in that run.

## External TUI commands and side panels

A standalone local owner launcher discovers installed TUI components in the server data directory’s `plugins` root with Chord’s existing discovery and `PluginRuntime`. The launcher reads commands from each guest’s local Chord object, invokes the guest with the command ID and slash arguments, and renders its published panel. Component execution uses the existing fuel-limited WIT host. The projection below is specific to the Rust launcher. Upstream serves JavaScript facet bundles to its terminal worker.

| Local WIT object member | Terminal interaction |
| --- | --- |
| `commands` state | Keyed command records on the local object named for the discovered plugin. Records can declare `keys`, `when`, and `slash.name`. Local key overrides and reserved launcher bindings take precedence. |
| `run` method | The launcher sends `{"id":"notes.show","args":"first"}` for `/notes first`. Shortcut invocation sends an empty argument string. |
| `panel` state | A published `{id,title,lines}` shows a panel alongside the transcript. A further publication with the same ID updates its contents while retaining focus and scroll position. Publishing null closes it. |

The upstream dispatcher is in `apps/p2/src/runtime/tui-commands.ts`. The mounted layout follows `besidePanel` in `apps/p2/src/runtime/tui.ts`. At 100 columns and above the body and side panel share width in a 3:2 ratio. The editor initially retains focus. F2 moves focus, Page Down scrolls the side panel, Escape returns to the editor, and `/close-panel` dismisses it. Navigation closes the previous conversation’s panel. Below 100 columns the side panel hides.

The regression fixture in `tests/fixtures/panel` is a compiled Rust guest for `chord:plugin@0.1.0`. Its source uses the same build and component-encoding procedure as the Chord B3 fixtures, with `serde_json` added to the guest dependencies. It publishes 50 labeled rows and changes the panel title to include the actual command argument. The standalone terminal test uses this installed component against an authenticated upstream session. Remote delivery of TUI components and live replacement of the component binary are outside this adapter.

The installed guest passed on the standalone launcher in a 120×14 tmux PTY in 3.576 s. The run exercised slash arguments, F4 dispatch, guest publications, wheel scrolling in both directions at column 100 and row 5, F2/Page Down/Escape, retained scroll after a guest update, resizing to 80 and 100 columns, and dismissal. Both new regression tests failed with their targeted mutations (discarding command arguments and replacing the mounted frame on update) and passed after restoration. The two crate suites went from 30 to 32 passing tests, with zero removed. The final test build took 1m07s, the guest test 2.53 s, and the headless suite 0.12 s.

All six recorded application cases replayed successfully. The full upstream TUI oracle ran 133 cases in 401.073 s with 130 passing, two failing and one cancelled. On focused retry, the real-worker external-editor case and changed-bundle case passed. The launcher clipboard case timed out at tui-actions.test.ts line 378. The three-case retry took 38.994 s.
