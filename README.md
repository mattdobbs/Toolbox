# Toolbox

A native macOS client for the servers you touch every day: SSH sessions in
split panes and workspace tabs (the part Termius gets right), plus RDP and VNC
with a credential vault, identities and 1Password sync (the part Royal TSX gets
right). Swift for the app, Rust for the connection engine.

## Layout

```
core/     Rust workspace. toolbox-core holds SSH (russh), the connection store,
          and later RDP (IronRDP), the vault and the 1Password / AD providers.
bridge/   UniFFI build: compiles the core, generates Swift bindings, and packs
          the static library into App/Frameworks/ToolboxCoreFFI.xcframework.
App/      Xcode project (generated from project.yml by XcodeGen). AppKit shell,
          SwiftUI for forms, SwiftTerm for terminals.
```

## Prerequisites

- Xcode 16 or newer
- Rust via rustup (`curl https://sh.rustup.rs -sSf | sh`)
- XcodeGen (`brew install xcodegen`)

## Build

```sh
make core      # Rust core → Swift bindings + static library (release)
make app       # SwiftPM build wrapped into build/Toolbox.app (no Xcode license needed)
make project   # App/project.yml → App/Toolbox.xcodeproj (XcodeGen)
make xcode     # build with xcodebuild (after sudo xcodebuild -license accept)
make test      # Rust unit + integration tests (spins up an in-process SSH server)
```

Open `App/Toolbox.xcodeproj` in Xcode for day-to-day work on the Swift side.
Re-run `make core` whenever the Rust API changes.

### Code signing and the Keychain prompt

macOS remembers a Keychain "Always Allow" per code signature. Without a
signing identity `make app` signs ad hoc, and that signature changes with
every build, so the "Toolbox wants to use your confidential information"
dialog for the vault key comes back after each rebuild. Fix it once per Mac:

```sh
make signing   # creates and trusts a "Toolbox Local Signing" certificate (asks for your password)
make app       # now signed with it; the next launch asks once, then stays quiet
```

`make app` prefers, in order: `TOOLBOX_SIGN_IDENTITY`, a Developer ID
Application identity, an Apple Development identity, Toolbox Local Signing,
then ad hoc (with a warning). To hand the app to coworkers, sign it with a
Developer ID and notarize it; a self-signed build makes them right-click →
Open the first time, and they answer the Keychain dialog once.

The remaining prompt, Touch ID (or the login password) before the vault
unlocks, is deliberate and can be turned off with Toolbox ▸ Require Touch ID
to Unlock Vault.

## What works

- Workspaces as native window tabs, split panes, SSH terminals (SwiftTerm on
  Metal) with themes and text-size shortcuts. Arrange panes by dragging:
  grab a pane's header and drop it on another pane's edge to split there or
  on its middle to swap; drag a connection from the sidebar onto a pane the
  same way to open it in place. Divider positions are kept when panes come
  and go. Sessions follow their pane: terminals reflow and RDP desktops are
  renegotiated at the new pixel size. A session that ends cleanly (RDP
  sign-out, shell logout) closes its pane by itself; one that fails stays so
  the message can be read.
- Workspaces are tabs (the bar stays visible; "+" or ⌘T adds one) and any mix
  of sessions can live in any of them. Move a pane to another workspace from
  its header's context menu or the Pane menu (⌘⇧M for a new workspace); the
  session keeps running through the move. Double-click a tab (or press ⌘⇧R)
  to rename the workspace.
- A sidebar tree like Royal TS: folders you create (nest them with `/`),
  providers as folders in place with what they generate underneath (an
  Active Directory provider shows its OU hierarchy), drag and drop between
  folders, context menus on every node, and flat "All Connections" and "All
  Credentials" lists at the bottom. Expansion is remembered.
- Credentials: username plus password or SSH key, kept encrypted in a local
  vault (key in the login Keychain, unlocked with Touch ID), fetched from
  1Password at connect time, or prompted for. A connection can pick an
  existing credential (the picker groups vault, 1Password and ask-each-time
  entries), carry its own username and password (stored in the vault just
  for that connection; the username lives under the method, not on the
  connection), or use its folder's credential: right-click a folder
  and choose Set Credential… and every connection underneath set to "Use the
  folder's credential" uses it, nearest folder first. Folders that hand one down show
  a key badge.
- Providers (dynamic folders): a 1Password provider turns vault items into
  credentials via the `op` CLI; an Active Directory provider turns computer
  objects into connections (Windows → RDP, macOS → VNC, Linux → SSH, OUs as
  folders, Domain Controllers get the admin credential). Providers refresh at
  launch and on a timer, or on demand with ⌘R.

- RDP through IronRDP: NLA logon, the desktop follows the pane size through
  Display Control (falling back to a reconnect at the new size), frames land
  in an IOSurface-backed layer, full keyboard and mouse.
- VNC through RoyalVNCKit, the engine Royal TS itself ships.
- SFTP file browsing for any SSH connection (sidebar context menu, ⌘⇧F, or
  the toolbar): navigate, upload, download with progress, new folder, rename
  and delete.

## Trying it without real hosts

`tools/e2e/` packages the throwaway setup used for end-to-end checks:
`make-fixture.sh` (keys, known_hosts, fake `op`, store), `servers.sh start`
(echo SSH+SFTP on 2222, RDP on 3390), `launch.sh --connect echo` and
`capture.sh` for screenshots. `CLAUDE.md` holds the working notes, machine
setup and current state for whoever (or whatever) picks the project up.

```sh
cd core
cargo run --example echo_server --features test-support -- --key <openssh-private-key>   # SSH + SFTP on 127.0.0.1:2222, demo / secret
                                                       # add --exit-after <secs> to have shells log out on their own
cargo run --example rdp_server --features test-support                                   # RDP on 127.0.0.1:3390, demo / secret
```

Launch overrides for a throwaway setup: `TOOLBOX_STORE_PATH`,
`TOOLBOX_VAULT_PATH`, `TOOLBOX_KNOWN_HOSTS`, `--connect <id>` to open a
connection on start, `--files <id>` to open its SFTP browser (both may repeat, each
opening another pane), `--expand-all` to expand the whole sidebar tree, `--demo-move` to move the
focused pane into a new workspace after launch, `--demo-close` to close the focused pane after launch,
`--edit-connection <id|new>` and
`--set-folder-credential <path>` to open those sheets at launch, `--unlock-vault` to unlock the vault at launch and
log the outcome, `TOOLBOX_KEYCHAIN_SERVICE` to use a separate Keychain item for the vault key, and
the standard defaults override `-vaultRequiresAuth NO` to skip the Touch ID step for that run.

`make icon` re-renders the app icon from `bridge/make-icon.swift`.
`docs/SECURITY-REVIEW.md` records the review findings and residual risks.

## Next

Clipboard and audio for RDP, a client-side RDP pointer, per-connection RDP
certificate verification, session restore, broadcast input, jump hosts and
port forwards.

`docs/royalts-reference/` holds the Royal TSX scripts the providers replace.
