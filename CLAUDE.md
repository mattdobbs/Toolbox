# Toolbox — working notes for Claude

Read this first on any Mac. It is the handoff between the two machines this
app is developed on; memory files do not travel, this file does.
**Update the "Current state" section at the end in every commit** (write it
before committing, so the commit describes itself).

## What this is

Toolbox is a macOS native replacement for Termius + Royal TSX for the owner
(uses Termius, Royal TSX and 1Password daily; prefers Rust).
SSH terminals in tabbed workspaces with split panes, RDP and VNC with
dynamic resolution, connection objects in a Royal-style folder tree, a
credential vault with Touch ID, SSH key identities, and dynamic folders fed
by 1Password (`op` CLI) and Active Directory. Royal TS reference material
the owner provided is in `docs/royalts-reference/`. The owner's Royal layout to
mirror: document "Work" → folder "ACME" → dynamic folders "1Password" and the
AD provider (OU hierarchy), folder "Jump Boxes".

Repo: https://github.com/mattdobbs/Toolbox.git. Work happens on branch
`toolbox-phases-1-3`; `main` only has the initial commit and the branch
fast-forwards onto it. The owner decides when to merge.

## Layout

- `core/toolbox-core/` — Rust core (edition 2021) exposed through UniFFI
  0.32: `ssh.rs` (russh), `sftp.rs` (russh-sftp), `rdp.rs` (IronRDP,
  git-pinned), `vault.rs` (XChaCha20-Poly1305), `onepassword.rs` (`op`
  subprocess), `ad.rs` (ldap3), `store.rs` (connections/credentials/
  providers/folders JSON document), `error.rs`. Test servers behind the
  `test-support` feature: `ssh_test_server.rs`, `rdp_test_server.rs`;
  examples `echo_server` (SSH+SFTP :2222) and `rdp_server` (:3390), both
  demo/secret.
- `App/` — SwiftPM package. AppKit shell: `App/` (delegate, menu),
  `Workspace/` (window = workspace tab, `PaneContainerViewController`,
  `PaneView`, `PaneLayout`), `Terminal/` (SwiftTerm), `Remote/` (RDP via
  IOSurface layer, VNC via RoyalVNCKit), `Files/` (SFTP browser, SwiftUI),
  `Sidebar/` (NSOutlineView tree + SwiftUI editors), `Model/` (AppModel,
  VaultManager, CredentialResolver, ProviderSync). `Sources/Generated/` and
  `Vendor/` are produced by `make core` and gitignored.
- `bridge/` — build scripts: `build-core.sh`, `bundle-app.sh`,
  `signing-identity.sh`, `make-icon.swift`.
- `tools/e2e/` — end-to-end harness (fixture, test servers, launch,
  capture). `docs/SECURITY-REVIEW.md` — review findings and addenda.

## First time on a Mac

1. Xcode installed (the SwiftUI macro plugin is borrowed from it; the
   license need not be accepted for `make app`). Command Line Tools too.
2. `rustup` stable toolchain; `cargo install cargo-audit` optional.
3. `brew install xcodegen` only if you want `make xcode` / the Xcode project
   (that path also needs `sudo xcodebuild -license accept`).
4. `make core` then `make app` → `build/Toolbox.app`. `make test` → Rust
   tests (35 as of 2026-11-13). `make run` launches the bundle.
5. `make signing` once (asks for the login password): creates and trusts a
   self-signed "Toolbox Local Signing" certificate so the app's signature
   stays stable across builds and the Keychain's "Always Allow" for the
   vault key sticks. Without it `make app` signs ad hoc and warns; the
   Keychain then asks after every rebuild. A Developer ID identity, if one
   is in the keychain, is picked up automatically (`TOOLBOX_SIGN_IDENTITY`
   overrides).
6. `DEVELOPER_DIR` must be unset for cargo/swift builds; the scripts do it.

App data: `~/Library/Application Support/Toolbox/{connections.json,
vault.json}`; Keychain generic password service `com.dobbs.Toolbox.vault-key`
("Toolbox vault key"); UserDefaults domain `com.dobbs.Toolbox`
(`vaultRequiresAuth`, `vaultKeyOwnerCertificate`, sidebar expansion).
The owner's real setup: `op` at `/opt/homebrew/bin/op`, account `work`, vault
`work`; AD binds with their regular domain account.

## Everyday commands

```sh
make core          # Rust → bindings + static lib (run after ANY Rust API change)
make app           # SwiftPM build + bundle (relaunch the app afterwards and say so)
make test          # cargo test --features test-support
make icon          # re-render the icon
cd core && cargo clippy --features test-support && cargo audit
```

Relaunch the app after every `make app` and tell the owner which build is
running; they once tested a stale instance and reported a missing feature.

## End-to-end checks (no real hosts needed)

```sh
tools/e2e/make-fixture.sh            # keys, known_hosts, fake op, store → build/e2e
tools/e2e/servers.sh start           # echo SSH+SFTP :2222 and RDP :3390 (add --exit-after 6 for auto-close)
tools/e2e/launch.sh --connect echo --connect echo-1p   # relaunch against the fixture, Touch ID skipped
tools/e2e/capture.sh                 # screenshot every Toolbox window into build/e2e/shots, report dialogs
tools/e2e/servers.sh stop
```

Fixture ids: `echo` (key file, folder Jump Boxes), `echo-1p` (1Password
credential through the fake `op`), `echo-folder` (folder credential →
prompts as `folderuser`), `rdp-local`, `vnc-local`, plus AD-style generated
connections under `ACME/CORP-AD/…`. The fixture uses its own Keychain item
(`TOOLBOX_KEYCHAIN_SERVICE`); delete it afterwards with
`security delete-generic-password -s com.dobbs.Toolbox.vault-key.e2e`.

Launch args: `--connect <id>` / `--files <id>` (repeatable), `--expand-all`,
`--demo-move` (focused pane → new workspace), `--demo-close` (close the
focused pane after 3 s), `--edit-connection <id|new>`,
`--set-folder-credential <path>`, `--unlock-vault`; defaults override
`-vaultRequiresAuth NO`. Env: `TOOLBOX_STORE_PATH`, `TOOLBOX_VAULT_PATH`,
`TOOLBOX_KNOWN_HOSTS`, `TOOLBOX_KEYCHAIN_SERVICE`.

Screenshots need Screen Recording permission for the terminal; GUI
scripting (clicks) is not available without Accessibility permission, so
UI paths are exercised with launch aids or synthetic `NSEvent`s posted from
inside the app. Crash reports land in `~/Library/Logs/DiagnosticReports/`
as `Toolbox-*.ips` (JSON: header line, then body with `threads[].frames`).

## Conventions

- Commit only when the owner asks. Commit messages end with
  `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`; PR bodies with
  `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
- Before committing: scan the diff for keys/tokens, make sure no temporary
  probe code is left, run `make test`, and update **Current state** below.
- Keep `README.md` "What works" and `docs/SECURITY-REVIEW.md` (an addendum
  per security-relevant change) current.
- Secrets never go in `connections.json`; they live in the vault,
  1Password, or are prompted for. New store fields need `#[serde(default)]`.
- Write shell scripts in bash (`#!/usr/bin/env bash`); zsh word-splitting
  bit earlier scripts.

## Invariants and gotchas (each cost real debugging time)

- `PaneSplitView` must not be its own `NSSplitViewDelegate` (AppKit's
  sidebar category recurses in `respondsToSelector:`); a separate
  `ResizeObserver` is the delegate.
- Anything that removes a pane must fix `focusedID` first:
  `rebuild()` synchronously calls `onLayoutChange` → toolbar validation →
  `focusedPane`. `closePane` does this; `focusedPane` repairs stale focus.
- `make core` after any Rust signature change; the Swift bindings are
  generated, not hand-edited.
- All IronRDP crates must be pinned to the same git revision or `picky`
  versions conflict. The RDP test server is not `Send`: it runs on its own
  thread with a current-thread runtime.
- RoyalVNCKit is a dylib dependency (by revision); `bundle-app.sh` copies
  it into `Contents/Frameworks` and adds the rpath.
- The echo test server must skip SFTP channels or it echoes SFTP INIT bytes.
- Session end: `SshListener.on_closed(clean)` /
  `RdpListener.on_closed(reason, clean)`; a pane auto-closes 0.7 s after a
  clean end only. Failures keep the pane with the message.
- Keychain access is keyed on the code signature; ad-hoc signatures change
  per build (see `make signing`). `VaultManager` re-owns the vault-key item
  when the signing certificate changes.
- SwiftUI `Picker` popups are lazy: the `NSPopUpButton` has no item titles
  until opened; `performClick` on it opens the menu for a screenshot.
- The workspace tab bar is private AppKit: `NSTabBar` is the hit-test view
  (tabs are not separate views), `NSTabBarNewTabButton` is the "+".
- `AuthKind` variants: `Password`, `KeyFile`, `Agent`, `Credential`,
  `StoredPassword` (secret in the vault under the connection id),
  `FolderCredential` (nearest folder's assignment; store keeps a path →
  credential map). Credential-based methods take the credential's username.

## Not verified against real infrastructure

A real Windows RDP host (sign-out detection relies on the server's
Terminate), a real VNC server, an AD domain controller, Touch ID on a
certificate-signed build, and distribution to other Macs (needs Developer
ID + notarization; self-signed builds need right-click → Open).

## Ideas not started

RDP clipboard/audio/pointer, per-connection RDP certificate verification,
session restore (workspaces + layouts), broadcast input, jump hosts, port
forwards, a Tasks concept like Royal's, terminal theme picker.

## Current state (update in every commit)

As of 2026-11-13, after commit 63bd487 (pane-close crash fix). This commit
adds this file and `tools/e2e/`.

Done and verified end to end: SSH terminals, SFTP browser, RDP (Display
Control resize) and VNC panes; vault with Keychain key + Touch ID; 1Password
and AD providers; Royal-style sidebar tree with folders, drag and drop and
context menus; draggable pane layouts with persisted ratios; tabbed
workspaces with pane moves and tab rename; automatic pane close on clean
session end; connection auth methods (existing credential, specify
username/password, folder's credential, agent, key file, ask) with the
username under the method; folder credentials with inheritance and key
badge; stable code signing path; closing a focused pane no longer crashes.

Pending: the owner still has to run `make signing` on each Mac (it needs their
password). Nothing else is in flight.
