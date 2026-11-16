//! Cancelling long-running work: a shared flag the GUI sets and the work
//! checks at safe points (between downloads, between mods).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cancel flag shared between the code that asks and the work that obeys.
/// `Cancel::default()` is never set, for callers that don't cancel.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

/// The error cancelled work returns.
#[derive(Debug, thiserror::Error)]
#[error("cancelled")]
pub struct Cancelled;

impl Cancel {
    /// Asks the work to stop.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// `Err(Cancelled)` once cancelled.
    pub fn check(&self) -> anyhow::Result<()> {
        if self.is_set() {
            Err(Cancelled.into())
        } else {
            Ok(())
        }
    }
}

/// Whether an error (or anything it wraps) is a cancellation.
pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Cancelled>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn flags_and_detects() {
        let c = Cancel::default();
        assert!(c.check().is_ok());
        let other = c.clone();
        other.cancel();
        let e = c.check().context("install X").expect_err("cancelled");
        assert!(is_cancelled(&e));
        assert!(!is_cancelled(&anyhow::anyhow!("other")));
    }
}
