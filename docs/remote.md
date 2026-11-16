# Remote servers

The Icarus server can run on another machine (a desktop, a cloud VM, a Linux box) while you use the app and CLI on your laptop. Agents and terminals then run on that machine and keep running when the laptop sleeps.

There are two ways to reach a server: pair with it over TLS (good on a private network or Tailscale), or reach it over SSH.

## Run a headless server

On the server machine, install the CLI and server (see [Install](getting-started.md#linux-and-the-cli-on-its-own)), sign in to `claude` / `codex` and `gh` there, and add your projects there:

```sh
icarus server install                # start at login
icarus project add ~/code/my-repo
```

## Pair over TLS (Tailscale, LAN)

1. On the server machine, make it listen on an address the laptop can reach, for example its Tailscale address:

   ```sh
   icarus config set server.listen '["100.101.102.103:7878"]'
   icarus server restart
   ```

   For a one-off run, `icarus server start --listen 100.101.102.103:7878` does the same. The server only listens on loopback, private and Tailscale addresses unless you add `--allow-public`.

2. Still on the server machine, print a pairing code:

   ```sh
   icarus server pair
   ```

   The code is single-use and valid for 10 minutes (`--ttl` changes that, up to an hour). The command also prints the certificate's fingerprint.

3. On your laptop, save the server with the code:

   ```sh
   icarus remote add desk 100.101.102.103:7878 --code ABCD-EFGH
   ```

   The laptop gets its own token and pins the server's certificate. `--fingerprint` makes it check the fingerprint printed in step 2.

The connection is TLS with a self-signed certificate that the client pins. Failed pairing attempts are rate-limited.

To see and revoke paired clients (on the server machine only):

```sh
icarus server clients
icarus server revoke <client>
```

## Reach a server over SSH

If you can already SSH to the machine, save it as an SSH server. SSH does the authentication; nothing listens on the network.

```sh
icarus remote add devbox ssh://devbox
```

The CLI runs `icarus-server proxy` on the host over SSH, which starts the server there if needed. The binary must be on the `PATH` of a non-interactive SSH session; otherwise pass the full command with `--command "~/.local/bin/icarus-server proxy"`.

## Use a saved server

Every command takes `--server <name>`:

```sh
icarus --server desk workspace list
icarus --server desk workspace create --project my-repo fix-login
icarus --server devbox terminal list
icarus remote list
icarus remote remove desk     # the server keeps the pairing until you revoke it
```

## Several servers in the app

The Workspaces panel lists the app's own server first, then one section per saved server, with its name, address and connection state. New servers appear within seconds of `icarus remote add`. The **Servers** menu in the panel header lists them, explains how to pair another, and picks which server the app's own connection uses.

Everything you do from a window goes to the server that owns its workspace: the next-action button, the GitHub panel, Source Control, notes, quick commands and Remote Control. With saved servers listed, the New workspace and Add project dialogs have a **Server** picker.

### Opening a remote workspace

Clicking a remote server's card opens its worktree:

- **Over SSH**, when the server has an SSH destination (an `ssh://` server, or a paired one saved with `--ssh <host>`). The workspace opens as a Zed remote project; agent tabs and terminals run on that host.
- **As a local folder**, when the folder exists on this machine (the same machine or a shared disk).

Otherwise the app explains how to add an SSH destination:

```sh
icarus remote add desk 100.101.102.103:7878 --code ABCD-EFGH --ssh desk
```

## SSH projects

Zed's SSH remoting runs a small server on the remote host. Release builds of Icarus download their own one from the matching Icarus release into `~/.icarus_server` on the host (with `curl` on the host, or through the app if the host can't download). An Icarus upgrade replaces it. Linux (x86_64, aarch64) and macOS hosts are supported; Windows hosts aren't.

Agent tabs in an SSH project need the `icarus` CLI on the host. If it's missing, the app asks **Install Icarus on <host>?** and installs the release matching the app into `~/.icarus/bin` on the host, checked against the release's checksums. The agent tab then starts the host's server if needed.

Terminals in an SSH project are Zed's remote shells, not server terminals, so they don't survive an app restart.
