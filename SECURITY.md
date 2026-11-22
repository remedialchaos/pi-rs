# Security Policy

This document describes the security model for `pi-rs` (`remedialchaos/pi-rs`) and how to report vulnerabilities.

## Security Model

Like upstream `pi`, the `pi` coding agent runs locally within the security boundary of the user account that launches it. Built-in tools (such as `read`, `write`, `edit`, `bash`, and `powershell`) operate with the permissions of that user and process. If you need OS-level isolation for tool execution, run `pi` inside a container or virtual machine.

`pi-rs` treats the local user account and files writable by that account as inside the same trust boundary as the `pi` process itself. If an attacker already has local write access to `~/.pi`, the workspace, shell startup files, or environment variables, reports relying on that prior write access are not security vulnerabilities unless they show how `pi-rs` grants that access or crosses an OS privilege boundary.

Where `pi-rs` explicitly enforces sandbox boundaries — specifically:

- `pi-rs-codemode` (`wasmtime`-hosted `quickjs.wasm` scripts having no ambient filesystem, network, or timer capabilities), and
- WASM/WIT dynamic extension and Chord plugin sandboxing (`wasmtime` capability grants)

— escapes from those declared sandbox boundaries **are** in scope.

## Reporting a Vulnerability

Please report security vulnerabilities privately through **GitHub Security Advisories** for this repository (`https://github.com/remedialchaos/pi-rs/security/advisories/new`).

Do not open a public issue for security-sensitive reports.

Please include:

- A description of the issue and its impact
- Steps to reproduce, proof of concept, or relevant logs
- Affected crate, binary, version, commit SHA, or configuration
- Any known mitigations

## Scope

In scope:

- Crates, binaries, wire protocols, parsers, and runtime code in `remedialchaos/pi-rs`
- Sandbox escapes from `pi-rs-codemode` or WASM/WIT plugin isolation
- Unintended privilege or capability boundary bypasses

## Out of Scope

- Expected local code execution by built-in coding-agent tools (the CLI intentionally runs with the local user's OS permissions unless containerized)
- Risks from working in untrusted repositories or running with untrusted local configuration
- Prompt injection via repository files, comments, or model outputs
- Issues caused by untrusted MITM proxies or exposing a local server to the public internet
- Exposed third-party or user-controlled credentials
- Reports requiring prior local write access to user-owned files (`~/.pi`, workspace files, dotfiles, environment) unless `pi-rs` itself granted that access
- Upstream `earendil-works/pi` infrastructure or services (report those to upstream directly)
