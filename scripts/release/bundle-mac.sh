#!/usr/bin/env bash
# Builds Icarus.app from the Zed fork in app/ and packs it as a .zip (for updates and Homebrew)
# and a .dmg (for people). Unsigned: ad-hoc codesigned, so macOS shows the usual
# "unidentified developer" prompt once, as Orca does.
#
#   scripts/release/bundle-mac.sh <target-triple> <version> <dir with icarus + icarus-server> <out dir>
#
# Mirrors app/script/bundle-mac without Zed's signing, notarization, Sentry, licence and git steps.
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: $0 <aarch64-apple-darwin|x86_64-apple-darwin> <version> <bin dir> <out dir>" >&2
  exit 2
fi
target=$1
version=$2
bin_dir=$(cd "$3" && pwd)
mkdir -p "$4"
out_dir=$(cd "$4" && pwd)
root=$(cd "$(dirname "$0")/../.." && pwd)

case "$target" in
  aarch64-apple-darwin) arch=aarch64 ;;
  x86_64-apple-darwin) arch=x86_64 ;;
  *)
    echo "unsupported target $target" >&2
    exit 2
    ;;
esac

for bin in icarus icarus-server; do
  if [[ ! -x "$bin_dir/$bin" ]]; then
    echo "missing $bin_dir/$bin" >&2
    exit 1
  fi
done

sdk_version=$(xcrun --sdk macosx --show-sdk-version)
if ((10#${sdk_version%%.*} < 26)); then
  echo "the Zed fork needs the macOS 26 SDK or newer (found $sdk_version); select a newer Xcode" >&2
  exit 1
fi

cd "$root/app"
export MACOSX_DEPLOYMENT_TARGET=12.0
export CXXFLAGS="-stdlib=libc++"
export ZED_BUNDLE=true
export CARGO_BUNDLE_SKIP_BUILD=true
# Turns Zed's own updater off at compile time; "Check for Updates" shows this instead.
export ZED_UPDATE_EXPLANATION="Icarus updates itself: run \`icarus update install\` (or \`brew upgrade --cask icarus\` if Homebrew installed it)."

rustup toolchain install
rustup target add "$target"

if [[ "$(cargo -q bundle --help 2>&1 | head -n 1 || true)" != "cargo-bundle v0.6.1-zed" ]]; then
  cargo install cargo-bundle --git https://github.com/zed-industries/cargo-bundle.git --branch zed-deploy
fi

cargo_config=(--config .cargo/bundle-config.toml --config profile.release.debug=false)
cargo "${cargo_config[@]}" rustc --release --package zed --bin zed --target "$target" -- -C 'link-arg=-Wl,-unexported_symbol,__R*'
cargo "${cargo_config[@]}" build --release --package cli --target "$target"

manifest=crates/zed/Cargo.toml
cp "$manifest" "$manifest.icarus-backup"
trap 'mv "$root/app/$manifest.icarus-backup" "$root/app/$manifest"' EXIT
cat >>"$manifest" <<'EOF'

[package.metadata.bundle]
icon = ["resources/app-icon-dev@2x.png", "resources/app-icon-dev.png"]
identifier = "dev.icarus.app"
name = "Icarus"
category = "Developer Tool"
osx_minimum_system_version = "12.0"
osx_info_plist_exts = ["resources/info/*"]
osx_url_schemes = ["icarus"]
EOF
app_path=$(cd crates/zed && cargo bundle --release --target "$target" --select-workspace-root | xargs)
mv "$manifest.icarus-backup" "$manifest"
trap - EXIT
echo "Bundled $app_path"

release_dir="target/$target/release"
strip -x "$release_dir/zed" "$release_dir/cli"
cp "$release_dir/zed" "$app_path/Contents/MacOS/zed"
cp "$release_dir/cli" "$app_path/Contents/MacOS/cli"
cp "$bin_dir/icarus" "$bin_dir/icarus-server" "$app_path/Contents/MacOS/"
mkdir -p "$app_path/Contents/Resources"
cp crates/zed/resources/Document.icns "$app_path/Contents/Resources/Document.icns"

plist="$app_path/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$plist"

entitlements=$(mktemp)
# Associated domains need a real team identity; ad-hoc signing rejects them.
sed '/com.apple.developer.associated-domains/,+1d' crates/zed/resources/zed.entitlements >"$entitlements"
codesign --force --deep --entitlements "$entitlements" --sign - "$app_path"
rm -f "$entitlements"
codesign --verify --verbose "$app_path" || echo "warning: codesign --verify failed on the ad-hoc signature" >&2

name="Icarus-$version-macos-$arch"
rm -f "$out_dir/$name.zip" "$out_dir/$name.dmg"
ditto -c -k --sequesterRsrc --keepParent "$app_path" "$out_dir/$name.zip"

dmg_src=$(mktemp -d)
cp -R "$app_path" "$dmg_src/"
ln -s /Applications "$dmg_src/Applications"
hdiutil create -volname Icarus -srcfolder "$dmg_src" -ov -format UDZO "$out_dir/$name.dmg"
rm -rf "$dmg_src"

ls -la "$out_dir"
