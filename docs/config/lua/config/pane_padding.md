---
tags:
  - appearance
---
# `pane_padding`

Controls the visual spacing between neighboring pane borders. Padding is
specified independently for each side and uses the same units as
[window_padding](window_padding.md), including pixels and cells.

The padding is applied between panes; the outer edge of the terminal uses
`window_padding` instead.

```lua
config.pane_padding = {
  left = '0.5cell',
  right = '0.5cell',
  top = '0.5cell',
  bottom = '0.5cell',
}
```
