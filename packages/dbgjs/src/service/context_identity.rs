use std::cmp::Ordering;
use std::fmt;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum ContextKind {
    Path,
    Named,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextIdentity {
    pub id: String,
    pub kind: ContextKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathRelation {
    pub distance: u32,
    pub ancestor: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ContextIdentityError {
    EmptyExpression,
    EmptyNamedId,
    InvalidNamedId(char),
    RelativeCwd,
    DriveRelativePath,
}

impl fmt::Display for ContextIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyExpression => write!(formatter, "context expression must not be empty"),
            Self::EmptyNamedId => write!(formatter, "named context ID must not be empty"),
            Self::InvalidNamedId(character) => write!(
                formatter,
                "named context ID contains invalid character {character:?}; use ASCII letters, digits, '.', '_', or '-'"
            ),
            Self::RelativeCwd => write!(formatter, "current working directory must be absolute"),
            Self::DriveRelativePath => {
                write!(
                    formatter,
                    "drive-relative Windows paths such as 'C:foo' are not supported"
                )
            }
        }
    }
}

impl std::error::Error for ContextIdentityError {}

pub fn resolve_context_expression(
    expression: &str,
    cwd: &Path,
) -> Result<ContextIdentity, ContextIdentityError> {
    if let Some(id) = expression.strip_prefix(':') {
        return Ok(ContextIdentity {
            id: normalize_named_id(id)?,
            kind: ContextKind::Named,
        });
    }
    if expression.is_empty() {
        return Err(ContextIdentityError::EmptyExpression);
    }
    Ok(ContextIdentity {
        id: normalize_path(expression, &cwd.to_string_lossy())?,
        kind: ContextKind::Path,
    })
}

pub fn normalize_absolute_path(path: &Path) -> Result<String, ContextIdentityError> {
    normalize_path(&path.to_string_lossy(), "")
}

pub fn path_relation(from: &str, to: &str) -> Option<PathRelation> {
    let from = ParsedPath::parse_absolute(from).ok()?;
    let to = ParsedPath::parse_absolute(to).ok()?;
    if from.root != to.root || from.separator != to.separator {
        return None;
    }
    let common = from
        .components
        .iter()
        .zip(&to.components)
        .take_while(|(left, right)| left == right)
        .count();
    Some(PathRelation {
        distance: (from.components.len() + to.components.len() - common * 2) as u32,
        ancestor: to.components.len() <= from.components.len() && to.components.len() == common,
    })
}

pub fn path_and_parents(path: &str) -> Result<Vec<String>, ContextIdentityError> {
    let mut parsed = ParsedPath::parse_absolute(path)?;
    let mut result = Vec::with_capacity(parsed.components.len() + 1);
    loop {
        result.push(parsed.render());
        if parsed.components.pop().is_none() {
            break;
        }
    }
    Ok(result)
}

pub fn compare_context_paths(cwd: &str, left: &str, right: &str) -> Ordering {
    let left_relation = path_relation(cwd, left);
    let right_relation = path_relation(cwd, right);
    match (left_relation, right_relation) {
        (Some(left_relation), Some(right_relation)) => left_relation
            .distance
            .cmp(&right_relation.distance)
            .then_with(|| right_relation.ancestor.cmp(&left_relation.ancestor))
            .then_with(|| left.cmp(right)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.cmp(right),
    }
}

pub fn synthetic_node_target_id(connection_id: &str) -> String {
    format!("$node-root:{connection_id}")
}

fn normalize_named_id(id: &str) -> Result<String, ContextIdentityError> {
    if id.is_empty() {
        return Err(ContextIdentityError::EmptyNamedId);
    }
    let normalized = id.to_ascii_lowercase();
    if let Some(character) = normalized.chars().find(|character| {
        !character.is_ascii_alphanumeric() && !matches!(character, '.' | '_' | '-')
    }) {
        return Err(ContextIdentityError::InvalidNamedId(character));
    }
    Ok(normalized)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedPath {
    root: String,
    components: Vec<String>,
    separator: char,
}

impl ParsedPath {
    fn parse_absolute(value: &str) -> Result<Self, ContextIdentityError> {
        let value = value.to_lowercase();
        if is_drive_relative(&value) {
            return Err(ContextIdentityError::DriveRelativePath);
        }
        if value.starts_with("\\\\") || value.starts_with("//") {
            let components = split_components(&value[2..], true);
            if components.len() < 2 {
                return Err(ContextIdentityError::RelativeCwd);
            }
            return Ok(Self {
                root: format!("\\\\{}\\{}", components[0], components[1]),
                components: normalize_components(components.into_iter().skip(2)),
                separator: '\\',
            });
        }
        if value.len() >= 3
            && value.as_bytes()[0].is_ascii_alphabetic()
            && value.as_bytes()[1] == b':'
            && matches!(value.as_bytes()[2], b'\\' | b'/')
        {
            return Ok(Self {
                root: value[..2].to_owned(),
                components: normalize_components(split_components(&value[3..], true)),
                separator: '\\',
            });
        }
        if let Some(rest) = value.strip_prefix('/') {
            return Ok(Self {
                root: "/".to_owned(),
                components: normalize_components(split_components(rest, false)),
                separator: '/',
            });
        }
        Err(ContextIdentityError::RelativeCwd)
    }

    fn render(&self) -> String {
        if self.root == "/" {
            if self.components.is_empty() {
                return self.root.clone();
            }
            return format!("/{}", self.components.join("/"));
        }
        if self.root.starts_with("\\\\") {
            if self.components.is_empty() {
                return self.root.clone();
            }
            return format!("{}\\{}", self.root, self.components.join("\\"));
        }
        if self.components.is_empty() {
            return format!("{}\\", self.root);
        }
        format!("{}\\{}", self.root, self.components.join("\\"))
    }
}

fn normalize_path(expression: &str, cwd: &str) -> Result<String, ContextIdentityError> {
    let expression = expression.to_lowercase();
    let expression = expression.as_str();
    if is_drive_relative(expression) {
        return Err(ContextIdentityError::DriveRelativePath);
    }
    let cwd_path = (!cwd.is_empty())
        .then(|| ParsedPath::parse_absolute(cwd))
        .transpose()?;
    if cwd_path.as_ref().is_some_and(|cwd| cwd.separator == '\\')
        && (expression.starts_with('\\') || expression.starts_with('/'))
        && !expression.starts_with("\\\\")
        && !expression.starts_with("//")
    {
        let mut path = cwd_path.expect("checked above");
        path.components = normalize_components(split_components(
            expression.trim_start_matches(['/', '\\']),
            true,
        ));
        return Ok(path.render());
    }
    let mut parsed = match ParsedPath::parse_absolute(expression) {
        Ok(path) => path,
        Err(ContextIdentityError::RelativeCwd) => {
            let mut cwd = cwd_path.ok_or(ContextIdentityError::RelativeCwd)?;
            let windows = cwd.separator == '\\';
            cwd.components.extend(split_components(expression, windows));
            cwd.components = normalize_components(cwd.components);
            cwd
        }
        Err(error) => return Err(error),
    };
    parsed.components = normalize_components(parsed.components);
    Ok(parsed.render())
}

fn is_drive_relative(value: &str) -> bool {
    value.len() >= 2
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value.as_bytes()[1] == b':'
        && (value.len() == 2 || !matches!(value.as_bytes()[2], b'\\' | b'/'))
}

fn split_components(value: &str, windows: bool) -> Vec<String> {
    if windows {
        value
            .split(['/', '\\'])
            .filter(|component| !component.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        value
            .split('/')
            .filter(|component| !component.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

fn normalize_components(components: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for component in components {
        match component.as_str() {
            "." => {}
            ".." => {
                normalized.pop();
            }
            _ => normalized.push(component),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_unix_paths_lexically_without_io() {
        let cwd = Path::new("/Work/Shop/packages");
        assert_eq!(
            resolve_context_expression("../missing/./app", cwd).unwrap(),
            ContextIdentity {
                id: "/work/shop/missing/app".into(),
                kind: ContextKind::Path,
            }
        );
    }

    #[test]
    fn resolves_windows_paths_and_named_ids() {
        let cwd = Path::new("D:\\Src\\Shop");
        assert_eq!(
            resolve_context_expression("frontend\\..\\API", cwd)
                .unwrap()
                .id,
            "d:\\src\\shop\\api"
        );
        assert_eq!(
            resolve_context_expression(":Incident-42", cwd).unwrap(),
            ContextIdentity {
                id: "incident-42".into(),
                kind: ContextKind::Named,
            }
        );
        assert_eq!(
            resolve_context_expression("\\\\Server\\Share\\Folder\\..\\App", cwd)
                .unwrap()
                .id,
            "\\\\server\\share\\app"
        );
        assert_eq!(
            resolve_context_expression("C:relative", cwd).unwrap_err(),
            ContextIdentityError::DriveRelativePath
        );
        assert_eq!(
            resolve_context_expression("\\Root\\App", cwd).unwrap().id,
            "d:\\root\\app"
        );
        assert_eq!(
            resolve_context_expression("/Root/App", cwd).unwrap().id,
            "d:\\root\\app"
        );
    }

    #[test]
    fn computes_segment_distance_and_ancestor_status() {
        assert_eq!(
            path_relation("/work/shop/packages/ui", "/work/shop"),
            Some(PathRelation {
                distance: 2,
                ancestor: true,
            })
        );
        assert_eq!(
            path_relation("/work/shop/packages/ui", "/work/shop/services/api"),
            Some(PathRelation {
                distance: 4,
                ancestor: false,
            })
        );
    }
}
