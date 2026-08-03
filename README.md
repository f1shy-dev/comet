# Zeron

Control your coding agents (Claude Code, Codex, Cursor, Grok, Hermes, Pi) from
any of your devices.

![Zeron driving a Claude Code session with a live branch diff sidebar](apps/landing/public/assets/app-screenshot.jpg)

Every device runs a small engine that keeps your sessions in sync: start an
agent on one machine, follow and drive it from another. Install the engine as
a daemon on an always-on machine (a VPS, a spare box) and your agents keep
working after you close your laptop.

## Install the daemon (Linux)

```bash
curl -fsSL https://comet.zeron.sh/install.sh | sh
comet login                          # sign in (paste a code, done)
systemctl --user start comet-native
```

No configuration needed. Day-to-day:

```bash
comet status      # signed in? engine running?
comet update      # update to the latest release
comet daemon start|stop|restart|status
```

On macOS, this checkout has a one-command daily build:

```bash
scripts/install-macos-local.sh
```

It installs `/Applications/Zeron.app`, registers the background IPC engine
against this checkout's `target/release/comet headless`, and opens the app.
Re-run it after pulling or rebasing to rebuild both sides. Use `--no-daemon` or
`--no-open` when you only want part of that flow. To build a launchable bundle
without installing it, run `scripts/build-macos-app.sh`; the result is
`target/package/Zeron.app`.

---

Developing or curious how it works? [![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/zeronsh/comet) or check out [ARCHITECTURE.md](ARCHITECTURE.md).

Licensed under the [MIT License](LICENSE).
