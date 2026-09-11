# Echo Scribe — Working Notes for Claude

## Error handling & diagnostics (required for every feature)

Build features so failures are *debuggable without a rebuild*. This is not optional polish — wire it in as you write the feature:

- **Log at every failure + key transition.** Use `tracing` (`info!`/`warn!`/`error!`) in Rust and surface sidecar failures as structured JSON events. Logs go to the daily `echo-scribe.log` (Settings → Diagnostics → open log folder; `log_dir()` in `src-tauri/src/lib.rs`). Give logs a `target:` (e.g. `target: "drive"`) so they're greppable. Log the *result* of fallible boundary ops (keychain, network, file IO, subprocess) — both success ("stored token, N chars") and failure (the error string + relevant status/body). Never log secrets/tokens (log lengths/ids instead).
- **Surface a *friendly* message in the UI; keep raw detail in the log.** Every user-triggered action that can fail must tell the user something went wrong (toast or inline) — but show a short, human message ("Upload to Drive failed. See Settings → Diagnostics → logs for details."), never raw API JSON, status bodies, or stack traces. Log the full technical detail with `error!`/`warn!` at the failure site, then return/store the friendly string for the frontend. Don't fail silently either.
- **HTTP/IPC boundaries:** check `resp.status().is_success()` before parsing, and put the response body in the error so a 401/403/quota failure reads as itself, not a generic "missing field". Same for subprocess: capture stderr and surface the structured error kind.
- **Diagnose with logs, not hypotheses.** If a bug's cause isn't provable from data in hand, add the logging that would capture it, have the user reproduce, then fix from what the logs show. Don't guess-and-rebuild in a loop.

## Build + reinstall workflow (macOS only)

As of 2026-09-11 the dev machine is Omarchy Linux (rebuilt from Windows 11 on 2026-09-01). The macOS steps below (codesign, tccutil, `/Applications` copy, `.app` bundles) are not runnable here; they are kept for when a Mac is available. HEAD is on branch `feat/windows-voice-actions`. Windows path: `docs/WINDOWS.md` describes a work-in-progress NSIS installer built by the GitHub Actions **Windows** workflow (`src-tauri/tauri.windows.conf.json`, targets `nsis`); no fully supported Windows release yet. No Linux build path documented.

**Signing identity (stable — this is why permissions now persist).** `tauri.conf.json` → `bundle.macOS.signingIdentity` is set to the SHA-1 `F6FD1D39BE4E52054A6B72E1EC5E90A03F5E5B77`, a self-signed **"Echo Scribe Local Dev"** code-signing identity in the login keychain. Because every build is signed with the *same* cert, its designated requirement is constant (`identifier "com.echoscribe.app" and certificate root = H"f6fd…"`), so macOS keeps TCC grants (including Screen Recording) across reinstalls — no more per-rebuild re-granting. Do **not** revert the committed config to `"-"` (ad-hoc), which changes the signature every build and drops grants.

- **CI / release builds stay ad-hoc.** The cert is local-only, so `release.yml` sets `APPLE_SIGNING_IDENTITY: "-"` on the build step (env overrides the config value). That's how distribution builds keep working without the cert. Don't remove that env override, and don't add signing secrets to CI.
- **If a local build fails with "no identity found", or reinstalls suddenly start dropping permissions again**, the login keychain lost the identity — re-import it: `"$HOME/Library/Application Support/EchoScribeSigning/reimport.sh"` (p12 password `echoscribe`). The identity is referenced by hash to dodge an ambiguous stale keyless copy of the same name in the System keychain. The very first codesign after an import can fail once (keychain ACL priming) — just rebuild.

**Default: SKIP the TCC reset.** Most rebuilds keep the same bundle identifier + Info.plist usage descriptions + entitlements + signing identity; macOS Sequoia+ then re-binds prior permission grants to the newly-signed binary automatically. The user's stated preference for this project is "skip TCC unless I say otherwise."

**Only reset TCC when this rebuild changes anything permission-related**, e.g.:

- `src-tauri/Info.plist` — `NSMicrophoneUsageDescription`, `NSScreenCaptureUsageDescription`, `NSAccessibilityUsageDescription`, `NSCalendarsFullAccessUsageDescription`, etc. (added, removed, or text changed).
- `src-tauri/tauri.conf.json` — `identifier` (bundle id), `macOSPrivateApi`, `entitlements` paths.
- `src-tauri/capabilities/*.json` — new windows or permission keys.
- `src-tauri/Cargo.toml` — TCC-touching deps (e.g. cpal feature flips, ScreenCaptureKit-related crates, calendar/EventKit crates).
- A new permission category needs to be re-requested (e.g. first build that adds calendar access).

Symptoms when you SHOULD HAVE reset and didn't: in-process permission prompts crash silently; the Accessibility list shows multiple stale "Echo Scribe.app" entries; the new binary thinks it has no permissions even though the System Settings toggle is on. Sidecar-specific: `stream_stopped: Failed to find any displays or windows to capture` from `echo-scribe-syscap` is the canonical Screen Recording-not-granted signal.

**Skip-TCC reinstall (default):**

```bash
osascript -e 'tell application "Echo Scribe" to quit' 2>/dev/null
pkill -f "Echo Scribe" 2>/dev/null
sleep 1
rm -rf "/Applications/Echo Scribe.app"
cp -R "src-tauri/target/release/bundle/macos/Echo Scribe.app" /Applications/
open "/Applications/Echo Scribe.app"
```

**Full TCC-reset reinstall (only when permission-related code changed, or user explicitly asks):**

```bash
osascript -e 'tell application "Echo Scribe" to quit' 2>/dev/null
pkill -f "Echo Scribe" 2>/dev/null
sleep 1
tccutil reset Microphone com.echoscribe.app
tccutil reset Accessibility com.echoscribe.app
tccutil reset ScreenCapture com.echoscribe.app
rm -rf "/Applications/Echo Scribe.app"
cp -R "src-tauri/target/release/bundle/macos/Echo Scribe.app" /Applications/
open "/Applications/Echo Scribe.app"
```

Things to know:

- Add `rm -rf "$HOME/Library/Application Support/EchoScribe/models"` if you also want to force a re-download of Parakeet models (e.g. you changed the model URLs). Otherwise leave the user's downloaded models alone.
- Add `rm -rf "$HOME/Library/Application Support/EchoScribe"` if you want a complete clean slate (wipes settings store too — user has to redo the hotkey choice).
- If the user explicitly says "reset TCC" / "do a full reset" → use the full sequence even if no permission code changed.

## Build commands

```bash
# Release .app bundle (what we install to /Applications/)
bun tauri build --bundles app

# Frontend-only dev (no Rust rebuild)
bun run dev

# Rust unit tests
cd src-tauri && cargo test --lib && cd ..

# Onboarding e2e (Playwright + mocked Tauri IPC in e2e/; CI runs this on every PR)
bun run test:e2e

# Install + first-launch smoke test: real install.sh + real bundle launch with
# ECHO_SCRIBE_SMOKE=1 in a throwaway HOME. Release CI runs this per arch and
# blocks publishing on failure. Locally (after a build):
tar -czf EchoScribe-aarch64.tar.gz -C src-tauri/target/release/bundle/macos "Echo Scribe.app"
bash scripts/smoke-test.sh EchoScribe-aarch64.tar.gz

# Installer test: exercises install.sh against a fixture tarball in a throwaway dir
bun run test:installer   # = bash scripts/test-installer.sh
```

Don't run `bun tauri dev` from a subagent — it spawns a window that won't terminate cleanly.

## Release workflow (distribution)

Echo Scribe is distributed unsigned via a curl install script + GitHub Releases. Full design at `docs/superpowers/specs/2026-05-02-distribution-design.md`.

**To cut a release:**
1. Bump `version` in `src-tauri/tauri.conf.json` and `src-tauri/Cargo.toml`
2. Commit the version bump
3. Tag and push:
```bash
git tag v0.x.y
git push origin v0.x.y
```
GitHub Actions (`release.yml`) builds Apple Silicon + Intel `.app` bundles and attaches them to the GitHub Release automatically. `GITHUB_TOKEN` is provided by GitHub Actions — no secrets to configure.

**User install command:**
```bash
curl -fsSL https://raw.githubusercontent.com/desduvauchelle/echo-scribe/main/install.sh | bash
```
The script detects arch, fetches the latest release from the GitHub API, downloads the matching `.tar.gz`, installs to `/Applications/`, and strips the quarantine attribute so Gatekeeper never blocks it.

## Plans + specs

Phase plans live under `docs/superpowers/plans/`. The Phase 0 plan and Phase 1 plan are the source of truth for what we've built so far. Future phases get their own plan files.

The architectural design lives at `docs/superpowers/specs/2026-05-01-tauri-rebuild-design.md`.
