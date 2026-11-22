# Contributing to Termlet

[简体中文](CONTRIBUTING.md) · **English**

Thanks for wanting to help Termlet! Termlet is maintained by one person with limited time, so for now it **mainly accepts issues and discussions**: bug reports, feature requests and feedback are all welcome. If you want to contribute code, please **open an issue first describing what you want to change**, and start once the direction is agreed, so your work isn't wasted.

Please follow the [Code of Conduct](CODE_OF_CONDUCT.en.md) when taking part in this project.

## Reporting bugs

- Open an issue in [Issues](https://github.com/termlet/termlet/issues) with the "Bug report" template, after searching for duplicates.
- Include the version info: in Settings → About, click "Copy version info" and paste it into the issue. If the problem is hard to pin down, also include "Copy diagnostics" from "Diagnostics and crash reports".
- **Never include passwords, private keys, tokens or TOTP secrets**; you may also hide host addresses and user names in logs and screenshots.
- **Do not report security vulnerabilities in public issues**; report them privately as described in [SECURITY.md](SECURITY.md) (GitHub's private vulnerability reporting, or email hi@termlet.dev).

## Feature requests and discussions

- For a concrete feature request, open an issue with the "Feature request" template, one request per issue.
- Rough ideas and usage questions go to [Discussions](https://github.com/termlet/termlet/discussions).

## Contributing code

1. Open an issue first (or comment on an existing one) describing what you want to change and how, and wait for the maintainer's reply.
2. Fork the repository and work on a new branch; keep each pull request to one change, with tests.
3. Before submitting, run the commands under "Checks" below locally.
4. Open a pull request that references the issue. Your first pull request requires signing the Contributor License Agreement (see below).

Code comments, documentation and commit messages follow the project's convention of Simplified Chinese; English is fine if that is easier for you, and the maintainer will tidy it up when merging.

### Contributor License Agreement (CLA)

Termlet is open source under the [AGPL-3.0-or-later](LICENSE). To keep the option of licensing it under other terms later (for example, a commercial license), contributions require signing the [Individual Contributor License Agreement](docs/CLA.md): you keep the copyright in your contribution, and grant the maintainer the right to use, modify and sublicense it (including under terms other than the AGPL), together with a matching patent license; in return, the maintainer commits to making your contribution available under the project's open-source license whenever it is released. The English version of the agreement prevails; the Chinese translation is for reference only.

Signing is handled by [CLA assistant](https://cla-assistant.io/): on your first pull request it posts a comment; follow its link, sign in with your GitHub account and accept the agreement. You sign once and it covers all your later contributions. Pull requests without a signed CLA cannot be merged.

## Development environment

- Rust: install through [rustup](https://rustup.rs); `rust-toolchain.toml` pins the version (1.99.1), installed automatically on the first build. A full first build takes about 5 minutes.
- Windows: Visual Studio Build Tools (MSVC and the Windows SDK).
- Linux: `libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev libfontconfig-dev libfreetype-dev` (Debian / Ubuntu package names); you can also build and test in Docker with `testing/linux/Dockerfile` (see the comments in it).
- macOS: the Xcode command-line tools. The maintainer has no macOS or Linux machine, so help testing on these platforms is welcome.
- Integration tests against real servers need Docker; building the Windows installer also needs Node.js and NSIS 3 (makensis).

## Project layout

```
crates/termlet-proto/     Protocol between the CLI and the app (requests, responses, error codes, exit codes) and the Agent Skill; the termlet command only depends on it
crates/termlet-io/        Byte interface of terminal sessions, shared by the core and the terminal
crates/termlet-core/      Core: data folder, storage, vault, SSH, SFTP, forwarding, Telnet, serial, local terminals, backup, sync… no GPUI dependency
crates/termlet-terminal/  Terminal engine (alacritty_terminal) and the GPUI terminal view; only knows byte transports
crates/termlet-desktop/   Desktop app (Termlet.exe); strings in locales/*.yml, UI integration tests in tests/
crates/termlet-cli/       The termlet command
packaging/                Windows installer (NSIS script, license notice) and the portable marker portable.txt, macOS Info.plist and icon set, Linux desktop entry, icons, install.sh and deb / rpm / AppImage packaging (package.sh, nfpm.yaml, AppRun), bundled license texts, the remote config source, and the icon rendering script
testing/sshd/             OpenSSH and Telnet server for integration tests (Docker image)
testing/x11/ nosftp/ sshcert/   Test containers for X11 forwarding, SCP without SFTP, and OpenSSH certificates (see the comments in each Dockerfile)
testing/linux/            Build environment for checking the Linux version in Docker locally (same as CI)
.github/workflows/        GitHub Actions; see "CI" below
scripts/                  Release script, window capture and input posting (for checking the interface), and scripts that generate the file icons and brand logos
docs/                     User guide (guide.md), Contributor License Agreement, brand assets and screenshots
docs/rust/                Architecture, usage statistics design, and the Traditional Chinese, Japanese and Korean glossaries
```

## Common commands

```bash
cargo run                                          # start the desktop app (debug builds use the termlet-dev data folder, not your real data)
TERMLET_DATA_DIR=<temp folder> TERMLET_VAULT_PROVIDER=file TERMLET_UPDATE_URL=off TERMLET_CRASH_URL=off DO_NOT_TRACK=1 cargo run
                                                   # try it with a temporary data folder: credentials kept in a file (nothing left in the
                                                   # system credential manager), no update checks, no remote configuration, no usage
                                                   # statistics or crash reports
cargo test --workspace                             # all tests; the UI integration tests are in crates/termlet-desktop/tests/
cargo test -p termlet-core                         # only the core (without building the UI)
cargo nextest run --workspace                      # the same test runner as CI (cargo install cargo-nextest --locked); run doc tests with cargo test --workspace --doc
cargo test -p termlet-terminal --release throughput -- --ignored --nocapture   # terminal parsing throughput
node packaging/render-icons.mjs                    # regenerate the icons after changing the logo (needs Chrome or Edge)
```

## Tests against real servers

These tests are enabled by environment variables and skipped without them:

| Environment variable | What it tests |
|---|---|
| `TERMLET_TEST_SSH=127.0.0.1:2222` | the sshd in the test container (see below) |
| `TERMLET_TEST_TELNET=127.0.0.1:2323` | Telnet in the same container |
| `TERMLET_TEST_WSL=<distribution>` | WSL |
| `TERMLET_TEST_WEBDAV=<folder URL>` | WebDAV (with `TERMLET_TEST_WEBDAV_USER` and `TERMLET_TEST_WEBDAV_PASSWORD`) |
| `TERMLET_TEST_TRASH=1` | moving to the Recycle Bin (which leaves test files there) |

Build the test container and run the tests:

```bash
docker build -t termlet-e2e-sshd testing/sshd
docker run -d --name termlet-sshd -p 2222:22 -p 2323:23 termlet-e2e-sshd
TERMLET_TEST_SSH=127.0.0.1:2222 cargo test -p termlet-core ssh
TERMLET_TEST_TELNET=127.0.0.1:2323 cargo test -p termlet-core telnet
```

## Checks

Pull requests must pass these checks (CI runs them too):

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings   # no warnings allowed
cargo test --workspace                                  # or cargo nextest run --workspace
cargo deny check                                        # dependency licenses, sources and security advisories (see deny.toml)
gitleaks git .                                          # don't commit secrets
```

## CI

- Every push: formatting, clippy, tests, dependencies and secrets are checked on Linux.
- Pushes to main and pull requests also run the full Windows clippy and tests, the integration tests against a real sshd on Linux, and the core tests on arm64.
- None of this runs when only documentation changes.
- Tests run with cargo-nextest, which works locally too (see [Common commands](#common-commands)).

| Workflow | What it does |
|---|---|
| `ci.yml` | checks on Linux on every push; on pushes to main and pull requests it also calls the next three |
| `windows.yml` | Windows clippy and tests |
| `sshd.yml` | integration tests against a real sshd |
| `arm64.yml` | core tests on arm64 |
| `release.yml` | tests and release builds of the three platforms when a version tag is pushed, then signing and publishing once approved |
| `remote-config.yml` | signing and publishing the remote config |

## Conventions

The full conventions are in [CLAUDE.md](CLAUDE.md) (in Chinese; it is also the project guide for AI coding assistants). The essentials:

- Dependencies must use permissive licenses (MIT, Apache-2.0, BSD and the like), checked by `cargo deny check`; **do not copy code from GPL or other copyleft projects** (for example ShellRS, OxideTerm or Zed's terminal_view; take ideas only).
- The UI is built on gpui-kit 0.8.1: read `.claude/skills/gpui-kit/` and `.claude/skills/gpui-kit-design-guides/` before writing UI code, and treat the crates.io sources as the reference for its API; use the theme and rem sizes instead of hard-coded colors and sizes.
- UI text lives in `crates/termlet-desktop/locales/*.yml`. Simplified Chinese is the source language; change the English, Traditional Chinese, Japanese and Korean text of the same key together (Traditional Chinese follows [`docs/rust/繁体中文用语.md`](docs/rust/繁体中文用语.md), Japanese and Korean follow [`docs/rust/日文韩文用语.md`](docs/rust/日文韩文用语.md)).
- Secrets never go into logs or `Debug` output, and never into the database in plain text.
- For changes users will notice (added, changed, fixed, security), add an entry to the section marked "unreleased" at the top of both [CHANGELOG.md](CHANGELOG.md) and [CHANGELOG.en.md](CHANGELOG.en.md); at release time the maintainer turns that section into the version number and date, and both the GitHub Release and the website's changelog are taken from it.
- How features work is described in the [user guide](docs/guide.en.md) (update `docs/guide.md` and `docs/guide.en.md` together); the README only gives an overview.
- The design is described in the [architecture document](docs/rust/技术方案.md) (in Chinese).

### New features and options

Termlet deliberately stays small and clear, so please open an issue to discuss additions first:

- Say who needs it, whether it overlaps with an existing feature, and whether it could be an option of one; "another app has it" isn't a reason on its own.
- If a good default solves it, don't add an option. New features are off by default or appear only when used, and don't do work at startup or keep running in the background.
- The UI has limits (controls per settings page, description length, context menu items, toolbar buttons shown by default), checked by `ui_limits_tests` in `cargo test`. If you go over, merge, fold or cut instead of raising the limit.
- Reuse the terms the UI already uses; don't introduce synonyms.
- Explain why a new dependency is needed; the lock file should only gain the new packages.

The full rules are in the "防臃肿" (avoiding bloat) section of [CLAUDE.md](CLAUDE.md) (in Chinese).

## Releasing (maintainers)

Building, signing and publishing all happen on GitHub Actions; see the comments at the top of `.github/workflows/release.yml`.

### The three steps

1. **Prepare**: `node scripts/release.mjs prepare <version>` bumps the version and turns the "unreleased" section at the top of `CHANGELOG.md` / `CHANGELOG.en.md` into this version with the date; check, commit and push.
   - The usage statistics receiver (`api.termlet.dev`) checks every value against an allowlist.
   - When the client starts sending a new value (such as the interface languages `ja` and `ko`), update and deploy the receiver first, then release the client, or the new client's statistics are rejected outright.
2. **Push the `v<version>` tag**: `release.yml` checks the tag, version and changelog, and runs the full Windows tests and the integration tests against a real sshd. Once both pass, it builds the unsigned packages:
   - the Windows x64 and arm64 installers and portable zips (for each architecture, both from the same staging folder);
   - the Linux x64 and arm64 tar.gz, deb, rpm and AppImage (`packaging/linux/package.sh`; nfpm for the deb and rpm, appimagetool for the AppImage, per architecture at fixed versions with checked checksums);
   - the macOS Termlet.app (zip, ad-hoc signed);
   - each with its version file (`latest.yml`, `latest-arm64.yml`, `latest-linux.yml`, `latest-linux-arm64.yml`, `latest-mac.yml`), kept as build artifacts for 14 days.

   None of these steps touch any secret. The arm64 jobs run the core tests on arm64 runners before packaging; being experimental, a failure there doesn't hold back the x64 and macOS release, and that version ships without the arm64 version file and packages (the ones already on the download site stay, and arm64 clients stay on the older version).
3. **Approve**: the `release` environment needs a maintainer's approval on the web or on a phone; only then does the signing and publishing job get the secrets and start. It:
   - first checks the artifacts against the digests reported by the build jobs (before touching any secret);
   - signs the version files and `SHA256SUMS` with the current key, and creates build provenance attestations;
   - uploads to the download site `dl.termlet.dev` (packages first, the `latest*.yml` files last), purges the CDN cache and fetches everything back over https to check it;
   - creates the GitHub Release in this repository (with this version's sections of the Chinese and English changelogs as notes);
   - finally rebuilds the website (so the download page and changelog show the new version).

   If it fails halfway, re-run that job on the web.

### Remote config

- The remote config is `dl.termlet.dev/update.json`, with its source in `packaging/remote-config/update.json`.
- After committing a change, or when its signature is about to expire, run the Remote config workflow by hand (it also uses the `release` environment and needs approval).

### Releasing locally (fallback)

- If CI is unavailable, or a key rotation needs the offline backup key, `fetch`, `sign --key` and `publish` also work locally.
- They upload with the signed-in Cloudflare `cf` command-line tool by default; see the comments at the top of `scripts/release.mjs`.

### release.mjs commands

```bash
node scripts/release.mjs prepare <version>          # before a release: bump the version in Cargo.toml (and Cargo.lock), and turn the "unreleased" section at the top of the changelogs into this version with today's date (try --dry-run first)
node scripts/release.mjs notes <version>            # check and print this version's GitHub Release notes (its sections of the Chinese and English changelogs)
node scripts/release.mjs build [--arch x64|arm64]  # local release build plus the installer termlet-<version>-win-x64.exe, the portable termlet-<version>-win-x64-portable.zip and latest.yml (listing both), into target/dist; for arm64, the two -win-arm64 packages and latest-arm64.yml (staging folder stage-arm64; cross-compiles when the local toolchain is another architecture, which needs the Rust target and MSVC's ARM64 build tools)
node scripts/release.mjs manifest --file <package> [--file <package>]...   # write the version file latest-linux.yml / latest-linux-arm64.yml (listing the tar.gz, deb, rpm and AppImage) or latest-mac.yml (called by CI after packaging)
node scripts/release.mjs fetch --tag v<version>     # local release (fallback): download what CI built for this tag (packages and version files of every platform and architecture) into target/dist/v<version> (gh auth login first)
node scripts/release.mjs sign --key <signing key file> [--dir <folder>]   # sign the latest*.yml files and create SHA256SUMS
node scripts/release.mjs publish [--dir <folder>] [--dry-run]   # publish the signed files: upload to the download site dl.termlet.dev (R2), purge the CDN cache and check them back, create this version's GitHub Release in this repository, and call the deploy hook to rebuild the website
node scripts/release.mjs sign-config --key <signing key file> [--days 90] <update.json>   # sign the remote config, with an expiry (normally done by remote-config.yml)
node scripts/release.mjs publish-config [--dry-run] <update.json>   # upload the signed remote config to the download site
node scripts/release.mjs newkey --out <folder outside the repo>   # rotate the update signing key (steps in crates/termlet-core/src/update/keys.rs)
node scripts/release.mjs licenses                  # write the license texts of the Rust dependencies on their own (generated automatically when packaging, into the licenses folder of the installation)
```

## Documentation

- [User guide](docs/guide.en.md): installing and using each feature
- [Architecture](docs/rust/技术方案.md) (in Chinese): design and the reasons behind it
- [Usage statistics design](docs/rust/使用统计方案.md) (in Chinese)
- [Traditional Chinese glossary](docs/rust/繁体中文用语.md), [Japanese and Korean glossary](docs/rust/日文韩文用语.md) (in Chinese)
- [Brand assets](docs/brand/README.md) (in Chinese)

## License

Your contributions are licensed under the [Contributor License Agreement](docs/CLA.md); Termlet as a whole is released under the [AGPL-3.0-or-later](LICENSE). For use of the "Termlet" name and logo, see [TRADEMARK.en.md](TRADEMARK.en.md).
