use config::keyassignment::InputSelectorEntry;
use mux::window::WindowId;
use mux::Mux;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};
use termwiz::cell::{AttributeChange, Intensity};
use termwiz_funcs::{format_as_escapes, FormatColor, FormatItem};
use wezterm_project_workspace::{default_registry_path, Registry, WorkspaceId};

pub const DEFAULT_WORKSPACE: &str = "default";
pub const WORKSPACE_PICKER_EVENT: &str = "__wezterm_workspace_picker";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct LastTab {
    #[serde(default)]
    tab_id: Option<usize>,
    #[serde(default)]
    tab_index: Option<usize>,
    #[serde(default)]
    tab_title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceOrderFile {
    version: u32,
    workspaces: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceTabsFile {
    version: u32,
    workspaces: HashMap<String, LastTab>,
}

pub struct WorkspaceManager {
    order_path: PathBuf,
    tabs_path: PathBuf,
    last_location_path: PathBuf,
    order: Vec<String>,
    last_tabs: HashMap<String, LastTab>,
    previous: Option<String>,
    // Native restore repopulates workspaces asynchronously. Keep the saved
    // order intact during that short bootstrap window instead of pruning
    // names that have not arrived yet.
    preserve_saved_order_until: Instant,
    registry_cache: Option<(PathBuf, Option<SystemTime>, Registry)>,
}

pub fn ordered_workspace_names() -> Vec<String> {
    WorkspaceManager::new().display_names()
}

pub fn select_workspace(entry: Option<InputSelectorEntry>) {
    let Some(workspace) = entry.and_then(|entry| entry.id) else {
        return;
    };
    if workspace_is_live(&workspace) {
        crate::frontend::front_end().switch_workspace(&workspace, false);
    }
}

impl WorkspaceManager {
    pub fn new() -> Self {
        let home = dirs_next::home_dir().expect("could not determine home directory");
        let share = home.join(".local/share/wezterm");
        let order_path = share.join("workspace_order.json");
        let tabs_path = share.join("workspace_last_tabs.json");
        let last_location_path = share.join("last_location.json");

        Self {
            order: load_order(&order_path),
            last_tabs: load_tabs(&tabs_path),
            last_location_path: last_location_path.clone(),
            order_path,
            tabs_path,
            previous: load_previous_workspace(&last_location_path),
            preserve_saved_order_until: Instant::now() + Duration::from_secs(5),
            registry_cache: None,
        }
    }

    pub fn display_names(&mut self) -> Vec<String> {
        self.sync_order();
        let live: HashSet<String> = Mux::get()
            .iter_workspaces()
            .into_iter()
            .filter(|name| name != DEFAULT_WORKSPACE)
            .collect();
        self.order
            .iter()
            .filter(|name| live.contains(name.as_str()))
            .cloned()
            .collect()
    }

    pub fn remember_workspace(&mut self, name: &str) {
        if name.is_empty() || name == DEFAULT_WORKSPACE || self.order.iter().any(|n| n == name) {
            return;
        }
        self.order.push(name.to_string());
        self.write_order();
    }

    #[allow(dead_code)]
    pub fn forget_workspace(&mut self, name: &str) {
        self.order.retain(|existing| existing != name);
        self.last_tabs.remove(name);
        self.write_order();
        self.write_tabs();
    }

    pub fn rename_workspace(&mut self, old_name: &str, new_name: &str) {
        for existing in &mut self.order {
            if existing == old_name {
                *existing = new_name.to_string();
            }
        }
        if let Some(tab) = self.last_tabs.remove(old_name) {
            self.last_tabs.insert(new_name.to_string(), tab);
        }
        self.write_order();
        self.write_tabs();
    }

    pub fn note_switch(&mut self, old_name: &str, new_name: &str) {
        if old_name != new_name {
            self.previous = Some(old_name.to_string());
        }
        if new_name != DEFAULT_WORKSPACE {
            self.remember_workspace(new_name);
        }
        self.write_last_location(new_name);
    }

    pub fn previous_workspace(&mut self, current: &str) -> Option<String> {
        self.sync_order();
        if let Some(previous) = self.previous.as_ref() {
            if previous != current && workspace_is_live(previous) {
                return Some(previous.clone());
            }
        }

        let live: HashSet<String> = Mux::get()
            .iter_workspaces()
            .into_iter()
            .filter(|name| name != DEFAULT_WORKSPACE)
            .collect();
        let mut names = vec![DEFAULT_WORKSPACE.to_string()];
        names.extend(
            self.order
                .iter()
                .filter(|name| live.contains(name.as_str()))
                .cloned(),
        );
        if names.len() < 2 {
            return None;
        }
        let index = names.iter().position(|name| name == current)?;
        Some(names[(index + names.len() - 1) % names.len()].clone())
    }

    pub fn remember_active_tab(&mut self, window_id: WindowId, workspace: &str) {
        let mux = Mux::get();
        let Some(window) = mux.get_window(window_id) else {
            return;
        };
        let Some(tab) = window.get_active() else {
            return;
        };
        let remembered = LastTab {
            tab_id: Some(tab.tab_id()),
            tab_index: Some(window.get_active_idx()),
            tab_title: Some(tab.get_title()),
        };
        if self.last_tabs.get(workspace) == Some(&remembered) {
            return;
        }
        self.last_tabs.insert(workspace.to_string(), remembered);
        self.write_tabs();
    }

    pub fn restore_active_tab(&self, window_id: WindowId, workspace: &str) {
        let Some(remembered) = self.last_tabs.get(workspace) else {
            return;
        };
        let mux = Mux::get();
        let Some(mut window) = mux.get_window_mut(window_id) else {
            return;
        };
        // Prefer the stable mux tab id while the persistent mux is alive.
        // After a full mux restart the id may change, so fall back to the
        // title and finally the saved position.
        let target = remembered
            .tab_id
            .and_then(|tab_id| window.iter().position(|tab| tab.tab_id() == tab_id))
            .or_else(|| {
                remembered
                    .tab_title
                    .as_deref()
                    .filter(|title| !title.is_empty())
                    .and_then(|title| window.iter().position(|tab| tab.get_title() == title))
            })
            .or_else(|| remembered.tab_index.filter(|index| *index < window.len()));
        if let Some(index) = target {
            if index < window.len() {
                window.set_active_without_saving(index);
            }
        }
    }

    pub fn status(&mut self, active: &str) -> (String, Vec<String>) {
        let active = if active.is_empty() {
            DEFAULT_WORKSPACE
        } else {
            active
        };
        let display_name = self.display_name(active).to_uppercase();
        let names = vec![active.to_string()];
        log::trace!(
            "workspace manager active={} display_name={} ordered={:?} previous={:?}",
            active,
            display_name,
            self.display_names(),
            self.previous,
        );
        let items = vec![
            FormatItem::Background(FormatColor::Color("#1d2021".to_string())),
            FormatItem::Text(" ".to_string()),
            FormatItem::Background(FormatColor::Color("#83a598".to_string())),
            FormatItem::Foreground(FormatColor::Color("#1d2021".to_string())),
            FormatItem::Attribute(AttributeChange::Intensity(Intensity::Bold)),
            FormatItem::Text(format!("  {display_name}  ")),
            FormatItem::Attribute(AttributeChange::Intensity(Intensity::Normal)),
            FormatItem::Background(FormatColor::Color("#1d2021".to_string())),
            FormatItem::Text(" ".to_string()),
        ];

        let status = format_as_escapes(items).unwrap_or_default();
        (status, names)
    }

    fn display_name(&mut self, active: &str) -> String {
        let label = self
            .workspace_registry()
            .and_then(|registry| registry.workspaces.get(&WorkspaceId(active.to_string())))
            .map(|workspace| workspace.label.trim())
            .filter(|label| !label.is_empty());
        label
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| active.to_string())
    }

    fn workspace_registry(&mut self) -> Option<&Registry> {
        let path = default_registry_path().ok()?;
        let modified = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok();
        let cache_is_current = self
            .registry_cache
            .as_ref()
            .map(|(cached_path, cached_modified, _)| {
                cached_path == &path && *cached_modified == modified
            })
            .unwrap_or(false);
        if !cache_is_current {
            let registry = Registry::load(&path).unwrap_or_default();
            self.registry_cache = Some((path, modified, registry));
        }
        self.registry_cache
            .as_ref()
            .map(|(_, _, registry)| registry)
    }

    pub fn picker_names(&mut self, active: &str) -> Vec<String> {
        picker_names_from(self.display_names(), active, workspace_is_live(active))
    }

    fn sync_order(&mut self) {
        let live: Vec<String> = Mux::get()
            .iter_workspaces()
            .into_iter()
            .filter(|name| name != DEFAULT_WORKSPACE)
            .collect();
        let live_set: HashSet<&str> = live.iter().map(String::as_str).collect();
        let restoring = Instant::now() < self.preserve_saved_order_until;
        let mut next = Vec::new();
        let mut seen = HashSet::new();

        for name in &self.order {
            if (restoring || live_set.contains(name.as_str())) && seen.insert(name.clone()) {
                next.push(name.clone());
            }
        }
        for name in live {
            if seen.insert(name.clone()) {
                next.push(name);
            }
        }

        if next != self.order {
            self.order = next;
            self.write_order();
        }
    }

    fn write_order(&self) {
        let file = WorkspaceOrderFile {
            version: 1,
            workspaces: self.order.clone(),
        };
        write_json(&self.order_path, &file);
    }

    fn write_tabs(&self) {
        let file = WorkspaceTabsFile {
            version: 1,
            workspaces: self.last_tabs.clone(),
        };
        write_json(&self.tabs_path, &file);
    }

    fn write_last_location(&self, workspace: &str) {
        // Lua stores tab/pane metadata in this shared file. Merge only the
        // workspace navigation fields so native status updates cannot erase
        // the data used to restore the active tab after a restart.
        let mut file = fs::read_to_string(&self.last_location_path)
            .ok()
            .and_then(|contents| serde_json::from_str::<Value>(&contents).ok())
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        file.insert("version".to_string(), Value::from(3));
        file.insert("workspace".to_string(), Value::from(workspace.to_string()));
        if let Some(previous) = &self.previous {
            file.insert(
                "previous_workspace".to_string(),
                Value::from(previous.clone()),
            );
        }
        write_json(&self.last_location_path, &Value::Object(file));
    }
}

fn picker_names_from(mut names: Vec<String>, active: &str, active_is_live: bool) -> Vec<String> {
    if !names.iter().any(|name| name == DEFAULT_WORKSPACE) {
        names.insert(0, DEFAULT_WORKSPACE.to_string());
    }
    if !names.iter().any(|name| name == active) && active != DEFAULT_WORKSPACE && active_is_live {
        names.push(active.to_string());
    }
    names
}

fn workspace_is_live(name: &str) -> bool {
    Mux::get()
        .iter_workspaces()
        .into_iter()
        .any(|live| live == name)
}

fn load_previous_workspace(path: &PathBuf) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    let value = serde_json::from_str::<Value>(&contents).ok()?;
    let workspace = value.get("previous_workspace")?.as_str()?;
    if workspace.is_empty() || workspace == DEFAULT_WORKSPACE {
        None
    } else {
        Some(workspace.to_string())
    }
}

fn load_order(path: &PathBuf) -> Vec<String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&contents) else {
        return Vec::new();
    };
    let value = value.get("workspaces").unwrap_or(&value);
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|name| !name.is_empty() && *name != DEFAULT_WORKSPACE)
        .map(str::to_string)
        .collect()
}

fn load_tabs(path: &PathBuf) -> HashMap<String, LastTab> {
    let Ok(contents) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&contents) else {
        return HashMap::new();
    };
    let value = value.get("workspaces").unwrap_or(&value);
    serde_json::from_value(value.clone()).unwrap_or_default()
}

fn write_json<T: Serialize>(path: &PathBuf, value: &T) {
    let Some(parent) = path.parent() else { return };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(encoded) = serde_json::to_string(value) else {
        return;
    };
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    if fs::write(&temporary, encoded).is_ok() {
        let _ = fs::rename(temporary, path);
    }
}

#[cfg(test)]
mod tests {
    use super::{picker_names_from, DEFAULT_WORKSPACE};

    #[test]
    fn picker_includes_default_workspace() {
        assert_eq!(
            picker_names_from(vec!["project".to_string()], DEFAULT_WORKSPACE, false),
            vec!["default", "project"]
        );
    }

    #[test]
    fn picker_adds_an_unordered_live_workspace() {
        assert_eq!(
            picker_names_from(vec![DEFAULT_WORKSPACE.to_string()], "live", true),
            vec!["default", "live"]
        );
    }
}
