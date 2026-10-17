use super::agent_loop::{AgentLoop, CancellationReason};
use crate::{
    durable::EventPayload,
    error::HarnessError,
    executor::Executor,
    model::ModelProvider,
    runtime::{ToolCallId, TurnId},
};
use tokio_util::sync::CancellationToken;

impl<P, E, S> AgentLoop<P, E, S>
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: crate::durable::EventStore + 'static,
{
    /// Applies a durable approval decision and, when approved, executes the
    /// pending tool before resuming model sampling for the same turn.
    ///
    /// 事件序列: `tool.approval.responded` → (approved) 执行工具 →
    /// `tool.completed` → 继续采样。拒绝则追加 `turn.cancelled` 并以
    /// [`HarnessError::Policy`] 结束 turn。
    pub async fn approve_tool(
        &mut self,
        turn: TurnId,
        call_id: ToolCallId,
        approved: bool,
        cancel: CancellationToken,
    ) -> Result<(TurnId, String), HarnessError> {
        let active = self
            .state
            .active_turn
            .as_ref()
            .filter(|active| active.turn_id == turn)
            .ok_or_else(|| {
                HarnessError::InvariantViolation("requested turn is not active".into())
            })?;
        if !matches!(
            active.approvals.get(&call_id),
            Some(crate::runtime::ApprovalStatus::Pending)
        ) {
            return Err(HarnessError::InvariantViolation(
                "tool does not have a pending approval".into(),
            ));
        }
        let tool_name =
            active.tool_names.get(&call_id).cloned().ok_or_else(|| {
                HarnessError::InvariantViolation("missing approved tool name".into())
            })?;
        let input = active.tool_inputs.get(&call_id).cloned().ok_or_else(|| {
            HarnessError::InvariantViolation("missing approved tool input".into())
        })?;
        self.append(
            Some(turn),
            EventPayload::ToolApprovalResponded { call_id, approved },
        )
        .await?;
        if !approved {
            self.append(Some(turn), EventPayload::TurnCancelled).await?;
            return Err(HarnessError::Policy(
                format!("approval denied for tool `{}`", tool_name.0).into(),
            ));
        }
        self.run_approved_tool(turn, call_id, input, cancel, CancellationReason::Cancelled)
            .await?;
        self.continue_turn(turn, CancellationToken::new()).await
    }
}
