use std::{collections::HashSet, num::NonZeroU32};

use chilidb_binder::{ColumnSchema, LogicalType, TableSchema, bound::TableConstraint};
use chilidb_storage::PageId;

pub(crate) const MANIFEST: &[u8] = b"CHILICAT\x01\x00";
const TABLE: &[u8] = b"CHILICAT\x01\x01";

pub(crate) struct Metadata {
    pub schema: TableSchema,
    pub head: PageId,
    pub constraints: Vec<TableConstraint>,
}

pub(crate) fn validate(m: &Metadata) -> Result<(), String> {
    if m.schema.name.is_empty() || m.schema.columns.is_empty() {
        return Err("empty table name or column list".into());
    }
    let mut names = HashSet::new();
    for c in &m.schema.columns {
        if c.name.is_empty() || !names.insert(&c.name) {
            return Err("empty or duplicate column name".into());
        }
        if c.data_type == LogicalType::Null {
            return Err("NULL is not a column type".into());
        }
    }
    let mut primary = false;
    for c in &m.constraints {
        let (indices, pk) = match c {
            TableConstraint::PrimaryKey(v) => (v, true),
            TableConstraint::Unique(v) => (v, false),
        };
        if pk && primary {
            return Err("multiple primary keys".into());
        }
        primary |= pk;
        if indices.is_empty() {
            return Err("empty constraint".into());
        }
        let mut seen = HashSet::new();
        for &i in indices {
            let column = m
                .schema
                .columns
                .get(i)
                .ok_or("constraint index out of bounds")?;
            if !seen.insert(i) {
                return Err("repeated constraint index".into());
            }
            if pk && column.nullable {
                return Err("nullable primary key".into());
            }
        }
    }
    Ok(())
}

fn number(out: &mut Vec<u8>, n: usize) -> Result<(), String> {
    out.extend_from_slice(
        &u32::try_from(n)
            .map_err(|_| "metadata count overflow")?
            .to_le_bytes(),
    );
    Ok(())
}
fn string(out: &mut Vec<u8>, s: &str) -> Result<(), String> {
    number(out, s.len())?;
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

pub(crate) fn encode(m: &Metadata) -> Result<Vec<u8>, String> {
    validate(m)?;
    let mut out = TABLE.to_vec();
    out.extend_from_slice(&m.head.to_le_bytes());
    string(&mut out, &m.schema.name)?;
    number(&mut out, m.schema.columns.len())?;
    for c in &m.schema.columns {
        string(&mut out, &c.name)?;
        out.push(u8::from(c.nullable));
        let tag = match c.data_type {
            LogicalType::Boolean => 1,
            LogicalType::Int32 => 2,
            LogicalType::Int64 => 3,
            LogicalType::Uint32 => 4,
            LogicalType::Float32 => 5,
            LogicalType::Float64 => 6,
            LogicalType::Text => 7,
            LogicalType::Varchar(None) => 8,
            LogicalType::Varchar(Some(_)) => 9,
            LogicalType::Null => return Err("NULL is not a column type".into()),
        };
        out.push(tag);
        if let LogicalType::Varchar(Some(n)) = c.data_type {
            out.extend_from_slice(&n.get().to_le_bytes());
        }
    }
    number(&mut out, m.constraints.len())?;
    for c in &m.constraints {
        let (tag, indices) = match c {
            TableConstraint::PrimaryKey(v) => (1, v),
            TableConstraint::Unique(v) => (2, v),
        };
        out.push(tag);
        number(&mut out, indices.len())?;
        for &i in indices {
            number(&mut out, i)?;
        }
    }
    if out.len() > chilidb_heapam::MAX_TUPLE_SIZE {
        return Err("catalog metadata exceeds maximum tuple size".into());
    }
    Ok(out)
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if n > self.0.len() {
            return Err("truncated metadata".into());
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn number(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn count(&mut self) -> Result<usize, String> {
        let n = self.number()? as usize;
        if n > self.0.len() {
            return Err("invalid metadata count".into());
        }
        Ok(n)
    }
    fn string(&mut self) -> Result<String, String> {
        let n = self.count()?;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| "invalid UTF-8".into())
    }
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Metadata, String> {
    let mut r = Reader(bytes);
    if r.take(TABLE.len())? != TABLE {
        return Err("unknown catalog record or version".into());
    }
    let head = r.number()?;
    let name = r.string()?;
    let n = r.count()?;
    let mut columns = Vec::new();
    for _ in 0..n {
        let name = r.string()?;
        let nullable = match r.byte()? {
            0 => false,
            1 => true,
            _ => return Err("invalid nullability".into()),
        };
        let data_type = match r.byte()? {
            1 => LogicalType::Boolean,
            2 => LogicalType::Int32,
            3 => LogicalType::Int64,
            4 => LogicalType::Uint32,
            5 => LogicalType::Float32,
            6 => LogicalType::Float64,
            7 => LogicalType::Text,
            8 => LogicalType::Varchar(None),
            9 => LogicalType::Varchar(Some(
                NonZeroU32::new(r.number()?).ok_or("zero varchar bound")?,
            )),
            _ => return Err("unknown logical type".into()),
        };
        columns.push(ColumnSchema {
            name,
            data_type,
            nullable,
        });
    }
    let n = r.count()?;
    let mut constraints = Vec::new();
    for _ in 0..n {
        let tag = r.byte()?;
        let count = r.count()?;
        let indices = (0..count)
            .map(|_| r.number().map(|n| n as usize))
            .collect::<Result<Vec<_>, _>>()?;
        constraints.push(match tag {
            1 => TableConstraint::PrimaryKey(indices),
            2 => TableConstraint::Unique(indices),
            _ => return Err("unknown constraint".into()),
        });
    }
    if !r.0.is_empty() {
        return Err("trailing metadata bytes".into());
    }
    let m = Metadata {
        schema: TableSchema { name, columns },
        head,
        constraints,
    };
    validate(&m)?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Vec<u8> {
        encode(&Metadata {
            head: 3,
            schema: TableSchema {
                name: "t".into(),
                columns: vec![ColumnSchema {
                    name: "x".into(),
                    data_type: LogicalType::Int32,
                    nullable: false,
                }],
            },
            constraints: vec![TableConstraint::PrimaryKey(vec![0])],
        })
        .unwrap()
    }

    #[test]
    fn rejects_truncation_trailing_bytes_and_unknown_tags() {
        let bytes = record();
        for n in 0..bytes.len() {
            assert!(decode(&bytes[..n]).is_err(), "length {n}");
        }
        let mut bad = bytes.clone();
        bad.push(0);
        assert!(decode(&bad).is_err());
        // Header, root, table string, count, column string, nullability, type.
        for (offset, value) in [
            (8, 2),
            (9, 9),
            (18, 255),
            (28, 255),
            (29, 255),
            (30, 255),
            (34, 255),
            (35, 255),
        ] {
            let mut bad = bytes.clone();
            bad[offset] = value;
            assert!(decode(&bad).is_err(), "offset {offset}");
        }
        let mut bad = bytes;
        bad[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(&bad).unwrap().head, u32::MAX);
    }
}
