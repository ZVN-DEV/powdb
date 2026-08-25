//! Typed errors for the sync crate.
//!
//! The public API keeps returning `io::Result` (embedded hosts and the
//! server thread sync failures through I/O plumbing), but every refusal this
//! crate raises now carries a [`SyncError`] as the `io::Error`'s source, so a
//! host can BRANCH on the refusal instead of substring-matching the rendered
//! message — the same boundary contract `powdb-storage` ships. Display is
//! byte-identical to the historical strings and the `io::ErrorKind` of every
//! site is unchanged, so nothing on the wire or in logs moves.
//!
//! The semantic variants cover the resume/repair protocol, the refusals an
//! embedded replica host must act on (resume from the recovered boundary,
//! repair, or rebootstrap). The two catch-alls carry the long tail; promote a
//! message to its own variant when a caller needs to branch on it.

use std::io;

/// A refusal raised by the sync crate. See the module docs for the contract.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// The replica's sync identity does not descend from the history it was
    /// asked to apply. Resuming is impossible; the host must rebootstrap.
    #[error("{0}")]
    IdentityMismatch(String),
    /// A stranded in-progress apply intent covers a different range than the
    /// caller's, and the catalog LSN does not prove it void. Resume exactly
    /// from the recovered catalog boundary, or repair.
    #[error("{0}")]
    ApplyInProgress(String),
    /// The local apply state and the catalog disagree in a way no crash of a
    /// well-behaved apply produces. Manual repair required before retry.
    #[error("{0}")]
    ApplyStateRequiresRepair(String),
    /// The requested chunk start is not a boundary the local apply state
    /// vouches for. Re-derive the resume point from the catalog LSN.
    #[error("{0}")]
    UntrustedApplyBoundary(String),
    /// Catch-all for refusals of the caller's request
    /// (`io::ErrorKind::InvalidInput`).
    #[error("{0}")]
    InvalidRequest(String),
    /// Catch-all for on-disk or protocol state this crate refuses to trust
    /// (`io::ErrorKind::InvalidData`).
    #[error("{0}")]
    CorruptState(String),
}

impl SyncError {
    /// The `io::ErrorKind` each variant has always surfaced as.
    fn kind(&self) -> io::ErrorKind {
        match self {
            SyncError::IdentityMismatch(_) | SyncError::InvalidRequest(_) => {
                io::ErrorKind::InvalidInput
            }
            SyncError::ApplyInProgress(_)
            | SyncError::ApplyStateRequiresRepair(_)
            | SyncError::UntrustedApplyBoundary(_)
            | SyncError::CorruptState(_) => io::ErrorKind::InvalidData,
        }
    }
}

/// Source-preserving: the rendered text is unchanged (io::Error's Display
/// delegates to the payload) and the typed error stays recoverable via
/// `err.get_ref().and_then(|e| e.downcast_ref::<SyncError>())`.
impl From<SyncError> for io::Error {
    fn from(err: SyncError) -> io::Error {
        io::Error::new(err.kind(), err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_variants() -> Vec<SyncError> {
        vec![
            SyncError::IdentityMismatch("m".into()),
            SyncError::ApplyInProgress("m".into()),
            SyncError::ApplyStateRequiresRepair("m".into()),
            SyncError::UntrustedApplyBoundary("m".into()),
            SyncError::InvalidRequest("m".into()),
            SyncError::CorruptState("m".into()),
        ]
    }

    #[test]
    fn conversion_preserves_kind_and_text_and_type() {
        for variant in all_variants() {
            let kind = variant.kind();
            let text = variant.to_string();
            let io_err: io::Error = variant.into();
            assert_eq!(io_err.kind(), kind);
            assert_eq!(io_err.to_string(), text, "Display must not change");
            assert!(
                io_err
                    .get_ref()
                    .and_then(|e| e.downcast_ref::<SyncError>())
                    .is_some(),
                "the typed error must survive the io::Error boundary"
            );
        }
    }

    #[test]
    fn kinds_match_the_historical_sites() {
        use io::ErrorKind::{InvalidData, InvalidInput};
        let expect = [
            (SyncError::IdentityMismatch("m".into()), InvalidInput),
            (SyncError::ApplyInProgress("m".into()), InvalidData),
            (SyncError::ApplyStateRequiresRepair("m".into()), InvalidData),
            (SyncError::UntrustedApplyBoundary("m".into()), InvalidData),
            (SyncError::InvalidRequest("m".into()), InvalidInput),
            (SyncError::CorruptState("m".into()), InvalidData),
        ];
        for (variant, kind) in expect {
            let io_err: io::Error = variant.into();
            assert_eq!(io_err.kind(), kind);
        }
    }
}
