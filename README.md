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

## What works

- Workspaces as native window tabs, split panes, SSH terminals (SwiftTerm on
  Metal) with themes and text-size shortcuts.
- Credentials: username plus password or SSH key, kept encrypted in a local
  vault (key in the login Keychain, unlocked with Touch ID), fetched from
  1Password at connect time, or prompted for.
- Providers (dynamic folders): a 1Password provider turns vault items into
  credentials via the `op` CLI; an Active Directory provider turns computer
  objects into connections (Windows → RDP, macOS → VNC, Linux → SSH, OUs as
  folders, Domain Controllers get the admin credential). Providers refresh at
  launch and on a timer, or on demand with ⌘R.

- RDP through IronRDP: NLA logon, the desktop follows the pane size through
  Display Control (falling back to a reconnect at the new size), frames land
  in an IOSurface-backed layer, full keyboard and mouse.
- VNC through RoyalVNCKit, the engine Royal TS itself ships.

## Trying it without real hosts

```sh
cd core
cargo run --example echo_server -- --key <openssh-private-key>   # SSH on 127.0.0.1:2222
cargo run --example rdp_server --features test-support           # RDP on 127.0.0.1:3390, demo / secret
```

Launch overrides for a throwaway setup: `TOOLBOX_STORE_PATH`,
`TOOLBOX_VAULT_PATH`, `TOOLBOX_KNOWN_HOSTS`, and `--connect <id>` to open a
connection on start.

## Next

SFTP, clipboard and audio for RDP, a client-side RDP pointer, session restore,
broadcast input, jump hosts and port forwards.

`docs/royalts-reference/` holds the Royal TSX scripts the providers replace.
