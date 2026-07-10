use anyhow::{bail, Result};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskRef {
    /// `KEY-番号` (tracked タスク)。API へは番号を渡す。
    Slug { project_key: String, number: u64 },
    /// UUID (untracked は number を持たないため UUID でのみ参照できる)。
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

/// `DEV-12` → Slug / UUID 文字列 → Uuid。
/// タスク番号にハイフンは含まれないため、末尾の `-数字` で一意に分解できる
/// (フロントの canonical URL 規約と同じルール。プロジェクトキーにハイフンがあっても曖昧にならない)。
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
