//! The SQL front end. Tokenizing, expressions, data types and column definitions come
//! from the `sqlparser` crate; this module splits a file into statements and gives the
//! dialect adapters (`crate::dialect`) the tools to drive `sqlparser` on parts of a
//! statement, so syntax the crate does not cover for a dialect (ClickHouse table clauses
//! in any order, `CODEC`, `CHECK` without parentheses...) can be handled around it.
//!
//! The output is a list of [`Ddl`] commands, dialect-neutral, that `crate::schema` folds
//! into the in-memory schema.

pub mod convert;

use std::fmt;
use std::ops::Range;

use sqlparser::ast::{Expr, ObjectName, ObjectNamePart};
use sqlparser::dialect::Dialect as SqlDialect;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer};

use crate::schema::DataType;

/// One source file.
pub struct Source {
    pub name: String,
    pub text: String,
    line_starts: Vec<usize>,
}

impl Source {
    /// A source named `name` (a file path, for messages).
    pub fn new(name: impl Into<String>, text: String) -> Source {
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Source { name: name.into(), text, line_starts }
    }

    /// Byte offset of a `sqlparser` location (1-based line and character column).
    pub fn offset(&self, loc: Location) -> usize {
        if loc.line == 0 {
            return self.text.len();
        }
        let Some(&start) = self.line_starts.get(loc.line as usize - 1) else {
            return self.text.len();
        };
        let line = &self.text[start..];
        let chars = loc.column.saturating_sub(1) as usize;
        start + line.char_indices().nth(chars).map_or(line.len(), |(i, _)| i)
    }

    /// `file:line:column` of a location, for messages.
    pub fn at(&self, loc: Location) -> String {
        format!("{}:{}:{}", self.name, loc.line, loc.column)
    }
}

/// A diagnostic tied to a place in a source file.
#[derive(Debug, Clone)]
pub struct Diag {
    pub at: String,
    pub message: String,
}

impl fmt::Display for Diag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.at, self.message)
    }
}

/// One statement: its tokens without whitespace and comments, and its source text.
pub struct Statement<'s> {
    pub source: &'s Source,
    pub toks: Vec<TokenWithSpan>,
    /// Byte range of the statement in the source, without the terminating `;`.
    pub range: Range<usize>,
}

impl<'s> Statement<'s> {
    /// The statement's source text.
    pub fn text(&self) -> &'s str {
        &self.source.text[self.range.clone()]
    }

    /// Where the statement starts.
    pub fn start(&self) -> Location {
        self.toks.first().map_or(Location { line: 0, column: 0 }, |t| t.span.start)
    }

    /// A diagnostic at `at`.
    pub fn diag(&self, at: Location, message: impl Into<String>) -> Diag {
        Diag { at: self.source.at(at), message: message.into() }
    }

    /// Source text of tokens `range` (original spelling, comments included).
    pub fn slice_text(&self, range: Range<usize>) -> String {
        if range.is_empty() {
            return String::new();
        }
        let a = self.source.offset(self.toks[range.start].span.start);
        let b = self.source.offset(self.toks[range.end - 1].span.end);
        self.source.text[a..b.max(a)].to_string()
    }

    /// Location of token `i`, or of the statement end when `i` is past it.
    pub fn loc(&self, i: usize) -> Location {
        match self.toks.get(i) {
            Some(t) => t.span.start,
            None => self.toks.last().map_or(Location { line: 0, column: 0 }, |t| t.span.end),
        }
    }
}

/// Tokenizes `source` with `dialect` and splits it into statements at top-level `;`.
/// The `;` inside the `BEGIN ... END` body of a `CREATE TRIGGER` do not split.
pub fn split<'s>(source: &'s Source, dialect: &dyn SqlDialect) -> Result<Vec<Statement<'s>>, Diag> {
    let tokens =
        Tokenizer::new(dialect, &source.text).tokenize_with_location().map_err(|e| Diag { at: source.at(e.location), message: e.message.clone() })?;
    let mut out = Vec::new();
    let mut current: Vec<TokenWithSpan> = Vec::new();
    let mut depth = 0i32;
    let flush = |current: &mut Vec<TokenWithSpan>, out: &mut Vec<Statement<'s>>| {
        if current.is_empty() {
            return;
        }
        let toks = std::mem::take(current);
        let start = source.offset(toks[0].span.start);
        let end = source.offset(toks[toks.len() - 1].span.end);
        out.push(Statement { source, toks, range: start..end.max(start) });
    };
    for t in tokens {
        match &t.token {
            Token::Whitespace(_) => continue,
            Token::EOF => break,
            Token::SemiColon if depth <= 0 && !inside_trigger_body(&current) => {
                flush(&mut current, &mut out);
                depth = 0;
                continue;
            }
            Token::LParen | Token::LBracket | Token::LBrace => depth += 1,
            Token::RParen | Token::RBracket | Token::RBrace => depth -= 1,
            _ => {}
        }
        current.push(t);
    }
    flush(&mut current, &mut out);
    Ok(out)
}

/// Whether `toks`, the start of a statement, is a `CREATE TRIGGER` whose `BEGIN` block is
/// still open. `CASE ... END` inside the body is balanced too.
fn inside_trigger_body(toks: &[TokenWithSpan]) -> bool {
    let mut i = usize::from(is_kw(toks.first(), "CREATE"));
    if i == 0 {
        return false;
    }
    if are_kws(toks, i, &["OR", "REPLACE"]) {
        i += 2;
    }
    if is_kw(toks.get(i), "TEMP") || is_kw(toks.get(i), "TEMPORARY") {
        i += 1;
    }
    if !is_kw(toks.get(i), "TRIGGER") {
        return false;
    }
    let mut open = 0i32;
    for t in &toks[i..] {
        if is_kw(Some(t), "BEGIN") || is_kw(Some(t), "CASE") {
            open += 1;
        } else if is_kw(Some(t), "END") {
            open -= 1;
        }
    }
    open > 0
}

// ---------------------------------------------------------------------------------------
// Token helpers for the dialect adapters.
// ---------------------------------------------------------------------------------------

/// Whether `t` is the unquoted keyword `kw` (case-insensitive).
pub fn is_kw(t: Option<&TokenWithSpan>, kw: &str) -> bool {
    matches!(t.map(|t| &t.token), Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case(kw))
}

/// Whether tokens from `i` are the keywords `kws` in order.
pub fn are_kws(toks: &[TokenWithSpan], i: usize, kws: &[&str]) -> bool {
    kws.iter().enumerate().all(|(k, kw)| is_kw(toks.get(i + k), kw))
}

/// The identifier (quoted or not) at `t`.
pub fn word(t: Option<&TokenWithSpan>) -> Option<String> {
    match t.map(|t| &t.token) {
        Some(Token::Word(w)) => Some(w.value.clone()),
        _ => None,
    }
}

/// The string literal at `t`.
pub fn string_lit(t: Option<&TokenWithSpan>) -> Option<String> {
    match t.map(|t| &t.token) {
        Some(Token::SingleQuotedString(s) | Token::DoubleQuotedString(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Index of the token closing the bracket opened at `open`, or `None` if unbalanced.
pub fn matching(toks: &[TokenWithSpan], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate().skip(open) {
        match t.token {
            Token::LParen | Token::LBracket | Token::LBrace => depth += 1,
            Token::RParen | Token::RBracket | Token::RBrace => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits `range` at top-level commas.
pub fn split_commas(toks: &[TokenWithSpan], range: Range<usize>) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = range.start;
    for i in range.clone() {
        match toks[i].token {
            Token::LParen | Token::LBracket | Token::LBrace => depth += 1,
            Token::RParen | Token::RBracket | Token::RBrace => depth -= 1,
            Token::Comma if depth == 0 => {
                if i > start {
                    out.push(start..i);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    if range.end > start {
        out.push(start..range.end);
    }
    out
}

/// Whether token `i` of `range` is at bracket depth 0 relative to the range start.
pub fn top_level_positions(toks: &[TokenWithSpan], range: Range<usize>) -> Vec<usize> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    for i in range {
        match toks[i].token {
            Token::LParen | Token::LBracket | Token::LBrace => {
                if depth == 0 {
                    out.push(i);
                }
                depth += 1;
            }
            Token::RParen | Token::RBracket | Token::RBrace => depth -= 1,
            _ => {
                if depth == 0 {
                    out.push(i);
                }
            }
        }
    }
    out
}

/// Runs a `sqlparser` routine on tokens `range` of `stmt` and requires it to consume them
/// all. This is how the adapters reuse the library on the parts it does understand.
pub fn parse_slice<T>(
    stmt: &Statement,
    range: Range<usize>,
    dialect: &dyn SqlDialect,
    what: &str,
    f: impl FnOnce(&mut Parser) -> Result<T, ParserError>,
) -> Result<T, Diag> {
    let at = stmt.loc(range.start);
    if range.is_empty() {
        return Err(stmt.diag(at, format!("expected {what}")));
    }
    parse_tokens(stmt, stmt.toks[range].to_vec(), dialect, what, f)
}

/// Like [`parse_slice`], on a token list an adapter has rewritten.
pub fn parse_tokens<T>(
    stmt: &Statement,
    mut toks: Vec<TokenWithSpan>,
    dialect: &dyn SqlDialect,
    what: &str,
    f: impl FnOnce(&mut Parser) -> Result<T, ParserError>,
) -> Result<T, Diag> {
    let Some(first) = toks.first() else {
        return Err(stmt.diag(stmt.start(), format!("expected {what}")));
    };
    let at = first.span.start;
    let end = toks.last().map(|t| t.span.end).unwrap_or(at);
    toks.push(TokenWithSpan { token: Token::EOF, span: sqlparser::tokenizer::Span::new(end, end) });
    let mut parser = Parser::new(dialect).with_tokens_with_locations(toks);
    let value = f(&mut parser).map_err(|e| stmt.diag(at, format!("cannot parse {what}: {}", parser_message(&e))))?;
    let next = parser.peek_token();
    if next.token != Token::EOF {
        return Err(stmt.diag(next.span.start, format!("unexpected '{}' in {what}", next.token)));
    }
    Ok(value)
}

fn parser_message(e: &ParserError) -> String {
    match e {
        ParserError::ParserError(m) | ParserError::TokenizerError(m) => m.clone(),
        other => other.to_string(),
    }
}

/// The last part of a possibly qualified name (`db.table` -> `table`).
pub fn unqualified(name: &ObjectName) -> String {
    match name.0.last() {
        Some(ObjectNamePart::Identifier(id)) => id.value.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Reads a possibly qualified name (`a`, `db.a`) starting at `i`; returns it unqualified
/// and the index after it.
pub fn read_name(toks: &[TokenWithSpan], mut i: usize) -> Option<(String, usize)> {
    let mut name = word(toks.get(i))?;
    i += 1;
    while matches!(toks.get(i).map(|t| &t.token), Some(Token::Period)) {
        name = word(toks.get(i + 1))?;
        i += 2;
    }
    Some((name, i))
}

// ---------------------------------------------------------------------------------------
// Dialect-neutral DDL commands.
// ---------------------------------------------------------------------------------------

/// One statement of the DDL input, reduced to its effect on the schema.
#[derive(Debug, Clone)]
pub enum Ddl {
    CreateTable(TableDef),
    /// `CREATE TABLE name AS source ...` or `... LIKE source`: the structure of `source`,
    /// with `def`'s clauses.
    CreateTableLike {
        def: TableDef,
        source: String,
    },
    AlterTable {
        table: String,
        actions: Vec<AlterAction>,
    },
    DropTables {
        names: Vec<String>,
        if_exists: bool,
    },
    RenameTables(Vec<(String, String)>),
    ExchangeTables(String, String),
    /// A named type usable in later column definitions: a PostgreSQL enum or domain.
    CreateType {
        name: String,
        ty: DataType,
    },
    /// `CREATE UNIQUE INDEX`: a uniqueness constraint under another name.
    CreateUniqueIndex {
        table: String,
        name: Option<String>,
        columns: Vec<String>,
    },
    /// `DROP INDEX`: removes the unique keys created under these names, if any.
    DropIndexes(Vec<String>),
    /// Data written by the DDL input itself (seed rows).
    Insert {
        table: String,
    },
    /// A view that writes into `target` whenever its source table receives rows.
    MaterializedView {
        name: String,
        target: Option<String>,
    },
    /// Anything that does not change table structure, by leading keywords.
    Ignored(String),
}

/// A `CREATE TABLE` statement.
#[derive(Debug, Clone, Default)]
pub struct TableDef {
    pub name: String,
    pub if_not_exists: bool,
    pub or_replace: bool,
    pub columns: Vec<ColumnSpec>,
    pub checks: Vec<CheckSpec>,
    pub primary_key: Option<Vec<String>>,
    pub uniques: Vec<UniqueSpec>,
    pub foreign_keys: Vec<ForeignKeySpec>,
    pub engine: Option<EngineSpec>,
    pub order_by: Vec<ExprSpec>,
    pub partition_by: Option<ExprSpec>,
    pub comment: Option<String>,
    /// Where the statement starts, for messages.
    pub at: String,
}

impl TableDef {
    /// Adds a column, moving its inline constraints (`PRIMARY KEY`, `UNIQUE`,
    /// `REFERENCES`, `CHECK`) to the table.
    pub fn add_column(&mut self, mut c: ColumnSpec) {
        if let Some(check) = c.check.take() {
            self.checks.push(check);
        }
        if let Some(fk) = c.references.take() {
            self.foreign_keys.push(fk);
        }
        if c.primary_key {
            self.primary_key = Some(vec![c.name.clone()]);
        }
        if c.unique {
            self.uniques.push(UniqueSpec { name: None, columns: vec![c.name.clone()] });
        }
        self.columns.push(c);
    }

    /// Adds a table constraint.
    pub fn add_constraint(&mut self, c: convert::Constraint) {
        match c {
            convert::Constraint::Check(c) => self.checks.push(*c),
            convert::Constraint::PrimaryKey(cols) => self.primary_key = Some(cols),
            convert::Constraint::Unique(u) => self.uniques.push(u),
            convert::Constraint::ForeignKey(fk) => self.foreign_keys.push(fk),
            convert::Constraint::Other => {}
        }
    }
}

/// A column definition, with the constraints written on it.
#[derive(Debug, Clone)]
pub struct ColumnSpec {
    pub name: String,
    pub ty: DataType,
    pub default: Option<DefaultSpec>,
    pub comment: Option<String>,
    pub primary_key: bool,
    pub unique: bool,
    pub references: Option<ForeignKeySpec>,
    pub check: Option<CheckSpec>,
}

/// How the database fills a column when an `INSERT` does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultKind {
    /// Filled by the database when the column is not inserted.
    Default,
    /// Computed and stored by the database; cannot be inserted.
    Materialized,
    /// Computed on read; cannot be inserted.
    Alias,
    /// Insertable but not stored.
    Ephemeral,
    /// Auto-increment / identity / serial.
    Identity,
}

/// A column default and its expression, when it has one.
#[derive(Debug, Clone)]
pub struct DefaultSpec {
    pub kind: DefaultKind,
    pub expr: Option<ExprSpec>,
}

/// An expression with its source text.
#[derive(Debug, Clone)]
pub struct ExprSpec {
    pub expr: Expr,
    pub sql: String,
}

/// A `CHECK` constraint.
#[derive(Debug, Clone)]
pub struct CheckSpec {
    pub name: Option<String>,
    pub expr: ExprSpec,
}

/// A `UNIQUE` constraint.
#[derive(Debug, Clone)]
pub struct UniqueSpec {
    pub name: Option<String>,
    pub columns: Vec<String>,
}

/// A `FOREIGN KEY` constraint.
#[derive(Debug, Clone)]
pub struct ForeignKeySpec {
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub ref_table: String,
    /// Empty: the referenced table's primary key.
    pub ref_columns: Vec<String>,
}

/// A ClickHouse table engine and its arguments.
#[derive(Debug, Clone)]
pub struct EngineSpec {
    pub name: String,
    pub args: Vec<ExprSpec>,
}

/// One change of an `ALTER TABLE` statement.
#[derive(Debug, Clone)]
pub enum AlterAction {
    AddColumn {
        column: ColumnSpec,
        if_not_exists: bool,
        after: Option<ColumnPosition>,
    },
    DropColumn {
        name: String,
        if_exists: bool,
    },
    /// Changes what is given, keeps the rest. `ty` None: the type is unchanged.
    ModifyColumn {
        name: String,
        if_exists: bool,
        ty: Option<DataType>,
        default: Option<DefaultSpec>,
        comment: Option<String>,
    },
    /// Replaces the whole definition of column `name` (MySQL `MODIFY`/`CHANGE COLUMN`).
    ReplaceColumn {
        name: String,
        column: ColumnSpec,
    },
    /// Changes the type and keeps the column's nullability (`ALTER COLUMN ... TYPE`).
    SetType {
        name: String,
        ty: DataType,
    },
    /// `SET NOT NULL` (false) or `DROP NOT NULL` (true).
    SetNullable {
        name: String,
        nullable: bool,
    },
    /// `REMOVE DEFAULT|MATERIALIZED|ALIAS|COMMENT...`: drops that property.
    RemoveColumnProperty {
        name: String,
        property: String,
    },
    RenameColumn {
        from: String,
        to: String,
        if_exists: bool,
    },
    CommentColumn {
        name: String,
        comment: String,
    },
    AddCheck(CheckSpec),
    AddPrimaryKey(Vec<String>),
    AddUnique(UniqueSpec),
    AddForeignKey(ForeignKeySpec),
    /// Drops the check, key or foreign key of that name.
    DropConstraint {
        name: String,
        if_exists: bool,
    },
    DropPrimaryKey,
    ModifyOrderBy(Vec<ExprSpec>),
    ModifyComment(String),
    RenameTable(String),
    Ignored(String),
}

/// Where `ADD COLUMN` puts the new column.
#[derive(Debug, Clone)]
pub enum ColumnPosition {
    First,
    After(String),
}
