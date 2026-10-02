# hypr-eve-preview

## What it does

`hypr-eve-preview` shows a live thumbnail of every EVE Online game client on one output (default `DP-3`), above all windows on every workspace. It runs on Hyprland.

- A game client is a window of class `steam_app_8500` whose title is `EVE` or starts with `EVE - `. The launcher and helper windows are not tracked.
- Each thumbnail has a label (the character name) and, on the focused client, a border ring.
- A left click switches the output to the client's workspace.
- A left drag moves a thumbnail. A left drag on its bottom-right 16 x 16 logical px grip, or the scroll wheel over the thumbnail, resizes it. The aspect ratio stays locked.
- Position and width are saved per EVE account.
- Clients appear and disappear as Hyprland's event socket reports them. A config change needs a restart.

The tool does not hide thumbnails, supports one output only, and does not reconnect to Hyprland: it exits instead. It reads no keyboard input.

## Command line

```
hypr-eve-preview [--config <path>] [--log <path>] [--verbose] [--seconds <N>] [--ignore-damage]
```

| Flag | Value | Default | Effect |
|---|---|---|---|
| `--config` | path | `$XDG_CONFIG_HOME/hypr-eve-preview/config.toml` | Read this config file. The file must exist. |
| `--log` | path | none | Append every report line to this file as well as stderr. Missing parent directories are created. |
| `--verbose` | none | off | Also print the per-frame lines. |
| `--seconds` | integer 1 to 86400 | run until SIGINT or SIGTERM | Exit 0 after N seconds. |
| `--ignore-damage` | none | off | Copy every frame with full damage and send no re-commit. Each copy repaints the whole output, so the flag is for comparing GPU load and update behaviour against the default mode. |

A flag may occur once. The value is the next argument. `--flag=value` and positional arguments are rejected. Any violation prints a `usage` line and exits 2 without opening a file or socket.

`XDG_CONFIG_HOME` counts when it is set, non-empty and absolute. Otherwise `$HOME/.config` is used.

## Hyprland setup

The user adds these lines to `hyprland.conf`. The tool never edits it.

```
exec-once = <absolute path to hypr-eve-preview> --log <path>
layerrule = no_anim on, match:namespace ^(hypr-eve-preview)$
```

- `exec-once` children have stdout and stderr on `/dev/null` and a `PATH` without `~/.cargo/bin`. Use an absolute path and `--log` to keep the report lines.
- `cd packaging/arch && makepkg -si` builds the checkout and installs `/usr/bin/hypr-eve-preview` and this document under `/usr/share/doc/hypr-eve-preview/`.
- The `layerrule` stops Hyprland from animating thumbnail moves while dragging.
- Thumbnails are placed by account slot. A client on a workspace named `EVE<n>` (n from 1 to 12) has slot n. `eve-workspaces.sh` puts each client on its `EVE<n>` workspace. Without it clients still get thumbnails, with no slot.

## Config file

TOML. Every key is optional. An unknown key, a wrong type, an integer outside 0 to 4294967295 or a failed rule is a config error and exits 2. The error names the dotted key and the reason. A missing default file means all defaults.

| Key | Type | Default | Rule |
|---|---|---|---|
| `output` | string | `DP-3` | Non-empty. The Hyprland monitor and `wl_output` name. |
| `thumbnail.width` | integer | 480 | Even. `min_width` to `max_width`. |
| `thumbnail.min_width` | integer | 160 | Even, at least 2. |
| `thumbnail.max_width` | integer | 1280 | Even, at least `min_width`. |
| `placement.x` | integer | 8 | Even. Logical px from the usable area's left edge. |
| `placement.y` | integer | 8 | Even. Logical px from the usable area's top edge. |
| `placement.gap` | integer | 8 | Even. Logical px between default-placed thumbnails. |
| `border.width` | integer | 2 | 0 to 64 logical px. 0 draws no ring. |
| `border.color` | string | `#40FF00` | `#RRGGBB` or `#RRGGBBAA`. |
| `label.font` | string | `Noto Sans Mono` | Non-empty fontconfig family. Resolved once with `fc-match`. |
| `label.font_file` | string | absent | Font file path. Wins over `label.font`. |
| `label.size` | integer | 15 | 1 to 256 logical px. |
| `label.color` | string | `#40FF00` | `#RRGGBB` or `#RRGGBBAA`. |
| `label.x` | integer | 6 | Logical px from the thumbnail's left edge. |
| `label.y` | integer | 6 | Logical px from the thumbnail's top edge. |
| `resize.step` | integer | 32 | Even, at least 2. Logical px per wheel step. |

The file below sets every key to its default. `label.font_file` has no default value, so its line is a comment.

```toml
output = "DP-3"

[thumbnail]
width = 480
min_width = 160
max_width = 1280

[placement]
x = 8
y = 8
gap = 8

[border]
width = 2
color = "#40FF00"

[label]
font = "Noto Sans Mono"
# font_file = "/usr/share/fonts/noto/NotoSansMono-Regular.ttf"
size = 15
color = "#40FF00"
x = 6
y = 6

[resize]
step = 32
```

## Layout file

`$XDG_STATE_HOME/hypr-eve-preview/layout.json`. `XDG_STATE_HOME` counts when it is set, non-empty and absolute. Otherwise `$HOME/.local/state` is used.

- One JSON object. Each key is an account key: `user:<id>` or `character:<name>`. Each value has exactly `x`, `y` and `width`, in logical px, with the position relative to the usable area's origin.
- Writers: the end of a drag, the end of a grip resize, a wheel step, and a key change of a user-placed client. Each writes only for a client that has an account key. Entries of accounts that are not running stay.
- At most one write per 500 ms. The write goes to `layout.json.tmp` in the same directory, then is renamed over `layout.json`. A save still pending at exit is written then. The directory is created with mode 0700.
- Startup never writes the file. An unreadable or malformed file is renamed to `layout.json.bad` (replacing an older one), a `layout-error` line reports it, and the layout starts empty. If the rename fails the tool exits 1.
- Account key sources, in order: the user id decoded from the client's `/LauncherData=` command-line argument, then the character name from the latest title, then none (default placement, nothing saved).

```json
{"user:1000001": {"x": 8, "y": 8, "width": 480}}
```

## Report lines

Every line goes to stderr and, with `--log`, to the log file. Each starts with `hypr-eve-preview: `. Strings marked `<json>` use JSON string encoding. `<addr>` is `0x` and lowercase hex. `-` stands for an absent value. The `exit` line is always last.

| Class | Level | When | Fields in print order |
|---|---|---|---|
| `start` | default | Once, after the setup reads. | `output`, `scale`, `usable=<x>,<y>,<w>x<h>`, `mode` (`recommit` or `ignore-damage`), `config` (`<json path>` or `-`), `layout`, `font` (`<json path>`) |
| `account` | default | The user id lookup fails, before that client's `client-added` line. | `address`, `error=<json>` |
| `client-added` | default | A game client is tracked. | `address`, `pid`, `workspace=<json>`, `slot` (`<n>` or `-`), `account` (`<json key>` or `-`), `label=<json>` |
| `client-removed` | default | A tracked or pending client leaves. The reason is `closed`, `title <json title>`, `not listed by j/clients` or a capture failure. A pending client (its `j/clients` lookup unanswered) leaves with `closed` or `not listed by j/clients`, without an earlier `client-added`. | `address`, `reason=<json>` |
| `title` | default | A tracked client's title changes and is still a game title. | `address`, `label=<json>`, `account` |
| `workspace` | default | A tracked client moves to another workspace. | `address`, `workspace=<json>`, `slot`, `label=<json>` |
| `focus` | default | The ring moves to another thumbnail or to none. | `address` (`-` for none) |
| `dispatch` | default | Each click, after the reply. | `address`, `request=<json>`, then `reply="ok"` or `error=<json>` |
| `layout-saved` | default | Each layout file write. | `path=<json>`, `entries=<n>` |
| `layout-error` | default | A layout write fails, or a startup read fails and the file is renamed. | `path=<json>`, `error=<json>`, and `renamed=<json path>` after a rename |
| `ignored` | default | A handled Hyprland event whose data does not parse. | `line=<json>` |
| `failed` | default | A capture `failed` or stall. | `address`, `seq`, `reason` (`failed` or `stall`), `consecutive` |
| `format` | verbose | Each buffer allocation. | `address`, `fourcc=<4 chars> (0x<8 hex>)`, `modifier=0x<16 hex>`, `size=<w>x<h>`, `planes` |
| `ready` | verbose | Each captured frame. | `address`, `seq`, `buffer` (0 or 1), `tv=<sec>.<9-digit nsec>`, `copy_to_ready_us` |
| `release-wait` | verbose | The next capture waits only for a buffer release. | `address`, `buffer` |
| `released` | verbose | That release arrives. | `address`, `buffer`, `waited_us` |
| `usage` | default | A usage error. | `message=<json>`, `syntax="hypr-eve-preview [--config <path>] [--log <path>] [--verbose] [--seconds <N>] [--ignore-damage]"` |
| `exit` | default | Always last. | `code`, `reason=<json>` |

`seq` is the 1-based number of the client's capture attempt. `buffer` is the slot. `scale` prints as a plain decimal, for example `1.5`.

## Exit codes

| Code | Cause |
|---|---|
| 0 | SIGINT, SIGTERM, or `--seconds` elapsed. |
| 1 | Runtime failure: no default config or state path, log file failure, layout rename failure, `fc-match` failure, missing `XDG_RUNTIME_DIR` or `HYPRLAND_INSTANCE_SIGNATURE`, Hyprland event socket connect error, EOF or read error, a failed or unparsable `j/monitors`, `j/clients` or `j/activewindow` request, missing output or Wayland global, no render node, a `gbm_create_device` failure, no `linux_dmabuf` event before `buffer_done`, unknown fourcc, empty modifier list, buffer allocation or import failure, overlay closed by the compositor, a chrome input region, shm pool or shm buffer failure, a refused `Buffer::attach_to` of the chrome buffer, a pointer call error (`get_pointer_with_theme`, `get_relative_pointer`, `set_cursor`), executor out of step (a copy, re-commit or present with no frame, overlay or buffer for its slot), Wayland connection or protocol error, a failure to build the event loop or the `Signals` source, a failed teardown step. |
| 2 | Usage error, or config error (unreadable or malformed file, unknown key, wrong type, value out of range, missing `--config` file, bad `label.font_file`). |

If a report line cannot be written, the tool exits 1 without an `exit` line.

## Guarantees

- It never focuses, moves, resizes or closes a window and never edits Hyprland config.
- The only dispatcher it sends is `workspace name:<ws>`, once per click. The only other Hyprland requests are `j/monitors`, `j/clients` and `j/activewindow`.
- The only subprocess is `fc-match`, run at most once at startup.
- It reads `/proc/<pid>/cmdline` of a game client once, to find the user id, and no other file under `/proc/<pid>/`. Nothing from a command line is printed or saved except the decoded decimal user id inside an account key.
- It does not read captured pixels on the CPU.
