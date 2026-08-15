use crate::spawn::SpawnWhere;
use crate::termwindow::TermWindowNotif;
use config::keyassignment::{SpawnCommand, SpawnTabDomain};
use config::TermConfig;
use mux::Mux;
use std::sync::Arc;
use window::WindowOps;

impl super::TermWindow {
    pub fn spawn_command(&self, spawn: &SpawnCommand, spawn_where: SpawnWhere) {
        let size = if spawn_where == SpawnWhere::NewWindow {
            self.config.initial_size(
                self.dimensions.dpi as u32,
                crate::cell_pixel_dims(&self.config, self.dimensions.dpi as f64).ok(),
            )
        } else {
            self.terminal_size
        };
        let term_config = Arc::new(TermConfig::with_config(self.config.clone()));
        let content_padding = self.pane_content_padding_cells();

        crate::spawn::spawn_command_impl(
            spawn,
            spawn_where,
            size,
            Some(self.mux_window_id),
            term_config,
            Some(content_padding),
        )
    }

    pub fn toggle_command_overlay(&mut self, spawn: &SpawnCommand) {
        let mux = Mux::get();
        let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        let tab_id = tab.tab_id();

        if self.tab_state(tab_id).overlay.is_some() {
            self.cancel_overlay_for_tab(tab_id, None);
            return;
        }

        let Some(source_pane) = self.get_active_pane_no_overlay() else {
            return;
        };
        let source_pane_id = source_pane.pane_id();
        let window_id = self.mux_window_id;
        let size = tab.get_size();
        let term_config = Arc::new(TermConfig::with_config(self.config.clone()));
        let spawn = spawn.clone();
        let window = self.window.clone().expect("GUI window to be available");

        promise::spawn::spawn(async move {
            match crate::spawn::spawn_command_overlay_internal(
                spawn,
                size,
                window_id,
                source_pane_id,
                term_config,
            )
            .await
            {
                Ok(overlay) => {
                    window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                        if Mux::get().get_tab(tab_id).is_some() {
                            term_window.assign_centered_overlay(tab_id, overlay);
                        } else {
                            Mux::get().remove_pane(overlay.pane_id());
                        }
                    })))
                }
                Err(err) => log::error!("Failed to spawn command overlay: {err:#}"),
            }
        })
        .detach();
    }

    pub fn spawn_tab(&mut self, domain: &SpawnTabDomain) {
        self.spawn_command(
            &SpawnCommand {
                domain: domain.clone(),
                ..Default::default()
            },
            SpawnWhere::NewTab,
        );
    }
}
