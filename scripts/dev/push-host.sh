#!/usr/bin/env bash
# Puts this checkout's Icarus on another Mac over SSH for quick testing, without a release.
#
#   scripts/dev/push-host.sh <ssh-host> [--app]
#
# The server and CLI go to ~/.icarus-dev/bin on the host, and the login service is pointed at
# them. With --app, the app's binaries are built with the release-fast profile and swapped into
# ~/.icarus-dev/Icarus.app, a copy of the host's installed app, which is then relaunched.
# The installed app stays untouched; `icarus server install` from it switches back.
set -euo pipefail
if [[ $# -lt 1 ]]; then
  echo "usage: $0 <ssh-host> [--app]" >&2
  exit 2
fi
host=$1
with_app=${2:-}
root=$(cd "$(dirname "$0")/../.." && pwd)
target=aarch64-apple-darwin

cd "$root"
cargo build --release --target "$target" -p icarus-server -p icarus-cli --bin icarus --bin icarus-server
ssh "$host" 'mkdir -p ~/.icarus-dev/incoming ~/.icarus-dev/bin'
scp -q "target/$target/release/icarus" "target/$target/release/icarus-server" "$host:.icarus-dev/incoming/"
# Overwriting a signed executable in place gets it killed for code signing; swap in new files.
ssh "$host" 'cd ~/.icarus-dev && mv -f incoming/icarus incoming/icarus-server bin/'

if [[ $with_app == --app ]]; then
  cd "$root/app"
  # The same compile-time switches as scripts/release/bundle-mac.sh.
  export ZED_RELEASE_CHANNEL=stable ZED_BUNDLE=true MACOSX_DEPLOYMENT_TARGET=12.0 CXXFLAGS="-stdlib=libc++"
  export ZED_UPDATE_EXPLANATION="This is a development build; push a new one with scripts/dev/push-host.sh."
  export ICARUS_VERSION
  ICARUS_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n 1)-dev"
  cargo build --profile release-fast --config profile.release-fast.debug=false \
    --target "$target" -p zed --bin zed -p cli --bin cli
  scp -q "target/$target/release-fast/zed" "target/$target/release-fast/cli" "$host:.icarus-dev/incoming/"
  ssh "$host" 'set -e
    osascript -e "quit app \"Icarus\"" >/dev/null 2>&1 || true
    rm -rf ~/.icarus-dev/Icarus.app
    cp -cR /Applications/Icarus.app ~/.icarus-dev/Icarus.app
    mv -f ~/.icarus-dev/incoming/zed ~/.icarus-dev/incoming/cli ~/.icarus-dev/Icarus.app/Contents/MacOS/
    cp ~/.icarus-dev/bin/icarus ~/.icarus-dev/bin/icarus-server ~/.icarus-dev/Icarus.app/Contents/MacOS/
    codesign --force --deep --sign - ~/.icarus-dev/Icarus.app >/dev/null 2>&1'
fi

# Restart keeps terminals running; the first push also points the login service at the dev build.
# A fresh bootstrap right after a boot-out can fail while launchd still holds the old job.
ssh "$host" 'if grep -q .icarus-dev ~/Library/LaunchAgents/dev.icarus.server.plist 2>/dev/null; then
    ~/.icarus-dev/bin/icarus server restart
  else
    for attempt in 1 2 3; do ~/.icarus-dev/bin/icarus server install >/dev/null 2>&1 && break; sleep 2; done
  fi
  for attempt in 1 2 3 4 5 6 7 8 9 10; do ~/.icarus-dev/bin/icarus server status 2>/dev/null && break; sleep 1; done'
if [[ $with_app == --app ]]; then
  ssh "$host" 'open ~/.icarus-dev/Icarus.app'
fi
