use crate::overlay::selector::{matcher_pattern, matcher_score};
use crate::termwindow::box_model::*;
use crate::termwindow::modal::Modal;
use crate::termwindow::{DimensionContext, GuiWin, TermWindow};
use crate::utilsprites::RenderMetrics;
use config::keyassignment::{
    Confirmation, InputSelector, InputSelectorEntry, KeyAssignment, PromptInputLine,
};
use config::Dimension;
use mux_lua::MuxPane;
use rayon::prelude::*;
use std::cell::{Ref, RefCell};
use std::rc::Rc;
use wezterm_term::{KeyCode, KeyModifiers, MouseEvent};
use window::RectF;

fn element_colors(bg: InheritableColor, text: InheritableColor) -> ElementColors {
    ElementColors {
        border: BorderColor::default(),
        bg,
        text,
    }
}

fn text_row(
    font: &Rc<wezterm_font::LoadedFont>,
    text: impl Into<String>,
    bg: InheritableColor,
    fg: InheritableColor,
) -> Element {
    Element::new(font, ElementContent::Text(text.into()))
        .colors(element_colors(bg, fg))
        .padding(BoxDimension {
            left: Dimension::Cells(0.5),
            right: Dimension::Cells(0.5),
            top: Dimension::Cells(0.),
            bottom: Dimension::Cells(0.),
        })
        .min_width(Some(Dimension::Percent(1.)))
        .display(DisplayType::Block)
}

fn panel_colors(term_window: &TermWindow) -> (InheritableColor, InheritableColor) {
    (
        term_window
            .config
            .command_palette_bg_color
            .to_linear()
            .into(),
        term_window
            .config
            .command_palette_fg_color
            .to_linear()
            .into(),
    )
}

fn selection_colors(term_window: &mut TermWindow) -> (InheritableColor, InheritableColor) {
    let palette = term_window.palette();
    (
        palette.colors.0[8].to_linear().into(),
        palette.foreground.to_linear().into(),
    )
}

fn clip_modal_children(element: &mut ComputedElement, clip: RectF) {
    let ComputedElementContent::Children(children) = &mut element.content else {
        return;
    };

    for child in children {
        child.bounds = child.bounds.intersection(&clip).unwrap_or_default();
        child.border_rect = child.border_rect.intersection(&clip).unwrap_or_default();
        child.padding = child.padding.intersection(&clip).unwrap_or_default();
        child.content_rect = child.content_rect.intersection(&clip).unwrap_or_default();
        clip_modal_children(child, clip);
    }
}

fn compute_centered_panel(
    term_window: &mut TermWindow,
    children: Vec<Element>,
    desired_cols: usize,
    desired_rows: usize,
) -> anyhow::Result<Vec<ComputedElement>> {
    let font = term_window
        .fonts
        .command_palette_font()
        .expect("to resolve native modal font");
    let metrics = RenderMetrics::with_font_metrics(&font.metrics());
    let (panel_bg, panel_fg) = panel_colors(term_window);

    // Use the active terminal grid: under XWayland the outer window dimensions
    // can temporarily retain the monitor width while the tiled client and its
    // mux panes have already been resized.
    let dimensions = term_window.dimensions;
    let size = term_window.terminal_size;
    let cols = desired_cols.min(size.cols.saturating_sub(4)).max(12);
    let rows = desired_rows.min(size.rows.saturating_sub(2)).max(4);
    let max_pixel_width = (size.pixel_width as f32
        - 4. * term_window.render_metrics.cell_size.width as f32)
        .max(12. * metrics.cell_size.width as f32);
    let max_pixel_height = (size.pixel_height as f32
        - 2. * term_window.render_metrics.cell_size.height as f32)
        .max(4. * metrics.cell_size.height as f32);
    let pixel_width = (cols as f32 * metrics.cell_size.width as f32).min(max_pixel_width);
    let pixel_height = (rows as f32 * metrics.cell_size.height as f32).min(max_pixel_height);

    let top_bar_height = if term_window.show_tab_bar && !term_window.config.tab_bar_at_bottom {
        term_window.tab_bar_pixel_height().unwrap_or(0.)
    } else {
        0.
    };
    let (padding_left, padding_top) = term_window.padding_left_top();
    let border = term_window.get_os_border();
    let top_pixel_y = top_bar_height + padding_top + border.top.get() as f32;
    let available_width = size.pixel_width as f32;
    let available_height = size.pixel_height as f32;
    let x = (padding_left + ((available_width - pixel_width).max(0.) / 2.)).round();
    let y = (top_pixel_y + ((available_height - pixel_height).max(0.) / 2.)).round();

    let panel = Element::new(&font, ElementContent::Children(children))
        .colors(ElementColors {
            border: BorderColor::new(term_window.palette().colors.0[5].to_linear().into()),
            bg: panel_bg,
            text: panel_fg,
        })
        .padding(BoxDimension::new(Dimension::Cells(0.75)))
        .border(BoxDimension::new(Dimension::Pixels(1.)))
        .min_width(Some(Dimension::Pixels(pixel_width)))
        .max_width(Some(Dimension::Pixels(pixel_width)))
        .min_height(Some(Dimension::Pixels(pixel_height)));

    let mut computed = term_window.compute_element(
        &LayoutContext {
            height: DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: dimensions.pixel_height as f32,
                pixel_cell: metrics.cell_size.height as f32,
            },
            width: DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: dimensions.pixel_width as f32,
                pixel_cell: metrics.cell_size.width as f32,
            },
            bounds: euclid::rect(x, y, pixel_width, pixel_height),
            metrics: &metrics,
            gl_state: term_window.render_state.as_ref().unwrap(),
            zindex: 100,
        },
        &panel,
    )?;

    // The generic box layout computes block children before it constrains the
    // parent to max_width. A full-width row can therefore extend through the
    // panel padding and paint over its right border. Clip every row to the
    // panel content box; text rendering also honors the clipped content_rect.
    let content_clip = computed.content_rect;
    clip_modal_children(&mut computed, content_clip);

    Ok(vec![computed])
}

#[derive(Default)]
struct SelectorView {
    filter: String,
    filtered: Vec<InputSelectorEntry>,
    selected: usize,
    top: usize,
}

pub struct NativeInputSelector {
    element: RefCell<Option<Vec<ComputedElement>>>,
    args: InputSelector,
    event_name: String,
    window: GuiWin,
    pane: MuxPane,
    view: RefCell<SelectorView>,
}

impl NativeInputSelector {
    pub fn new(term_window: &mut TermWindow, args: InputSelector) -> anyhow::Result<Self> {
        let event_name = match args.action.as_ref() {
            KeyAssignment::EmitEvent(name) => name.clone(),
            _ => anyhow::bail!(
                "InputSelector requires action to be defined by wezterm.action_callback"
            ),
        };
        let pane = term_window
            .get_active_pane_no_overlay()
            .ok_or_else(|| anyhow::anyhow!("no active pane for InputSelector"))?;
        let mut view = SelectorView::default();
        view.filtered = args.choices.clone();
        Ok(Self {
            element: RefCell::new(None),
            args,
            event_name,
            window: GuiWin::new(term_window),
            pane: MuxPane(pane.pane_id()),
            view: RefCell::new(view),
        })
    }

    fn update_filter(&self, view: &mut SelectorView) {
        if view.filter.is_empty() {
            view.filtered = self.args.choices.clone();
        } else {
            let pattern = matcher_pattern(&view.filter);
            let mut scored: Vec<(usize, u32)> = self
                .args
                .choices
                .par_iter()
                .enumerate()
                .filter_map(|(idx, entry)| matcher_score(&pattern, &entry.label).map(|s| (idx, s)))
                .collect();
            scored.sort_by(|a, b| b.1.cmp(&a.1));
            view.filtered = scored
                .into_iter()
                .map(|(idx, _)| self.args.choices[idx].clone())
                .collect();
        }
        view.selected = 0;
        view.top = 0;
    }

    fn finish(&self, term_window: &mut TermWindow, entry: Option<InputSelectorEntry>) {
        term_window.cancel_modal();
        crate::overlay::selector::trampoline(
            self.event_name.clone(),
            self.window.clone(),
            self.pane.clone(),
            entry,
        );
    }

    fn invalidate(&self, term_window: &mut TermWindow) {
        self.element.borrow_mut().take();
        term_window.invalidate_modal();
    }

    fn visible_rows(term_window: &TermWindow) -> usize {
        (term_window.terminal_size.rows * 3 / 5).clamp(4, 18)
    }

    fn move_selection(&self, term_window: &mut TermWindow, delta: isize) {
        let mut view = self.view.borrow_mut();
        let last = view.filtered.len().saturating_sub(1);
        if delta < 0 {
            view.selected = view.selected.saturating_sub(delta.unsigned_abs());
        } else {
            view.selected = view.selected.saturating_add(delta as usize).min(last);
        }
        let rows = Self::visible_rows(term_window);
        if view.selected < view.top {
            view.top = view.selected;
        } else if view.selected >= view.top + rows {
            view.top = view.selected.saturating_sub(rows - 1);
        }
    }
}

impl Modal for NativeInputSelector {
    fn mouse_event(&self, _event: MouseEvent, _term_window: &mut TermWindow) -> anyhow::Result<()> {
        Ok(())
    }

    fn key_down(
        &self,
        key: KeyCode,
        mods: KeyModifiers,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<bool> {
        match (key, mods) {
            (KeyCode::Escape, KeyModifiers::NONE)
            | (KeyCode::Char('g'), KeyModifiers::CTRL)
            | (KeyCode::Char('c'), KeyModifiers::CTRL) => {
                self.finish(term_window, None);
                return Ok(true);
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                let entry = {
                    let view = self.view.borrow();
                    view.filtered.get(view.selected).cloned()
                };
                self.finish(term_window, entry);
                return Ok(true);
            }
            (KeyCode::UpArrow, KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CTRL)
            | (KeyCode::Char('k'), KeyModifiers::CTRL) => {
                self.move_selection(term_window, -1);
            }
            (KeyCode::DownArrow, KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CTRL)
            | (KeyCode::Char('j'), KeyModifiers::CTRL) => {
                self.move_selection(term_window, 1);
            }
            (KeyCode::Backspace, KeyModifiers::NONE) => {
                let mut view = self.view.borrow_mut();
                view.filter.pop();
                self.update_filter(&mut view);
            }
            (KeyCode::Char('u'), KeyModifiers::CTRL) => {
                let mut view = self.view.borrow_mut();
                view.filter.clear();
                self.update_filter(&mut view);
            }
            (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                let mut view = self.view.borrow_mut();
                if self.args.fuzzy || !c.is_control() {
                    view.filter.push(c);
                    self.update_filter(&mut view);
                }
            }
            _ => return Ok(false),
        }
        self.invalidate(term_window);
        Ok(true)
    }

    fn computed_element(
        &self,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<Ref<'_, [ComputedElement]>> {
        if self.element.borrow().is_none() {
            let font = term_window
                .fonts
                .command_palette_font()
                .expect("to resolve native selector font");
            let (bg, fg) = panel_colors(term_window);
            let (selected_bg, selected_fg) = selection_colors(term_window);
            let view = self.view.borrow();
            let max_rows = Self::visible_rows(term_window);
            let visible = view
                .filtered
                .iter()
                .enumerate()
                .skip(view.top)
                .take(max_rows)
                .collect::<Vec<_>>();
            let label_width = self
                .args
                .choices
                .iter()
                .map(|entry| entry.label.chars().count())
                .max()
                .unwrap_or(20);
            let width = label_width
                .max(self.args.title.chars().count() + 4)
                .clamp(40, 96)
                + 4;
            let mut rows = vec![text_row(
                &font,
                if self.args.title.is_empty() {
                    "select".to_string()
                } else {
                    self.args.title.clone()
                },
                bg.clone(),
                fg.clone(),
            )];
            rows.push(text_row(&font, "", bg.clone(), fg.clone()));
            let query = if view.filter.is_empty() {
                self.args.fuzzy_description.clone()
            } else {
                format!("{}{}█", self.args.fuzzy_description, view.filter)
            };
            if !query.is_empty() {
                rows.push(text_row(
                    &font,
                    query,
                    selected_bg.clone(),
                    selected_fg.clone(),
                ));
            }

            if visible.is_empty() {
                rows.push(text_row(&font, "  no matches", bg.clone(), fg.clone()));
            } else {
                for (idx, entry) in visible {
                    let selected = idx == view.selected;
                    rows.push(text_row(
                        &font,
                        format!("  {}", entry.label),
                        if selected {
                            selected_bg.clone()
                        } else {
                            bg.clone()
                        },
                        if selected {
                            selected_fg.clone()
                        } else {
                            fg.clone()
                        },
                    ));
                }
            }
            rows.push(text_row(&font, "", bg.clone(), fg.clone()));
            rows.push(text_row(
                &font,
                "↑↓ move   Enter select   Esc close",
                bg.clone(),
                fg.clone(),
            ));
            // Size to the rows we actually render: the two blank rows are
            // intentional separators rather than unused space at the bottom.
            let height = rows.len().clamp(5, max_rows + 5);
            drop(view);
            self.element.borrow_mut().replace(compute_centered_panel(
                term_window,
                rows,
                width,
                height,
            )?);
        }
        Ok(Ref::map(self.element.borrow(), |value| {
            value.as_ref().unwrap().as_slice()
        }))
    }

    fn reconfigure(&self, _term_window: &mut TermWindow) {
        self.element.borrow_mut().take();
    }
}

struct PromptView {
    input: Vec<char>,
    cursor: usize,
}

pub struct NativePromptInput {
    element: RefCell<Option<Vec<ComputedElement>>>,
    args: PromptInputLine,
    event_name: String,
    window: GuiWin,
    pane: MuxPane,
    view: RefCell<PromptView>,
}

impl NativePromptInput {
    pub fn new(term_window: &mut TermWindow, args: PromptInputLine) -> anyhow::Result<Self> {
        let event_name = match args.action.as_ref() {
            KeyAssignment::EmitEvent(name) => name.clone(),
            _ => anyhow::bail!(
                "PromptInputLine requires action to be defined by wezterm.action_callback"
            ),
        };
        let pane = term_window
            .get_active_pane_no_overlay()
            .ok_or_else(|| anyhow::anyhow!("no active pane for PromptInputLine"))?;
        let input = args
            .initial_value
            .as_deref()
            .unwrap_or_default()
            .chars()
            .collect::<Vec<_>>();
        let cursor = input.len();
        Ok(Self {
            element: RefCell::new(None),
            args,
            event_name,
            window: GuiWin::new(term_window),
            pane: MuxPane(pane.pane_id()),
            view: RefCell::new(PromptView { input, cursor }),
        })
    }

    fn finish(&self, term_window: &mut TermWindow, line: Option<String>) {
        term_window.cancel_modal();
        crate::overlay::prompt::trampoline(
            self.event_name.clone(),
            self.window.clone(),
            self.pane.clone(),
            line,
        );
    }

    fn invalidate(&self, term_window: &mut TermWindow) {
        self.element.borrow_mut().take();
        term_window.invalidate_modal();
    }
}

impl Modal for NativePromptInput {
    fn mouse_event(&self, _event: MouseEvent, _term_window: &mut TermWindow) -> anyhow::Result<()> {
        Ok(())
    }

    fn key_down(
        &self,
        key: KeyCode,
        mods: KeyModifiers,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<bool> {
        match (key, mods) {
            (KeyCode::Escape, KeyModifiers::NONE) | (KeyCode::Char('g'), KeyModifiers::CTRL) => {
                self.finish(term_window, None);
                return Ok(true);
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                let line = self.view.borrow().input.iter().collect::<String>();
                self.finish(term_window, Some(line));
                return Ok(true);
            }
            (KeyCode::Char('u'), KeyModifiers::CTRL) => {
                let mut view = self.view.borrow_mut();
                view.input.clear();
                view.cursor = 0;
            }
            (KeyCode::Char('a'), KeyModifiers::CTRL) | (KeyCode::Home, KeyModifiers::NONE) => {
                self.view.borrow_mut().cursor = 0;
            }
            (KeyCode::Char('e'), KeyModifiers::CTRL) | (KeyCode::End, KeyModifiers::NONE) => {
                let mut view = self.view.borrow_mut();
                view.cursor = view.input.len();
            }
            (KeyCode::LeftArrow, KeyModifiers::NONE) => {
                let mut view = self.view.borrow_mut();
                view.cursor = view.cursor.saturating_sub(1);
            }
            (KeyCode::RightArrow, KeyModifiers::NONE) => {
                let mut view = self.view.borrow_mut();
                view.cursor = view.cursor.saturating_add(1).min(view.input.len());
            }
            (KeyCode::Backspace, KeyModifiers::NONE) => {
                let mut view = self.view.borrow_mut();
                if view.cursor > 0 {
                    view.cursor -= 1;
                    let cursor = view.cursor;
                    view.input.remove(cursor);
                }
            }
            (KeyCode::Delete, KeyModifiers::NONE) => {
                let mut view = self.view.borrow_mut();
                if view.cursor < view.input.len() {
                    let cursor = view.cursor;
                    view.input.remove(cursor);
                }
            }
            (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                let mut view = self.view.borrow_mut();
                let cursor = view.cursor;
                view.input.insert(cursor, c);
                view.cursor += 1;
            }
            _ => return Ok(false),
        }
        self.invalidate(term_window);
        Ok(true)
    }

    fn computed_element(
        &self,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<Ref<'_, [ComputedElement]>> {
        if self.element.borrow().is_none() {
            let font = term_window
                .fonts
                .command_palette_font()
                .expect("to resolve native prompt font");
            let (bg, fg) = panel_colors(term_window);
            let (input_bg, input_fg) = selection_colors(term_window);
            let view = self.view.borrow();
            let before = view.input[..view.cursor].iter().collect::<String>();
            let after = view.input[view.cursor..].iter().collect::<String>();
            let input = format!("{}{}█{}", self.args.prompt, before, after);
            let description = self.args.description.replace('\r', "").replace('\n', " ");
            let width = description
                .chars()
                .count()
                .max(input.chars().count())
                .clamp(40, 76)
                + 4;
            let rows = vec![
                text_row(&font, description, bg.clone(), fg.clone()),
                text_row(&font, "", bg.clone(), fg.clone()),
                text_row(&font, input, input_bg, input_fg),
                text_row(&font, "", bg.clone(), fg.clone()),
                text_row(
                    &font,
                    "Enter save   Esc close   Ctrl+U clear",
                    bg.clone(),
                    fg.clone(),
                ),
            ];
            drop(view);
            self.element
                .borrow_mut()
                .replace(compute_centered_panel(term_window, rows, width, 5)?);
        }
        Ok(Ref::map(self.element.borrow(), |value| {
            value.as_ref().unwrap().as_slice()
        }))
    }

    fn reconfigure(&self, _term_window: &mut TermWindow) {
        self.element.borrow_mut().take();
    }
}

pub struct NativeConfirmation {
    element: RefCell<Option<Vec<ComputedElement>>>,
    args: Confirmation,
    action_name: String,
    cancel_name: Option<String>,
    window: GuiWin,
    pane: MuxPane,
    yes_selected: RefCell<bool>,
}

impl NativeConfirmation {
    pub fn new(term_window: &mut TermWindow, args: Confirmation) -> anyhow::Result<Self> {
        let action_name = match args.action.as_ref() {
            KeyAssignment::EmitEvent(name) => name.clone(),
            _ => anyhow::bail!(
                "Confirmation requires action to be defined by wezterm.action_callback"
            ),
        };
        let cancel_name = args.cancel.as_deref().and_then(|action| match action {
            KeyAssignment::EmitEvent(name) => Some(name.clone()),
            _ => None,
        });
        let pane = term_window
            .get_active_pane_no_overlay()
            .ok_or_else(|| anyhow::anyhow!("no active pane for Confirmation"))?;
        Ok(Self {
            element: RefCell::new(None),
            args,
            action_name,
            cancel_name,
            window: GuiWin::new(term_window),
            pane: MuxPane(pane.pane_id()),
            yes_selected: RefCell::new(false),
        })
    }

    fn finish(&self, term_window: &mut TermWindow, confirmed: bool) {
        term_window.cancel_modal();
        if confirmed {
            crate::overlay::confirm::trampoline(
                self.action_name.clone(),
                self.window.clone(),
                self.pane.clone(),
            );
        } else if let Some(name) = &self.cancel_name {
            crate::overlay::confirm::trampoline(
                name.clone(),
                self.window.clone(),
                self.pane.clone(),
            );
        }
    }

    fn invalidate(&self, term_window: &mut TermWindow) {
        self.element.borrow_mut().take();
        term_window.invalidate_modal();
    }
}

impl Modal for NativeConfirmation {
    fn mouse_event(&self, _event: MouseEvent, _term_window: &mut TermWindow) -> anyhow::Result<()> {
        Ok(())
    }

    fn key_down(
        &self,
        key: KeyCode,
        mods: KeyModifiers,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<bool> {
        match (key, mods) {
            (KeyCode::Char('y'), _) => {
                self.finish(term_window, true);
                return Ok(true);
            }
            (KeyCode::Char('n'), _)
            | (KeyCode::Escape, KeyModifiers::NONE)
            | (KeyCode::Char('g'), KeyModifiers::CTRL) => {
                self.finish(term_window, false);
                return Ok(true);
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                self.finish(term_window, *self.yes_selected.borrow());
                return Ok(true);
            }
            (KeyCode::LeftArrow, KeyModifiers::NONE)
            | (KeyCode::RightArrow, KeyModifiers::NONE) => {
                let selected = *self.yes_selected.borrow();
                *self.yes_selected.borrow_mut() = !selected;
            }
            _ => return Ok(false),
        }
        self.invalidate(term_window);
        Ok(true)
    }

    fn computed_element(
        &self,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<Ref<'_, [ComputedElement]>> {
        if self.element.borrow().is_none() {
            let font = term_window
                .fonts
                .command_palette_font()
                .expect("to resolve native confirmation font");
            let (bg, fg) = panel_colors(term_window);
            let (selected_bg, selected_fg) = selection_colors(term_window);
            let width = self.args.message.chars().count().clamp(40, 72) + 4;
            let wrapped = textwrap::wrap(&self.args.message, width.saturating_sub(4));
            let mut rows = vec![
                text_row(&font, "Confirm", bg.clone(), fg.clone()),
                text_row(&font, "", bg.clone(), fg.clone()),
            ];
            for line in &wrapped {
                rows.push(text_row(&font, line.to_string(), bg.clone(), fg.clone()));
            }
            rows.push(text_row(&font, "", bg.clone(), fg.clone()));

            let yes_selected = *self.yes_selected.borrow();
            let yes = text_row(
                &font,
                "  Yes  ",
                if yes_selected {
                    selected_bg.clone()
                } else {
                    bg.clone()
                },
                if yes_selected {
                    selected_fg.clone()
                } else {
                    fg.clone()
                },
            )
            .display(DisplayType::Inline);
            let no = text_row(
                &font,
                "  No  ",
                if yes_selected {
                    bg.clone()
                } else {
                    selected_bg
                },
                if yes_selected {
                    fg.clone()
                } else {
                    selected_fg
                },
            )
            .display(DisplayType::Inline);
            rows.push(
                Element::new(&font, ElementContent::Children(vec![yes, no]))
                    .colors(element_colors(bg.clone(), fg.clone()))
                    .display(DisplayType::Block),
            );
            rows.push(text_row(&font, "", bg.clone(), fg.clone()));
            rows.push(text_row(
                &font,
                "←→ choose   Enter confirm   Esc close",
                bg.clone(),
                fg.clone(),
            ));
            let height = (wrapped.len() + 6).clamp(7, 14);
            self.element.borrow_mut().replace(compute_centered_panel(
                term_window,
                rows,
                width,
                height,
            )?);
        }
        Ok(Ref::map(self.element.borrow(), |value| {
            value.as_ref().unwrap().as_slice()
        }))
    }

    fn reconfigure(&self, _term_window: &mut TermWindow) {
        self.element.borrow_mut().take();
    }
}
