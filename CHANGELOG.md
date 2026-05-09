# Changelog

## [0.5.1](https://github.com/MikeGrier/cargo-mcp-rs/compare/v0.5.0...v0.5.1) (2026-05-09)


### Bug Fixes

* correct apply_rustc_env doc comment and README wording per review feedback ([63be067](https://github.com/MikeGrier/cargo-mcp-rs/commit/63be0670d980aec2d52644ae8f63dbb07c4d2e96))

## [0.5.0](https://github.com/MikeGrier/cargo-mcp-rs/compare/v0.4.1...v0.5.0) (2026-05-08)


### Features

* **extension:** add icon, gallery banner, and screenshot placeholders for Marketplace listing ([4ca4a43](https://github.com/MikeGrier/cargo-mcp-rs/commit/4ca4a43829c56fdbc4c1d2bc97e9d9bd5afe5747))
* **retry:** retry idempotent cargo invocations on transient Windows file-busy errors ([5a6f890](https://github.com/MikeGrier/cargo-mcp-rs/commit/5a6f89047cd463340de08a9560e022e335d1ff88))
* **retry:** retry idempotent cargo invocations on transient Windows file-busy errors ([6cfffe8](https://github.com/MikeGrier/cargo-mcp-rs/commit/6cfffe89d8fe7a906f114742e2d03e4345b990ec))


### Bug Fixes

* **retry:** gate retries to idempotent subcommands and tighten busy-error patterns ([629fd69](https://github.com/MikeGrier/cargo-mcp-rs/commit/629fd697b3012acc96f36b873154b42bbdbef371))
* **retry:** tighten allowlist, surface retry notice in JSON mode, serialize tests ([e5f9334](https://github.com/MikeGrier/cargo-mcp-rs/commit/e5f93344403badd353443c1f5517c805018ecbbd))

## [0.4.1](https://github.com/MikeGrier/cargo-mcp-rs/compare/v0.4.0...v0.4.1) (2026-05-08)


### Bug Fixes

* **extension:** point Marketplace Q&A link to GitHub Discussions ([c728e69](https://github.com/MikeGrier/cargo-mcp-rs/commit/c728e69c5d6b9cc80d9cf852bfd4793512a41dd3))
* **extension:** point Marketplace Q&A link to GitHub Discussions ([aab6844](https://github.com/MikeGrier/cargo-mcp-rs/commit/aab68449245ecc72f3c711d39c21a84c51baae2c))

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
