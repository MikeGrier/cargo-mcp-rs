# Changelog

## [0.6.0](https://github.com/MikeGrier/cargo-mcp-rs/compare/v0.5.7...v0.6.0) (2026-06-07)


### Features

* plumb standard cargo options through build-graph tools ([a1a2d56](https://github.com/MikeGrier/cargo-mcp-rs/commit/a1a2d56d38a0245371a3b6a82cbab483993e3709))
* prefix progress lines with "Cargo" and add a profile tag ([56b9886](https://github.com/MikeGrier/cargo-mcp-rs/commit/56b988649ef0d0d01f5df1ed4c237e77dc34fffe))
* **tools:** add `toolchain` override parameter to cargo tools ([84ec09c](https://github.com/MikeGrier/cargo-mcp-rs/commit/84ec09ce80077223f804139ae9a1688639caaac8))


### Bug Fixes

* abbreviate test/bench/doc profile tags in progress lines ([a41e441](https://github.com/MikeGrier/cargo-mcp-rs/commit/a41e441959192546694174c2e5ad6201e2caec26))
* apply cargo_test timeout to test execution only, not build/link ([8d5d1c6](https://github.com/MikeGrier/cargo-mcp-rs/commit/8d5d1c6982435c2a6490a157cb34194393df383a))
* gate --exclude on workspace and prefer --profile over --release ([9553d9d](https://github.com/MikeGrier/cargo-mcp-rs/commit/9553d9dab9284280371bedc4894501350c7202b4))

## [0.5.7](https://github.com/MikeGrier/cargo-mcp-rs/compare/v0.5.6...v0.5.7) (2026-06-04)


### Bug Fixes

* clarify timeout default scope and add unit tests for opt_timeout_explicit ([fa404a3](https://github.com/MikeGrier/cargo-mcp-rs/commit/fa404a34cf8d15acf20a8399846429e520e5310b))
* distinguish explicit timeout_secs:0 from omitted in cargo_test\n\nAdd opt_timeout_explicit() returning Option&lt;Option&lt;Duration&gt;&gt; so\ncall_test() can tell the caller explicitly disabled the timeout\n(Some(None)) from the caller not supplying the field at all (None).\nWhen absent, the server default (cargo-mcp.test.timeoutSecs) applies;\nwhen explicitly 0, no timeout is used for that run regardless of the\nserver default. ([f7dd1c7](https://github.com/MikeGrier/cargo-mcp-rs/commit/f7dd1c78e6a11ee66c2f77db6d2c0ffc343e4e0e))
* expose cargo_test timeout options ([b0115fb](https://github.com/MikeGrier/cargo-mcp-rs/commit/b0115fbe9577d01a5d425c00c0b8c41bc5738ebc))
* expose cargo_test timeout options ([4d8d918](https://github.com/MikeGrier/cargo-mcp-rs/commit/4d8d918cb3f5116bf119341caf8c6902c14d8d93))

## [0.5.6](https://github.com/MikeGrier/cargo-mcp-rs/compare/v0.5.5...v0.5.6) (2026-06-04)


### Bug Fixes

* preserve cargo test output in ndjson stream ([7e86bfe](https://github.com/MikeGrier/cargo-mcp-rs/commit/7e86bfe756c763e6e233dc335aea19bffa3e3b64))
* preserve cargo test output in ndjson stream ([547dd3b](https://github.com/MikeGrier/cargo-mcp-rs/commit/547dd3bc54d0177312f6f0204187d61a7a206c8c))

## [0.5.5](https://github.com/MikeGrier/cargo-mcp-rs/compare/v0.5.4...v0.5.5) (2026-05-24)


### Bug Fixes

* **invoke:** add timeout_secs and tree-kill on cancel/timeout ([74b66ee](https://github.com/MikeGrier/cargo-mcp-rs/commit/74b66eee37652e466485e56e85da256fd5ddcd9f))
* **invoke:** add timeout_secs and tree-kill on cancel/timeout ([4ba3429](https://github.com/MikeGrier/cargo-mcp-rs/commit/4ba3429ed9f5e9baa11221d0c2cf8f357714ba30))
* **invoke:** use checked_add for deadline and report real elapsed on timeout ([ecace29](https://github.com/MikeGrier/cargo-mcp-rs/commit/ecace29886e19093967f2639ecd5da1143875f76))

## [0.5.4](https://github.com/MikeGrier/cargo-mcp-rs/compare/v0.5.3...v0.5.4) (2026-05-12)


### Features

* **extension:** gate Restart Manager lookup behind opt-in setting ([b600803](https://github.com/MikeGrier/cargo-mcp-rs/commit/b600803842bb58067f14571bcab30f08ec34be9d))
* **tools:** emit invocation header as NDJSON record instead of shell-style banner ([a7417d4](https://github.com/MikeGrier/cargo-mcp-rs/commit/a7417d49cbce7dfb0b5116891b769a411e5b7eca))


### Bug Fixes

* **rm:** opt-in Restart Manager holder reporting + NDJSON invocation header ([cdf6a37](https://github.com/MikeGrier/cargo-mcp-rs/commit/cdf6a371822524ed1bbf3b96e86f7923cc7d7091))
* **tools:** emit cargo stderr as NDJSON record; address PR review ([e8d154b](https://github.com/MikeGrier/cargo-mcp-rs/commit/e8d154b1711b7fc9752edf447453146c65e9d8e0))
* **tools:** strict NDJSON output for JSON-mode tools; address PR review ([67ce9a7](https://github.com/MikeGrier/cargo-mcp-rs/commit/67ce9a7e447c1f0aa442ae2842da6b57e671e543))
* **tools:** surface stderr on JSON-mode tool failures so RM holder report reaches the user ([4f2daaf](https://github.com/MikeGrier/cargo-mcp-rs/commit/4f2daaf1e01128ae7c567631033b556270c991fa))


### Miscellaneous Chores

* release 0.5.4 ([1057936](https://github.com/MikeGrier/cargo-mcp-rs/commit/1057936a0c998b280b58d16a5774bd064af10714))

## [0.5.3](https://github.com/MikeGrier/cargo-mcp-rs/compare/v0.5.2...v0.5.3) (2026-05-11)


### Bug Fixes

* **busy-files:** extract paths from cargo JSON and `at path` form ([7ec29ef](https://github.com/MikeGrier/cargo-mcp-rs/commit/7ec29ef89df11024c447a97b6bfd35cd6d8497eb))
* **busy-files:** extract paths from cargo JSON and `at path` form ([579d4b4](https://github.com/MikeGrier/cargo-mcp-rs/commit/579d4b48a354c3fe724a4c4e6eeb3b53b610750f))
* **busy-files:** silence dead_code on AppKind for non-Windows builds ([93b60ba](https://github.com/MikeGrier/cargo-mcp-rs/commit/93b60ba1646d96b6622f5a3656217629ee8a56ec))
* **busy-files:** un-escape Debug-quoted backslashes in at-path captures ([51d6ff3](https://github.com/MikeGrier/cargo-mcp-rs/commit/51d6ff3664989f08778e66051c1078292fc54035))

## [0.5.2](https://github.com/MikeGrier/cargo-mcp-rs/compare/v0.5.1...v0.5.2) (2026-05-09)


### Bug Fixes

* address Copilot review comments on busy_files ([aa4b62a](https://github.com/MikeGrier/cargo-mcp-rs/commit/aa4b62a04031f2a8e00874a24d497481d21d6427))
* diagnose Windows file-busy errors via Restart Manager ([85b819b](https://github.com/MikeGrier/cargo-mcp-rs/commit/85b819ba37b6e9de7ad2683963d26c98af668a71))
* gate (os error 32/5) busy indicators on Windows to match invoke ([efbc8ef](https://github.com/MikeGrier/cargo-mcp-rs/commit/efbc8ef2a0662e6f83c3ba1490e9247d7459e330))
* harden strip_unc_prefix against lossy paths and verbatim UNC ([c35ef7c](https://github.com/MikeGrier/cargo-mcp-rs/commit/c35ef7c30d5adc520225e5d09adfab386e29f467))
* prefer en-US for FormatMessageW with system-default fallback ([e9fe08c](https://github.com/MikeGrier/cargo-mcp-rs/commit/e9fe08ca588bab23e58e02f978622ba27895a382))
* reframe RmGetList loop as resize-to-fit, not bounded retry ([925f099](https://github.com/MikeGrier/cargo-mcp-rs/commit/925f0993cc99a275b13630484e477b114e6f7d06))
* retry RmGetList fetch on ERROR_MORE_DATA race ([4f648d8](https://github.com/MikeGrier/cargo-mcp-rs/commit/4f648d82a9ecdcb86e11e3e3f2ac7910ac332a97))

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
