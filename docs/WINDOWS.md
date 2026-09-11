# Windows support

Echo Scribe does not currently have a fully supported Windows release.
A Windows port and development installer build are in progress.

The current app is macOS-first and depends on several macOS-only pieces:

- Swift sidecars for system audio capture, calendar matching, and screen recording.
- macOS privacy permission flows for microphone, accessibility, calendar, and screen recording access.
- macOS-specific focus/window metadata used for capture context.
- CoreAudio and Apple-native keychain dependencies.
- Metal-backed local LLM inference in the Rust dependency configuration.
- Release assets and install scripts that only publish and install `.app` bundles.

That means there is not a fully supported Windows release asset yet. A Windows
installer build is available through GitHub Actions once the Windows workflow is
green.

## Installing a GitHub Actions build

1. Open the repository on GitHub.
2. Go to **Actions**.
3. Select the **Windows** workflow.
4. Run the workflow manually with **Run workflow**, or open the latest successful
   run from `main`.
5. Download the `echo-scribe-windows` artifact.
6. Unzip the artifact and run the `*-setup.exe` installer.

Windows may show a SmartScreen warning because this development build is not
code signed yet.

## Current porting approach

The app can be ported because Tauri supports Windows, but this repository needs
platform work before Windows is a supported release target. The first milestone
is a limited Windows build that launches, stores local data, and clearly disables
macOS-only features. Voice capture, meeting capture, screen recording, and
calendar matching can then be added back with Windows-native implementations.

Minimum porting checklist:

1. Move macOS-only dependencies behind `cfg(target_os = "macos")`.
2. Add Windows equivalents or feature fallbacks for focus context, microphone
   permission UX, meeting audio capture, screen recording, calendar lookup, and
   credential storage. (Global hotkeys are done — see "Hotkeys on Windows" below.)
3. Replace the unconditional Metal LLM build with a Windows-compatible llama.cpp
   backend, likely CPU first, then optional CUDA/Vulkan later.
4. Add Windows sidecar binaries or disable the features that require the current
   Swift sidecars.
5. Build and test on a real Windows machine or a Windows CI runner.
6. Publish a Windows installer from Tauri, typically an NSIS `*-setup.exe` or an
   MSI package.

## Hotkeys on Windows

Windows has no CGEventTap, so the configured bindings are registered with the
global-shortcut plugin (`RegisterHotKey`) instead of the macOS event tap. Two
consequences the UI has to live with:

- **A shortcut must contain a regular key.** `RegisterHotKey` cannot register a
  bare modifier, so the macOS default (Right Control on its own) is not
  expressible. Settings reports this rather than silently doing nothing.
- **Left/right modifiers are not distinguished.** "Right Alt + Slash" registers
  as "Alt + Slash".
- **Registration is first-come-first-served across the whole system.** If another
  running application already owns the combination, registration fails and Echo
  Scribe surfaces the problem in Settings next to that shortcut.

Because a second instance of the app would lose that race against the first and
end up with no working hotkey at all, the app registers
`tauri-plugin-single-instance` and focuses the existing window instead of
starting a second process.

## Validation commands for a Windows developer

On a Windows development machine:

```powershell
winget install --id Rustlang.Rustup
winget install --id Oven-sh.Bun
```

Install Microsoft Visual Studio Build Tools with the "Desktop development with
C++" workload. WebView2 is normally already present on Windows 10 version 1803
and newer, and on Windows 11.

Then from the repository:

```powershell
bun install
bun run build
cargo test --manifest-path src-tauri/Cargo.toml --lib
bun tauri build
```

For the current codebase, these commands may still fail until the Windows CI
workflow is green. Once they pass on Windows, the generated installer will be
under `src-tauri\target\release\bundle\`.

## Current limitations

These features are still macOS-only:

- Global hotkey voice capture.
- Meeting auto-detection.
- System audio capture.
- Calendar matching.
- Screen recording.
- In-app self-update.
