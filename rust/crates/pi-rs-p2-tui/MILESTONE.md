# Milestone 3.2-tui

Worktree base `18bd5be94`. The crate builds and passes its local gates. Full upstream behavior parity remains open.

| Gate | Result | Evidence |
| --- | --- | --- |
| Formatting | Pass | Crate-scoped `cargo fmt -- --check` |
| Offline Clippy | Pass | All targets, warnings denied |
| Offline Cargo tests | Pass | 5 unit tests and 10 integration tests |
| Mutation checks | Pass | All 15 tests failed under deliberate behavior changes and passed after restoration |
| Upstream parity | Partial | Native Mermaid, headless masked input, shell status, settings paths, slash arguments, list actions, palette validation and transcript anchors have direct behavioral checks |

Tests added 15, removed 0. The inherited draft failed to compile, so it had no runnable suite baseline. The final Cargo test command took 23.893 seconds including compilation. The three test binaries reported 0.00, 0.01 and 0.02 seconds.

The Rust host mounts a terminal, routes commands and accepts contributed commands and entry/tool renderers. The transcript supports native Mermaid, shell entries, tool cards and key-based reading anchors. The next parity work is matching the upstream incremental transcript lifecycle, panel scrolling and tree layout, and exercising plugin dispatch and live theme reload end to end. There is no complete upstream differential replay for this milestone.
