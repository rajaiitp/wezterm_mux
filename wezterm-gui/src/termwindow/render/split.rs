use crate::termwindow::{UIItem, UIItemType};
use mux::tab::{PositionedSplit, SplitDirection};

impl crate::TermWindow {
    /// Register the split hit target for mouse-driven resizing. Pane borders
    /// provide the visual divider; no separate separator stroke is painted.
    pub fn register_split(&mut self, split: &PositionedSplit) -> anyhow::Result<()> {
        let cell_width = self.render_metrics.cell_size.width as f32;
        let cell_height = self.render_metrics.cell_size.height as f32;
        let border = self.get_os_border();
        let first_row_offset = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height()?
        } else {
            0.
        } + border.top.get() as f32;
        let (padding_left, padding_top) = self.padding_left_top();
        let x =
            border.left.get() as usize + padding_left as usize + split.left * cell_width as usize;
        let y = padding_top as usize + first_row_offset as usize + split.top * cell_height as usize;

        let (width, height) = if split.direction == SplitDirection::Horizontal {
            (cell_width as usize, split.size * cell_height as usize)
        } else {
            (split.size * cell_width as usize, cell_height as usize)
        };

        self.ui_items.push(UIItem {
            x,
            width,
            y,
            height,
            item_type: UIItemType::Split(split.clone()),
        });
        Ok(())
    }
}
