use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationProgress {
    pub phase: String,
    pub completed: usize,
    pub total: usize,
    pub current_item: Option<String>,
}

#[derive(Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken").finish_non_exhaustive()
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

pub fn ensure_not_cancelled(cancellation: Option<&CancellationToken>) -> crate::Result<()> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(crate::CheckPoError::Cancelled);
    }
    Ok(())
}

pub fn report_operation_progress(
    progress: Option<&dyn Fn(OperationProgress)>,
    phase: impl Into<String>,
    completed: usize,
    total: usize,
    current_item: Option<String>,
) {
    if let Some(progress) = progress {
        progress(OperationProgress {
            phase: phase.into(),
            completed,
            total,
            current_item,
        });
    }
}
