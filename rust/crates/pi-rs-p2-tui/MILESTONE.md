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

The crate is a native library. The terminal checks cover its mounted client and a registered Rust contribution. Standalone p2 startup, dynamically loaded guests, mouse scrolling and full upstream screen comparison were outside these checks. The crate has no cases.jsonl or rust-anything.json, so rust-anything diff and gate could not run a crate differential. The coordinator reported 6/6 for the separate p2 HTTP oracle and owns root replay.
