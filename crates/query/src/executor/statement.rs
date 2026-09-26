//! The outer query boundary owns transaction completion, including fast paths.
//! Nested source queries (for example view materialization) share the boundary.

use super::Engine;
use crate::plan::PlanNode;
use crate::result::{QueryError, QueryResult};
use std::sync::atomic::Ordering;

/// Test-only rendezvous points around an implicit statement commit.
#[cfg(feature = "testing")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementCommitPhase {
    /// Row application finished, but no statement commit was written yet.
    BeforeCommit,
    /// The inline commit returned; a caller using deferred durability still
    /// has to settle its ticket before acknowledging the result.
    AfterCommit,
}

impl Engine {
    /// Install a test-only hook for deterministic process-crash validation.
    /// This API and its callback branches do not exist in shipping builds.
    #[cfg(feature = "testing")]
    pub fn set_statement_commit_hook_for_testing(
        &mut self,
        hook: Option<fn(StatementCommitPhase)>,
    ) {
        self.statement_commit_hook = hook;
    }

    pub(super) fn ensure_usable(&self) -> Result<(), QueryError> {
        if self.poisoned || self.catalog.is_sync_poisoned() {
            return Err(QueryError::EnginePoisoned);
        }
        Ok(())
    }

    pub(super) fn ensure_read_allowed(&self) -> Result<(), QueryError> {
        self.ensure_usable()?;
        if self.transaction_aborted.load(Ordering::Relaxed) {
            return Err(QueryError::TransactionAborted);
        }
        Ok(())
    }

    pub(super) fn ensure_plan_allowed(&self, plan: &PlanNode) -> Result<(), QueryError> {
        self.ensure_usable()?;
        if self.transaction_aborted.load(Ordering::Relaxed) && !matches!(plan, PlanNode::Rollback) {
            return Err(QueryError::TransactionAborted);
        }
        Ok(())
    }

    pub(super) fn begin_mutation_statement(&mut self) -> Result<(), QueryError> {
        self.ensure_read_allowed()?;
        if self.read_only {
            return Err(QueryError::ReadonlyMode);
        }
        if !self.in_transaction {
            if let Err(error) = self.catalog.begin_statement_transaction() {
                if self.catalog.is_sync_poisoned() {
                    self.poison_live_state();
                }
                return Err(QueryError::from_storage_io(error));
            }
            self.in_transaction = true;
            self.implicit_transaction = true;
        }
        Ok(())
    }

    pub(super) fn begin_plan_mutation(&mut self, plan: &PlanNode) -> Result<(), QueryError> {
        if let PlanNode::Insert { table, .. }
        | PlanNode::Update { table, .. }
        | PlanNode::Delete { table, .. }
        | PlanNode::Upsert { table, .. } = plan
        {
            self.begin_mutation_statement()?;
            // Persist a conservative dirty flag before any base row changes.
            // A failed statement may leave the flag dirty, never falsely clean.
            self.view_registry
                .mark_dependents_dirty(table)
                .map_err(QueryError::from_storage_io)?;
        }
        Ok(())
    }

    pub(super) fn poison_live_state(&mut self) {
        self.poisoned = true;
        self.catalog.abandon_untrusted_state();
    }

    /// Mark the current explicit transaction aborted after a rejected request.
    /// Server callers must invoke this only for the connection owning the
    /// transaction gate, never for another connection's admission/auth errors.
    pub fn mark_transaction_aborted(&self) {
        if self.in_transaction && !self.implicit_transaction {
            self.transaction_aborted.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn run_read_statement(
        &self,
        execute: impl FnOnce(&Self) -> Result<QueryResult, QueryError>,
    ) -> Result<QueryResult, QueryError> {
        self.ensure_read_allowed()?;
        let result = execute(self);
        if result.is_err() && !matches!(result, Err(QueryError::ReadonlyNeedsWrite)) {
            self.mark_transaction_aborted();
        }
        result
    }

    pub(super) fn run_statement(
        &mut self,
        execute: impl FnOnce(&mut Self) -> Result<QueryResult, QueryError>,
    ) -> Result<QueryResult, QueryError> {
        self.ensure_usable()?;
        if self.statement_depth != 0 {
            return execute(self);
        }
        self.statement_depth = 1;
        let result = execute(self);
        self.statement_depth = 0;
        if self.read_only {
            return result;
        }

        if self.implicit_transaction {
            self.implicit_transaction = false;
            return match result {
                Ok(result) => {
                    #[cfg(feature = "testing")]
                    if let Some(hook) = self.statement_commit_hook {
                        hook(StatementCommitPhase::BeforeCommit);
                    }
                    if let Err(error) = self.catalog.commit_transaction() {
                        tracing::error!(%error, "statement commit outcome is uncertain");
                        self.poison_live_state();
                        return Err(QueryError::CommitOutcomeUnknown);
                    }
                    self.in_transaction = false;
                    if let Err(error) = self.commit_statement() {
                        tracing::error!(%error, "statement durability completion failed");
                        self.poison_live_state();
                        return Err(QueryError::CommitOutcomeUnknown);
                    }
                    #[cfg(feature = "testing")]
                    if let Some(hook) = self.statement_commit_hook {
                        hook(StatementCommitPhase::AfterCommit);
                    }
                    Ok(result)
                }
                Err(error) => {
                    if let Err(rollback_error) = self.rollback_transaction_preserving_wal_archive()
                    {
                        tracing::error!(%rollback_error, "failed statement could not be rolled back");
                        self.poison_live_state();
                        return Err(QueryError::EnginePoisoned);
                    }
                    Err(error)
                }
            };
        }

        match result {
            Ok(result) => {
                if let Err(error) = self.commit_statement() {
                    tracing::error!(%error, "statement durability completion failed");
                    self.poison_live_state();
                    return Err(QueryError::CommitOutcomeUnknown);
                }
                Ok(result)
            }
            Err(error) => {
                self.mark_transaction_aborted();
                Err(error)
            }
        }
    }

    pub(super) fn commit_explicit_transaction(&mut self) -> Result<(), QueryError> {
        self.ensure_read_allowed()?;
        if let Err(error) = self.catalog.commit_transaction() {
            tracing::error!(%error, "explicit transaction commit outcome is uncertain");
            self.poison_live_state();
            return Err(QueryError::CommitOutcomeUnknown);
        }
        self.in_transaction = false;
        self.transaction_aborted.store(false, Ordering::Relaxed);
        Ok(())
    }
}
