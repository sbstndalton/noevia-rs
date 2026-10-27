//! The frozen managed-store layout plus the proposed receipt table. Validation
//! only: this module never creates, migrates, repairs, or writes a schema.
use crate::Fault;
use rusqlite::{Connection, OptionalExtension};

const MAX_DDL_BYTES: usize = 64 * 1024;
#[derive(Clone, Copy)]
struct Column {
    name: &'static str,
    kind: &'static str,
    required: bool,
    primary: bool,
    default: Option<i64>,
}
const fn c(
    name: &'static str,
    kind: &'static str,
    required: bool,
    primary: bool,
    default: Option<i64>,
) -> Column {
    Column {
        name,
        kind,
        required,
        primary,
        default,
    }
}
const TABLES: &[(&str, &[Column])] = &[
    (
        "files",
        &[
            c("path", "TEXT", false, true, None),
            c("data", "BLOB", true, false, None),
            c("version", "TEXT", true, false, None),
            c("updated", "REAL", true, false, None),
        ],
    ),
    ("directories", &[c("path", "TEXT", false, true, None)]),
    (
        "workspace_imports",
        &[
            c("fingerprint", "TEXT", false, true, None),
            c("destination", "TEXT", true, false, None),
        ],
    ),
    (
        "workspace_index_outbox",
        &[c("path", "TEXT", false, true, None)],
    ),
    (
        "meta",
        &[
            c("key", "TEXT", false, true, None),
            c("value", "TEXT", true, false, None),
        ],
    ),
    (
        "backups",
        &[
            c("destination", "TEXT", false, true, None),
            c("generation", "INTEGER", true, false, Some(-1)),
            c("last_success", "REAL", false, false, None),
            c("retry_at", "REAL", true, false, Some(0)),
            c("failures", "INTEGER", true, false, Some(0)),
            c("error", "TEXT", false, false, None),
        ],
    ),
    (
        "workspace_operation_receipts",
        &[
            c("id", "TEXT", false, true, None),
            c("fingerprint", "TEXT", true, false, None),
            c("op", "TEXT", true, false, None),
            c("response", "BLOB", true, false, None),
            c("created", "REAL", true, false, None),
        ],
    ),
];

pub(crate) fn validate(db: &Connection) -> Result<(), Fault> {
    // Extra tables can introduce FK side effects; triggers/views or additional
    // constraint indexes are not part of this explicitly supported layout.
    let extra: bool = db.query_row(concat!(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type IN ('trigger','view') OR ",
        "(type='table' AND name NOT GLOB 'sqlite_*' AND lower(name) NOT IN ",
        "('files','directories','workspace_imports','workspace_index_outbox','meta','backups','workspace_operation_receipts')) LIMIT 1)"),[],|r|r.get(0))?;
    if extra {
        return Err(Fault::Schema);
    }
    for (table, expected) in TABLES {
        let Some((Some(kind), Some(sql))) = definition(db, table)? else {
            return Err(Fault::Schema);
        };
        if kind != "table" || sql.len() > MAX_DDL_BYTES {
            return Err(Fault::Schema);
        }
        Ddl::new(&sql)?.table(table, expected)?;
        let mut info = db.prepare(
            "SELECT name,type,\"notnull\",dflt_value,pk,hidden FROM pragma_table_xinfo(?)",
        )?;
        let mut rows = info.query([table])?;
        for column in *expected {
            let row = rows.next()?.ok_or(Fault::Schema)?;
            let name: String = row.get(0)?;
            let kind: String = row.get(1)?;
            let required: bool = row.get(2)?;
            let default: Option<String> = row.get(3)?;
            let primary: i64 = row.get(4)?;
            let hidden: i64 = row.get(5)?;
            let default = default.map(|d| Ddl::new(&d)?.integer_only()).transpose()?;
            if !name.eq_ignore_ascii_case(column.name)
                || !kind.eq_ignore_ascii_case(column.kind)
                || required != column.required
                || primary != i64::from(column.primary)
                || hidden != 0
                || default != column.default
            {
                return Err(Fault::Schema);
            }
        }
        if rows.next()?.is_some() {
            return Err(Fault::Schema);
        }
        let mut indexes = index_metadata(db, table)?.into_iter();
        let index = indexes.next().ok_or(Fault::Schema)?;
        let name = index.name.ok_or(Fault::Schema)?;
        if !index.unique || index.origin != "pk" || index.partial || indexes.next().is_some() {
            return Err(Fault::Schema);
        }
        let mut keys =
            db.prepare("SELECT name,coll FROM pragma_index_xinfo(?) WHERE key=1 LIMIT 2")?;
        let mut key_rows = keys.query([name])?;
        let row = key_rows.next()?.ok_or(Fault::Schema)?;
        let name: String = row.get(0)?;
        let collation: String = row.get(1)?;
        let primary = expected.iter().find(|c| c.primary).ok_or(Fault::Internal)?;
        if !name.eq_ignore_ascii_case(primary.name)
            || !collation.eq_ignore_ascii_case("BINARY")
            || key_rows.next()?.is_some()
        {
            return Err(Fault::Schema);
        }
        let foreign: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_list(?) LIMIT 1)",
            [table],
            |r| r.get(0),
        )?;
        if foreign {
            return Err(Fault::Schema);
        }
    }
    let invalid_meta:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE typeof(key)!='text' OR typeof(value)!='text' LIMIT 1)",[],|r|r.get(0))?;
    if invalid_meta {
        return Err(Fault::Schema);
    }
    Ok(())
}

type Definition = (Option<String>, Option<String>);
fn definition(db: &Connection, table: &str) -> Result<Option<Definition>, Fault> {
    Ok(db.query_row("SELECT CASE WHEN typeof(type)='text' AND length(CAST(type AS BLOB))<=16 THEN type END,CASE WHEN typeof(sql)='text' AND length(CAST(sql AS BLOB))<=65536 THEN sql END FROM sqlite_schema WHERE name=? COLLATE NOCASE",[table],|r|Ok((r.get(0)?,r.get(1)?))).optional()?)
}
struct IndexMetadata {
    name: Option<String>,
    unique: bool,
    origin: String,
    partial: bool,
}
fn index_metadata(db: &Connection, table: &str) -> Result<Vec<IndexMetadata>, Fault> {
    let mut query=db.prepare("SELECT CASE WHEN typeof(name)='text' AND length(CAST(name AS BLOB))<=128 THEN name END,\"unique\",origin,partial FROM pragma_index_list(?) LIMIT 2")?;
    let rows = query
        .query_map([table], |r| {
            Ok(IndexMetadata {
                name: r.get(0)?,
                unique: r.get(1)?,
                origin: r.get(2)?,
                partial: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Name(String),
    Literal(String),
    Number(String),
    Symbol(char),
}
fn quoted(
    input: &mut std::iter::Peekable<std::str::Chars<'_>>,
    end: char,
    doubled: bool,
) -> Result<String, Fault> {
    let mut out = String::new();
    loop {
        let c = input.next().ok_or(Fault::Schema)?;
        if c == end {
            if doubled && input.peek() == Some(&end) {
                input.next();
                out.push(end);
            } else {
                return Ok(out);
            }
        } else {
            out.push(c);
        }
    }
}
fn tokens(sql: &str) -> Result<Vec<Token>, Fault> {
    if sql.len() > MAX_DDL_BYTES {
        return Err(Fault::Schema);
    }
    let mut chars = sql.chars().peekable();
    let mut out = Vec::new();
    while let Some(c) = chars.next() {
        if c.is_ascii_whitespace() {
            continue;
        }
        match c {
            '-' if chars.peek() == Some(&'-') => {
                chars.next();
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                loop {
                    let c = chars.next().ok_or(Fault::Schema)?;
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
            }
            '"' | '`' => out.push(Token::Name(
                quoted(&mut chars, c, true)?.to_ascii_lowercase(),
            )),
            '[' => out.push(Token::Name(
                quoted(&mut chars, ']', false)?.to_ascii_lowercase(),
            )),
            '\'' => out.push(Token::Literal(quoted(&mut chars, '\'', true)?)),
            '(' | ')' | ',' | ';' | '.' | '+' | '-' => out.push(Token::Symbol(c)),
            c if c.is_ascii_digit() => {
                let mut n = String::from(c);
                while chars.peek().is_some_and(char::is_ascii_digit) {
                    n.push(chars.next().ok_or(Fault::Schema)?);
                }
                out.push(Token::Number(n));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut n = String::from(c);
                while chars
                    .peek()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
                {
                    n.push(chars.next().ok_or(Fault::Schema)?);
                }
                out.push(Token::Name(n.to_ascii_lowercase()));
            }
            _ => return Err(Fault::Schema),
        }
    }
    Ok(out)
}
struct Ddl {
    tokens: Vec<Token>,
    at: usize,
}
impl Ddl {
    fn new(sql: &str) -> Result<Self, Fault> {
        Ok(Self {
            tokens: tokens(sql)?,
            at: 0,
        })
    }
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }
    fn pop(&mut self) -> Result<Token, Fault> {
        let value = self.peek().cloned().ok_or(Fault::Schema)?;
        self.at += 1;
        Ok(value)
    }
    fn eat_name(&mut self, name: &str) -> bool {
        if self.peek() == Some(&Token::Name(name.into())) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn name(&mut self, name: &str) -> Result<(), Fault> {
        if self.eat_name(name) {
            Ok(())
        } else {
            Err(Fault::Schema)
        }
    }
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(&Token::Symbol(c)) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn symbol(&mut self, c: char) -> Result<(), Fault> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(Fault::Schema)
        }
    }
    fn end(&self) -> Result<(), Fault> {
        if self.peek().is_none() {
            Ok(())
        } else {
            Err(Fault::Schema)
        }
    }
    fn integer(&mut self) -> Result<i64, Fault> {
        let mut wrappers = 0;
        while self.eat('(') {
            wrappers += 1;
            if wrappers > 16 {
                return Err(Fault::Schema);
            }
        }
        let sign = if self.eat('-') {
            "-"
        } else {
            self.eat('+');
            ""
        };
        let text = match self.pop()? {
            Token::Number(n) | Token::Literal(n) => n,
            _ => return Err(Fault::Schema),
        };
        let value = format!("{sign}{text}")
            .parse::<i64>()
            .map_err(|_| Fault::Schema)?;
        for _ in 0..wrappers {
            self.symbol(')')?;
        }
        Ok(value)
    }
    fn integer_only(mut self) -> Result<i64, Fault> {
        let n = self.integer()?;
        self.end()?;
        Ok(n)
    }
    fn column(&mut self, column: &Column) -> Result<bool, Fault> {
        self.name(column.name)?;
        self.name(&column.kind.to_ascii_lowercase())?;
        let mut required = false;
        let mut null_seen = false;
        let mut primary = false;
        let mut default = None;
        let mut collated = false;
        while !matches!(self.peek(), Some(Token::Symbol(',' | ')'))) {
            if self.eat_name("not") {
                if null_seen {
                    return Err(Fault::Schema);
                }
                self.name("null")?;
                required = true;
                null_seen = true;
            } else if self.eat_name("null") {
                if null_seen {
                    return Err(Fault::Schema);
                }
                null_seen = true;
            } else if self.eat_name("primary") {
                if primary || !column.primary {
                    return Err(Fault::Schema);
                }
                self.name("key")?;
                primary = true;
                if !self.eat_name("asc") {
                    self.eat_name("desc");
                }
            } else if self.eat_name("collate") {
                if collated {
                    return Err(Fault::Schema);
                }
                self.name("binary")?;
                collated = true;
            } else if self.eat_name("default") {
                if default.is_some() {
                    return Err(Fault::Schema);
                }
                default = Some(self.integer()?);
            } else {
                return Err(Fault::Schema);
            }
        }
        if required != column.required || default != column.default {
            return Err(Fault::Schema);
        }
        Ok(primary)
    }
    fn table(mut self, table: &str, columns: &[Column]) -> Result<(), Fault> {
        self.name("create")?;
        self.name("table")?;
        if self.eat_name("if") {
            self.name("not")?;
            self.name("exists")?;
        }
        if self.peek() == Some(&Token::Name("main".into())) {
            self.pop()?;
            self.symbol('.')?;
        }
        self.name(table)?;
        self.symbol('(')?;
        let mut at = 0;
        let mut primary_count = 0;
        loop {
            if self.eat_name("primary") {
                self.name("key")?;
                self.symbol('(')?;
                let primary = columns.iter().find(|c| c.primary).ok_or(Fault::Internal)?;
                self.name(primary.name)?;
                if self.eat_name("collate") {
                    self.name("binary")?;
                }
                if !self.eat_name("asc") {
                    self.eat_name("desc");
                }
                self.symbol(')')?;
                primary_count += 1;
            } else {
                let column = columns.get(at).ok_or(Fault::Schema)?;
                primary_count += usize::from(self.column(column)?);
                at += 1;
            }
            if self.eat(')') {
                break;
            }
            self.symbol(',')?;
        }
        self.eat(';');
        self.end()?;
        if at != columns.len() || primary_count != 1 {
            return Err(Fault::Schema);
        }
        Ok(())
    }
}

#[cfg(test)]
mod text_bound_controls {
    use super::*;
    #[test]
    fn nul_ddl_preflight_does_not_return_oversized_text() -> Result<(), Box<dyn std::error::Error>>
    {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE probe(path TEXT PRIMARY KEY)")?;
        // Fault injection is confined to this synthetic in-memory catalog.
        // Production validation never enables writable_schema or changes DDL.
        db.pragma_update(None, "writable_schema", "ON")?;
        db.execute(
            "UPDATE sqlite_schema SET sql=sql||char(0)||? WHERE name='probe'",
            ["x".repeat(100_000)],
        )?;
        db.pragma_update(None, "writable_schema", "OFF")?;
        let lengths: (i64, i64) = db.query_row(
            "SELECT length(sql),length(CAST(sql AS BLOB)) FROM sqlite_schema WHERE name='probe'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert!(lengths.0 <= i64::try_from(MAX_DDL_BYTES)?);
        assert!(lengths.1 > i64::try_from(MAX_DDL_BYTES)?);
        let (_, sql) = definition(&db, "probe")?.ok_or("missing catalog row")?;
        assert!(
            sql.is_none(),
            "DDL preflight returned oversized stored text"
        );
        Ok(())
    }
    #[test]
    fn index_name_preflight_is_a_byte_bound() -> Result<(), Box<dyn std::error::Error>> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE probe(path TEXT PRIMARY KEY)")?;
        let name = "é".repeat(100);
        db.execute_batch(&format!("CREATE INDEX \"{name}\" ON probe(path)"))?;
        let result = index_metadata(&db, "probe")?;
        let extra = result
            .into_iter()
            .find(|r| r.origin == "c")
            .ok_or("missing index")?;
        assert!(
            extra.name.is_none(),
            "index preflight returned more than128UTF8bytes"
        );
        Ok(())
    }
}

#[cfg(test)]
mod catalog_type_bound {
    use super::*;
    #[test]
    fn nul_catalog_type_preflight_refuses_before_returning_large_text(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE probe(path TEXT PRIMARY KEY)")?;
        db.pragma_update(None, "writable_schema", "ON")?;
        db.execute(
            "UPDATE sqlite_schema SET type=? WHERE name='probe'",
            [format!("table\0{}", "x".repeat(100_000))],
        )?;
        db.pragma_update(None, "writable_schema", "OFF")?;
        let (kind, _) = definition(&db, "probe")?.ok_or("missing catalog row")?;
        assert!(
            kind.is_none(),
            "catalog type preflight returned oversized stored text"
        );
        Ok(())
    }
}
