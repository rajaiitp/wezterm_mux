---
tags:
  - appearance
---
# `pane_content_padding`

Controls the inset between a pane border and the terminal content. Padding is
specified independently for each side and uses the same units as
[window_padding](window_padding.md), including pixels and cells.

The available terminal rows and columns are reduced to match the inset, so
terminal applications lay themselves out within the padded content area.

```lua
config.pane_content_padding = {
  left = '0.5cell',
  right = '0.5cell',
  top = '0.3cell',
  bottom = '0cell',
}
```
