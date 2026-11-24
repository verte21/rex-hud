# rex-hud

Pure-display Retina Bento Wallboard for **Rex**, **Lenovo Legion Go (Bazzite)**, and **TermRover** (`SSH` / `Mosh` / `tmux`).

## Features

- **Full-Viewport Vector Bento UI (`resvg` + Kitty Graphics)**: Renders rounded cards, CPU load area sparklines, RAM + `zram` pressure bars, Git `+insertions / -deletions` diff bars, and a circular TODO completion ring at native Retina pixel resolution with embedded `Roboto` and `Fira Code` fonts.
- **Zero Interactive Bloat**: Pure glanceable display (`q`/`Esc` to dismiss, `r` to refresh). Emits aggregate `OSC 7501` status onto its own terminal so Rex's native tab bar lights up when any agent needs input.
- **Standalone Linux / Bazzite Kiosk Support**: Runs directly inside **Ghostty, Kitty, WezTerm, or KDE Konsole** without needing a Rex GUI client, with native Linux `/proc`, `zram`, AMD APU `hwmon` temperature, and Legion Go battery/wattage/charge-limit telemetry.
- **Automatic `TermRover` Fallback**: Automatically switches to a clean read-only `ratatui` text wallboard over `SSH`, `Mosh`, or `tmux` (or via `rex-hud --text`).

## Install on Bazzite / Linux (`x86_64`)

Download the static Linux binary (zero external dependencies) from Releases:

```sh
mkdir -p ~/.local/bin
curl -fsSL https://github.com/verte21/rex-hud/releases/latest/download/rex-hud-linux-x86_64 -o ~/.local/bin/rex-hud
chmod +x ~/.local/bin/rex-hud
```

Run directly in Ghostty, Kitty, WezTerm, or Konsole (or via the included Nushell launcher):

```nu
rex-hud
# or via Nushell kiosk launcher:
nu scripts/rex-hud.nu
```

## Build from Source (macOS / Linux)

```sh
cargo build --release
./target/release/rex-hud install
```

Cross-compile the static `x86_64-unknown-linux-musl` binary on macOS via Docker:

```sh
./scripts/build-linux.sh
```
