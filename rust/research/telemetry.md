# Telemetry dependency fit

Primary sources consulted November 22, 2026:
- https://docs.rs/uuid/latest/uuid/struct.Uuid.html documents `Uuid::new_v4` and its `v4` feature. This supplies the recorder scope corresponding to `crypto.randomUUID()`.
- https://docs.rs/futures/latest/futures/executor/fn.block_on.html documents the runtime-independent test executor. Used only in tests.
- Generated `rust/pi-rs-telemetry-render/src/js.rs` provides JavaScript number rendering and sort behavior already checked against the 210 recorded renderer calls.

The library otherwise uses std futures, Arc/Mutex, Instant/SystemTime, indexmap insertion order, and serde JSON. These avoid selecting an async runtime and keep context identity shareable across threads. JSON schemas remain data, not validators. The typed Rust wrapper uses schema marker traits to constrain vocabulary without runtime validation.
