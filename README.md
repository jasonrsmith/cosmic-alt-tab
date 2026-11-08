# cosmic-alt-tab

Alt+Tab for the [COSMIC](https://github.com/pop-os/cosmic-epoch) desktop that behaves like Windows, macOS and GNOME:

- **Tap Alt+Tab:** instantly flip to the previous window, with no popup.
- **Hold Alt (>250 ms):** a switcher panel appears with app icons and the selected window's title.
- **Most-recently-used order:** releasing Alt commits your choice, and the windows you passed over keep their order.
- **While the panel is open:** Tab / Shift+Tab or the arrow keys move the selection, Enter picks, and Esc cancels.

![picker](https://github.com/jasonrsmith/cosmic-alt-tab/raw/main/docs/picker.png)

## How it works

A small daemon tracks windows through COSMIC's toplevel protocols (`ext_foreign_toplevel_list_v1`, `zcosmic_toplevel_info_v1`) and keeps an MRU list. Alt+Tab is bound to `cosmic-alt-tab next`, which tells the daemon over a Unix socket to start or advance a cycle.

Programs launched from a shortcut can't see when Alt is released. To catch the release, the daemon maps a layer-shell overlay that takes keyboard focus while a cycle is active. The overlay starts as an invisible 1×1 surface that ignores mouse input. When it sees the modifiers change, it knows Alt is up. If Alt is still held after 250 ms, the same surface grows into the picker. The picker is software-rendered with `tiny-skia`, `resvg` (SVG icons) and `fontdue` (text), at the output's buffer scale.

Icons are looked up from the app's `.desktop` file (matched by app_id or `StartupWMClass`), then the icon themes (your COSMIC theme, Cosmic, Pop, hicolor, …), then pixmaps.

If the daemon isn't running, `next`/`prev` fall back to COSMIC's built-in switcher (`cosmic-launcher alt-tab`), so the key never goes dead.

## Install

```sh
cargo build --release
install -Dm755 target/release/cosmic-alt-tab ~/.local/bin/cosmic-alt-tab

# start with the session (and now)
install -Dm644 contrib/cosmic-alt-tab.service ~/.config/systemd/user/cosmic-alt-tab.service
systemctl --user daemon-reload
systemctl --user enable --now cosmic-alt-tab.service
```

Then bind the keys: merge [`contrib/shortcuts.ron`](contrib/shortcuts.ron) into
`~/.config/cosmic/com.system76.CosmicSettings.Shortcuts/v1/custom`, or add custom shortcuts in
**Settings → Keyboard → Keyboard Shortcuts** that run `cosmic-alt-tab next` (Alt+Tab) and
`cosmic-alt-tab prev` (Alt+Shift+Tab).

## Commands

| Command | |
|---|---|
| `cosmic-alt-tab daemon` | run the tracker (normally via the systemd unit) |
| `cosmic-alt-tab next` / `prev` | Alt+Tab / Alt+Shift+Tab |
| `cosmic-alt-tab list` | print the MRU list (`*` = focused) |
| `cosmic-alt-tab demo` | show the picker for 1.5 s without Alt held, then cancel |
| `cosmic-alt-tab snapshot out.png` | render the picker for the current windows to a PNG |

## Notes / limitations

- Tested on Pop!_OS 24.04 with cosmic-comp 1.0 at 4K, 200% scale.
- Assumes Alt is xkb `Mod1`, which it is in standard keymaps.
- At startup, the MRU order of windows you haven't focused yet is the order they were opened.
- One icon per window, like Windows. Windows aren't grouped by app.
- The picker is shown on the output the compositor chooses for the overlay (normally the active one).

## License

MIT
