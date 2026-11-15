cask "icarus" do
  # Written by the release workflow in frbarbre/icarus on every stable release; edit it there.
  version "@VERSION@"
  sha256 "@SHA256@"

  url "https://github.com/frbarbre/icarus/releases/download/v#{version}/Icarus-#{version}-macos-aarch64.zip"
  name "Icarus"
  desc "Native workspace for running coding agents in parallel git worktrees"
  homepage "https://github.com/frbarbre/icarus"

  depends_on arch: :arm64
  depends_on macos: :monterey

  app "Icarus.app"
  binary "#{appdir}/Icarus.app/Contents/MacOS/icarus"
  binary "#{appdir}/Icarus.app/Contents/MacOS/icarus-server"

  zap trash: [
    "~/.config/icarus",
    "~/.icarus",
    "~/Library/LaunchAgents/dev.icarus.server.plist",
    "~/Library/Saved Application State/dev.icarus.app.savedState",
  ]
end
