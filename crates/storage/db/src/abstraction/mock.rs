//! Mock database
use std::{collections::BTreeMap, ops::RangeBounds};

use crate::{
    common::{PairResult, ValueOnlyResult},
    cursor::{
        DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW, DupWalker, RangeWalker,
        ReverseWalker, Walker,
    },
    database::{Database, DatabaseGAT},
    table::{DupSort, Table, TableImporter},
    transaction::{DbTx, DbTxGAT, DbTxMut, DbTxMutGAT},
    DatabaseError,
};

/// Mock database used for testing with inner BTreeMap structure
/// TODO
#[derive(Clone, Debug, Default)]
pub struct DatabaseMock {
    /// Main data. TODO (Make it table aware)
    pub data: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl Database for DatabaseMock {
    fn tx(&self) -> Result<<Self as DatabaseGAT<'_>>::TX, DatabaseError> {
        Ok(TxMock::default())
    }

    fn tx_mut(&self) -> Result<<Self as DatabaseGAT<'_>>::TXMut, DatabaseError> {
        Ok(TxMock::default())
    }
}

impl<'a> DatabaseGAT<'a> for DatabaseMock {
    type TX = TxMock;

    type TXMut = TxMock;
}

/// Mock read only tx
#[derive(Debug, Clone, Default)]
pub struct TxMock {
    /// Table representation
    _table: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl<'a> DbTxGAT<'a> for TxMock {
    type Cursor<T: Table> = CursorMock;
    type DupCursor<T: DupSort> = CursorMock;
}

impl<'a> DbTxMutGAT<'a> for TxMock {
    type CursorMut<T: Table> = CursorMock;
    type DupCursorMut<T: DupSort> = CursorMock;
}

impl DbTx for TxMock {
    fn get<T: Table>(&self, _key: T::Key) -> Result<Option<T::Value>, DatabaseError> {
        todo!()
    }

    fn commit(self) -> Result<bool, DatabaseError> {
        Ok(true)
    }

    fn abort(self) {}

    fn cursor_read<T: Table>(&self) -> Result<<Self as DbTxGAT<'_>>::Cursor<T>, DatabaseError> {
        Ok(CursorMock { _cursor: 0 })
    }

    fn cursor_dup_read<T: DupSort>(
        &self,
    ) -> Result<<Self as DbTxGAT<'_>>::DupCursor<T>, DatabaseError> {
        Ok(CursorMock { _cursor: 0 })
    }

    fn entries<T: Table>(&self) -> Result<usize, DatabaseError> {
        Ok(self._table.len())
    }
}

impl DbTxMut for TxMock {
    fn put<T: Table>(&self, _key: T::Key, _value: T::Value) -> Result<(), DatabaseError> {
        todo!()
    }

    fn delete<T: Table>(
        &self,
        _key: T::Key,
        _value: Option<T::Value>,
    ) -> Result<bool, DatabaseError> {
        todo!()
    }

    fn clear<T: Table>(&self) -> Result<(), DatabaseError> {
        todo!()
    }

    fn cursor_write<T: Table>(
        &self,
    ) -> Result<<Self as DbTxMutGAT<'_>>::CursorMut<T>, DatabaseError> {
        todo!()
    }

    fn cursor_dup_write<T: DupSort>(
        &self,
    ) -> Result<<Self as DbTxMutGAT<'_>>::DupCursorMut<T>, DatabaseError> {
        todo!()
    }
}

impl TableImporter for TxMock {}

/// Cursor that iterates over table
#[derive(Debug)]
pub struct CursorMock {
    _cursor: u32,
}

impl<T: Table> DbCursorRO<T> for CursorMock {
    fn first(&mut self) -> PairResult<T> {
        todo!()
    }

    fn seek_exact(&mut self, _key: T::Key) -> PairResult<T> {
        todo!()
    }

    fn seek(&mut self, _key: T::Key) -> PairResult<T> {
        todo!()
    }

    fn next(&mut self) -> PairResult<T> {
        todo!()
    }

    fn prev(&mut self) -> PairResult<T> {
        todo!()
    }

    fn last(&mut self) -> PairResult<T> {
        todo!()
    }

    fn current(&mut self) -> PairResult<T> {
        todo!()
    }

    fn walk(&mut self, _start_key: Option<T::Key>) -> Result<Walker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        todo!()
    }

    fn walk_range(
        &mut self,
        _range: impl RangeBounds<T::Key>,
    ) -> Result<RangeWalker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        todo!()
    }

    fn walk_back(
        &mut self,
        _start_key: Option<T::Key>,
    ) -> Result<ReverseWalker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        todo!()
    }
}

impl<T: DupSort> DbDupCursorRO<T> for CursorMock {
    fn next_dup(&mut self) -> PairResult<T> {
        todo!()
    }

    fn next_no_dup(&mut self) -> PairResult<T> {
        todo!()
    }

    fn next_dup_val(&mut self) -> ValueOnlyResult<T> {
        todo!()
    }

    fn seek_by_key_subkey(
        &mut self,
        _key: <T as Table>::Key,
        _subkey: <T as DupSort>::SubKey,
    ) -> ValueOnlyResult<T> {
        todo!()
    }

    fn walk_dup(
        &mut self,
        _key: Option<<T>::Key>,
        _subkey: Option<<T as DupSort>::SubKey>,
    ) -> Result<DupWalker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        todo!()
    }
}

impl<T: Table> DbCursorRW<T> for CursorMock {
    fn upsert(
        &mut self,
        _key: <T as Table>::Key,
        _value: <T as Table>::Value,
    ) -> Result<(), DatabaseError> {
        todo!()
    }

    fn insert(
        &mut self,
        _key: <T as Table>::Key,
        _value: <T as Table>::Value,
    ) -> Result<(), DatabaseError> {
        todo!()
    }

    fn append(
        &mut self,
        _key: <T as Table>::Key,
        _value: <T as Table>::Value,
    ) -> Result<(), DatabaseError> {
        todo!()
    }

    fn delete_current(&mut self) -> Result<(), DatabaseError> {
        todo!()
    }
}

impl<T: DupSort> DbDupCursorRW<T> for CursorMock {
    fn delete_current_duplicates(&mut self) -> Result<(), DatabaseError> {
        todo!()
    }

    fn append_dup(&mut self, _key: <T>::Key, _value: <T>::Value) -> Result<(), DatabaseError> {
        todo!()
    }
}
