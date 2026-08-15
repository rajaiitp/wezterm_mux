use crate::termwindow::{RenderFrame, TermWindowNotif};
use ::window::bitmaps::atlas::OutOfTextureSpace;
use ::window::WindowOps;
use anyhow::Context;
use smol::Timer;
use std::time::{Duration, Instant};
use wezterm_font::ClearShapeCache;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowImage {
    Yes,
    Scale(usize),
    No,
}

impl crate::TermWindow {
    pub fn paint_impl(&mut self, frame: &mut RenderFrame) {
        self.num_frames += 1;
        // If nothing on screen needs animating, then we can avoid
        // invalidating as frequently
        *self.has_animation.borrow_mut() = None;
        // Start with the assumption that we should allow images to render
        self.allow_images = AllowImage::Yes;

        let start = Instant::now();

        {
            let diff = start.duration_since(self.last_fps_check_time);
            if diff > Duration::from_secs(1) {
                let seconds = diff.as_secs_f32();
                self.fps = self.num_frames as f32 / seconds;
                self.num_frames = 0;
                self.last_fps_check_time = start;
            }
        }

        'pass: for pass in 0.. {
            match self.paint_pass() {
                Ok(_) => match self.render_state.as_mut().unwrap().allocated_more_quads() {
                    Ok(allocated) => {
                        if !allocated {
                            break 'pass;
                        }
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();
                    }
                    Err(err) => {
                        log::error!("{:#}", err);
                        break 'pass;
                    }
                },
                Err(err) => {
                    if let Some(&OutOfTextureSpace {
                        size: Some(size),
                        current_size,
                    }) = err.root_cause().downcast_ref::<OutOfTextureSpace>()
                    {
                        let result = if pass == 0 {
                            // Let's try clearing out the atlas and trying again
                            // self.clear_texture_atlas()
                            log::trace!("recreate_texture_atlas");
                            self.recreate_texture_atlas(Some(current_size))
                        } else {
                            log::trace!("grow texture atlas to {}", size);
                            self.recreate_texture_atlas(Some(size))
                        };
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();

                        if let Err(err) = result {
                            self.allow_images = match self.allow_images {
                                AllowImage::Yes => AllowImage::Scale(2),
                                AllowImage::Scale(2) => AllowImage::Scale(4),
                                AllowImage::Scale(4) => AllowImage::Scale(8),
                                AllowImage::Scale(8) => AllowImage::No,
                                AllowImage::No | _ => {
                                    log::error!(
                                        "Failed to {} texture: {}",
                                        if pass == 0 { "clear" } else { "resize" },
                                        err
                                    );
                                    break 'pass;
                                }
                            };

                            log::info!(
                                "Not enough texture space ({:#}); \
                                     will retry render with {:?}",
                                err,
                                self.allow_images,
                            );
                        }
                    } else if err.root_cause().downcast_ref::<ClearShapeCache>().is_some() {
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();
                        self.shape_generation += 1;
                        self.shape_cache.borrow_mut().clear();
                        self.line_to_ele_shape_cache.borrow_mut().clear();
                    } else {
                        log::error!("paint_pass failed: {:#}", err);
                        break 'pass;
                    }
                }
            }
        }
        log::debug!("paint_impl before call_draw elapsed={:?}", start.elapsed());

        self.call_draw(frame).ok();
        self.last_frame_duration = start.elapsed();
        log::debug!(
            "paint_impl elapsed={:?}, fps={}",
            self.last_frame_duration,
            self.fps
        );
        metrics::histogram!("gui.paint.impl").record(self.last_frame_duration);
        metrics::histogram!("gui.paint.impl.rate").record(1.);

        // If self.has_animation is some, then the last render detected
        // image attachments with multiple frames, so we also need to
        // invalidate the viewport when the next frame is due
        if self.focused.is_some() {
            if let Some(next_due) = *self.has_animation.borrow() {
                let prior = self.scheduled_animation.borrow_mut().take();
                match prior {
                    Some(prior) if prior <= next_due => {
                        // Already due before that time
                    }
                    _ => {
                        self.scheduled_animation.borrow_mut().replace(next_due);
                        let window = self.window.clone().take().unwrap();
                        promise::spawn::spawn(async move {
                            Timer::at(next_due).await;
                            let win = window.clone();
                            window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                                tw.scheduled_animation.borrow_mut().take();
                                if tw.tab_bar.next_progress_frame_due().is_some() {
                                    tw.update_title_post_status();
                                }
                                win.invalidate();
                            })));
                        })
                        .detach();
                    }
                }
            }
        }
    }

    pub fn paint_modal(&mut self) -> anyhow::Result<()> {
        if let Some(modal) = self.get_modal() {
            for computed in modal.computed_element(self)?.iter() {
                let mut ui_items = computed.ui_items();

                let gl_state = self.render_state.as_ref().unwrap();
                self.render_element(&computed, gl_state, None)?;

                self.ui_items.append(&mut ui_items);
            }
        }

        Ok(())
    }

    pub fn paint_pass(&mut self) -> anyhow::Result<()> {
        {
            let gl_state = self.render_state.as_ref().unwrap();
            for layer in gl_state.layers.borrow().iter() {
                layer.clear_quad_allocation();
            }
        }

        // Clear out UI item positions; we'll rebuild these as we render
        self.ui_items.clear();

        let panes = self.get_panes_to_render();
        let focused = self.focused.is_some();
        let pane_backgrounds_are_transparent =
            self.config.active_pane_opacity < 1.0 || self.config.inactive_pane_opacity < 1.0;
        let window_is_transparent = !self.window_background.is_empty()
            || self.config.window_background_opacity != 1.0
            || pane_backgrounds_are_transparent;

        let start = Instant::now();
        let gl_state = self.render_state.as_ref().unwrap();
        let layer = gl_state
            .layer_for_zindex(0)
            .context("layer_for_zindex(0)")?;
        let mut layers = layer.quad_allocator();
        log::trace!("quad map elapsed {:?}", start.elapsed());
        metrics::histogram!("quad.map").record(start.elapsed());

        let mut paint_terminal_background = false;

        // Render the full window background
        match (self.window_background.is_empty(), self.allow_images) {
            (false, AllowImage::Yes | AllowImage::Scale(_)) => {
                let bg_color = self.palette().background.to_linear();

                let top = panes
                    .iter()
                    .find(|p| p.is_active)
                    .map(|p| match self.get_viewport(p.pane.pane_id()) {
                        Some(top) => top,
                        None => p.pane.get_dimensions().physical_top,
                    })
                    .unwrap_or(0);

                let loaded_any = self
                    .render_backgrounds(bg_color, top)
                    .context("render_backgrounds")?;

                if !loaded_any {
                    // Either there was a problem loading the background(s)
                    // or they haven't finished loading yet.
                    // Use the regular terminal background until that changes.
                    paint_terminal_background = true;
                }
            }
            _ if window_is_transparent => {
                // Avoid doubling up the background color: the panes
                // will render out through the padding so there
                // should be no gaps that need filling in
            }
            _ => {
                paint_terminal_background = true;
            }
        }

        if paint_terminal_background {
            // Regular window background color
            let background = if panes.len() == 1 {
                // If we're the only pane, use the pane's palette
                // to draw the padding background
                panes[0].pane.palette().background
            } else {
                self.palette().background
            }
            .to_linear()
            .mul_alpha(self.config.window_background_opacity);

            self.filled_rectangle(
                &mut layers,
                0,
                euclid::rect(
                    0.,
                    0.,
                    self.dimensions.pixel_width as f32,
                    self.dimensions.pixel_height as f32,
                ),
                background,
            )
            .context("filled_rectangle for window background")?;
        }

        for pos in panes.iter() {
            let centered_overlay = self.is_centered_overlay_pane(pos.pane.pane_id());
            if centered_overlay {
                let tab_bar_height = if self.show_tab_bar {
                    self.tab_bar_pixel_height()
                        .context("tab_bar_pixel_height")?
                } else {
                    0.
                };
                let terminal_top = if self.config.tab_bar_at_bottom {
                    0.
                } else {
                    tab_bar_height
                };
                self.filled_rectangle(
                    &mut layers,
                    0,
                    euclid::rect(
                        0.,
                        terminal_top,
                        self.dimensions.pixel_width as f32,
                        self.dimensions.pixel_height as f32 - tab_bar_height,
                    ),
                    window::color::LinearRgba::with_components(0., 0., 0., 0.48),
                )
                .context("dim background behind centered overlay")?;
            }

            if pos.is_active {
                self.update_text_cursor(&pos);
                if focused {
                    pos.pane.advise_focus();
                    mux::Mux::get().record_focus_for_current_identity(pos.pane.pane_id());
                }
            }
            if centered_overlay {
                // Keep the popup above all underlying pane layers so its
                // background fully occludes text and images beneath it.
                let popup_layer = self
                    .render_state
                    .as_ref()
                    .expect("render state")
                    .layer_for_zindex(50)
                    .context("allocate centered overlay render layer")?;
                let mut popup_layers = popup_layer.quad_allocator();
                self.paint_pane(&pos, &mut popup_layers)
                    .context("paint centered overlay pane")?;

                let cell_width = self.render_metrics.cell_size.width as f32;
                let cell_height = self.render_metrics.cell_size.height as f32;
                let (padding_left, padding_top) = self.padding_left_top();
                let tab_bar_height = if self.show_tab_bar {
                    self.tab_bar_pixel_height()
                        .context("tab_bar_pixel_height")?
                } else {
                    0.
                };
                let top_bar_height = if self.config.tab_bar_at_bottom {
                    0.
                } else {
                    tab_bar_height
                };
                let os_border = self.get_os_border();
                let x = padding_left + os_border.left.get() as f32 + pos.left as f32 * cell_width
                    - cell_width / 2.;
                let y = top_bar_height
                    + padding_top
                    + os_border.top.get() as f32
                    + pos.top as f32 * cell_height
                    - cell_height / 2.;
                let width = pos.width as f32 * cell_width + cell_width;
                let height = pos.height as f32 * cell_height + cell_height;
                let thickness = 2.;
                let color = self.palette().colors.0[5].to_linear();
                for rect in [
                    euclid::rect(x, y, width, thickness),
                    euclid::rect(x, y + height - thickness, width, thickness),
                    euclid::rect(x, y, thickness, height),
                    euclid::rect(x + width - thickness, y, thickness, height),
                ] {
                    self.filled_rectangle(&mut popup_layers, 0, rect, color)
                        .context("paint centered overlay border")?;
                }
            } else {
                self.paint_pane(&pos, &mut layers).context("paint_pane")?;
            }
        }

        // Draw inactive borders first so the active pane's border wins at
        // shared edges. A single pane always owns the full terminal surface,
        // even while its cached PTY dimensions are catching up.
        let single_pane = panes.len() == 1;
        for pos in panes.iter().filter(|pos| !pos.is_active) {
            self.paint_pane_border(pos, &mut layers, single_pane)
                .context("paint inactive pane border")?;
        }
        for pos in panes.iter().filter(|pos| pos.is_active) {
            self.paint_pane_border(pos, &mut layers, single_pane)
                .context("paint active pane border")?;
        }

        let splits = self.get_splits();
        for split in &splits {
            self.register_split(split).context("register_split")?;
        }

        if self.show_tab_bar {
            self.paint_tab_bar(&mut layers).context("paint_tab_bar")?;
        }

        self.paint_window_borders(&mut layers)
            .context("paint_window_borders")?;
        drop(layers);
        self.paint_modal().context("paint_modal")?;

        Ok(())
    }
}
