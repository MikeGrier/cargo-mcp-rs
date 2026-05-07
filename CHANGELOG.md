# Changelog

## [0.4.0](https://github.com/MikeGrier/cargo-mcp-rs/compare/v0.3.2...v0.4.0) (2026-05-07)


### Features

* **progress:** prefix per-crate progress messages with the cargo verb ([da71bc8](https://github.com/MikeGrier/cargo-mcp-rs/commit/da71bc819ecb9febea7ee2b2943cbabfea343a8c))


### Bug Fixes

* **tools:** warn that working_dir defaults to the server CWD; prefix progress messages with the cargo verb ([341dd82](https://github.com/MikeGrier/cargo-mcp-rs/commit/341dd8291daed6eaff0977b4f1436ba0e8058c6f))
* **tools:** warn that working_dir defaults to the server process CWD ([c60337b](https://github.com/MikeGrier/cargo-mcp-rs/commit/c60337bca76b0f31e10a3f5a7d4f48dcc275aedd))

## [0.3.2](https://github.com/MikeGrier/cargo-mcp-rs/compare/v0.3.1...v0.3.2) (2026-05-07)


### Bug Fixes

* package extension with --pre-release for odd-minor tags ([cba0446](https://github.com/MikeGrier/cargo-mcp-rs/commit/cba0446d77dbd84a191c417e9b08979c7de62d76))
* use github.event.inputs for pre_release on push events ([1ecc58a](https://github.com/MikeGrier/cargo-mcp-rs/commit/1ecc58a357a76283047af8112414855ede40f4e4))


### Miscellaneous Chores

* release 0.3.2 ([f7db078](https://github.com/MikeGrier/cargo-mcp-rs/commit/f7db078f2689b8eee45fbe9a149c483dd4a41677))

## [0.3.1](https://github.com/MikeGrier/cargo-mcp-rs/compare/v0.2.0...v0.3.1) (2026-05-07)


### Features

* toolchain resolver, cargo_diagnostic tool, invocation header ([#6](https://github.com/MikeGrier/cargo-mcp-rs/issues/6)) ([c29139a](https://github.com/MikeGrier/cargo-mcp-rs/commit/c29139a61262c5c5703cc139efb8cb295ed1953b))


### Bug Fixes

* annotate Cargo.toml workspace version for release-please ([#8](https://github.com/MikeGrier/cargo-mcp-rs/issues/8)) ([6f6d401](https://github.com/MikeGrier/cargo-mcp-rs/commit/6f6d40155659ae16f8c76b573d20bddd8d6f12e3))


### Miscellaneous Chores

* release 0.3.1 ([6d8b5e1](https://github.com/MikeGrier/cargo-mcp-rs/commit/6d8b5e119fb86c508b16f3184ec55b0203b6fdd5))

## [0.2.0](https://github.com/MikeGrier/cargo-mcp-rs/compare/v0.1.2...v0.2.0) (2026-05-05)


### Features

* initial implementation of cargo-mcp MCP server v0.1.0 ([#1](https://github.com/MikeGrier/cargo-mcp-rs/issues/1)) ([20eed94](https://github.com/MikeGrier/cargo-mcp-rs/commit/20eed9429d1ac177769efb37fd3fc1aa684f5d30))
* release-please workflow, workspace version, backtrace in CI ([#4](https://github.com/MikeGrier/cargo-mcp-rs/issues/4)) ([334fb82](https://github.com/MikeGrier/cargo-mcp-rs/commit/334fb8259e35c53be389c25641884e7e36d65fb4))
