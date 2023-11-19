//! Transaction wrapper for libmdbx-sys.

use super::cursor::Cursor;
use crate::{
    metrics::{
        Operation, OperationMetrics, TransactionMetrics, TransactionMode, TransactionOutcome,
    },
    table::{Compress, DupSort, Encode, Table, TableImporter},
    tables::{utils::decode_one, Tables, NUM_TABLES},
    transaction::{DbTx, DbTxMut},
    DatabaseError,
};
use parking_lot::RwLock;
use reth_interfaces::db::{DatabaseWriteError, DatabaseWriteOperation};
use reth_libmdbx::{ffi::DBI, Transaction, TransactionKind, WriteFlags, RW};
use std::{marker::PhantomData, str::FromStr, sync::Arc, time::Instant};

/// Wrapper for the libmdbx transaction.
#[derive(Debug)]
pub struct Tx<K: TransactionKind> {
    /// Libmdbx-sys transaction.
    pub inner: Transaction<K>,
    /// Database table handle cache.
    pub(crate) db_handles: Arc<RwLock<[Option<DBI>; NUM_TABLES]>>,
    /// Handler for metrics with its own [Drop] implementation for cases when the transaction isn't
    /// closed by [Tx::commit] or [Tx::abort], but we still need to report it in the metrics.
    ///
    /// If [Some], then metrics are reported.
    metrics_handler: Option<MetricsHandler<K>>,
}

impl<K: TransactionKind> Tx<K> {
    /// Creates new `Tx` object with a `RO` or `RW` transaction.
    pub fn new(inner: Transaction<K>) -> Self {
        Self { inner, db_handles: Default::default(), metrics_handler: None }
    }

    /// Creates new `Tx` object with a `RO` or `RW` transaction and optionally enables metrics.
    pub fn new_with_metrics(inner: Transaction<K>, with_metrics: bool) -> Self {
        let metrics_handler = with_metrics.then(|| {
            let handler = MetricsHandler::<K> {
                txn_id: inner.id(),
                start: Instant::now(),
                close_recorded: false,
                _marker: PhantomData,
            };
            TransactionMetrics::record_open(handler.transaction_mode());
            handler
        });
        Self { inner, db_handles: Default::default(), metrics_handler }
    }

    /// Gets this transaction ID.
    pub fn id(&self) -> u64 {
        self.metrics_handler.as_ref().map_or_else(|| self.inner.id(), |handler| handler.txn_id)
    }

    /// Gets a table database handle if it exists, otherwise creates it.
    pub fn get_dbi<T: Table>(&self) -> Result<DBI, DatabaseError> {
        let mut handles = self.db_handles.write();

        let table = Tables::from_str(T::NAME).expect("Requested table should be part of `Tables`.");

        let dbi_handle = handles.get_mut(table as usize).expect("should exist");
        if dbi_handle.is_none() {
            *dbi_handle = Some(
                self.inner
                    .open_db(Some(T::NAME))
                    .map_err(|e| DatabaseError::InitCursor(e.into()))?
                    .dbi(),
            );
        }

        Ok(dbi_handle.expect("is some; qed"))
    }

    /// Create db Cursor
    pub fn new_cursor<T: Table>(&self) -> Result<Cursor<K, T>, DatabaseError> {
        let inner = self
            .inner
            .cursor_with_dbi(self.get_dbi::<T>()?)
            .map_err(|e| DatabaseError::InitCursor(e.into()))?;

        Ok(Cursor::new_with_metrics(inner, self.metrics_handler.is_some()))
    }

    /// If `self.metrics_handler == Some(_)`, measure the time it takes to execute the closure and
    /// record a metric with the provided transaction outcome.
    ///
    /// Otherwise, just execute the closure.
    fn execute_with_close_transaction_metric<R>(
        mut self,
        outcome: TransactionOutcome,
        f: impl FnOnce(Self) -> R,
    ) -> R {
        if let Some(mut metrics_handler) = self.metrics_handler.take() {
            metrics_handler.close_recorded = true;

            let start = Instant::now();
            let result = f(self);
            let close_duration = start.elapsed();
            let open_duration = metrics_handler.start.elapsed();

            TransactionMetrics::record_close(
                metrics_handler.transaction_mode(),
                outcome,
                open_duration,
                Some(close_duration),
            );

            result
        } else {
            f(self)
        }
    }

    /// If `self.metrics_handler == Some(_)`, measure the time it takes to execute the closure and
    /// record a metric with the provided operation.
    ///
    /// Otherwise, just execute the closure.
    fn execute_with_operation_metric<T: Table, R>(
        &self,
        operation: Operation,
        value_size: Option<usize>,
        f: impl FnOnce(&Transaction<K>) -> R,
    ) -> R {
        if self.metrics_handler.is_some() {
            OperationMetrics::record(T::NAME, operation, value_size, || f(&self.inner))
        } else {
            f(&self.inner)
        }
    }
}

#[derive(Debug)]
struct MetricsHandler<K: TransactionKind> {
    /// Cached internal transaction ID provided by libmdbx.
    txn_id: u64,
    /// The time when transaction has started.
    start: Instant,
    /// If true, the metric about transaction closing has already been recorded and we don't need
    /// to do anything on [Drop::drop].
    close_recorded: bool,
    _marker: PhantomData<K>,
}

impl<K: TransactionKind> MetricsHandler<K> {
    const fn transaction_mode(&self) -> TransactionMode {
        if K::IS_READ_ONLY {
            TransactionMode::ReadOnly
        } else {
            TransactionMode::ReadWrite
        }
    }
}

impl<K: TransactionKind> Drop for MetricsHandler<K> {
    fn drop(&mut self) {
        if !self.close_recorded {
            TransactionMetrics::record_close(
                self.transaction_mode(),
                TransactionOutcome::Drop,
                self.start.elapsed(),
                None,
            );
        }
    }
}

impl TableImporter for Tx<RW> {}

impl<K: TransactionKind> DbTx for Tx<K> {
    type Cursor<T: Table> = Cursor<K, T>;
    type DupCursor<T: DupSort> = Cursor<K, T>;

    fn get<T: Table>(&self, key: T::Key) -> Result<Option<<T as Table>::Value>, DatabaseError> {
        self.execute_with_operation_metric::<T, _>(Operation::Get, None, |tx| {
            tx.get(self.get_dbi::<T>()?, key.encode().as_ref())
                .map_err(|e| DatabaseError::Read(e.into()))?
                .map(decode_one::<T>)
                .transpose()
        })
    }

    fn commit(self) -> Result<bool, DatabaseError> {
        self.execute_with_close_transaction_metric(TransactionOutcome::Commit, |this| {
            this.inner.commit().map_err(|e| DatabaseError::Commit(e.into()))
        })
    }

    fn abort(self) {
        self.execute_with_close_transaction_metric(TransactionOutcome::Abort, |this| {
            drop(this.inner)
        })
    }

    // Iterate over read only values in database.
    fn cursor_read<T: Table>(&self) -> Result<Self::Cursor<T>, DatabaseError> {
        self.new_cursor()
    }

    /// Iterate over read only values in database.
    fn cursor_dup_read<T: DupSort>(&self) -> Result<Self::DupCursor<T>, DatabaseError> {
        self.new_cursor()
    }

    /// Returns number of entries in the table using cheap DB stats invocation.
    fn entries<T: Table>(&self) -> Result<usize, DatabaseError> {
        Ok(self
            .inner
            .db_stat_with_dbi(self.get_dbi::<T>()?)
            .map_err(|e| DatabaseError::Stats(e.into()))?
            .entries())
    }
}

impl DbTxMut for Tx<RW> {
    type CursorMut<T: Table> = Cursor<RW, T>;
    type DupCursorMut<T: DupSort> = Cursor<RW, T>;

    fn put<T: Table>(&self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        let key = key.encode();
        let value = value.compress();
        self.execute_with_operation_metric::<T, _>(
            Operation::Put,
            Some(value.as_ref().len()),
            |tx| {
                tx.put(self.get_dbi::<T>()?, key.as_ref(), value, WriteFlags::UPSERT).map_err(|e| {
                    DatabaseWriteError {
                        code: e.into(),
                        operation: DatabaseWriteOperation::Put,
                        table_name: T::NAME,
                        key: key.into(),
                    }
                    .into()
                })
            },
        )
    }

    fn delete<T: Table>(
        &self,
        key: T::Key,
        value: Option<T::Value>,
    ) -> Result<bool, DatabaseError> {
        let mut data = None;

        let value = value.map(Compress::compress);
        if let Some(value) = &value {
            data = Some(value.as_ref());
        };

        self.execute_with_operation_metric::<T, _>(Operation::Delete, None, |tx| {
            tx.del(self.get_dbi::<T>()?, key.encode(), data)
                .map_err(|e| DatabaseError::Delete(e.into()))
        })
    }

    fn clear<T: Table>(&self) -> Result<(), DatabaseError> {
        self.inner.clear_db(self.get_dbi::<T>()?).map_err(|e| DatabaseError::Delete(e.into()))?;

        Ok(())
    }

    fn cursor_write<T: Table>(&self) -> Result<Self::CursorMut<T>, DatabaseError> {
        self.new_cursor()
    }

    fn cursor_dup_write<T: DupSort>(&self) -> Result<Self::DupCursorMut<T>, DatabaseError> {
        self.new_cursor()
    }
}
