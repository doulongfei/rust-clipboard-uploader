#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ $(uname -s) != Darwin ]]; then
    echo "macOS packaging requires macOS and Xcode command-line tools" >&2
    exit 1
fi
target=${1:?Usage: package-macos.sh <aarch64-apple-darwin|x86_64-apple-darwin>}
case "$target" in
    aarch64-apple-darwin) arch=arm64 ;;
    x86_64-apple-darwin) arch=x86_64 ;;
    *) echo "Unsupported target: $target" >&2; exit 1 ;;
esac
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
binary="target/$target/release/rust-clipboard-uploader"
[[ -x "$binary" ]] || { echo "Build $target first" >&2; exit 1; }
mkdir -p dist
work=$(mktemp -d "${TMPDIR:-/tmp}/rcu-package.XXXXXX")
smoke_app=
cleanup() {
    if [[ -n "$smoke_app" ]]; then rm -rf "$smoke_app"; fi
    rm -rf "$work"
}
trap cleanup EXIT
app="$work/image/RustClipboardUploader.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/rust-clipboard-uploader"
sed "s/__VERSION__/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"
swift packaging/macos/make-icon.swift "$work/AppIcon.iconset"
iconutil -c icns "$work/AppIcon.iconset" -o "$app/Contents/Resources/AppIcon.icns"
ln -s /Applications "$work/image/Applications"

identity=${MACOS_SIGNING_IDENTITY:--}
sign_flags=(--force --sign "$identity")
if [[ "$identity" != - ]]; then
    sign_flags+=(--options runtime --timestamp)
else
    echo "No Developer ID configured: producing an ad-hoc signed, unnotarized build."
fi
codesign "${sign_flags[@]}" "$app"
codesign --verify --deep --strict --verbose=2 "$app"
if [[ "${MACOS_TEST_LOGIN_ITEM:-}" == 1 ]]; then
    # SMAppService requires an app registered with Launch Services. A bundle
    # inside a temporary disk-image staging directory reports NotFound.
    mkdir -p "$HOME/Applications"
    smoke_app="$HOME/Applications/RustClipboardUploader.app"
    if [[ -e "$smoke_app" ]]; then
        echo "Refusing to replace an existing application: $smoke_app" >&2
        smoke_app=
        exit 1
    fi
    ditto "$app" "$smoke_app"
    /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$smoke_app"
    "$smoke_app/Contents/MacOS/rust-clipboard-uploader" --macos-smoke-test
    rm -rf "$smoke_app"
    smoke_app=
else
    "$app/Contents/MacOS/rust-clipboard-uploader" --macos-smoke-test
fi

notary_args=(--keychain-profile "${MACOS_NOTARY_PROFILE:-}")
if [[ -n "${MACOS_NOTARY_KEYCHAIN:-}" ]]; then
    notary_args+=(--keychain "$MACOS_NOTARY_KEYCHAIN")
fi
# Notarize the app first so its ticket survives copying it out of the disk image.
if [[ -n "${MACOS_NOTARY_PROFILE:-}" ]]; then
    [[ "$identity" != - ]] || { echo "Notarization requires Developer ID" >&2; exit 1; }
    ditto -c -k --keepParent "$app" "$work/application.zip"
    xcrun notarytool submit "$work/application.zip" "${notary_args[@]}" --wait
    xcrun stapler staple "$app"
    xcrun stapler validate "$app"
fi

dmg="dist/RustClipboardUploader-$version-macos-$arch.dmg"
hdiutil create -volname "RustClipboardUploader $version" -srcfolder "$work/image" \
    -ov -format UDZO "$dmg"
if [[ "$identity" != - ]]; then
    codesign --force --sign "$identity" --timestamp "$dmg"
fi
if [[ -n "${MACOS_NOTARY_PROFILE:-}" ]]; then
    xcrun notarytool submit "$dmg" "${notary_args[@]}" --wait
    xcrun stapler staple "$dmg"
    xcrun stapler validate "$dmg"
fi
hdiutil verify "$dmg"
(cd dist && shasum -a 256 "$(basename "$dmg")" > "$(basename "$dmg").sha256")
{
    echo
    echo "- **macOS $arch**: signing=$([[ "$identity" == - ]] && echo Ad-hoc || echo Developer-ID); notarized=$([[ -n "${MACOS_NOTARY_PROFILE:-}" ]] && echo yes || echo no)"
} > "dist/macos-$arch-build-info.txt"
echo "Created $dmg"
