//! Cursor wrapper for libmdbx-sys.

use reth_interfaces::db::DatabaseWriteOperation;
use std::{borrow::Cow, collections::Bound, ops::RangeBounds};

use crate::{
    common::{PairResult, ValueOnlyResult},
    cursor::{
        DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW, DupWalker, RangeWalker,
        ReverseWalker, Walker,
    },
    table::{Compress, DupSort, Encode, Table},
    tables::utils::*,
    DatabaseError,
};
use reth_libmdbx::{self, Error as MDBXError, TransactionKind, WriteFlags, RO, RW};

/// Read only Cursor.
pub type CursorRO<'tx, T> = Cursor<'tx, RO, T>;
/// Read write cursor.
pub type CursorRW<'tx, T> = Cursor<'tx, RW, T>;

/// Cursor wrapper to access KV items.
#[derive(Debug)]
pub struct Cursor<'tx, K: TransactionKind, T: Table> {
    /// Inner `libmdbx` cursor.
    pub inner: reth_libmdbx::Cursor<'tx, K>,
    /// Table name as is inside the database.
    pub table: &'static str,
    /// Phantom data to enforce encoding/decoding.
    pub _dbi: std::marker::PhantomData<T>,
    /// Cache buffer that receives compressed values.
    pub buf: Vec<u8>,
}

/// Takes `(key, value)` from the database and decodes it appropriately.
#[macro_export]
macro_rules! decode {
    ($v:expr) => {
        $v.map_err(|e| $crate::DatabaseError::Read(e.into()))?.map(decoder::<T>).transpose()
    };
}

/// Some types don't support compression (eg. B256), and we don't want to be copying them to the
/// allocated buffer when we can just use their reference.
macro_rules! compress_or_ref {
    ($self:expr, $value:expr) => {
        if let Some(value) = $value.uncompressable_ref() {
            value
        } else {
            $self.buf.truncate(0);
            $value.compress_to_buf(&mut $self.buf);
            $self.buf.as_ref()
        }
    };
}

impl<K: TransactionKind, T: Table> DbCursorRO<T> for Cursor<'_, K, T> {
    fn first(&mut self) -> PairResult<T> {
        decode!(self.inner.first())
    }

    fn seek_exact(&mut self, key: <T as Table>::Key) -> PairResult<T> {
        decode!(self.inner.set_key(key.encode().as_ref()))
    }

    fn seek(&mut self, key: <T as Table>::Key) -> PairResult<T> {
        decode!(self.inner.set_range(key.encode().as_ref()))
    }

    fn next(&mut self) -> PairResult<T> {
        decode!(self.inner.next())
    }

    fn prev(&mut self) -> PairResult<T> {
        decode!(self.inner.prev())
    }

    fn last(&mut self) -> PairResult<T> {
        decode!(self.inner.last())
    }

    fn current(&mut self) -> PairResult<T> {
        decode!(self.inner.get_current())
    }

    fn walk(&mut self, start_key: Option<T::Key>) -> Result<Walker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        let start = if let Some(start_key) = start_key {
            self.inner
                .set_range(start_key.encode().as_ref())
                .map_err(|e| DatabaseError::Read(e.into()))?
                .map(decoder::<T>)
        } else {
            self.first().transpose()
        };

        Ok(Walker::new(self, start))
    }

    fn walk_range(
        &mut self,
        range: impl RangeBounds<T::Key>,
    ) -> Result<RangeWalker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        let start = match range.start_bound().cloned() {
            Bound::Included(key) => self.inner.set_range(key.encode().as_ref()),
            Bound::Excluded(_key) => {
                unreachable!("Rust doesn't allow for Bound::Excluded in starting bounds");
            }
            Bound::Unbounded => self.inner.first(),
        }
        .map_err(|e| DatabaseError::Read(e.into()))?
        .map(decoder::<T>);

        Ok(RangeWalker::new(self, start, range.end_bound().cloned()))
    }

    fn walk_back(
        &mut self,
        start_key: Option<T::Key>,
    ) -> Result<ReverseWalker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        let start = if let Some(start_key) = start_key {
            decode!(self.inner.set_range(start_key.encode().as_ref()))
        } else {
            self.last()
        }
        .transpose();

        Ok(ReverseWalker::new(self, start))
    }
}

impl<K: TransactionKind, T: DupSort> DbDupCursorRO<T> for Cursor<'_, K, T> {
    /// Returns the next `(key, value)` pair of a DUPSORT table.
    fn next_dup(&mut self) -> PairResult<T> {
        decode!(self.inner.next_dup())
    }

    /// Returns the next `(key, value)` pair skipping the duplicates.
    fn next_no_dup(&mut self) -> PairResult<T> {
        decode!(self.inner.next_nodup())
    }

    /// Returns the next `value` of a duplicate `key`.
    fn next_dup_val(&mut self) -> ValueOnlyResult<T> {
        self.inner
            .next_dup()
            .map_err(|e| DatabaseError::Read(e.into()))?
            .map(decode_value::<T>)
            .transpose()
    }

    fn seek_by_key_subkey(
        &mut self,
        key: <T as Table>::Key,
        subkey: <T as DupSort>::SubKey,
    ) -> ValueOnlyResult<T> {
        self.inner
            .get_both_range(key.encode().as_ref(), subkey.encode().as_ref())
            .map_err(|e| DatabaseError::Read(e.into()))?
            .map(decode_one::<T>)
            .transpose()
    }

    /// Depending on its arguments, returns an iterator starting at:
    /// - Some(key), Some(subkey): a `key` item whose data is >= than `subkey`
    /// - Some(key), None: first item of a specified `key`
    /// - None, Some(subkey): like first case, but in the first key
    /// - None, None: first item in the table
    /// of a DUPSORT table.
    fn walk_dup(
        &mut self,
        key: Option<T::Key>,
        subkey: Option<T::SubKey>,
    ) -> Result<DupWalker<'_, T, Self>, DatabaseError> {
        let start = match (key, subkey) {
            (Some(key), Some(subkey)) => {
                // encode key and decode it after.
                let key = key.encode().as_ref().to_vec();

                self.inner
                    .get_both_range(key.as_ref(), subkey.encode().as_ref())
                    .map_err(|e| DatabaseError::Read(e.into()))?
                    .map(|val| decoder::<T>((Cow::Owned(key), val)))
            }
            (Some(key), None) => {
                let key = key.encode().as_ref().to_vec();

                self.inner
                    .set(key.as_ref())
                    .map_err(|e| DatabaseError::Read(e.into()))?
                    .map(|val| decoder::<T>((Cow::Owned(key), val)))
            }
            (None, Some(subkey)) => {
                if let Some((key, _)) = self.first()? {
                    let key = key.encode().as_ref().to_vec();

                    self.inner
                        .get_both_range(key.as_ref(), subkey.encode().as_ref())
                        .map_err(|e| DatabaseError::Read(e.into()))?
                        .map(|val| decoder::<T>((Cow::Owned(key), val)))
                } else {
                    let err_code = MDBXError::to_err_code(&MDBXError::NotFound);
                    Some(Err(DatabaseError::Read(err_code)))
                }
            }
            (None, None) => self.first().transpose(),
        };

        Ok(DupWalker::<'_, T, Self> { cursor: self, start })
    }
}

impl<T: Table> DbCursorRW<T> for Cursor<'_, RW, T> {
    /// Database operation that will update an existing row if a specified value already
    /// exists in a table, and insert a new row if the specified value doesn't already exist
    ///
    /// For a DUPSORT table, `upsert` will not actually update-or-insert. If the key already exists,
    /// it will append the value to the subkey, even if the subkeys are the same. So if you want
    /// to properly upsert, you'll need to `seek_exact` & `delete_current` if the key+subkey was
    /// found, before calling `upsert`.
    fn upsert(&mut self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        let key = key.encode();
        // Default `WriteFlags` is UPSERT
        self.inner.put(key.as_ref(), compress_or_ref!(self, value), WriteFlags::UPSERT).map_err(
            |e| DatabaseError::Write {
                code: e.into(),
                operation: DatabaseWriteOperation::CursorUpsert,
                table_name: T::NAME,
                key: Box::from(key.as_ref()),
            },
        )
    }

    fn insert(&mut self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        let key = key.encode();
        self.inner
            .put(key.as_ref(), compress_or_ref!(self, value), WriteFlags::NO_OVERWRITE)
            .map_err(|e| DatabaseError::Write {
                code: e.into(),
                operation: DatabaseWriteOperation::CursorInsert,
                table_name: T::NAME,
                key: Box::from(key.as_ref()),
            })
    }

    /// Appends the data to the end of the table. Consequently, the append operation
    /// will fail if the inserted key is less than the last table key
    fn append(&mut self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        let key = key.encode();
        self.inner.put(key.as_ref(), compress_or_ref!(self, value), WriteFlags::APPEND).map_err(
            |e| DatabaseError::Write {
                code: e.into(),
                operation: DatabaseWriteOperation::CursorAppend,
                table_name: T::NAME,
                key: Box::from(key.as_ref()),
            },
        )
    }

    fn delete_current(&mut self) -> Result<(), DatabaseError> {
        self.inner.del(WriteFlags::CURRENT).map_err(|e| DatabaseError::Delete(e.into()))
    }
}

impl<T: DupSort> DbDupCursorRW<T> for Cursor<'_, RW, T> {
    fn delete_current_duplicates(&mut self) -> Result<(), DatabaseError> {
        self.inner.del(WriteFlags::NO_DUP_DATA).map_err(|e| DatabaseError::Delete(e.into()))
    }

    fn append_dup(&mut self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        let key = key.encode();
        self.inner.put(key.as_ref(), compress_or_ref!(self, value), WriteFlags::APPEND_DUP).map_err(
            |e| DatabaseError::Write {
                code: e.into(),
                operation: DatabaseWriteOperation::CursorAppendDup,
                table_name: T::NAME,
                key: Box::from(key.as_ref()),
            },
        )
    }
}
