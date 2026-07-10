use anyhow::{anyhow, bail, Result};

use crate::models::{Stage, Workflow};

/// タスクが従う workflow のステージ集合を解決する。
/// project workflows API の先頭は id=null の「デフォルト (未割り当て)」合成エントリで、
/// タスクの workflow.id が null の場合はそれに一致する。
pub fn stages_for_workflow<'a>(
    workflows: &'a [Workflow],
    task_workflow_id: &Option<String>,
) -> Result<&'a [Stage]> {
    if let Some(flow) = workflows.iter().find(|w| w.id == *task_workflow_id) {
        return Ok(&flow.stages);
    }
    if let Some(flow) = workflows.iter().find(|w| w.id.is_none()) {
        return Ok(&flow.stages);
    }
    bail!("no workflow stages available for this project");
}

/// 数値ならそのまま (サーバー側で workflow 妥当性を検証)、
/// それ以外は active なステージのラベル完全一致で値を解決する。
pub fn resolve_status(input: &str, stages: &[Stage]) -> Result<i64> {
    let input = input.trim();
    if let Ok(value) = input.parse::<i64>() {
        return Ok(value);
    }
    if let Some(stage) = stages.iter().find(|s| s.active && s.label == input) {
        return Ok(stage.value);
    }
    bail!(
        "unknown status '{}'. available: {}",
        input,
        stage_labels(stages)
    );
}

/// 終端ステージ (status をこの値に変えると検収/完了フェーズへ自動遷移する)。
pub fn terminal_stage(stages: &[Stage]) -> Result<&Stage> {
    stages
        .iter()
        .filter(|s| s.active)
        .find(|s| s.is_terminal)
        .ok_or_else(|| {
            anyhow!(
                "no terminal stage in workflow. available: {}",
                stage_labels(stages)
            )
        })
}

/// claim の遷移先: --status 指定 > 「対応中」ラベル > 値 40 > エラー。
pub fn claim_stage(stages: &[Stage], override_status: Option<&str>) -> Result<i64> {
    if let Some(status) = override_status {
        return resolve_status(status, stages);
    }
    if let Some(stage) = stages.iter().find(|s| s.active && s.label == "対応中") {
        return Ok(stage.value);
    }
    if let Some(stage) = stages.iter().find(|s| s.active && s.value == 40) {
        return Ok(stage.value);
    }
    bail!(
        "cannot determine the in-progress stage; specify --status. available: {}",
        stage_labels(stages)
    );
}

pub fn stage_labels(stages: &[Stage]) -> String {
    stages
        .iter()
        .filter(|s| s.active)
        .map(|s| format!("{}={}", s.value, s.label))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(value: i64, label: &str, is_initial: bool, is_terminal: bool) -> Stage {
        Stage {
            value,
            label: label.into(),
            is_initial,
            is_terminal,
            active: true,
        }
    }

    fn default_flow_stages() -> Vec<Stage> {
        vec![
            stage(10, "起案", true, false),
            stage(20, "見積もり中", false, false),
            stage(30, "アサイン済み", false, false),
            stage(40, "対応中", false, false),
            stage(50, "対応保留中", false, false),
            stage(60, "確認待ち", false, false),
            stage(70, "完了", false, true),
        ]
    }

    fn workflow(id: Option<&str>, is_default: bool, stages: Vec<Stage>) -> Workflow {
        Workflow {
            id: id.map(str::to_string),
            name: "flow".into(),
            active: true,
            is_default,
            stages,
        }
    }

    #[test]
    fn resolve_status_by_label_and_number() {
        let stages = default_flow_stages();
        assert_eq!(resolve_status("対応中", &stages).unwrap(), 40);
        assert_eq!(resolve_status("70", &stages).unwrap(), 70);
        assert!(resolve_status("存在しない", &stages).is_err());
    }

    #[test]
    fn resolve_status_ignores_inactive_labels() {
        let mut stages = default_flow_stages();
        stages[3].active = false;
        assert!(resolve_status("対応中", &stages).is_err());
    }

    #[test]
    fn terminal_stage_finds_last() {
        let stages = default_flow_stages();
        assert_eq!(terminal_stage(&stages).unwrap().value, 70);
        let no_terminal = vec![stage(10, "起案", true, false)];
        assert!(terminal_stage(&no_terminal).is_err());
    }

    #[test]
    fn claim_prefers_override_then_label_then_40() {
        let stages = default_flow_stages();
        assert_eq!(claim_stage(&stages, Some("確認待ち")).unwrap(), 60);
        assert_eq!(claim_stage(&stages, None).unwrap(), 40);
        // 「対応中」ラベルが無くても値 40 があればそれを使う
        let renamed: Vec<Stage> = stages
            .iter()
            .cloned()
            .map(|mut s| {
                if s.value == 40 {
                    s.label = "作業中".into();
                }
                s
            })
            .collect();
        assert_eq!(claim_stage(&renamed, None).unwrap(), 40);
        // どちらも無ければエラー
        let custom = vec![stage(1, "todo", true, false), stage(2, "done", false, true)];
        assert!(claim_stage(&custom, None).is_err());
    }

    #[test]
    fn stages_for_workflow_matches_id_and_falls_back() {
        let flows = vec![
            workflow(None, false, default_flow_stages()),
            workflow(
                Some("wf-1"),
                true,
                vec![stage(1, "todo", true, false), stage(2, "done", false, true)],
            ),
        ];
        let by_id = stages_for_workflow(&flows, &Some("wf-1".into())).unwrap();
        assert_eq!(by_id.len(), 2);
        let null_flow = stages_for_workflow(&flows, &None).unwrap();
        assert_eq!(null_flow.len(), 7);
        // 未知の id はデフォルト (id=null) にフォールバック
        let unknown = stages_for_workflow(&flows, &Some("missing".into())).unwrap();
        assert_eq!(unknown.len(), 7);
    }

}
