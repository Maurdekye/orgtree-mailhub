//! Translation between people (v2.4.0): not written yet.

use std::sync::Arc;

use crate::api::Hub;

/// One bounded pass over the due translation jobs.
pub async fn dispatch_due(_hub: &Arc<Hub>) -> anyhow::Result<usize> {
    Ok(0)
}
