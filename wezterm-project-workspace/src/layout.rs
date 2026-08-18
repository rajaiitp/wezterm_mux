use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneRole {
    Editor,
    Agent,
    Review,
    Terminal,
}

impl PaneRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Editor => "editor",
            Self::Agent => "agent",
            Self::Review => "review",
            Self::Terminal => "terminal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationSpec {
    pub role: PaneRole,
    pub launch: Vec<String>,
    #[serde(default)]
    pub resume: Option<Vec<String>>,
    #[serde(default = "default_true")]
    pub required: bool,
}

/// A named layout selected by configuration. The mux owns the actual cell
/// sizing and split tree; this crate only validates the workflow description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutProfile {
    pub name: String,
    pub layout: String,
    pub applications: Vec<ApplicationSpec>,
}

impl LayoutProfile {
    pub fn default_agentic() -> Self {
        Self {
            name: "columns".to_string(),
            layout: "columns".to_string(),
            applications: vec![
                ApplicationSpec {
                    role: PaneRole::Agent,
                    launch: vec!["pi".to_string(), "--continue".to_string()],
                    resume: Some(vec!["pi".to_string(), "--continue".to_string()]),
                    required: true,
                },
                ApplicationSpec {
                    role: PaneRole::Editor,
                    launch: vec!["nvim".to_string(), ".".to_string()],
                    resume: None,
                    required: true,
                },
            ],
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let layout = self.layout.trim().to_ascii_lowercase();
        anyhow::ensure!(
            matches!(
                layout.as_str(),
                "single" | "tabs" | "columns" | "rows" | "three-pane" | "grid"
            ),
            "unknown project layout {layout:?}"
        );
        anyhow::ensure!(!self.applications.is_empty(), "project layout has no modules");
        anyhow::ensure!(
            self.applications.len() <= 4,
            "project layouts support at most four modules"
        );

        let mut app_roles = BTreeSet::new();
        for app in &self.applications {
            anyhow::ensure!(
                app_roles.insert(app.role.as_str()),
                "project layout contains duplicate {} module",
                app.role.as_str()
            );
            anyhow::ensure!(
                !app.launch.is_empty() && app.launch.iter().all(|arg| !arg.is_empty()),
                "application {} has an empty launch argv",
                app.role.as_str()
            );
        }

        if layout == "single" {
            anyhow::ensure!(
                self.applications.len() == 1,
                "single layout requires exactly one module"
            );
        }
        if layout == "three-pane" {
            anyhow::ensure!(
                self.applications.len() <= 3,
                "three-pane layout supports at most three modules"
            );
        }
        Ok(())
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_is_valid() {
        LayoutProfile::default_agentic().validate().unwrap();
    }

    #[test]
    fn validates_named_layouts_without_ratios() {
        let profile = LayoutProfile {
            name: "grid".to_string(),
            layout: "grid".to_string(),
            applications: vec![
                ApplicationSpec {
                    role: PaneRole::Editor,
                    launch: vec!["nvim".to_string()],
                    resume: None,
                    required: true,
                },
                ApplicationSpec {
                    role: PaneRole::Agent,
                    launch: vec!["pi".to_string()],
                    resume: None,
                    required: true,
                },
                ApplicationSpec {
                    role: PaneRole::Review,
                    launch: vec!["tuicr".to_string()],
                    resume: None,
                    required: true,
                },
                ApplicationSpec {
                    role: PaneRole::Terminal,
                    launch: vec!["zsh".to_string()],
                    resume: None,
                    required: true,
                },
            ],
        };
        profile.validate().unwrap();

        let mut tabs = profile.clone();
        tabs.name = "tabs".to_string();
        tabs.layout = "tabs".to_string();
        tabs.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_layout_and_duplicate_modules() {
        let mut profile = LayoutProfile::default_agentic();
        profile.layout = "diagonal".to_string();
        assert!(profile.validate().is_err());

        let mut profile = LayoutProfile::default_agentic();
        profile.applications[1].role = PaneRole::Agent;
        assert!(profile.validate().is_err());
    }
}
