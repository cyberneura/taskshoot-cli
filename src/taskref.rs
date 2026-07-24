use anyhow::{bail, Result};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskRef {
    /// `KEY-<number>` (a tracked task). The number is sent to the API.
    Slug { project_key: String, number: u64 },
    /// UUID (untracked tasks have no number, so they can only be referenced by UUID).
    Uuid(String),
}

impl TaskRef {
    pub fn api_ref(&self) -> String {
        match self {
            TaskRef::Slug { number, .. } => number.to_string(),
            TaskRef::Uuid(uuid) => uuid.clone(),
        }
    }
}

/// `DEV-12` → Slug / a UUID string → Uuid.
/// Task numbers never contain a hyphen, so the trailing `-<digits>` splits
/// unambiguously (the same rule as the frontend's canonical URL convention;
/// a project key containing a hyphen stays unambiguous).
pub fn parse_task_ref(input: &str) -> Result<TaskRef> {
    let input = input.trim();
    if Uuid::parse_str(input).is_ok() {
        return Ok(TaskRef::Uuid(input.to_string()));
    }
    if let Some((key, number)) = input.rsplit_once('-') {
        if !key.is_empty() && !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) {
            return Ok(TaskRef::Slug {
                project_key: key.to_string(),
                number: number.parse()?,
            });
        }
    }
    bail!("invalid task reference '{input}': expected KEY-<number> (e.g. DEV-12) or a task UUID");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_slug() {
        assert_eq!(
            parse_task_ref("DEV-12").unwrap(),
            TaskRef::Slug {
                project_key: "DEV".into(),
                number: 12
            }
        );
    }

    #[test]
    fn parses_hyphenated_project_key() {
        assert_eq!(
            parse_task_ref("MY-PROJ-3").unwrap(),
            TaskRef::Slug {
                project_key: "MY-PROJ".into(),
                number: 3
            }
        );
    }

    #[test]
    fn parses_uuid() {
        let uuid = "0197f9a2-1234-7abc-8def-0123456789ab";
        assert_eq!(parse_task_ref(uuid).unwrap(), TaskRef::Uuid(uuid.into()));
    }

    #[test]
    fn rejects_invalid() {
        assert!(parse_task_ref("DEV").is_err());
        assert!(parse_task_ref("DEV-").is_err());
        assert!(parse_task_ref("-12").is_err());
        assert!(parse_task_ref("DEV-12a").is_err());
        assert!(parse_task_ref("").is_err());
    }

    #[test]
    fn api_ref_forms() {
        assert_eq!(parse_task_ref("DEV-12").unwrap().api_ref(), "12");
        let uuid = "0197f9a2-1234-7abc-8def-0123456789ab";
        assert_eq!(parse_task_ref(uuid).unwrap().api_ref(), uuid);
    }
}
