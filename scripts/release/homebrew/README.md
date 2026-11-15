# homebrew-tap

Homebrew tap for [Icarus](https://github.com/frbarbre/icarus).

```sh
brew install --cask frbarbre/tap/icarus
```

This installs `Icarus.app` into `/Applications` and puts `icarus` and `icarus-server` on your `PATH`.
Then start the server at login with `icarus server install`.

Icarus is not signed with an Apple Developer ID, so macOS asks once before the first launch:
open System Settings → Privacy & Security and choose **Open Anyway** for Icarus.

Upgrade with `brew upgrade --cask icarus`, or `icarus update install` (which runs that for you and
restarts the server; agents sleep and resume).

The cask is Apple silicon only for now. On Intel Macs, use the `universal-apple-darwin` CLI and
server tarball from the [releases](https://github.com/frbarbre/icarus/releases).

Everything here is written by the release workflow in frbarbre/icarus
(`scripts/release/homebrew/`); change it there, not here.
