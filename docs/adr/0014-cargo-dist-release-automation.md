# Cargo Dist Release Automation

qgh will use `cargo-dist` as the release automation path for version-tagged builds, GitHub Release artifacts, checksums, and Homebrew tap publication to the existing public `juicyjusung/homebrew-tap` repository. This supports the required one-command install path, `brew install juicyjusung/tap/qgh`, while keeping the installed binary self-contained.

The release gate proves install-time readiness with `qgh --version`, help, `qgh init`, and local/diagnostic commands. The first-use smoke gate is separate: after the user has GitHub authentication and an explicit repo scope, `qgh init -y && qgh sync && qgh query` must work. Repository bootstrap, authentication, and sync remain explicit qgh commands because they touch private GitHub scope and local derivative data.

The current official release target matrix is macOS Apple Silicon (`aarch64-apple-darwin`) and Linux x86_64 (`x86_64-unknown-linux-gnu`). macOS Intel is not an official target because `ort` 2.0.0-rc.12 does not provide the Intel-mac prebuilt ONNX Runtime required by the `fastembed-provider` release feature; direct Intel builds are subject to the same dependency constraint. Restore `x86_64-apple-darwin` only after the release dependency set provides a supported Intel-mac runtime and the complete release-contract, build, Homebrew install, and smoke-test gates pass on Intel hardware. Linux ARM64 and Windows packages remain later distribution targets unless a user-facing release requirement changes.

The release integrity gate requires artifact checksums, Homebrew `sha256` verification, and GitHub Artifact Attestations. Separate `cosign` or `minisign` signing is deferred until there is a concrete key-management owner and user need.

The release trigger is an explicit `Cargo.toml` version bump commit followed by pushing a matching `vX.Y.Z` git tag. Automated version PR tooling such as `release-plz` and manual versioned `workflow_dispatch` releases are deferred until the first release path is stable.

The release preflight gate includes the existing qgh release-contract tests, `cargo test`, `cargo dist plan`, `cargo dist build`, and a smoke test for the generated Homebrew formula. Live dogfood against `juicyjusung/qgh` remains a first-use/manual release checklist item, not a blocking CI gate, because it depends on GitHub auth, live API state, and rate-limit behavior.

Publishing the generated formula to `juicyjusung/homebrew-tap` uses a repo secret named `HOMEBREW_TAP_TOKEN`. The token must be a fine-grained GitHub token scoped to contents write on only the tap repository; qgh runtime tokens and developer user tokens must not be reused for release publication.

The install documentation surface is split between `README.md` for the user-facing one-command install and first-use path, and `docs/release-checklist.md` plus `docs/release-artifact.json` for release-gate truth.

The first implementation slice should be one release-system issue rather than separate docs, workflow, and smoke-test issues. The work is mostly standardized release plumbing, and the acceptance criteria should keep the docs, `cargo-dist` workflow, tap publication, and smoke verification aligned in one lane.
