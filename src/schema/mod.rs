//! The in-memory schema: what the DDL input defines once every statement has been
//! applied in order, as a migration tool would apply it.

pub mod types;

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Serialize, Serializer};
use sqlparser::ast::Expr;

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::sql::convert::{unwrap_nullable, wrap_nullable};
use crate::sql::{AlterAction, ColumnPosition, ColumnSpec, Ddl, DefaultKind, EngineSpec, ExprSpec, ForeignKeySpec, Source, TableDef};

pub use types::DataType;

/// Every table the DDL input defines, in definition order.
#[derive(Debug, Clone, Serialize)]
pub struct Schema {
    pub dialect: Dialect,
    pub tables: Vec<Table>,
    /// Named types (PostgreSQL enums and domains) that columns may use.
    #[serde(skip)]
    pub types: Vec<(String, DataType)>,
    /// Non-fatal observations made while compiling (unknown tables dropped, ...).
    pub warnings: Vec<String>,
    /// Every statement of the input, for `dbgen apply`.
    #[serde(skip)]
    pub statements: Vec<SourceStatement>,
    /// Statements that do not change table structure, counted by leading keywords.
    pub ignored: Vec<(String, usize)>,
}

/// One statement of the DDL input and where it comes from.
#[derive(Debug, Clone)]
pub struct SourceStatement {
    pub at: String,
    pub sql: String,
}

/// A table as the DDL input leaves it.
#[derive(Debug, Clone, Serialize)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    /// Column sets whose values must be unique across rows.
    pub keys: Vec<Key>,
    pub foreign_keys: Vec<ForeignKey>,
    pub checks: Vec<Check>,
    pub engine: Option<String>,
    #[serde(skip)]
    pub engine_spec: Option<EngineSpec>,
    #[serde(serialize_with = "ser_exprs")]
    pub order_by: Vec<ExprSpec>,
    #[serde(serialize_with = "ser_opt_expr")]
    pub partition_by: Option<ExprSpec>,
    /// Rows sharing a key are merged by the database; reads must deduplicate.
    pub collapsing: bool,
    /// The table stores rows (not a view, an integration engine, `Null`...).
    pub storage: bool,
    pub comment: Option<String>,
    /// The DDL input inserts rows into it (reference data).
    pub seeded: bool,
    /// A materialized view writes into it.
    pub derived_by: Option<String>,
    pub defined_at: String,
}

/// A column of a [`Table`].
#[derive(Debug, Clone, Serialize)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type", serialize_with = "ser_display")]
    pub ty: DataType,
    pub default: Option<ColumnDefault>,
    pub comment: Option<String>,
    /// A value the table engine requires (e.g. ClickHouse `is_deleted` = 0).
    pub fixed: Option<i64>,
}

/// How the database fills a column, and the SQL of its expression.
#[derive(Debug, Clone, Serialize)]
pub struct ColumnDefault {
    pub kind: DefaultKind,
    pub sql: Option<String>,
    #[serde(skip)]
    pub expr: Option<Expr>,
}

/// A set of columns (or expressions) whose values must be unique across rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Key {
    pub name: String,
    pub kind: KeyKind,
    pub parts: Vec<KeyPart>,
}

/// Where a [`Key`] comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyKind {
    PrimaryKey,
    Unique,
    /// ClickHouse collapsing engines: rows with equal sorting keys are merged.
    SortingKey,
}

/// One part of a [`Key`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyPart {
    Column(String),
    /// An expression over columns, by its SQL text.
    Expr(String),
}

/// A declared reference to another table.
#[derive(Debug, Clone, Serialize)]
pub struct ForeignKey {
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

/// A `CHECK` constraint: its name, SQL text and parsed expression.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub sql: String,
    #[serde(skip)]
    pub expr: Expr,
}

fn ser_display<T: fmt::Display, S: Serializer>(v: &T, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.collect_str(v)
}

fn ser_exprs<S: Serializer>(v: &[ExprSpec], s: S) -> std::result::Result<S::Ok, S::Error> {
    s.collect_seq(v.iter().map(|e| e.sql.as_str()))
}

fn ser_opt_expr<S: Serializer>(v: &Option<ExprSpec>, s: S) -> std::result::Result<S::Ok, S::Error> {
    match v {
        Some(e) => s.serialize_some(&e.sql),
        None => s.serialize_none(),
    }
}

impl Table {
    /// The column named `name`.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// The position of the column named `name`.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    fn column_mut(&mut self, name: &str) -> std::result::Result<&mut Column, String> {
        let table = &self.name;
        self.columns.iter_mut().find(|c| c.name == name).ok_or_else(|| format!("ALTER TABLE {table}: no column {name}"))
    }

    /// The primary key's columns, when it has one made only of columns.
    pub fn primary_key(&self) -> Option<Vec<&str>> {
        let key = self.keys.iter().find(|k| k.kind == KeyKind::PrimaryKey)?;
        key.parts
            .iter()
            .map(|p| match p {
                KeyPart::Column(c) => Some(c.as_str()),
                KeyPart::Expr(_) => None,
            })
            .collect()
    }
}

impl Column {
    /// Whether an INSERT may set the column.
    pub fn insertable(&self) -> bool {
        !matches!(self.default.as_ref().map(|d| d.kind), Some(DefaultKind::Materialized | DefaultKind::Alias))
    }

    /// Whether the database fills the column when an INSERT leaves it out.
    pub fn has_default(&self) -> bool {
        matches!(self.default.as_ref().map(|d| d.kind), Some(DefaultKind::Default | DefaultKind::Identity))
    }
}

impl Schema {
    /// The table named `name`.
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// The position of the table named `name`.
    pub fn table_index(&self, name: &str) -> Option<usize> {
        self.tables.iter().position(|t| t.name == name)
    }

    fn table_mut(&mut self, name: &str) -> Option<&mut Table> {
        self.tables.iter_mut().find(|t| t.name == name)
    }
}

// ---------------------------------------------------------------------------------------
// Loading and compiling
// ---------------------------------------------------------------------------------------

/// Expands the configured schema paths into source files, in application order. A
/// directory contributes its `*.sql` files sorted by name; when it holds migration pairs,
/// only the `*.up.sql` halves.
pub fn read_sources(paths: &[PathBuf], base: &Path) -> Result<Vec<Source>> {
    let mut out = Vec::new();
    for p in paths {
        let path = if p.is_absolute() || base == Path::new(".") { p.clone() } else { base.join(p) };
        let meta = std::fs::metadata(&path).map_err(|e| Error::Config(format!("schema source {}: {e}", path.display())))?;
        let mut files = Vec::new();
        if meta.is_dir() {
            let entries = std::fs::read_dir(&path).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".sql") && entry.path().is_file() {
                    files.push(entry.path());
                }
            }
            if files.iter().any(|f| f.to_string_lossy().ends_with(".up.sql")) {
                files.retain(|f| f.to_string_lossy().ends_with(".up.sql"));
            }
            files.sort();
            if files.is_empty() {
                return Err(Error::Config(format!("schema source {}: no .sql files in the directory", path.display())));
            }
        } else {
            files.push(path);
        }
        for f in files {
            let text = std::fs::read_to_string(&f).map_err(|e| Error::Config(format!("{}: {e}", f.display())))?;
            out.push(Source::new(f.display().to_string(), text));
        }
    }
    Ok(out)
}

/// Compiles the DDL `sources` into a schema: every statement is split and parsed by the
/// dialect's connector (with `sqlparser`) and applied in order.
pub fn compile(dialect: Dialect, sources: &[Source]) -> Result<Schema> {
    let mut schema = Schema { dialect, tables: Vec::new(), types: Vec::new(), warnings: Vec::new(), statements: Vec::new(), ignored: Vec::new() };
    for source in sources {
        let statements = dialect.split(source).map_err(|d| Error::Schema(d.to_string()))?;
        for stmt in &statements {
            let at = source.at(stmt.start());
            let ddl = dialect.parse_statement(stmt).map_err(|d| Error::Schema(d.to_string()))?;
            schema.statements.push(SourceStatement { at: at.clone(), sql: stmt.text().to_string() });
            schema.apply(ddl, &at, dialect).map_err(|m| Error::Schema(format!("{at}: {m}")))?;
        }
    }
    schema.finalize(dialect);
    Ok(schema)
}

impl Schema {
    fn apply(&mut self, ddl: Ddl, at: &str, dialect: Dialect) -> std::result::Result<(), String> {
        match ddl {
            Ddl::CreateTable(def) => self.create(def, None, dialect),
            Ddl::CreateTableLike { def, source } => self.create(def, Some(source), dialect),
            Ddl::AlterTable { table, mut actions } => {
                for action in &mut actions {
                    if let AlterAction::AddColumn { column, .. } | AlterAction::ReplaceColumn { column, .. } = action {
                        resolve_types(&mut column.ty, &self.types);
                    }
                    if let AlterAction::SetType { ty, .. } = action {
                        resolve_types(ty, &self.types);
                    }
                }
                let Some(t) = self.table_mut(&table) else {
                    if actions.iter().all(|a| matches!(a, AlterAction::Ignored(_))) {
                        return Ok(());
                    }
                    return Err(format!("ALTER TABLE {table}: the table is not defined by the statements before this one"));
                };
                let mut warnings = Vec::new();
                for action in actions {
                    if let Some(rename) = alter(t, action, &mut warnings)? {
                        let old = std::mem::replace(&mut t.name, rename.clone());
                        self.renamed(&old, &rename);
                        break;
                    }
                }
                self.warnings.extend(warnings.into_iter().map(|w| format!("{at}: {w}")));
                Ok(())
            }
            Ddl::DropTables { names, if_exists } => {
                for name in names {
                    match self.table_index(&name) {
                        Some(i) => {
                            self.tables.remove(i);
                        }
                        None if if_exists => {}
                        None => self.warnings.push(format!("{at}: DROP TABLE {name}: no such table")),
                    }
                }
                Ok(())
            }
            Ddl::RenameTables(pairs) => {
                for (from, to) in pairs {
                    let t = self.table_mut(&from).ok_or_else(|| format!("RENAME TABLE {from}: no such table"))?;
                    t.name = to.clone();
                    self.renamed(&from, &to);
                }
                Ok(())
            }
            Ddl::ExchangeTables(a, b) => {
                let ia = self.table_index(&a).ok_or_else(|| format!("EXCHANGE TABLES: no table {a}"))?;
                let ib = self.table_index(&b).ok_or_else(|| format!("EXCHANGE TABLES: no table {b}"))?;
                self.tables[ia].name = b;
                self.tables[ib].name = a;
                Ok(())
            }
            Ddl::CreateType { name, ty } => {
                self.types.retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
                self.types.push((name, ty));
                Ok(())
            }
            Ddl::CreateUniqueIndex { table, name, columns } => {
                match self.table_mut(&table) {
                    Some(t) => t.keys.push(unique_key(name, columns)),
                    None => self.warnings.push(format!("{at}: CREATE UNIQUE INDEX on {table}, which is not defined")),
                }
                Ok(())
            }
            Ddl::DropIndexes(names) => {
                for t in &mut self.tables {
                    t.keys.retain(|k| k.kind != KeyKind::Unique || !names.contains(&k.name));
                }
                Ok(())
            }
            Ddl::Insert { table } => {
                match self.table_mut(&table) {
                    Some(t) => t.seeded = true,
                    None => self.warnings.push(format!("{at}: INSERT INTO {table}: the table is not defined by the DDL input")),
                }
                Ok(())
            }
            Ddl::MaterializedView { name, target } => {
                if let Some(target) = target {
                    match self.table_mut(&target) {
                        Some(t) => t.derived_by = Some(name),
                        None => self.warnings.push(format!("{at}: materialized view {name} writes into {target}, which is not defined")),
                    }
                }
                Ok(())
            }
            Ddl::Ignored(what) => {
                match self.ignored.iter_mut().find(|(k, _)| *k == what) {
                    Some((_, n)) => *n += 1,
                    None => self.ignored.push((what, 1)),
                }
                Ok(())
            }
        }
    }

    fn create(&mut self, mut def: TableDef, like: Option<String>, dialect: Dialect) -> std::result::Result<(), String> {
        for c in &mut def.columns {
            resolve_types(&mut c.ty, &self.types);
        }
        if let Some(existing) = self.table_index(&def.name) {
            if def.if_not_exists {
                return Ok(());
            }
            if !def.or_replace {
                return Err(format!("CREATE TABLE {}: already defined at {}", def.name, self.tables[existing].defined_at));
            }
            self.tables.remove(existing);
        }
        let mut table = match &like {
            Some(source) => {
                let src = self.table(source).ok_or_else(|| format!("CREATE TABLE {} AS {source}: no such table", def.name))?;
                let mut t = src.clone();
                t.name = def.name.clone();
                t.seeded = false;
                t.derived_by = None;
                t.defined_at = def.at.clone();
                if def.engine.is_some() {
                    t.engine_spec = def.engine.clone();
                    t.order_by = def.order_by.clone();
                    t.partition_by = def.partition_by.clone();
                }
                t
            }
            None => Table {
                name: def.name.clone(),
                columns: Vec::new(),
                keys: Vec::new(),
                foreign_keys: Vec::new(),
                checks: Vec::new(),
                engine: None,
                engine_spec: def.engine.clone(),
                order_by: def.order_by.clone(),
                partition_by: def.partition_by.clone(),
                collapsing: false,
                storage: dialect.is_storage_table(&def),
                comment: def.comment.clone(),
                seeded: false,
                derived_by: None,
                defined_at: def.at.clone(),
            },
        };
        for spec in def.columns {
            if table.column(&spec.name).is_some() {
                return Err(format!("CREATE TABLE {}: column {} is defined twice", table.name, spec.name));
            }
            table.columns.push(column_from(spec));
        }
        for (n, c) in def.checks.into_iter().enumerate() {
            table.checks.push(Check { name: c.name.unwrap_or_else(|| format!("check_{}", n + 1)), sql: c.expr.sql, expr: c.expr.expr });
        }
        if let Some(cols) = def.primary_key {
            table.keys.push(Key { name: "primary key".into(), kind: KeyKind::PrimaryKey, parts: cols.into_iter().map(KeyPart::Column).collect() });
        }
        for u in def.uniques {
            table.keys.push(unique_key(u.name, u.columns));
        }
        table.foreign_keys.extend(def.foreign_keys.into_iter().map(foreign_key));
        self.tables.push(table);
        Ok(())
    }

    /// Follows a table rename in the foreign keys of other tables.
    fn renamed(&mut self, from: &str, to: &str) {
        for t in &mut self.tables {
            for fk in &mut t.foreign_keys {
                if fk.ref_table == from {
                    fk.ref_table = to.to_string();
                }
            }
        }
    }

    /// Applies the dialect's table semantics (keys implied by engines, fixed columns).
    fn finalize(&mut self, dialect: Dialect) {
        for t in &mut self.tables {
            let sem = dialect.table_semantics(t.engine_spec.as_ref(), &t.order_by);
            t.engine = t.engine_spec.as_ref().map(|e| e.name.clone());
            t.collapsing = sem.collapsing;
            t.keys.retain(|k| k.kind != KeyKind::SortingKey);
            t.keys.extend(sem.keys);
            for (name, value) in sem.fixed {
                if let Some(c) = t.columns.iter_mut().find(|c| c.name == name) {
                    c.fixed = Some(value);
                }
            }
            for key in &t.keys {
                for part in &key.parts {
                    if let KeyPart::Column(c) = part
                        && !t.columns.iter().any(|col| &col.name == c)
                    {
                        self.warnings.push(format!("table {}: {} names column {c}, which does not exist", t.name, key.name));
                    }
                }
            }
        }
    }
}

fn unique_key(name: Option<String>, columns: Vec<String>) -> Key {
    Key {
        name: name.unwrap_or_else(|| format!("unique ({})", columns.join(", "))),
        kind: KeyKind::Unique,
        parts: columns.into_iter().map(KeyPart::Column).collect(),
    }
}

fn foreign_key(fk: ForeignKeySpec) -> ForeignKey {
    ForeignKey { name: fk.name, columns: fk.columns, ref_table: fk.ref_table, ref_columns: fk.ref_columns }
}

/// Replaces references to named types (`mood`, `public.mood`) with their definition.
fn resolve_types(ty: &mut DataType, types: &[(String, DataType)]) {
    match ty {
        DataType::Nullable(t) | DataType::LowCardinality(t) | DataType::Array(t) => resolve_types(t, types),
        DataType::Unsupported(name) => {
            let bare = name.rsplit('.').next().unwrap_or(name).trim_matches('"');
            if let Some((_, t)) = types.iter().find(|(n, _)| n.eq_ignore_ascii_case(bare)) {
                *ty = t.clone();
            }
        }
        _ => {}
    }
}

fn column_from(spec: ColumnSpec) -> Column {
    Column {
        name: spec.name,
        ty: spec.ty,
        default: spec.default.map(|d| ColumnDefault { kind: d.kind, sql: d.expr.as_ref().map(|e| e.sql.clone()), expr: d.expr.map(|e| e.expr) }),
        comment: spec.comment,
        fixed: None,
    }
}

/// Applies one ALTER action; returns the new name when the table is renamed.
fn alter(t: &mut Table, action: AlterAction, warnings: &mut Vec<String>) -> std::result::Result<Option<String>, String> {
    let table = t.name.clone();
    let find = |t: &Table, name: &str| t.columns.iter().position(|c| c.name == name);
    match action {
        AlterAction::AddColumn { column, if_not_exists, after } => {
            if find(t, &column.name).is_some() {
                if if_not_exists {
                    return Ok(None);
                }
                return Err(format!("ALTER TABLE {table} ADD COLUMN {}: the column exists", column.name));
            }
            let col = column_from(column);
            let at = match after {
                Some(ColumnPosition::First) => 0,
                Some(ColumnPosition::After(a)) => {
                    find(t, &a).map(|i| i + 1).ok_or_else(|| format!("ALTER TABLE {table}: AFTER {a}: no such column"))?
                }
                None => t.columns.len(),
            };
            t.columns.insert(at, col);
        }
        AlterAction::DropColumn { name, if_exists } => match find(t, &name) {
            Some(i) => {
                t.columns.remove(i);
            }
            None if if_exists => {}
            None => return Err(format!("ALTER TABLE {table} DROP COLUMN {name}: no such column")),
        },
        AlterAction::ModifyColumn { name, if_exists, ty, default, comment } => match find(t, &name) {
            Some(i) => {
                let c = &mut t.columns[i];
                if let Some(ty) = ty {
                    c.ty = ty;
                }
                if let Some(d) = default {
                    c.default = Some(ColumnDefault { kind: d.kind, sql: d.expr.as_ref().map(|e| e.sql.clone()), expr: d.expr.map(|e| e.expr) });
                }
                if let Some(cm) = comment {
                    c.comment = Some(cm);
                }
            }
            None if if_exists => {}
            None => return Err(format!("ALTER TABLE {table} MODIFY COLUMN {name}: no such column")),
        },
        AlterAction::ReplaceColumn { name, column } => {
            *t.column_mut(&name)? = column_from(column);
        }
        AlterAction::SetType { name, ty } => {
            let c = t.column_mut(&name)?;
            c.ty = if c.ty.is_nullable() { wrap_nullable(ty) } else { unwrap_nullable(ty) };
        }
        AlterAction::SetNullable { name, nullable } => {
            let c = t.column_mut(&name)?;
            let ty = std::mem::replace(&mut c.ty, DataType::Bool);
            c.ty = if nullable { wrap_nullable(ty) } else { unwrap_nullable(ty) };
        }
        AlterAction::RemoveColumnProperty { name, property } => {
            let i = find(t, &name).ok_or_else(|| format!("ALTER TABLE {table} MODIFY COLUMN {name}: no such column"))?;
            match property.as_str() {
                "DEFAULT" | "MATERIALIZED" | "ALIAS" | "EPHEMERAL" => t.columns[i].default = None,
                "COMMENT" => t.columns[i].comment = None,
                _ => {}
            }
        }
        AlterAction::RenameColumn { from, to, if_exists } => match find(t, &from) {
            Some(i) => {
                t.columns[i].name = to.clone();
                for key in &mut t.keys {
                    for p in &mut key.parts {
                        if *p == KeyPart::Column(from.clone()) {
                            *p = KeyPart::Column(to.clone());
                        }
                    }
                }
                for fk in &mut t.foreign_keys {
                    for c in &mut fk.columns {
                        if *c == from {
                            *c = to.clone();
                        }
                    }
                }
                if t.checks.iter().any(|c| c.sql.contains(&from)) {
                    warnings.push(format!("table {table}: column {from} renamed to {to}, but a CHECK may still use the old name"));
                }
            }
            None if if_exists => {}
            None => return Err(format!("ALTER TABLE {table} RENAME COLUMN {from}: no such column")),
        },
        AlterAction::CommentColumn { name, comment } => {
            if let Some(i) = find(t, &name) {
                t.columns[i].comment = Some(comment);
            }
        }
        AlterAction::AddCheck(c) => {
            let name = c.name.unwrap_or_else(|| format!("check_{}", t.checks.len() + 1));
            t.checks.push(Check { name, sql: c.expr.sql, expr: c.expr.expr });
        }
        AlterAction::AddPrimaryKey(cols) => {
            t.keys.retain(|k| k.kind != KeyKind::PrimaryKey);
            t.keys.push(Key { name: "primary key".into(), kind: KeyKind::PrimaryKey, parts: cols.into_iter().map(KeyPart::Column).collect() });
        }
        AlterAction::AddUnique(u) => t.keys.push(unique_key(u.name, u.columns)),
        AlterAction::AddForeignKey(fk) => t.foreign_keys.push(foreign_key(fk)),
        AlterAction::DropConstraint { name, if_exists } => {
            let before = t.checks.len() + t.keys.len() + t.foreign_keys.len();
            t.checks.retain(|c| c.name != name);
            t.keys.retain(|k| k.name != name);
            t.foreign_keys.retain(|fk| fk.name.as_deref() != Some(name.as_str()));
            if t.checks.len() + t.keys.len() + t.foreign_keys.len() == before && !if_exists {
                warnings.push(format!("table {table}: DROP CONSTRAINT {name}: no such constraint"));
            }
        }
        AlterAction::DropPrimaryKey => t.keys.retain(|k| k.kind != KeyKind::PrimaryKey),
        AlterAction::ModifyOrderBy(exprs) => t.order_by = exprs,
        AlterAction::ModifyComment(c) => t.comment = Some(c),
        AlterAction::RenameTable(to) => return Ok(Some(to)),
        AlterAction::Ignored(_) => {}
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile_str(sql: &str) -> Schema {
        compile(Dialect::Clickhouse, &[Source::new("t.sql", sql.to_string())]).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn statements_fold_in_order() {
        let s = compile_str(
            "CREATE TABLE a (id Int32, x String, _v UInt64 DEFAULT 1, _d UInt8 DEFAULT 0) ENGINE = ReplacingMergeTree(_v, _d) ORDER BY id;
             CREATE TABLE IF NOT EXISTS a (other Int8) ENGINE = MergeTree ORDER BY other;
             ALTER TABLE a ADD COLUMN y Nullable(Int64) AFTER id, DROP COLUMN x;
             CREATE TABLE b AS a ENGINE = MergeTree ORDER BY id;
             INSERT INTO a (id) VALUES (1);
             RENAME TABLE b TO c;
             DROP TABLE IF EXISTS gone;
             OPTIMIZE TABLE a FINAL;",
        );
        let a = s.table("a").unwrap();
        let names: Vec<&str> = a.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "y", "_v", "_d"]);
        assert!(a.seeded);
        assert!(a.collapsing);
        assert_eq!(a.keys.len(), 1);
        assert_eq!(a.column("_d").unwrap().fixed, Some(0));
        let c = s.table("c").unwrap();
        assert!(!c.collapsing);
        assert!(c.keys.is_empty());
        assert_eq!(c.columns.len(), 4);
        assert_eq!(s.ignored, vec![("OPTIMIZE TABLE".to_string(), 1)]);
    }

    #[test]
    fn unknown_alter_target_is_an_error_with_location() {
        let e = compile(Dialect::Clickhouse, &[Source::new("m.sql", "\nALTER TABLE nope ADD COLUMN a Int32".into())]).unwrap_err();
        assert!(e.to_string().contains("m.sql:2:1"), "{e}");
    }
}
