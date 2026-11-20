#!/usr/bin/env bash
# Builds a Toolbox release for colleagues: release build, signed with your
# Developer ID Application certificate and the hardened runtime, notarized
# by Apple, stapled, and packed into build/Toolbox-<version>.dmg (plus a
# .zip). Recipients open it like any downloaded app, with no Gatekeeper
# warning.
#
# One-time setup per Mac (see README, "Sharing builds with the team"):
#   1. A "Developer ID Application: <Name> (<TEAMID>)" certificate with its
#      private key in the login keychain.
#   2. Notarization credentials stored under a keychain profile:
#        xcrun notarytool store-credentials toolbox-notary \
#          --apple-id <you@example.com> --team-id <TEAMID>
#      (it asks for an app-specific password from account.apple.com).
#
#   make release                      # everything
#   NOTARIZE=0 make release           # sign and package only
#   NOTARY_PROFILE=other make release # different keychain profile
#   TOOLBOX_SIGN_IDENTITY=<sha1|name> # pick among several Developer IDs
#   ALLOW_ADHOC=1 NOTARIZE=0          # rehearse the pipeline without a certificate
#   ALLOW_BUILD_PATHS=1               # keep going if the binary names this Mac (never for a published build)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/build/Toolbox.app"
NOTARY_PROFILE="${NOTARY_PROFILE:-toolbox-notary}"
NOTARIZE="${NOTARIZE:-1}"
unset DEVELOPER_DIR

step() { printf '\n==> %s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- identity ---------------------------------------------------------------
if [[ -z "${TOOLBOX_SIGN_IDENTITY:-}" ]]; then
  line="$(security find-identity -v -p codesigning | grep -F "Developer ID Application:" | head -1 || true)"
  if [[ -n "$line" ]]; then
    TOOLBOX_SIGN_IDENTITY="$(awk '{print $2}' <<<"$line")"
    IDENTITY_NAME="$(sed -E 's/^[^"]*"(.*)"[^"]*$/\1/' <<<"$line")"
  elif [[ "${ALLOW_ADHOC:-0}" == "1" ]]; then
    TOOLBOX_SIGN_IDENTITY="-"
    IDENTITY_NAME="ad-hoc (rehearsal only, not shareable)"
    NOTARIZE=0
  else
    fail "no \"Developer ID Application\" certificate in the keychain.
       Create one in Xcode ▸ Settings ▸ Accounts ▸ Manage Certificates ▸ + ▸ Developer ID Application,
       or import the .p12 from the Mac that has it. Then run make release again."
  fi
else
  IDENTITY_NAME="$TOOLBOX_SIGN_IDENTITY"
fi
export TOOLBOX_SIGN_IDENTITY TOOLBOX_HARDENED=1
echo "signing identity: $IDENTITY_NAME"

if [[ "$NOTARIZE" == "1" ]]; then
  xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1 \
    || fail "notarization profile \"$NOTARY_PROFILE\" is missing or invalid. Create it with:
       xcrun notarytool store-credentials $NOTARY_PROFILE --apple-id <you@example.com> --team-id <TEAMID>"
fi

# --- build ------------------------------------------------------------------
step "Building the core (release)"
"$ROOT/bridge/build-core.sh" | grep -E "^ok:" || true
step "Building and signing the app (release, hardened runtime)"
CONFIG=release TOOLBOX_SIGN_IDENTITY="$TOOLBOX_SIGN_IDENTITY" "$ROOT/bridge/bundle-app.sh" | grep -vE "^\[|Compiling|search path" || true

VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")"
BUILD="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$APP/Contents/Info.plist")"
NAME="Toolbox-$VERSION"
OUT="$ROOT/build/release"
rm -rf "$OUT"; mkdir -p "$OUT"

# The download is public. Nothing in it may name the Mac it was built on:
# both build scripts rewrite source paths for release builds, and this
# makes sure nothing slipped through (a new dependency, a new tool).
step "Checking for paths from this Mac"
LEAKS=0
while IFS= read -r -d '' item; do
  # Capture first, as below: `grep -q` in a pipeline trips pipefail.
  found="$(strings -a "$item" | grep -c -F -e "$HOME" -e "/Users/$(id -un)" || true)"
  if [[ "$found" != "0" ]]; then
    echo "  ${item#"$APP"/}: $found strings contain a path under $HOME"
    LEAKS=1
  fi
done < <(find "$APP/Contents/MacOS" "$APP/Contents/Frameworks" -type f -print0)
if [[ "$LEAKS" == "1" && "${ALLOW_BUILD_PATHS:-0}" != "1" ]]; then
  fail "the app embeds paths from this Mac (see above); fix the path remapping in bridge/build-core.sh and
       bridge/bundle-app.sh, or set ALLOW_BUILD_PATHS=1 for a build that will not be published"
fi
[[ "$LEAKS" == "1" ]] || echo "none"

step "Verifying the signature"
codesign --verify --deep --strict --verbose=2 "$APP"
SIGNATURE="$(codesign -dvv "$APP" 2>&1)"
grep -E "^(Authority|TeamIdentifier|CodeDirectory)" <<<"$SIGNATURE" || true
[[ "$SIGNATURE" == *"runtime"* ]] || fail "the app is not signed with the hardened runtime"
if [[ "$TOOLBOX_SIGN_IDENTITY" != "-" ]]; then
  # Apple refuses to notarize signatures without a secure timestamp; catch
  # that here instead of after a round trip to the notary service.
  while IFS= read -r -d '' item; do
    # Capture first: piping into `grep -q` trips pipefail when grep exits early.
    details="$(codesign -dvv "$item" 2>&1 || true)"
    [[ "$details" == *$'\n'"Timestamp="* ]] \
      || fail "no secure timestamp in the signature of ${item#"$APP"/}"
  done < <(find "$APP/Contents/MacOS" "$APP/Contents/Frameworks" -type f -print0)
  echo "secure timestamps: present"
fi

notarize() { # $1 = file to submit
  step "Notarizing $(basename "$1") (usually a few minutes)"
  local result id status
  result="$(xcrun notarytool submit "$1" --keychain-profile "$NOTARY_PROFILE" --wait --output-format plist)"
  id="$(/usr/libexec/PlistBuddy -c 'Print :id' /dev/stdin <<<"$result" 2>/dev/null || true)"
  status="$(/usr/libexec/PlistBuddy -c 'Print :status' /dev/stdin <<<"$result" 2>/dev/null || true)"
  echo "submission $id: $status"
  if [[ "$status" != "Accepted" ]]; then
    [[ -n "$id" ]] && xcrun notarytool log "$id" --keychain-profile "$NOTARY_PROFILE" "$OUT/notary-$id.json" >/dev/null 2>&1 \
      && echo "Apple's log: $OUT/notary-$id.json"
    fail "notarization was not accepted"
  fi
}

# --- notarize the app, staple it --------------------------------------------
if [[ "$NOTARIZE" == "1" ]]; then
  ditto -c -k --keepParent "$APP" "$OUT/submit.zip"
  notarize "$OUT/submit.zip"
  rm -f "$OUT/submit.zip"
  step "Stapling the ticket to the app"
  xcrun stapler staple "$APP"
fi

# --- package ----------------------------------------------------------------
step "Packaging"
ditto -c -k --keepParent "$APP" "$OUT/$NAME.zip"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -quiet -volname "Toolbox $VERSION" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$OUT/$NAME.dmg"
if [[ "$TOOLBOX_SIGN_IDENTITY" != "-" ]]; then
  codesign --force --timestamp --sign "$TOOLBOX_SIGN_IDENTITY" "$OUT/$NAME.dmg"
fi
if [[ "$NOTARIZE" == "1" ]]; then
  notarize "$OUT/$NAME.dmg"
  step "Stapling the ticket to the disk image"
  xcrun stapler staple "$OUT/$NAME.dmg"
  step "Gatekeeper assessment"
  spctl --assess --type execute --verbose=2 "$APP"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$OUT/$NAME.dmg"
fi

shasum -a 256 "$OUT/$NAME.dmg" "$OUT/$NAME.zip" | sed "s|$OUT/||" > "$OUT/SHA256SUMS"
step "Done: Toolbox $VERSION (build $BUILD)"
ls -lh "$OUT" | awk 'NR>1 {print "  " $5 "  " $9}'
[[ "$NOTARIZE" == "1" ]] || echo "  (not notarized: recipients would see a Gatekeeper warning)"
