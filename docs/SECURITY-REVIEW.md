# Security and bug review — 2026-11-13

Scope: the whole repository as of the phase 4 build (SSH, SFTP, RDP, VNC,
vault, credentials, 1Password and Active Directory providers). Method: a
read-through of every Rust and Swift source file, `cargo clippy` on all
targets, `cargo audit` on the dependency tree, and the end-to-end runs
against the in-process SSH and RDP servers.

## Fixed in this pass

| Area | Finding | Fix |
|---|---|---|
| Core | A panic while holding a mutex would poison it and turn every later lock into a crash | `lock_or_recover` recovers the guard everywhere the core locks |
| Core | Records holding passwords derived `Debug`, so a stray log line could print them | Redacting `Debug` impls for `SshAuth`, `RdpConfig`, `BindCredentials`, `Secret`, `ResolvedCredential` |
| Core | `connections.json` and `vault.json` were written with default permissions | Written as mode 0600 |
| SFTP | A hostile server could answer a listing with names like `../x` and steer a download outside the chosen folder | Core drops names containing `/`, NUL, `.` or `..`; the app only ever writes the last path component |
| SSH | No timeout on the authentication exchange, so a server that accepts TCP but never answers would hang the pane | 60 s auth timeout |
| Vault | If the owner check (Touch ID or password) was unavailable the vault unlocked anyway | Unlock now refuses with a reason |
| App | Closing a pane while its credentials were still being resolved left a connected "zombie" session with no UI | Terminal, RDP, VNC and SFTP panes ignore a late start after close |
| App | An unreadable store could crash the app at launch | Falls back to a temporary store with an alert |
| App | Passwords lingered in closed RDP/VNC views | Cleared on terminate |

## Reviewed and acceptable

- **Host keys.** Unknown keys prompt with the SHA-256 fingerprint and are
  recorded in `~/.ssh/known_hosts`; a changed key refuses to connect.
- **Secrets at rest.** Vault entries use XChaCha20-Poly1305 with a random
  nonce and the entry id as associated data; the key is random, lives in the
  login Keychain as this-device-only, and is held in memory behind
  `Zeroizing`. 1Password items are fetched at use time and never written.
- **Subprocess use.** The `op` CLI is run with an argument vector, never a
  shell, with user-configured vault and account names as separate arguments.
- **LDAP.** LDAPS first; the plain-LDAP fallback is a per-provider setting
  that defaults on to match the previous Royal TS script, and a sync that
  used it reports a warning. Certificate validation defaults on.
- **Remote content.** Terminal OSC 52 clipboard reads are denied; link
  opening is limited to http, https and mailto.
- **Logging.** No code path logs credentials; error messages carry host and
  user names only.

## Residual risks and known gaps

- **RDP server certificates are accepted without validation.** This matches
  the default of most RDP clients for self-signed hosts, and NLA (CredSSP)
  still binds the server's TLS key into the credential exchange, but a
  per-connection "verify certificate" setting is the right next step.
- **`rsa` crate advisory RUSTSEC-2023-0071 (medium).** Timing side channel in
  RSA private-key operations. It is reached only when authenticating with an
  RSA SSH key, the version is pinned by russh, and the fix ships with the
  crate's final release. Ed25519 keys are unaffected.
- **Unmaintained transitive crates** flagged by `cargo audit`:
  `atomic-polyfill`, `basic-toml`, `pin-utils`, `rustls-pemfile`. No known
  vulnerabilities; worth re-checking when upstreams move.
- **Secrets in Swift strings** (prompts, resolved passwords) cannot be
  zeroized; they are released as soon as the session starts.
- **VNC clipboard redirection is on by default**, as in Royal TS. Turn it off
  in `VncSessionView` if sharing the clipboard with a remote is unwanted.
- **No hardened runtime or sandbox yet.** The SwiftPM bundle is ad-hoc
  signed; enable the hardened runtime and a signing identity in the Xcode
  build before distributing.
- **Launch overrides** (`TOOLBOX_STORE_PATH`, `TOOLBOX_VAULT_PATH`,
  `TOOLBOX_KNOWN_HOSTS`, `--connect`, `--files`) exist for testing. They only
  affect the user's own process.

## Not covered

Live testing against a Windows RDP host, a real VNC server, an Active
Directory domain controller, and the Keychain/Touch ID unlock, none of which
are reachable from the development machine.

## Addendum — 2026-11-13, later the same day

Reviewed with the sidebar tree, pane arranging, workspace moves and
automatic pane close added.

- **Drag and drop** uses app-private pasteboard types carrying only node or
  pane ids; drops from other applications are ignored. Folder moves refuse a
  folder dropped into itself, and provider-generated items cannot be moved.
- **Folder paths** are normalized in the core (empty and whitespace-only
  components dropped), so the slash-separated paths in the store cannot
  contain empty segments; deleting a folder requires it to be empty.
- **Moving a pane between workspaces** re-parents the live view; the
  session's credentials are not touched or re-resolved, and the source pane
  is closed without terminating the session.
- **Automatic close** fires only on an end the core marks clean (remote
  logout or sign-out, or a close started here). Failures keep the pane with
  the error text, so a dropped connection cannot silently vanish.
- A crash was fixed where a split view acting as its own delegate recursed
  in AppKit; the delegate is now a separate object.

Verification: 33 core tests, clippy clean, `cargo audit` unchanged from the
morning (one advisory on the pinned `rsa` release candidate, four
unmaintained transitive crates), end-to-end runs for the three-pane layout,
the workspace move and the automatic close.
