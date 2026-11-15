# Icarus

A fast, native workspace for running coding agents in parallel git worktrees. A Rust server owns every agent, terminal, automation and repo, locally or on a remote machine. A GPUI client (a soft fork of Zed) connects to one or more servers. Everything is configurable through layered config files and a complete CLI, so agents can run and tune it as well as people.

Icarus can import an [Orca](https://github.com/stablyai/orca) settings export and reproduce its workspace automations.

## Install

On an Apple silicon Mac:

```sh
brew install --cask frbarbre/tap/icarus
icarus server install   # start the server at login
```

The app isn't signed with an Apple Developer ID, so macOS asks once before the first launch (System Settings → Privacy & Security → Open Anyway). Other platforms: the `icarus` and `icarus-server` tarballs on the [releases page](https://github.com/frbarbre/icarus/releases).

Update with `icarus update check` and `icarus update install`. Agents sleep through the server restart and resume.

- [Plan](plan.md): goals, architecture, milestones and open decisions.
- [Feature inventory](features.md): what the Orca fork does, with priorities.
- [Settings compatibility](settings-compat.md): the Orca export format and how it maps onto Icarus.
- [GitHub panel](github-panel.md), [GitHub review](github-review.md), [Source control](source-control.md), [Panes and tabs](panes-and-tabs.md), [UI reference](ui-reference.md): detailed specs.
- [Open-source research](oss-research.md): the building blocks we reuse.
