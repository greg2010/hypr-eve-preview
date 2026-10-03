# hypr-eve-preview

`hypr-eve-preview` shows a live thumbnail of every EVE Online game client on any monitor, above all windows on every workspace. It runs on Hyprland. A left click on a thumbnail switches to that client's workspace. A left drag moves a thumbnail, also onto another monitor. A left drag on its bottom-right grip, or the scroll wheel, resizes it. While snapping is on, a dragged thumbnail snaps to the edges of its monitor's usable area and to the other thumbnails on that monitor. One global lock stops moving and resizing. Hide removes every thumbnail until show. Every thumbnail has one base opacity, and the thumbnail under the pointer is fully opaque. A tray item and a control socket set the lock, hide, snapping and opacity at runtime. Position, width and monitor are saved per EVE account.

## Requirements

| Need | Why |
|---|---|
| EVE Online started by the EVE launcher, through Steam or standalone | A game client is a window started by the EVE launcher (its command line carries `/LauncherData=`) whose title is `EVE` or starts with `EVE - `. Other windows get no thumbnail. |
| Hyprland (`hyprland`) | The compositor the tool runs on. Clients are tracked through Hyprland's event socket. |
| `dbus` | The tool needs the system libdbus-1 at run time. The tray item uses the session bus. |
| `fontconfig` | `fc-match` resolves the label font at most once at startup. |
| `glibc`, `libgcc`, `mesa` | Run-time libraries the binary needs. |
| A StatusNotifier tray host (optional) | Shows the tray item. ashell is the tested host. The daemon runs without one. |
| Rust stable toolchain with `cargo` | Builds the binary. The crate uses Rust edition 2024. |
| pkg-config and the dbus-1 and gbm development files, to build | The `libdbus-sys` build script probes `dbus-1`, version 1.6 or later, through pkg-config. The `gbm-sys` crate links `libgbm`. Distributions that split development files need those packages for both. |

## Install

### Arch Linux

```
cd packaging/arch && makepkg -si
```

`makepkg` builds the checked-out tree. It installs the binary as `/usr/bin/hypr-eve-preview`, the Markdown docs in `/usr/share/doc/hypr-eve-preview/` and the license as `/usr/share/licenses/hypr-eve-preview/LICENSE`.

### Any distribution

```
cargo install --path . --locked
```

Run it from the repository root. The binary lands in `~/.cargo/bin` by default. This route installs the binary only.

## Hyprland setup

Add these lines to `hyprland.conf`. The tool never edits it.

```
exec-once = <absolute path to hypr-eve-preview> --log <path>
layerrule = no_anim on, match:namespace ^(hypr-eve-preview)$
```

- `exec-once` starts the daemon with Hyprland. It is required unless you start the daemon another way inside the Hyprland session. Its children have stdout and stderr on `/dev/null` and a `PATH` without `~/.cargo/bin`, so give the absolute path, and `--log` to keep the report lines.
- `layerrule` stops Hyprland from animating thumbnail moves while dragging, and the fade when thumbnails hide and show. Without it the daemon still runs, but a dragged thumbnail trails the pointer.

A missing default config file means every default applies.

## Run and control

```
hypr-eve-preview [--config <path>] [--log <path>] [--verbose] [--seconds <N>] [--ignore-damage]
hypr-eve-preview lock|unlock|hide|show|toggle-lock|toggle-hide|snap|unsnap|toggle-snap|opacity <N>
```

The first form runs the daemon. The second form sends one command to the running daemon through its control socket, `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.hypr-eve-preview.sock`. One daemon runs per Hyprland instance.

The command form exits 1 when `XDG_RUNTIME_DIR` or `HYPRLAND_INSTANCE_SIGNATURE` is unset or empty. Run it inside the Hyprland session: Hyprland exports `HYPRLAND_INSTANCE_SIGNATURE` to the programs it starts, such as a `bind` command or a terminal.

| Command | Effect |
|---|---|
| `lock`, `unlock`, `toggle-lock` | Lock, unlock or flip the lock on thumbnail position and size. |
| `hide`, `show`, `toggle-hide` | Hide, show or flip the visibility of every thumbnail. |
| `snap`, `unsnap`, `toggle-snap` | Turn drag snapping on, off or flip it. Snapping is on by default. |
| `opacity <N>` | Set the base opacity of every thumbnail to N percent, 0 to 100. |

| File | Path | Base when the variable is unset, empty or not absolute | Content |
|---|---|---|---|
| Config | `$XDG_CONFIG_HOME/hypr-eve-preview/config.toml` | `$HOME/.config` | TOML. Every key is optional. A change needs a restart. `output` is the monitor for thumbnails with no saved monitor; unset means the monitor focused at start. |
| Layout | `$XDG_STATE_HOME/hypr-eve-preview/layout.json` | `$HOME/.local/state` | Written by the daemon: the lock, snapping, opacity, and position, width and monitor per EVE account. |

The tray menu sets the same switches, with opacity in 10% steps, and has a `Quit` item.

## Documentation

[docs/hypr-eve-preview.md](docs/hypr-eve-preview.md) is the operator reference: command line, config keys, layout file and report lines. [docs/architecture.md](docs/architecture.md) is where to start before changing the code.

## License

MIT License.
