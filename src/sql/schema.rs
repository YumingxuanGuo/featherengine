
use crate::error::{Error, Result};
use serde_derive::{Deserialize, Serialize};

use super::{types::{DataType, Value}, engine::SqlTxn};

/// The catalog stores schema information
pub trait Catalog {
    /// Creates a new table.
    fn create_table(&mut self, table: Table) -> Result<()>;

    /// Reads a table, if it exists.
    fn read_table(&self, table_name: &str) -> Result<Option<Table>>;

    /// Deletes an existing table, or errors if it does not exist.
    fn delete_table(&mut self, table_name: &str) -> Result<()>;

    /// Iterates over all tables.
    fn scan_tables(&self) -> Result<Tables>;

    /// Creates an index on a table.
    fn create_index(&mut self, table_name: &str, column_name: &str) -> Result<()>;

    /// Reads a table, or errors if it does not exist.
    fn read_table_or_error(&self, table_name: &str) -> Result<Table> {
        self.read_table(table_name)?
            .ok_or_else(|| Error::Value(format!("Table {} does not exist", table_name)))
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
}

impl Table {
    /// Creates a new table schema
    pub fn new(name: String, columns: Vec<Column>) -> Result<Self> {
        Ok(Self { name, columns })
    }

    /// Returns the primary key value of a row
    pub fn get_row_primary_key(&self, row: &[Value]) -> Result<Value> {
        Ok(Value::Null)
    }

    /// Validates the table schema.
    pub fn validate_schema(&self, txn: &mut dyn SqlTxn) -> Result<()> {
        if self.columns.is_empty() {
            return Err(Error::Value(format!("Table {} has no columns", self.name)));
        }
        // Checks if there is exactly one primary key.
        match self.columns.iter().filter(|c| c.is_primary).count() {
            1 => {},
            0 => return Err(Error::Value(format!("No primary key in table {}", self.name))),
            _ => return Err(Error::Value(format!("Multiple primary keys in table {}", self.name))),
        };
        // Checks if each column is valid.
        for column in &self.columns {
            column.validate_schema(self, txn)?;
        }
        Ok(())
    }

    /// Validates a row
    pub fn validate_row(&self, row: &[Value], txn: &mut dyn SqlTxn) -> Result<()> {
        if row.len() != self.columns.len() {
            return Err(Error::Value(format!("Invalid row size for table {}", self.name)));
        }
        let primary_key = self.get_row_primary_key(row)?;
        for (column, value) in self.columns.iter().zip(row.iter()) {
            column.validate_value(self, &primary_key, value, txn)?;
        }
        Ok(())
    }
}

pub type Tables = Box<dyn DoubleEndedIterator<Item = Table> + Send>;

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Column {
    /// Column name
    pub name: String,
    /// Column datatype
    pub datatype: DataType,
    /// Whether the column is the primary key
    pub is_primary: bool,
    /// Whether the column allows null values
    pub is_nullable: bool,
    /// The default value of the column
    pub default: Option<Value>,
    /// Whether the column should only take unique values
    pub is_unique: bool,
    /// The table which is referenced by this foreign key
    pub references: Option<String>,
    /// Whether this column is indexed
    pub is_indexed: bool,
}

impl Column {
    /// Validates the column schema
    pub fn validate_schema(&self, table: &Table, txn: &mut dyn SqlTxn) -> Result<()> {
        Ok(())
    }

    /// Validates a column value
    pub fn validate_value(
        &self,
        table: &Table,
        primary_key: &Value,
        value: &Value,
        txn: &mut dyn SqlTxn,
    ) -> Result<()> {
        Ok(())
    }
}