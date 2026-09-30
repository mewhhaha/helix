# Configuration

To override global configuration parameters, create a `config.toml` file located in your config directory:

- Linux and Mac: `~/.config/helix/config.toml`
- Windows: `%AppData%\helix\config.toml`

> 💡 You can easily open the config file by typing `:config-open` within Helix normal mode.

Example config:

```toml
theme = "onedark"

[editor]
line-number = "relative"
mouse = false

[editor.cursor-shape]
insert = "bar"
normal = "block"
select = "underline"

[editor.file-picker]
hidden = false
```

Cursor smearing draws the focused editor cursor as a pixel image in direct Kitty
and Ghostty sessions. Its corners stretch and settle after movement, and the
cursor remains visible when the animation stops. It follows the configured
block, bar, or underline shape for each mode. It is disabled by default. To enable
it, add:

```toml
[editor.cursor-smear]
enabled = true
duration = 120
max-distance = 40
```

| Key | Description | Default |
| --- | --- | --- |
| `enabled` | Enable the graphics cursor and movement animation. | `false` |
| `duration` | Animation duration in milliseconds, clamped to `16`–`1000`. | `120` |
| `max-distance` | Maximum corner travel in terminal column widths, clamped to `1`–`256`. Farther jumps animate with a shorter stretch near the destination. | `40` |

Distance is measured in terminal column widths using the actual pixel dimensions
of terminal cells. Options you omit retain their defaults. Cursor jumps animate
even when they scroll the target into view. Scrolling without moving the cursor,
view changes, mode changes, and popups reset the animation. Automatic key-prefix
help pauses graphics and retains the cursor position for the completed command.
Frames update about 60 times per second during movement and stop when the cursor
settles.

This requires terminal graphics support and available cell pixel dimensions.
It is disabled inside tmux, GNU Screen, and Zellij, and in other terminals, where
Helix uses its ordinary cursor. Terminal image rendering must also be enabled;
for example, Ghostty's `image-storage-limit = 0` disables images.

The graphics cursor uses the primary cursor's theme color by default. To choose
a separate color, add this optional entry to your [theme file](./themes.md):

```toml
"ui.cursor.smear" = { bg = "#89b4fa" }
```

Helix uses this entry's `bg` color, or its `fg` color if `bg` is omitted. See the
[editor options](./editor.md#editorcursor-smear-section) for the setting reference.

You can use a custom configuration file by specifying it with the `-c` or
`--config` command line argument, for example `hx -c path/to/custom-config.toml`.
You can reload the config file by issuing the `:config-reload` command. Alternatively, on Unix operating systems, you can reload it by sending the USR1
signal to the Helix process, such as by using the command `pkill -USR1 hx`.

Finally, you can have a `config.toml` and a `languages.toml` local to a project by putting it under a `.helix` directory in your repository.
Its settings will be merged with the configuration directory and the built-in configuration.
