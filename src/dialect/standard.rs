//! The shared DDL adapter, for dialects whose statements `sqlparser` parses whole
//! (PostgreSQL, MySQL, SQLite): it reduces each parsed statement to dialect-neutral
//! [`Ddl`] commands. It is the default of
//! [`Connector::parse_statement`](crate::connector::Connector::parse_statement) and
//! [`Connector::parse_type`](crate::connector::Connector::parse_type).
//!
//! Only statements that can change table structure are parsed. Migrations also hold
//! functions, triggers, grants and data changes whose syntax only the database needs to
//! understand; they are counted as ignored without being parsed.

use sqlparser::ast::{
    AlterColumnOperation, AlterTableOperation, ColumnDef, ColumnOptionDef, CreateTableLikeKind, Expr, MySQLColumnPosition, ObjectType,
    RenameTableNameKind, Statement as Ast, UserDefinedTypeRepresentation,
};
use sqlparser::dialect::Dialect as SqlDialect;
use sqlparser::tokenizer::Token;

use crate::schema::DataType;
use crate::sql::convert::{self, Constraint, TypeRules};
use crate::sql::{
    AlterAction, ColumnPosition, ColumnSpec, Ddl, DefaultKind, DefaultSpec, Diag, Statement, TableDef, are_kws, is_kw, parse_slice, read_name,
    unqualified, word,
};

/// What a statement can do to the schema, judged from its leading keywords.
enum Kind {
    /// May change table structure: parse it.
    Structure,
    /// Writes rows into the table named at this token index.
    Insert(usize),
    /// Cannot change table structure.
    Other,
}

fn classify(stmt: &Statement) -> Kind {
    let t = &stmt.toks;
    let mut i = 0;
    if is_kw(t.first(), "CREATE") {
        i = 1;
        if are_kws(t, i, &["OR", "REPLACE"]) {
            i += 2;
        }
        while ["GLOBAL", "LOCAL", "UNLOGGED"].iter().any(|kw| is_kw(t.get(i), kw)) {
            i += 1;
        }
        let structural = is_kw(t.get(i), "TABLE") || are_kws(t, i, &["UNIQUE", "INDEX"]) || is_kw(t.get(i), "TYPE") || is_kw(t.get(i), "DOMAIN");
        return if structural { Kind::Structure } else { Kind::Other };
    }
    if are_kws(t, 0, &["ALTER", "TABLE"]) || are_kws(t, 0, &["RENAME", "TABLE"]) {
        return Kind::Structure;
    }
    if is_kw(t.first(), "DROP") && (is_kw(t.get(1), "TABLE") || is_kw(t.get(1), "INDEX")) {
        return Kind::Structure;
    }
    if is_kw(t.first(), "INSERT") || is_kw(t.first(), "REPLACE") {
        // INSERT [OR action | IGNORE] INTO name, REPLACE INTO name
        while i < t.len() && !is_kw(t.get(i), "INTO") {
            i += 1;
        }
        if i + 1 < t.len() {
            return Kind::Insert(i + 1);
        }
    }
    Kind::Other
}

fn leading_words(stmt: &Statement, n: usize) -> String {
    stmt.toks.iter().take(n).map_while(|t| word(Some(t))).map(|w| w.to_uppercase()).collect::<Vec<_>>().join(" ")
}

/// Reduces one statement to its effect on the schema, parsing it with `sql` and reading
/// its types with `rules`.
pub fn parse_statement(stmt: &Statement, sql: &dyn SqlDialect, rules: &TypeRules) -> Result<Ddl, Diag> {
    match classify(stmt) {
        Kind::Other => return Ok(Ddl::Ignored(leading_words(stmt, 2))),
        Kind::Insert(i) => {
            return Ok(match read_name(&stmt.toks, i) {
                Some((table, _)) => Ddl::Insert { table },
                None => Ddl::Ignored(leading_words(stmt, 2)),
            });
        }
        Kind::Structure => {}
    }
    let ast = parse_slice(stmt, 0..stmt.toks.len(), sql, "statement", |p| p.parse_statement())?;
    Ok(match ast {
        Ast::CreateTable(ct) if ct.temporary => Ddl::Ignored("CREATE TEMPORARY TABLE".into()),
        Ast::CreateTable(ct) => {
            let mut def = TableDef {
                name: unqualified(&ct.name),
                if_not_exists: ct.if_not_exists,
                or_replace: ct.or_replace,
                at: stmt.source.at(stmt.start()),
                ..TableDef::default()
            };
            for column in ct.columns {
                for spec in convert::column(stmt, column, rules) {
                    def.add_column(spec);
                }
            }
            for c in ct.constraints {
                def.add_constraint(convert::table_constraint(stmt, c));
            }
            match ct.like {
                Some(CreateTableLikeKind::Parenthesized(like) | CreateTableLikeKind::Plain(like)) => {
                    Ddl::CreateTableLike { def, source: unqualified(&like.name) }
                }
                None if def.columns.is_empty() => Ddl::Ignored("CREATE TABLE ... AS SELECT (columns come from a query)".into()),
                None => Ddl::CreateTable(def),
            }
        }
        Ast::AlterTable(at) => {
            let mut actions = Vec::new();
            for op in at.operations {
                alter_operation(stmt, op, rules, &mut actions);
            }
            Ddl::AlterTable { table: unqualified(&at.name), actions }
        }
        Ast::Drop { object_type: ObjectType::Table, names, if_exists, .. } => {
            Ddl::DropTables { names: names.iter().map(unqualified).collect(), if_exists }
        }
        Ast::Drop { object_type: ObjectType::Index, names, .. } => Ddl::DropIndexes(names.iter().map(unqualified).collect()),
        Ast::CreateIndex(ci) => {
            let columns: Option<Vec<String>> = ci
                .columns
                .iter()
                .map(|c| match &c.column.expr {
                    Expr::Identifier(id) => Some(id.value.clone()),
                    _ => None,
                })
                .collect();
            match columns {
                Some(columns) => Ddl::CreateUniqueIndex { table: unqualified(&ci.table_name), name: ci.name.as_ref().map(unqualified), columns },
                None => Ddl::Ignored("CREATE UNIQUE INDEX on expressions".into()),
            }
        }
        Ast::CreateType { name, representation: Some(UserDefinedTypeRepresentation::Enum { labels }) } => Ddl::CreateType {
            name: unqualified(&name),
            ty: DataType::Enum { bits: 16, variants: labels.into_iter().zip(1..).map(|(l, i)| (l.value, i)).collect() },
        },
        Ast::CreateDomain(d) => Ddl::CreateType { name: unqualified(&d.name), ty: convert::data_type(&d.data_type, rules) },
        Ast::RenameTable(pairs) => Ddl::RenameTables(pairs.iter().map(|r| (unqualified(&r.old_name), unqualified(&r.new_name))).collect()),
        _ => Ddl::Ignored(leading_words(stmt, 2)),
    })
}

/// Converts one `ALTER TABLE` operation, appending to `out`: a column added with inline
/// constraints yields one action per constraint.
fn alter_operation(stmt: &Statement, op: AlterTableOperation, rules: &TypeRules, out: &mut Vec<AlterAction>) {
    use AlterTableOperation as Op;
    match op {
        Op::AddColumn { if_not_exists, column_def, column_position, .. } => {
            for mut column in convert::column(stmt, column_def, rules) {
                let constraints = take_inline_constraints(&mut column);
                out.push(AlterAction::AddColumn { column, if_not_exists, after: column_position.as_ref().map(position) });
                out.extend(constraints);
            }
        }
        Op::AddConstraint { constraint, .. } => out.push(match convert::table_constraint(stmt, constraint) {
            Constraint::Check(c) => AlterAction::AddCheck(*c),
            Constraint::PrimaryKey(cols) => AlterAction::AddPrimaryKey(cols),
            Constraint::Unique(u) => AlterAction::AddUnique(u),
            Constraint::ForeignKey(fk) => AlterAction::AddForeignKey(fk),
            Constraint::Other => AlterAction::Ignored("ADD CONSTRAINT".into()),
        }),
        Op::DropColumn { column_names, if_exists, .. } => {
            out.extend(column_names.into_iter().map(|c| AlterAction::DropColumn { name: c.value, if_exists }));
        }
        Op::RenameColumn { old_column_name, new_column_name } => {
            out.push(AlterAction::RenameColumn { from: old_column_name.value, to: new_column_name.value, if_exists: false });
        }
        Op::RenameTable { table_name: RenameTableNameKind::As(name) | RenameTableNameKind::To(name) } => {
            out.push(AlterAction::RenameTable(unqualified(&name)));
        }
        Op::AlterColumn { column_name, op } => {
            let name = column_name.value;
            out.push(match op {
                AlterColumnOperation::SetNotNull => AlterAction::SetNullable { name, nullable: false },
                AlterColumnOperation::DropNotNull => AlterAction::SetNullable { name, nullable: true },
                AlterColumnOperation::SetDefault { value } => AlterAction::ModifyColumn {
                    name,
                    if_exists: false,
                    ty: None,
                    default: Some(DefaultSpec { kind: DefaultKind::Default, expr: Some(convert::expr_spec(stmt, value)) }),
                    comment: None,
                },
                AlterColumnOperation::DropDefault => AlterAction::RemoveColumnProperty { name, property: "DEFAULT".into() },
                AlterColumnOperation::SetDataType { data_type, .. } => AlterAction::SetType { name, ty: convert::data_type(&data_type, rules) },
                AlterColumnOperation::AddGenerated { .. } => AlterAction::ModifyColumn {
                    name,
                    if_exists: false,
                    ty: None,
                    default: Some(DefaultSpec { kind: DefaultKind::Identity, expr: None }),
                    comment: None,
                },
                other => AlterAction::Ignored(format!("ALTER COLUMN {other}")),
            });
        }
        Op::ChangeColumn { old_name, new_name, data_type, options, .. } => {
            if old_name.value != new_name.value {
                out.push(AlterAction::RenameColumn { from: old_name.value, to: new_name.value.clone(), if_exists: false });
            }
            replace_column(stmt, ColumnDef { name: new_name, data_type, options: option_defs(options) }, rules, out);
        }
        Op::ModifyColumn { col_name, data_type, options, .. } => {
            replace_column(stmt, ColumnDef { name: col_name, data_type, options: option_defs(options) }, rules, out);
        }
        Op::DropConstraint { name, if_exists, .. } => out.push(AlterAction::DropConstraint { name: name.value, if_exists }),
        Op::DropForeignKey { name, .. } => out.push(AlterAction::DropConstraint { name: name.value, if_exists: false }),
        Op::DropIndex { name } => out.push(AlterAction::DropConstraint { name: name.value, if_exists: true }),
        Op::DropPrimaryKey { .. } => out.push(AlterAction::DropPrimaryKey),
        other => out.push(AlterAction::Ignored(other.to_string().split_whitespace().take(2).collect::<Vec<_>>().join(" "))),
    }
}

/// MySQL `MODIFY COLUMN` and `CHANGE COLUMN` redefine the column entirely.
fn replace_column(stmt: &Statement, def: ColumnDef, rules: &TypeRules, out: &mut Vec<AlterAction>) {
    for mut column in convert::column(stmt, def, rules) {
        let constraints = take_inline_constraints(&mut column);
        out.push(AlterAction::ReplaceColumn { name: column.name.clone(), column });
        out.extend(constraints);
    }
}

fn option_defs(options: Vec<sqlparser::ast::ColumnOption>) -> Vec<ColumnOptionDef> {
    options.into_iter().map(|option| ColumnOptionDef { name: None, option }).collect()
}

/// Moves the constraints written on a column into separate actions.
fn take_inline_constraints(c: &mut ColumnSpec) -> Vec<AlterAction> {
    let mut out = Vec::new();
    if let Some(check) = c.check.take() {
        out.push(AlterAction::AddCheck(check));
    }
    if let Some(fk) = c.references.take() {
        out.push(AlterAction::AddForeignKey(fk));
    }
    if std::mem::take(&mut c.primary_key) {
        out.push(AlterAction::AddPrimaryKey(vec![c.name.clone()]));
    }
    if std::mem::take(&mut c.unique) {
        out.push(AlterAction::AddUnique(crate::sql::UniqueSpec { name: None, columns: vec![c.name.clone()] }));
    }
    out
}

fn position(p: &MySQLColumnPosition) -> ColumnPosition {
    match p {
        MySQLColumnPosition::First => ColumnPosition::First,
        MySQLColumnPosition::After(id) => ColumnPosition::After(id.value.clone()),
    }
}

/// Parses a type as the database spells it, in DDL or in its catalog.
pub fn parse_type(s: &str, sql: &dyn SqlDialect, rules: &TypeRules) -> Result<DataType, String> {
    let mut parser = sqlparser::parser::Parser::new(sql).try_with_sql(s).map_err(|e| e.to_string())?;
    let t = parser.parse_data_type().map_err(|e| format!("cannot parse type '{s}': {e}"))?;
    if parser.peek_token().token != Token::EOF {
        return Err(format!("cannot parse type '{s}'"));
    }
    Ok(convert::data_type(&t, rules))
}

#[cfg(all(test, feature = "postgres", feature = "mysql", feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::dialect::Dialect;
    use crate::sql::{Source, split};

    fn parse(dialect: Dialect, sql: &str) -> Vec<Ddl> {
        let src = Source::new("test.sql", sql.to_string());
        split(&src, dialect.sql())
            .unwrap()
            .iter()
            .map(|s| parse_statement(s, dialect.sql(), &dialect.type_rules()).unwrap_or_else(|e| panic!("{e}")))
            .collect()
    }

    #[test]
    fn postgres_tables_types_and_constraints() {
        let ddl = parse(
            Dialect::Postgres,
            "CREATE TYPE mood AS ENUM ('sad', 'ok');
             CREATE TABLE public.people (
                 id bigserial PRIMARY KEY,
                 email varchar(80) NOT NULL UNIQUE,
                 born date,
                 at timestamptz DEFAULT now(),
                 boss_id bigint REFERENCES people (id),
                 mood mood,
                 tags text[],
                 CONSTRAINT ck_born CHECK (born > '1900-01-01')
             );
             CREATE UNIQUE INDEX people_lower ON people (email);
             CREATE INDEX people_born ON people (born);
             CREATE FUNCTION f() RETURNS trigger AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
             ALTER TABLE people ALTER COLUMN born SET NOT NULL, ADD COLUMN n int CHECK (n > 0);
             INSERT INTO people (email) VALUES ('a@b.c');",
        );
        assert!(matches!(&ddl[0], Ddl::CreateType { name, ty: DataType::Enum { variants, .. } } if name == "mood" && variants.len() == 2));
        let Ddl::CreateTable(def) = &ddl[1] else { panic!("{:?}", ddl[1]) };
        assert_eq!(def.name, "people");
        assert_eq!(def.primary_key.as_deref(), Some(&["id".to_string()][..]));
        assert_eq!(def.columns[0].ty, DataType::Int { bits: 64, signed: true });
        assert_eq!(def.columns[0].default.as_ref().unwrap().kind, DefaultKind::Identity);
        assert_eq!(def.columns[1].ty, DataType::String { max_len: Some(80) });
        assert!(def.columns[2].ty.is_nullable());
        assert_eq!(def.foreign_keys[0].ref_table, "people");
        assert_eq!(def.uniques.len(), 1);
        assert_eq!(def.checks.len(), 1);
        assert!(matches!(&def.columns[6].ty, DataType::Nullable(t) if matches!(**t, DataType::Array(_))));
        assert!(matches!(&ddl[2], Ddl::CreateUniqueIndex { table, columns, .. } if table == "people" && columns == &["email"]));
        assert!(matches!(&ddl[3], Ddl::Ignored(_)));
        assert!(matches!(&ddl[4], Ddl::Ignored(_)));
        let Ddl::AlterTable { actions, .. } = &ddl[5] else { panic!() };
        assert!(matches!(&actions[0], AlterAction::SetNullable { nullable: false, .. }));
        assert!(matches!(&actions[1], AlterAction::AddColumn { .. }));
        assert!(matches!(&actions[2], AlterAction::AddCheck(_)));
        assert!(matches!(&ddl[6], Ddl::Insert { table } if table == "people"));
    }

    #[test]
    fn mysql_and_sqlite_specifics() {
        let ddl = parse(
            Dialect::Mysql,
            "CREATE TABLE `t` (
                 id INT UNSIGNED NOT NULL AUTO_INCREMENT,
                 kind ENUM('a', 'b') NOT NULL,
                 at DATETIME NOT NULL,
                 PRIMARY KEY (id),
                 UNIQUE KEY uk_kind_at (kind, at)
             ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
             ALTER TABLE t MODIFY COLUMN kind ENUM('a', 'b', 'c') NULL;",
        );
        let Ddl::CreateTable(def) = &ddl[0] else { panic!() };
        assert_eq!(def.columns[0].ty, DataType::Int { bits: 32, signed: false });
        assert_eq!(def.columns[0].default.as_ref().unwrap().kind, DefaultKind::Identity);
        assert_eq!(def.columns[2].ty, DataType::DateTime { precision: Some(0), tz: None });
        assert_eq!(def.uniques[0].name.as_deref(), Some("uk_kind_at"));
        let Ddl::AlterTable { actions, .. } = &ddl[1] else { panic!() };
        assert!(matches!(&actions[0], AlterAction::ReplaceColumn { column, .. } if column.ty.is_nullable()));

        let ddl =
            parse(Dialect::Sqlite, "CREATE TABLE IF NOT EXISTS s (id INTEGER PRIMARY KEY, anything, n REAL NOT NULL); PRAGMA foreign_keys = ON;");
        let Ddl::CreateTable(def) = &ddl[0] else { panic!() };
        assert_eq!(def.columns[1].ty, DataType::Nullable(Box::new(DataType::String { max_len: None })));
        assert!(matches!(&ddl[1], Ddl::Ignored(w) if w == "PRAGMA FOREIGN_KEYS"));
    }
}
