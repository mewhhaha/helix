# Fork Notes

This repository is a fork of [Helix](https://github.com/helix-editor/helix).

Most of the editor still follows upstream Helix closely. This README only calls out the behavior and options added in this fork.

# Extra Options And Features

## LSP document colors

This fork adds configurable rendering for document colors reported by the language server.

- `editor.lsp.display-color-swatches` enables or disables document color decorations.
- `editor.lsp.document-color-mode` controls how those colors are rendered:
  - `virtual`: shows an inline swatch beside the color.
  - `foreground`: paints the text itself with the reported color.
  - `background`: paints the text background with the reported color and automatically chooses a contrasting foreground color.

Example:

```toml
[editor.lsp]
display-color-swatches = true
document-color-mode = "background"
```

## File tree toggle

This fork also includes a file tree sidebar that can be toggled with `Cmd-b` / `SUPER+b` (or `C-A-b`).

# Upstream

For the base editor, upstream documentation, installation notes, and the original project, see [Helix](https://github.com/helix-editor/helix).
