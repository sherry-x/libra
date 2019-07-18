//! This file defines system store APIs that operates data not part of the Libra core data
//! structures but information with regard to system running status, statistics, etc.

use crate::{ledger_counters::LedgerCounters, schema::ledger_counters::LedgerCountersSchema};
use failure::prelude::*;
use logger::prelude::*;
use schemadb::{SchemaBatch, DB};
use std::sync::Arc;
use types::transaction::Version;

pub(crate) struct SystemStore {
    db: Arc<DB>,
}

impl SystemStore {
    pub fn new(db: Arc<DB>) -> Self {
        Self { db }
    }

    /// Increase ledger counters.
    ///
    /// The base values are read out of db, to which the `diff` is combined to, and the result is
    /// stored to the db, keyed by `last_version`.
    #[allow(dead_code)]
    pub fn inc_ledger_counters(
        &self,
        first_version: Version,
        last_version: Version,
        counters_diff: LedgerCounters,
        batch: &mut SchemaBatch,
    ) -> Result<LedgerCounters> {
        let mut counters = if first_version > 0 {
            let base_version = first_version - 1;
            if let Some(counters) = self.db.get::<LedgerCountersSchema>(&base_version)? {
                counters
            } else {
                crit!(
                    "Base version ({}) ledger counters not found. Assuming zeros.",
                    base_version
                );
                LedgerCounters::new()
            }
        } else {
            LedgerCounters::new()
        };

        counters.combine(counters_diff);
        batch.put::<LedgerCountersSchema>(&last_version, &counters)?;

        Ok(counters)
    }
}

#[cfg(test)]
mod test;
