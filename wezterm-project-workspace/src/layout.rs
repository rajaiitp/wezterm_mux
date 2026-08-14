use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneRole {
    Editor,
    Agent,
    Review,
}

impl PaneRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Editor => "editor",
            Self::Agent => "agent",
            Self::Review => "review",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LayoutNode {
    Pane {
        role: PaneRole,
    },
    Split {
        direction: SplitDirection,
        /// Fraction assigned to the first child. Must be in (0, 1).
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutProfile {
    pub name: String,
    pub layout: LayoutNode,
    pub applications: Vec<ApplicationSpec>,
}

impl LayoutProfile {
    pub fn default_agentic() -> Self {
        Self {
            name: "agentic".to_string(),
            // Provisional profile. The pane topology remains configurable and
            // can be changed without changing workspace identity.
            layout: LayoutNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.65,
                first: Box::new(LayoutNode::Pane {
                    role: PaneRole::Editor,
                }),
                second: Box::new(LayoutNode::Split {
                    direction: SplitDirection::Vertical,
                    ratio: 0.5,
                    first: Box::new(LayoutNode::Pane {
                        role: PaneRole::Agent,
                    }),
                    second: Box::new(LayoutNode::Pane {
                        role: PaneRole::Review,
                    }),
                }),
            },
            applications: vec![
                ApplicationSpec {
                    role: PaneRole::Editor,
                    launch: vec!["nvim".to_string(), ".".to_string()],
                    resume: None,
                    required: true,
                },
                ApplicationSpec {
                    role: PaneRole::Agent,
                    launch: vec!["pi".to_string(), "--continue".to_string()],
                    resume: Some(vec!["pi".to_string(), "--continue".to_string()]),
                    required: true,
                },
                ApplicationSpec {
                    role: PaneRole::Review,
                    launch: vec![
                        "tuicr".to_string(),
                        "--working-tree".to_string(),
                        "--no-update-check".to_string(),
                    ],
                    resume: None,
                    required: true,
                },
            ],
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut layout_roles = BTreeSet::new();
        collect_roles(&self.layout, &mut layout_roles)?;
        let app_roles = self
            .applications
            .iter()
            .map(|app| app.role.as_str())
            .collect::<BTreeSet<_>>();
        if layout_roles != app_roles {
            anyhow::bail!(
                "layout roles {:?} do not match application roles {:?}",
                layout_roles,
                app_roles
            );
        }
        for app in &self.applications {
            if app.launch.is_empty() || app.launch.iter().any(String::is_empty) {
                anyhow::bail!("application {} has an empty launch argv", app.role.as_str());
            }
        }
        Ok(())
    }
}

fn collect_roles(node: &LayoutNode, roles: &mut BTreeSet<&'static str>) -> anyhow::Result<()> {
    match node {
        LayoutNode::Pane { role } => {
            if !roles.insert(role.as_str()) {
                anyhow::bail!("layout contains duplicate {} pane", role.as_str());
            }
        }
        LayoutNode::Split {
            ratio,
            first,
            second,
            ..
        } => {
            if !ratio.is_finite() || !(0.0..1.0).contains(ratio) {
                anyhow::bail!("layout split ratio must be finite and between 0 and 1");
            }
            collect_roles(first, roles)?;
            collect_roles(second, roles)?;
        }
    }
    Ok(())
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_is_valid_and_has_three_roles() {
        let profile = LayoutProfile::default_agentic();
        profile.validate().unwrap();
        assert_eq!(profile.applications.len(), 3);
    }

    #[test]
    fn invalid_ratios_and_duplicate_roles_are_rejected() {
        let mut profile = LayoutProfile::default_agentic();
        if let LayoutNode::Split { ratio, .. } = &mut profile.layout {
            *ratio = 1.0;
        }
        assert!(profile.validate().is_err());

        let mut profile = LayoutProfile::default_agentic();
        profile.applications[2].role = PaneRole::Agent;
        assert!(profile.validate().is_err());
    }
}
