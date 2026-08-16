# inm

A native macOS manager for [Incus](https://linuxcontainers.org/incus/) virtual
machines, with the SPICE console rendered **inside the app** — no
`remote-viewer` window, no browser.

![demo](docs/demo.gif)

## Why

`incus console --type vga` hands you off to an external SPICE viewer, one
window per machine. With a fleet spread across several Incus projects that
means a lot of window juggling, and every reconnect is a fresh handshake.

`inm` keeps every console you open as a tab. Background tabs stay connected, so
switching back is instant.

## Features

- **Project tree** — every VM across every Incus project, live status, search
  (`⌘F`), auto-refreshed.
- **Tab groups** — tabs are grouped by project the way Chrome groups tabs, with
  a stable colour per project and collapsible groups.
- **Embedded console** — the guest screen is decoded by spice-gtk and painted
  directly into the window; mouse and keyboard are forwarded to the guest.
- **Quick open** — `⌘P` to jump to any VM by name or project.
- **Keyboard first** — `⌘1…9` switch tabs, `⌘W` close, `⌘B` toggle the sidebar.
  Hold `⌘` to reveal the tab numbers.
- **Non-QWERTY hosts** — a Dvorak host keyboard is reverse-mapped to physical
  key positions, so the guest applies its own layout instead of translating
  twice (`INM_LAYOUT=dvorak`).
- **Ctrl+Alt+Del** — sendable from the status bar, since a Mac keyboard cannot
  type it.

## How it works

```
incus console --type vga  ──►  SPICE unix socket  ──►  libspice-client-glib  ──►  gpui
        (tunnel)                                          (decode)              (paint)
```

- The tunnel comes from the `incus` CLI. Run with a trimmed `PATH` it finds no
  local viewer, so it prints the raw socket path instead of launching one.
- Protocol and image decoding are spice-gtk's — the same library
  `remote-viewer` uses.
- All SPICE sessions share **one** GLib thread and one main loop. A
  `GMainContext` can only be owned by one thread, so a thread per session
  deadlocks.
- The UI is [gpui](https://www.gpui.rs/), Zed's GPU-accelerated framework.
  Frames are coalesced to 60fps and hidden tabs skip conversion entirely.

## Requirements

- macOS with Homebrew
- `brew install incus spice-gtk`
- A configured Incus remote (`incus remote add …`); `inm` uses the CLI's own
  configuration and credentials.

## Build

```sh
PKG_CONFIG_PATH=/opt/homebrew/opt/spice-gtk/lib/pkgconfig cargo build --release
```

## Status

A personal tool, built for a specific fleet. The `incus` binary path is
currently hardcoded to the Homebrew location in `src/incus.rs`.

## License

MIT
