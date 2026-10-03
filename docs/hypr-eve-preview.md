# hypr-eve-preview

## What it does

`hypr-eve-preview` shows a live thumbnail of every EVE Online game client on any monitor, above all windows on every workspace. It runs on Hyprland.

- A game client is a window started by the EVE launcher (its command line carries `/LauncherData=`) whose title is `EVE` or starts with `EVE - `. The launcher and helper windows are not tracked.
- Each thumbnail has a label (the character name, else the `EVE<n>` workspace name, else `EVE`) and, on the focused client, a border ring.
- A left click switches to the client's workspace.
- A thumbnail starts on the monitor its saved layout entry names, else on the default monitor: the one `output` names, else the one focused at startup.
- A left drag moves a thumbnail. The thumbnail shows wherever its rectangle overlaps a monitor's usable area, at the same logical size, its label and ring drawn at each monitor's scale; a release on a monitor keeps it there, and the width may be capped to that monitor's usable width at the release. A left drag on its bottom-right 16 x 16 logical px grip, or the scroll wheel over the thumbnail, resizes it. The aspect ratio stays locked.
- While snapping is on, a dragged thumbnail snaps to the edges of its monitor's usable area and of the other thumbnails on that monitor within `thumbnail.snap_distance` logical px.
- One global lock stops moving and resizing. A click still switches the workspace.
- Every thumbnail has one base opacity, `thumbnail.opacity` until set at runtime. A thumbnail under the pointer is fully opaque.
- Hide removes every thumbnail until show. The state is runtime only and the tool starts visible.
- Lock, hide, snapping and opacity are runtime switches. The tray menu and the control socket set them.
- A command form (`lock`, `opacity <N>` and eight more) sends one command to the running daemon.
- Position, width, monitor, the lock, snapping and opacity are saved in the layout file. Position, width and monitor are per EVE account.
- Clients appear and disappear as Hyprland's event socket reports them. A config change needs a restart.

The tool reads the set of monitors once at startup and does not follow monitor hotplug. It does not reconnect to Hyprland: it exits instead. It reads no keyboard input.

## Command line

Two forms.

```
hypr-eve-preview [--config <path>] [--log <path>] [--verbose] [--seconds <N>] [--ignore-damage]
hypr-eve-preview lock|unlock|hide|show|toggle-lock|toggle-hide|snap|unsnap|toggle-snap|opacity <N>
```

The first form runs the daemon. The second form is the command form.

| Flag | Value | Default | Effect |
|---|---|---|---|
| `--config` | path | `$XDG_CONFIG_HOME/hypr-eve-preview/config.toml` | Read this config file. The file must exist. |
| `--log` | path | none | Append every report line to this file as well as stderr. Missing parent directories are created. |
| `--verbose` | none | off | Also print the per-frame lines. |
| `--seconds` | integer 1 to 86400 | run until SIGINT or SIGTERM | Exit 0 after N seconds. |
| `--ignore-damage` | none | off | Copy every frame with full damage and send no re-commit. Each copy repaints the whole output, so the flag is for comparing GPU load and update behaviour against the default mode. |

A flag may occur once. The value is the next argument. `--flag=value` and positional arguments are rejected. Any violation prints a `usage` line and exits 2 without opening a file or socket.

`XDG_CONFIG_HOME` counts when it is set, non-empty and absolute. Otherwise `$HOME/.config` is used.

### Commands

| Command | Effect |
|---|---|
| `lock` | Lock thumbnail position and size. |
| `unlock` | Unlock them. |
| `hide` | Hide every thumbnail. |
| `show` | Show every thumbnail. |
| `toggle-lock` | Flip the lock. |
| `toggle-hide` | Flip the hide state. |
| `snap` | Turn drag snapping on. Snapping is on by default. |
| `unsnap` | Turn drag snapping off. |
| `toggle-snap` | Flip drag snapping. |
| `opacity <N>` | Set the base opacity of every thumbnail to N percent, 0 to 100. The hovered thumbnail stays opaque. |

A command word is recognised only as the first argument. `opacity` takes exactly one value, 1 to 3 digits worth at most 100. The other words take none. Any other use is a usage error and exits 2.

| Usage error | `usage` message |
|---|---|
| `opacity` without a value | `opacity needs a value` |
| Any other `opacity` value | `invalid opacity "<v>": want 0 to 100` |
| An argument after the command | `<command>: unexpected argument "<arg>"` |

The command form opens no log, config, layout or Hyprland socket. It connects to the control socket, sends the command and reads the reply, and each write and each read call waits at most 2 s, so a peer that sends slowly can make the exchange longer.

| Outcome | Exit | stderr |
|---|---|---|
| Reply `ok` | 0 | nothing |
| Reply `error: <reason>` | 1 | `exit` line with that reason |
| `XDG_RUNTIME_DIR` or `HYPRLAND_INSTANCE_SIGNATURE` unset or empty | 1 | `exit` line |
| No daemon (connect error) | 1 | `exit` line |
| A write or read call waits 2 s with no progress | 1 | `exit` line |
| Other read or write error, or an unexpected reply | 1 | `exit` line |
| Usage error | 2 | `usage` line, then `exit` line |

## Control socket

The daemon listens on `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.hypr-eve-preview.sock`. Both variables must be set and non-empty. One daemon runs per Hyprland instance: a second daemon exits 1 when the first answers on the socket. Any file at the socket path that refuses the connection, a stale socket or another file, is removed before the daemon binds.

```
request = command ( "\n" | EOF )
command = word | "opacity " value
word    = "lock" | "unlock" | "hide" | "show" | "toggle-lock" | "toggle-hide"
        | "snap" | "unsnap" | "toggle-snap"
value   = 1*3DIGIT                  ; at most 100
reply   = "ok\n" | "error: " reason "\n"
reason  = "empty line" | "unknown command" | "line too long" | "timeout" | "busy" | "bad value"
```

| Limit | Value |
|---|---|
| Request size before the terminator | 64 bytes |
| Time to send the terminator | 1 s from accept |
| Open connections | 8 |

- One command per connection. Matching is exact and bytewise. Case variants, spaces and `\r` make an unknown command, except after `opacity `, where they make a bad value. `opacity` alone is an unknown command.

| Received | Reply |
|---|---|
| A command, then `\n` or EOF | `ok` |
| `\n` as the first byte | `error: empty line` |
| `opacity ` and then anything but 1 to 3 digits worth at most 100 | `error: bad value` |
| Any other line of at most 64 bytes | `error: unknown command` |
| 65 bytes without `\n` | `error: line too long` |
| No terminator 1 s after accept | `error: timeout` |
| Accepted while 8 connections are open | `error: busy` |

- `ok` is sent after the command is applied. A command that asks for the current state changes nothing and also gets `ok`.
- A connection that closes before sending a byte gets no reply.
- After a stop is requested no command is applied.

## Hyprland setup

The user adds these lines to `hyprland.conf`. The tool never edits it.

```
exec-once = <absolute path to hypr-eve-preview> --log <path>
layerrule = no_anim on, match:namespace ^(hypr-eve-preview)$
bind = SUPER SHIFT, H, exec, hypr-eve-preview toggle-hide
```

- `exec-once` children have stdout and stderr on `/dev/null` and a `PATH` without `~/.cargo/bin`. Use an absolute path and `--log` to keep the report lines.
- `cd packaging/arch && makepkg -si` builds the checkout and installs `/usr/bin/hypr-eve-preview` and this document under `/usr/share/doc/hypr-eve-preview/`.
- The `layerrule` stops Hyprland from animating thumbnail moves while dragging. It also stops the fade when thumbnails hide and show. Without it the daemon still runs, but a dragged thumbnail trails the pointer.
- The `bind` line is an example. Hyprland exports `HYPRLAND_INSTANCE_SIGNATURE` to the programs it starts, so the command form finds the socket.
- Default-placed thumbnails are ordered by slot. A client on a workspace named `EVE<n>` (n from 1 to 12) has slot n. A client on any other workspace has no slot and is placed after the slotted ones.

## Config file

TOML. Every key is optional. An unknown key, a wrong type, an integer outside 0 to 4294967295 or a failed rule is a config error and exits 2. The error names the dotted key and the reason. A missing default file means all defaults.

| Key | Type | Default | Rule |
|---|---|---|---|
| `output` | string | the monitor focused at startup | Non-empty. The Hyprland monitor and `wl_output` name of the default monitor, where a thumbnail starts unless its saved entry names a monitor present at startup. |
| `thumbnail.width` | integer | 480 | Even. `min_width` to `max_width`. |
| `thumbnail.min_width` | integer | 160 | Even, at least 2. |
| `thumbnail.max_width` | integer | 1280 | Even, at least `min_width`. |
| `thumbnail.opacity` | integer | 100 | 0 to 100 percent. Base opacity of every thumbnail, image and chrome. Once the tray or the `opacity` command changes the opacity, the layout file's value wins. |
| `thumbnail.snap_distance` | integer | 10 | Logical px. 0 never snaps. The tray and the snap commands turn snapping off at runtime without changing the key. |
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

Dotted keys are TOML tables, so `[thumbnail]` with `opacity = 50` and `thumbnail.opacity = 50` are the same key.

## Layout file

`$XDG_STATE_HOME/hypr-eve-preview/layout.json`. `XDG_STATE_HOME` counts when it is set, non-empty and absolute. Otherwise `$HOME/.local/state` is used.

- One JSON object. Each key is `locked`, `snapping`, `opacity` or an account key (`user:<id>` or `character:<name>`). Each account value has exactly `x`, `y` and `width`, in logical px, and an optional `output`, the monitor name, with the position relative to the origin of that monitor's usable area. Every save writes `output`. An entry without `output`, or naming a monitor absent at startup, loads on the default monitor.
- A save writes the keys in the order `locked`, `snapping`, `opacity`, then the account entries.

| Key | Value | When absent | Written |
|---|---|---|---|
| `locked` | boolean | false | every save |
| `snapping` | boolean | true | every save |
| `opacity` | integer 0 to 100 | `thumbnail.opacity` applies | every save once the opacity has changed |

- The file is invalid when `locked` or `snapping` is not a boolean, or when `opacity` is not an integer from 0 to 4294967295 (a JSON error) or is above 100 (an error of its own). `null` counts as absent for `opacity`.
- Writers: the end of a drag, the end of a grip resize, a wheel step, a key change of a user-placed client, and a lock, snapping or opacity change. Account entries are written only for a client that has an account key. Entries of accounts that are not running stay.
- At most one write per 500 ms while running; a save still pending at exit is written at once. The write goes to `layout.json.tmp` in the same directory, then is renamed over `layout.json`. The directory is created with mode 0700.
- Startup never writes the file. An unreadable, malformed or invalid file is renamed to `layout.json.bad` (replacing an older one), a `layout-error` line reports it, and the layout starts empty, unlocked, with snapping on and no `opacity`. If the rename fails the tool exits 1.
- Account key sources, in order: the user id decoded from the client's `/LauncherData=` command-line argument, then the character name from the latest title that carries one, then none (default placement, nothing saved).

```json
{"locked": false, "snapping": true, "opacity": 80, "user:1000001": {"x": 8, "y": 8, "width": 480, "output": "HDMI-A-1"}}
```

## Tray

The tool registers one StatusNotifierItem on the session bus under the name `org.kde.StatusNotifierItem-<pid>-1`. The icon is a ring in `border.color` while thumbnails are shown and red while they are hidden. ashell is the tested host.

| Menu item | Kind | A click |
|---|---|---|
| `Lock thumbnails` | checkmark, checked when locked | Toggles the lock. |
| `Hide thumbnails` | checkmark, checked when hidden | Toggles hide. |
| `Snap thumbnails` | checkmark, checked when snapping is on | Toggles snapping. |
| `Opacity` | submenu of checkmark steps `10%`, `20%`, ... `100%` | A click on a step sets that base opacity. |
| `Quit` | plain | Stops the daemon with exit 0 and reason `tray quit`. |

- The step equal to the base opacity is checked. No step is checked when the value is not a step.
- The checkmarks and the icon follow the state when a command from the control socket changes it.
- Activating the icon itself does nothing.
- A tray failure never stops the daemon. A session-bus, name or match-rule error, or a failure to set up the tray's event sources at startup, prints `tray-unavailable` and the daemon runs without the tray. A failed registration and a watcher that leaves the bus print `tray-unavailable` and keep the tray, which registers again when a watcher appears. A `drain: 16 passes` report prints `tray-unavailable` and keeps the tray; it is informational and starts no registration.
- A lost session-bus connection ends the tray until the next start. There is no reconnection.
- The tool needs the system libdbus-1 at run time.

## Runtime switches

Snapping and opacity behave like the lock: each is saved in the layout file and restored at startup, and a change prints a `snap` or `opacity` line and saves the layout. Snapping applies only to drags. An opacity change applies at once to every shown thumbnail except the hovered one, which stays opaque.

**Lock.**
- One global boolean, saved in the layout file. It applies at startup.
- Sources: `lock`, `unlock`, `toggle-lock` and the tray item. A change prints a `lock` line and saves the layout. A command for the current state changes nothing.
- While locked, the grip shows no resize cursor, a drag does nothing, and the wheel does nothing. A press and release under 4 px still clicks.
- A drag or resize already running when the lock turns on completes and saves.
- The cursor shape updates at the next pointer event.
- Lock works while hidden.

**Hide.**
- Runtime state, never saved. The tool starts visible.
- Sources: `hide`, `show`, `toggle-hide` and the tray item. A change prints a `visibility` line. A command for the current state changes nothing.
- A running drag or resize completes first.
- Hide destroys every overlay and stops every capture. The tool does no GPU work while hidden.
- Each client keeps its position, width, slot, label and account. Clients that appear or leave while hidden are tracked. Title, workspace, focus and lock changes are handled and reported.
- Show re-creates each thumbnail at its kept position, or at its current default-row position when it is default-placed. Thumbnails return one by one after their first frame.

## Report lines

Every line goes to stderr and, with `--log`, to the log file. Each starts with `hypr-eve-preview: `. Strings marked `<json>` use JSON string encoding. `<addr>` is `0x` and lowercase hex. `source` names the origin of a state change. `-` stands for an absent value. The `exit` line is always last for the daemon.

The command form prints nothing on `ok`, only an `exit` line on a failure, and a `usage` line and an `exit` line on a usage error.

| Class | Level | When | Fields in print order |
|---|---|---|---|
| `start` | default | Once, after the setup reads. | `output`, `scale`, `usable=<x>,<y>,<w>x<h>` (the default monitor's name, scale and usable area), `mode` (`recommit` or `ignore-damage`), `config` (`<json path>` or `-`), `layout`, `locked=<true or false>`, `font` (`<json path>`) |
| `account` | default | A client's `/LauncherData=` argument gives no user id, so it is tracked without a user id; its key is the character name from its latest title that carries one, or none until the title carries one (before that client's `client-added` line). | `address`, `error=<json>` |
| `window-skipped` | default | A window with a game title has a pid of 0 or below, or a command line that cannot be read. It is not tracked. | `address`, `pid`, `error=<json>` |
| `client-added` | default | A game client is tracked. | `address`, `pid`, `workspace=<json>`, `slot` (`<n>` or `-`), `account` (`<json key>` or `-`), `label=<json>` |
| `client-removed` | default | A tracked or pending client leaves. The reason is `closed`, `title <json title>`, `not listed by j/clients` or a capture failure. A pending client (its `j/clients` lookup unanswered) leaves with `closed` or `not listed by j/clients`, without an earlier `client-added`. | `address`, `reason=<json>` |
| `title` | default | A tracked client's title changes and is still a game title. | `address`, `label=<json>`, `account` |
| `workspace` | default | A tracked client moves to another workspace. | `address`, `workspace=<json>`, `slot`, `label=<json>` |
| `focus` | default | The ring moves to another thumbnail or to none. | `address` (`-` for none) |
| `dispatch` | default | Each click, after the reply. | `address`, `request=<json>`, then `reply="ok"` or `error=<json>` |
| `layout-saved` | default | Each layout file write. | `path=<json>`, `entries=<n>` |
| `layout-error` | default | A layout write fails, or a startup read fails and the file is renamed. | `path=<json>`, `error=<json>`, and `renamed=<json path>` after a rename |
| `lock` | default | The lock changes. | `state=on` or `state=off`, `source` (`tray` or `socket`). Example: `lock state=on source=socket` |
| `visibility` | default | Hide or show changes the state. | `state=hidden` or `state=shown`, `source`. Example: `visibility state=hidden source=tray` |
| `snap` | default | Snapping changes. | `state=on` or `state=off`, `source`. Example: `snap state=off source=tray` |
| `opacity` | default | The base opacity changes. | `percent=<n>`, `source`. Example: `opacity percent=50 source=socket` |
| `tray-registered` | default | The watcher accepts the item. | `name=<json>` |
| `tray-unavailable` | default | A tray failure, watcher loss, or the drain bound. | `reason=<json>` |
| `control-listening` | default | Once, at startup, before the loop runs. | `path=<json path>` |
| `control-error` | default | An accept, read, protocol, reply or shutdown error on the control socket. | `error=<json>` |
| `ignored` | default | A handled Hyprland event whose data does not parse. | `line=<json>` |
| `failed` | default | A capture `failed` or stall. | `address`, `seq`, `reason` (`failed` or `stall`), `consecutive` |
| `format` | verbose | Each buffer allocation. | `address`, `fourcc=<4 chars> (0x<8 hex>)`, `modifier=0x<16 hex>`, `size=<w>x<h>`, `planes` |
| `ready` | verbose | Each captured frame. | `address`, `seq`, `buffer` (0 or 1), `tv=<sec>.<9-digit nsec>`, `copy_to_ready_us` |
| `release-wait` | verbose | The next capture waits only for a buffer release. | `address`, `buffer` |
| `released` | verbose | That release arrives. | `address`, `buffer`, `waited_us` |
| `usage` | default | A usage error. | `message=<json>`, `syntax`, the two command-line forms joined by ` \| ` |
| `exit` | default | Always last. | `code`, `reason=<json>` |

`seq` is the 1-based number of the client's capture attempt, counted from when the client is tracked or from the last show, whichever is later. `buffer` is the index of the client's dmabuf buffer, 0 or 1. `scale` prints as a plain decimal, for example `1.5`.

## Exit codes

| Code | Cause |
|---|---|
| 0 | SIGINT, SIGTERM, `--seconds` elapsed, or the tray `Quit` item. |
| 1 | Runtime failure: no default config or state path, log file failure, layout rename failure, `fc-match` failure, missing `XDG_RUNTIME_DIR` or `HYPRLAND_INSTANCE_SIGNATURE`, Hyprland event socket connect error, EOF or read error, a failed or unparsable `j/monitors`, `j/clients` or `j/activewindow` request, no default monitor in `j/monitors` (`no monitor named <name> in j/monitors`; with `output` unset, `no focused monitor in j/monitors`), no `wl_output` for the default monitor at startup or for a monitor a surface is created on (`no wl_output named <name>`), a missing Wayland global, no render node, a `gbm_create_device` failure, no `linux_dmabuf` event before `buffer_done`, unknown fourcc, empty modifier list, buffer allocation or import failure, overlay closed by the compositor, a chrome input region, shm pool or shm buffer failure, a refused `Buffer::attach_to` of the chrome buffer, a pointer call error (`get_pointer_with_theme`, `get_relative_pointer`, `set_cursor`), executor out of step (`<action>: no frame, overlay or buffer for the slot`, where the action is `copy`, `recommit` or `present`), Wayland connection or protocol error, an event-loop error (building the loop or the `Signals` source, inserting or re-enabling a source or timer other than a control connection's or the tray's, or a failed dispatch with no Wayland error), a failed teardown step, another daemon answering on the control socket, a control socket probe, removal or bind failure, a missing `wp_alpha_modifier_v1` global. A tray failure is never a cause. |
| 2 | Usage error, or config error (unreadable or malformed file, unknown key, wrong type, value out of range, missing `--config` file, bad `label.font_file`, an out-of-range `thumbnail.opacity`). |

The command form exits 0, 1 or 2 as the command-line table lists.

If a report line cannot be written, the tool exits 1 without an `exit` line.

## Guarantees

- It never focuses, moves, resizes or closes a window and never edits Hyprland config.
- The only dispatcher it sends is `workspace name:<ws>`, once per click. The only other Hyprland requests are `j/monitors`, `j/clients` and `j/activewindow`.
- The only subprocess is `fc-match`, run at most once at startup.
- It reads `/proc/<pid>/cmdline` at most once each time a window with a game title appears (startup snapshot or `openwindow`), to identify the client and find the user id, and no other file under `/proc/<pid>/`. Nothing from a command line is printed or saved except the decoded decimal user id inside an account key.
- It does not read captured pixels on the CPU.
- It listens on one Unix socket in Hyprland's instance directory, and owns one session-bus name. Neither exposes client data. The socket accepts only the ten commands. The only state-changing bus method is a click on `Lock thumbnails`, `Hide thumbnails`, `Snap thumbnails`, an opacity step or `Quit`.
