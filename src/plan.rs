//! The generation plan: which tables are generated and how many rows, how tables refer
//! to each other, and how every column gets its values. Built from the compiled schema,
//! the configuration and (for tables that are referenced but not generated) rows read
//! from the database.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use crate::config::{yaml_str, ColumnConfig, Config, Distribution, MissingTables, TableTarget};
use crate::error::{Error, Result};
use crate::expr::{cast, CExpr, Compiler, EvalCtx};
use crate::generator::domain::{self, default_domain, domain_nth, domain_size, Domain, TextKind, Unique};
use crate::generator::perm::Perm;
use crate::generator::{CheckGen, ColGen, Component, ExistingRows, Mutability, Pick, ProductGen, RelGen, Row, Strategy, TableGen, World};
use crate::rng::{hash, tag};
use crate::schema::{DataType, KeyPart, Schema, Table};
use crate::sql::DefaultKind;
use crate::timeutil::{parse_duration, parse_instant, today_utc_midnight, Rfc3339, US_PER_DAY, US_PER_HOUR, US_PER_MIN, US_PER_SEC};
use crate::value::Value;

/// Bump when a change makes the same seed and configuration produce different data, so
/// a resume across versions is refused.
///
/// 2: a table that references itself points at earlier rows only.
pub const MODEL_VERSION: u32 = 2;

/// What the load does with a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Generate exactly this many rows.
    Generate(u64),
    /// Not written; read when referenced.
    Existing,
    /// Neither written nor read.
    Skip,
}

/// The plan for one table of the schema.
#[derive(Debug, Clone)]
pub struct TablePlan {
    pub name: String,
    pub mode: Mode,
    /// Why the table has this mode, when it was not configured explicitly.
    pub reason: Option<String>,
}

/// A relation by table and column names, for display and verification.
#[derive(Debug, Clone)]
pub struct RelationInfo {
    pub child: String,
    pub child_cols: Vec<String>,
    pub parent: String,
    pub parent_cols: Vec<String>,
    /// Where it was found: `config`, `ddl` or `inferred`.
    pub source: &'static str,
}

/// Everything needed to generate and load the data, validated.
pub struct Plan {
    /// The generators of every table.
    pub world: World,
    /// One entry per schema table, in schema order.
    pub tables: Vec<TablePlan>,
    pub relations: Vec<RelationInfo>,
    /// Start and end of the time window, in microseconds since the epoch.
    pub window: (i64, i64),
    pub warnings: Vec<String>,
    /// Generated tables, parents before children.
    pub order: Vec<usize>,
    /// The same tables by dependency level: a table's parents are all in earlier levels
    /// (references to itself aside).
    pub levels: Vec<Vec<usize>>,
}

/// Reads rows of tables that are referenced but not generated.
pub trait RowSource {
    /// Up to `limit` distinct rows of `columns`, in a deterministic order.
    fn read_rows(&self, table: &Table, columns: &[usize], limit: u64) -> Result<Vec<Vec<Value>>>;
}

/// What validation found: problems stop the run, warnings are reported.
#[derive(Default)]
struct Findings {
    problems: Vec<String>,
    warnings: Vec<String>,
}

/// A relation found in the configuration, the DDL or the column names, by indices.
struct RelDraft {
    child: usize,
    child_cols: Vec<usize>,
    parent: usize,
    parent_cols: Vec<usize>,
    source: &'static str,
    distribution: Distribution,
    skew: f64,
}

impl RelDraft {
    fn info(&self, schema: &Schema) -> RelationInfo {
        let (child, parent) = (&schema.tables[self.child], &schema.tables[self.parent]);
        RelationInfo {
            child: child.name.clone(),
            child_cols: self.child_cols.iter().map(|&c| child.columns[c].name.clone()).collect(),
            parent: parent.name.clone(),
            parent_cols: self.parent_cols.iter().map(|&c| parent.columns[c].name.clone()).collect(),
            source: self.source,
        }
    }
}

/// Builds and validates the plan. `rows` reads the tables that are referenced but not
/// generated; without it (no database), references to them are left unresolved.
pub fn build(config: &Config, schema: &Schema, rows: Option<&dyn RowSource>) -> Result<Plan> {
    let g = &config.generation;
    let mut found = Findings::default();
    let window = time_window(g)?;
    let tables = table_modes(config, schema, &mut found);
    let generated: Vec<bool> = tables.iter().map(|t| matches!(t.mode, Mode::Generate(_))).collect();
    let drafts = find_relations(config, schema, &generated, &mut found);
    let existing = read_referenced_rows(schema, &generated, &drafts, rows, g.max_existing_rows)?;

    let mut world = World {
        seed: g.seed,
        ctx: EvalCtx { now_us: window.1 },
        tables: vec![None; schema.tables.len()],
        relations: Vec::new(),
        products: Vec::new(),
        existing,
    };
    for d in &drafts {
        let parent_rows = match tables[d.parent].mode {
            Mode::Generate(n) => n,
            _ => world.existing[d.parent].as_ref().map_or(0, |e| e.rows.len() as u64),
        };
        world.relations.push(relation_gen(d, schema, parent_rows));
    }
    for (ti, tp) in tables.iter().enumerate() {
        if let Mode::Generate(n) = tp.mode {
            world.tables[ti] = Some(build_table(config, schema, ti, n, window, &mut world, &mut found));
        }
    }
    make_referenced_keys_unique(&mut world, g.seed, window);
    resolve_references_to_empty_tables(&mut world, schema, &tables, rows.is_some(), &mut found);
    check_reference_ranges(&world, &mut found.problems);
    detect_cycles(&world, schema, &mut found.problems);
    if !found.problems.is_empty() {
        return Err(Error::Validation(found.problems.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n")));
    }

    let relations = drafts.iter().map(|d| d.info(schema)).collect();
    let levels = load_levels(&world, &tables);
    let order = levels.concat();
    Ok(Plan { world, tables, relations, window, warnings: found.warnings, order, levels })
}

/// `history_days` ending at `end_time` (default: today 00:00 UTC), in microseconds.
fn time_window(g: &crate::config::Generation) -> Result<(i64, i64)> {
    let end = match &g.end_time {
        Some(s) => parse_instant(s).map_err(|e| Error::Config(format!("generation.end_time: {e}")))?,
        None => today_utc_midnight(),
    };
    Ok((end - i64::from(g.history_days) * US_PER_DAY, end))
}

/// What happens to each table of the schema, from `tables` or, for tables it does not
/// list, from what the DDL says about them.
fn table_modes(config: &Config, schema: &Schema, found: &mut Findings) -> Vec<TablePlan> {
    for name in config.tables.keys() {
        if schema.table(name).is_none() {
            found.problems.push(format!("tables.{name}: no such table in the schema{}", suggest(name, schema.tables.iter().map(|t| t.name.as_str()))));
        }
    }
    let mut missing = Vec::new();
    let mut tables = Vec::new();
    for t in &schema.tables {
        let (mode, reason) = match config.tables.get(&t.name) {
            Some(TableTarget::Rows(0) | TableTarget::Skip) => (Mode::Skip, None),
            Some(TableTarget::Rows(n)) => {
                if !t.storage {
                    found.problems.push(format!("tables.{}: engine {} does not store rows", t.name, t.engine.as_deref().unwrap_or("?")));
                }
                if let Some(view) = &t.derived_by {
                    found.warnings.push(format!("table {}: materialized view {view} also writes into it", t.name));
                }
                (Mode::Generate(*n), None)
            }
            Some(TableTarget::Existing) => (Mode::Existing, None),
            None if t.seeded => (Mode::Existing, Some("seeded by the DDL input".to_string())),
            None if t.derived_by.is_some() => (Mode::Existing, Some(format!("written by materialized view {}", t.derived_by.as_deref().unwrap_or("")))),
            None if !t.storage => (Mode::Skip, Some("does not store rows".to_string())),
            None => match config.generation.default_rows {
                Some(0) => (Mode::Skip, Some("default_rows is 0".to_string())),
                Some(n) => (Mode::Generate(n), Some("default_rows".to_string())),
                None => {
                    missing.push(t.name.clone());
                    (Mode::Existing, Some("not in the configuration".to_string()))
                }
            },
        };
        tables.push(TablePlan { name: t.name.clone(), mode, reason });
    }
    if !missing.is_empty() {
        let msg = format!("tables not in the configuration, left as they are: {}", missing.join(", "));
        match config.generation.missing_tables {
            MissingTables::Warn => found.warnings.push(msg),
            MissingTables::Error => found.problems.push(msg),
        }
    }
    tables
}

/// The relations of generated tables: configured first, then DDL foreign keys, then
/// inferred from names, each skipping child columns an earlier source covers.
fn find_relations(config: &Config, schema: &Schema, generated: &[bool], found: &mut Findings) -> Vec<RelDraft> {
    let mut drafts: Vec<RelDraft> = Vec::new();
    let mut covered: HashSet<(usize, usize)> = HashSet::new();
    let ignored: HashSet<(&str, &str)> =
        config.ignore_relations.iter().flat_map(|r| r.columns.iter().map(move |c| (r.table.as_str(), c.as_str()))).collect();
    let problems = &mut found.problems;

    for rc in &config.relations {
        let (Some(ci), Some(pi)) = (schema.table_index(&rc.from.table), schema.table_index(&rc.to.table)) else {
            problems.push(format!("relation {} -> {}: no such table", rc.from, rc.to));
            continue;
        };
        let child_cols = resolve_cols(&schema.tables[ci], &rc.from.columns, problems, &format!("relation {}", rc.from));
        let parent_cols = resolve_cols(&schema.tables[pi], &rc.to.columns, problems, &format!("relation {}", rc.to));
        let (Some(child_cols), Some(parent_cols)) = (child_cols, parent_cols) else { continue };
        for (c, p) in child_cols.iter().zip(&parent_cols) {
            let (ct, pt) = (&schema.tables[ci].columns[*c].ty, &schema.tables[pi].columns[*p].ty);
            if !compatible(ct, pt) {
                problems.push(format!("relation {} -> {}: {ct} and {pt} are not compatible", rc.from, rc.to));
            }
        }
        if generated[ci] {
            covered.extend(child_cols.iter().map(|&c| (ci, c)));
            drafts.push(RelDraft { child: ci, child_cols, parent: pi, parent_cols, source: "config", distribution: rc.distribution, skew: rc.skew });
        }
    }

    for (ci, t) in schema.tables.iter().enumerate().filter(|&(ci, _)| generated[ci]) {
        let is_ignored = |c: usize| ignored.contains(&(t.name.as_str(), t.columns[c].name.as_str()));
        for fk in &t.foreign_keys {
            let Some(pi) = schema.table_index(&fk.ref_table) else {
                problems.push(format!("table {}: foreign key to {}, which the schema does not define", t.name, fk.ref_table));
                continue;
            };
            let parent = &schema.tables[pi];
            let ref_cols: Vec<String> = if fk.ref_columns.is_empty() {
                match parent.primary_key() {
                    Some(pk) => pk.iter().map(|s| s.to_string()).collect(),
                    None => {
                        problems.push(format!("table {}: foreign key to {} names no columns and {} has no primary key", t.name, parent.name, parent.name));
                        continue;
                    }
                }
            } else {
                fk.ref_columns.clone()
            };
            let ctx = format!("table {} foreign key", t.name);
            let (Some(child_cols), Some(parent_cols)) = (resolve_cols(t, &fk.columns, problems, &ctx), resolve_cols(parent, &ref_cols, problems, &ctx)) else {
                continue;
            };
            if child_cols.iter().any(|&c| covered.contains(&(ci, c)) || is_ignored(c)) {
                continue;
            }
            covered.extend(child_cols.iter().map(|&c| (ci, c)));
            drafts.push(RelDraft { child: ci, child_cols, parent: pi, parent_cols, source: "ddl", distribution: Distribution::Uniform, skew: 1.5 });
        }
        if config.generation.infer_relations {
            drafts.extend(infer_relations(config, schema, ci, &mut covered, &is_ignored));
        }
    }
    drafts
}

/// Columns of one table inferred to reference the same parent in the same role.
struct InferredRef {
    parent: usize,
    role: String,
    /// `(child column, parent column)`.
    pairs: Vec<(usize, usize)>,
}

/// The relations of table `ci` that its column names suggest. Columns naming the same
/// parent in the same role (`patient_key` and `patient_id`, but not
/// `acknowledger_staff_key` and `resolver_staff_key`) form one reference, so they point
/// at the same parent row.
fn infer_relations(config: &Config, schema: &Schema, ci: usize, covered: &mut HashSet<(usize, usize)>, is_ignored: &dyn Fn(usize) -> bool) -> Vec<RelDraft> {
    let t = &schema.tables[ci];
    let mut refs: Vec<InferredRef> = Vec::new();
    for (c, col) in t.columns.iter().enumerate() {
        if covered.contains(&(ci, c)) || is_ignored(c) || !col.insertable() || col.fixed.is_some() {
            continue;
        }
        if config.column(&t.name, &col.name).is_some_and(|cc| cc.values.is_some() || cc.value.is_some() || cc.range.is_some()) {
            continue;
        }
        let Some((pi, pc, role)) = infer_relation(schema, ci, c) else { continue };
        match refs.iter_mut().find(|r| r.parent == pi && r.role == role) {
            Some(r) if r.pairs.iter().any(|&(_, p)| p == pc) => {}
            Some(r) => {
                r.pairs.push((c, pc));
                covered.insert((ci, c));
            }
            None => {
                refs.push(InferredRef { parent: pi, role, pairs: vec![(c, pc)] });
                covered.insert((ci, c));
            }
        }
    }
    refs.into_iter()
        .map(|r| RelDraft {
            child: ci,
            child_cols: r.pairs.iter().map(|p| p.0).collect(),
            parent: r.parent,
            parent_cols: r.pairs.iter().map(|p| p.1).collect(),
            source: "inferred",
            distribution: Distribution::Uniform,
            skew: 1.5,
        })
        .collect()
}

/// Reads the rows that generated tables reference in tables that are not generated.
fn read_referenced_rows(
    schema: &Schema,
    generated: &[bool],
    drafts: &[RelDraft],
    rows: Option<&dyn RowSource>,
    limit: u64,
) -> Result<Vec<Option<ExistingRows>>> {
    let mut needed: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for d in drafts.iter().filter(|d| !generated[d.parent]) {
        needed.entry(d.parent).or_default().extend(d.parent_cols.iter().copied());
    }
    let mut existing: Vec<Option<ExistingRows>> = vec![None; schema.tables.len()];
    for (pi, cols) in needed {
        let columns: Vec<usize> = cols.into_iter().collect();
        let data = match rows {
            Some(src) => src.read_rows(&schema.tables[pi], &columns, limit)?,
            None => Vec::new(),
        };
        existing[pi] = Some(ExistingRows { columns, rows: data });
    }
    Ok(existing)
}

/// The generator of a relation: how a child row picks its parent row.
fn relation_gen(d: &RelDraft, schema: &Schema, parent_rows: u64) -> RelGen {
    let rel_tag = hash(&[tag(&schema.tables[d.child].name), tag(&schema.tables[d.parent].name), d.child_cols[0] as u64, 0x72656c]);
    let pick = match d.distribution {
        // Earlier rows only: hierarchies stay acyclic, and rows loaded in order satisfy
        // the foreign key.
        _ if d.child == d.parent => Pick::Earlier,
        Distribution::Uniform => Pick::Uniform,
        Distribution::Skewed => Pick::Skewed { skew: d.skew, perm: Perm::new(parent_rows.max(1), rel_tag) },
    };
    RelGen {
        child: d.child,
        parent: d.parent,
        child_cols: d.child_cols.clone(),
        parent_cols: d.parent_cols.clone(),
        parent_rows,
        pick,
        tag: rel_tag,
    }
}

/// A unique parent key makes references unambiguous: referenced single columns that the
/// plan left random become unique.
fn make_referenced_keys_unique(world: &mut World, seed: u64, window: (i64, i64)) {
    for r in 0..world.relations.len() {
        let (parent, cols) = (world.relations[r].parent, &world.relations[r].parent_cols);
        let &[col] = cols.as_slice() else { continue };
        let Some(tg) = world.tables[parent].as_mut() else { continue };
        if !matches!(tg.cols[col].strategy, Strategy::Random(_)) || tg.checks.iter().any(|c| c.expr.references(col)) {
            continue;
        }
        if let Some(u) = make_unique(&tg.cols[col], tg.rows, seed, tg.tag, window) {
            tg.cols[col].describe = describe_unique(&u);
            tg.cols[col].strategy = Strategy::Unique(u);
            tg.cols[col].mutability = Mutability::Fixed;
            tg.cols[col].pure = true;
        }
    }
}

/// References to tables without rows: left NULL when the columns allow it, a problem
/// otherwise. Without a database (`have_rows` false) tables that are not generated have
/// not been read, so this is only noted.
fn resolve_references_to_empty_tables(world: &mut World, schema: &Schema, tables: &[TablePlan], have_rows: bool, found: &mut Findings) {
    for rel in world.relations.iter().filter(|r| r.parent_rows == 0) {
        let (child, parent) = (&schema.tables[rel.child], &schema.tables[rel.parent]);
        let tg = world.tables[rel.child].as_mut().expect("child is generated");
        let what = if matches!(tables[rel.parent].mode, Mode::Generate(_)) || have_rows { "has no rows" } else { "is not read without a database" };
        if rel.child_cols.iter().all(|&c| tg.cols[c].nullable) {
            for &c in &rel.child_cols {
                tg.cols[c].strategy = Strategy::Constant(Value::Null);
                tg.cols[c].mutability = Mutability::Fixed;
                tg.cols[c].describe = format!("NULL ({} {what})", parent.name);
            }
            if have_rows {
                found.warnings.push(format!("{}.{} references {}, which {what}: the column is left NULL", child.name, names(child, &rel.child_cols), parent.name));
            }
        } else if have_rows {
            found.problems.push(format!(
                "{}.{} references {}, which {what}, and is not nullable: give {} a row count, or remove the relation",
                child.name,
                names(child, &rel.child_cols),
                parent.name,
                parent.name
            ));
        }
    }
}

/// References must fit the child column: a parent sequence can outgrow a narrower type.
fn check_reference_ranges(world: &World, problems: &mut Vec<String>) {
    for rel in &world.relations {
        let Some(ptg) = world.tables[rel.parent].as_ref() else { continue };
        let ctg = world.table(rel.child);
        for (&pc, &cc) in rel.parent_cols.iter().zip(&rel.child_cols) {
            if let (Strategy::Unique(Unique::Int { start, step }), Some((lo, hi))) = (&ptg.cols[pc].strategy, ctg.cols[cc].ty.int_range()) {
                let last = start + step * (ptg.rows.max(1) as i128 - 1);
                if *start < lo || last > hi {
                    problems.push(format!(
                        "{}.{} ({}) cannot hold the keys of {}.{}, which go up to {last}",
                        ctg.name, ctg.cols[cc].name, ctg.cols[cc].ty, ptg.name, ptg.cols[pc].name
                    ));
                }
            }
        }
    }
}

fn names(t: &Table, cols: &[usize]) -> String {
    let n: Vec<&str> = cols.iter().map(|&c| t.columns[c].name.as_str()).collect();
    if n.len() == 1 {
        n[0].to_string()
    } else {
        format!("({})", n.join(", "))
    }
}

fn resolve_cols(t: &Table, cols: &[String], problems: &mut Vec<String>, ctx: &str) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    for c in cols {
        match t.column_index(c) {
            Some(i) => out.push(i),
            None => {
                problems.push(format!("{ctx}: table {} has no column {c}{}", t.name, suggest(c, t.columns.iter().map(|c| c.name.as_str()))));
                return None;
            }
        }
    }
    Some(out)
}

/// ` (did you mean X?)` when one of `candidates` is close to `name`.
pub fn suggest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> String {
    let best = candidates.map(|c| (strsim::jaro_winkler(name, c), c)).filter(|(s, _)| *s > 0.85).max_by(|a, b| a.0.total_cmp(&b.0));
    best.map_or(String::new(), |(_, c)| format!(" (did you mean {c}?)"))
}

fn compatible(child: &DataType, parent: &DataType) -> bool {
    use DataType::*;
    matches!(
        (child.base(), parent.base()),
        (Int { .. }, Int { .. })
            | (Uuid, Uuid)
            | (String { .. } | FixedString(_) | Enum { .. }, String { .. } | FixedString(_) | Enum { .. })
            | (Date | Date32 | DateTime { .. }, Date | Date32 | DateTime { .. })
            | (Decimal { .. } | Float { .. }, Decimal { .. } | Int { .. } | Float { .. })
            | (Int { .. }, Decimal { scale: 0, .. })
            | (Bool, Bool)
            | (Ipv4, Ipv4)
            | (Ipv6, Ipv6)
    )
}

/// `customer_id` -> a table named `customer(s)` (with or without `dim_`, `fact_`...
/// prefixes) and its column `customer_id` or `id`; the longest matching suffix of the name
/// wins, so `acknowledger_staff_key` finds `dim_staff.staff_key` in the role
/// `acknowledger`. Returns the parent table, its column, and the role.
fn infer_relation(schema: &Schema, ci: usize, c: usize) -> Option<(usize, usize, String)> {
    let child = &schema.tables[ci];
    let col = &child.columns[c];
    let name = col.name.to_ascii_lowercase();
    let (stem, suffix) = ["_id", "_key", "_uuid", "_code"].iter().find_map(|s| name.strip_suffix(s).map(|st| (st.to_string(), &s[1..])))?;
    let tokens: Vec<&str> = stem.split('_').filter(|t| !t.is_empty()).collect();
    for k in 0..tokens.len() {
        let cand = tokens[k..].join("_");
        let matches: Vec<usize> = schema
            .tables
            .iter()
            .enumerate()
            .filter(|(pi, p)| *pi != ci && p.storage && {
                let n = domain::strip_table_prefix(&p.name.to_ascii_lowercase());
                n == cand || domain::singular(&n) == cand
            })
            .map(|(pi, _)| pi)
            .collect();
        if matches.len() != 1 {
            continue;
        }
        let pi = matches[0];
        let parent = &schema.tables[pi];
        let wanted = [format!("{cand}_{suffix}"), name.clone(), suffix.to_string()];
        let pc = wanted.iter().find_map(|w| parent.columns.iter().position(|pcol| pcol.name.eq_ignore_ascii_case(w))).or_else(|| {
            let pk = parent.primary_key()?;
            if pk.len() == 1 {
                parent.column_index(pk[0])
            } else {
                None
            }
        })?;
        if compatible(&col.ty, &parent.columns[pc].ty) {
            return Some((pi, pc, tokens[..k].join("_")));
        }
        return None;
    }
    None
}

/// The value of a configured literal for a column of type `ty`.
fn yaml_value(y: &serde_yaml_ng::Value, ty: &DataType) -> std::result::Result<Value, String> {
    use serde_yaml_ng::Value as Y;
    let raw = match y {
        Y::Null => return Ok(Value::Null),
        Y::Bool(b) => Value::Bool(*b),
        Y::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i as i128),
            None => match n.as_u64() {
                Some(u) => Value::Int(u as i128),
                None => crate::expr::parse_number(&n.to_string()).unwrap_or(Value::Float(n.as_f64().unwrap_or(0.0))),
            },
        },
        Y::String(s) => Value::Str(s.clone()),
        other => return Err(format!("{} is not a scalar", yaml_str(other))),
    };
    if let DataType::Enum { variants, .. } = ty.base() {
        let s = match &raw {
            Value::Str(s) => s.clone(),
            other => other.to_string(),
        };
        return if variants.iter().any(|(n, _)| *n == s) { Ok(Value::Str(s)) } else { Err(format!("'{s}' is not a value of {ty}")) };
    }
    let v = cast(raw.clone(), ty);
    if v.is_null() {
        return Err(format!("{} is not a valid {}", yaml_str(y), ty.base()));
    }
    Ok(v)
}

fn configured_domain(cfg: &ColumnConfig, ty: &DataType, window: (i64, i64)) -> std::result::Result<Option<Domain>, String> {
    if let Some(values) = &cfg.values {
        let vals = values.iter().map(|y| yaml_value(y, ty)).collect::<std::result::Result<Vec<_>, _>>()?;
        if vals.is_empty() {
            return Err("values is empty".into());
        }
        return Ok(Some(Domain::values(vals, cfg.weights.as_deref())));
    }
    if let Some([a, b]) = &cfg.range {
        let (lo, hi) = (yaml_value(a, ty)?, yaml_value(b, ty)?);
        if lo.compare(&hi) == Some(std::cmp::Ordering::Greater) {
            return Err("range: the first bound is above the second".into());
        }
        let d = match (ty.base(), lo, hi) {
            (DataType::Int { .. }, Value::Int(l), Value::Int(h)) => Domain::Int { lo: l, hi: h },
            (DataType::Decimal { scale, .. }, l, h) => Domain::Decimal {
                lo: domain::scaled(&l, *scale).ok_or("bad bound")?,
                hi: domain::scaled(&h, *scale).ok_or("bad bound")?,
                scale: *scale,
            },
            (DataType::Float { .. }, l, h) => Domain::Float { lo: l.as_f64().unwrap_or(0.0), hi: h.as_f64().unwrap_or(0.0) },
            (DataType::Date | DataType::Date32, Value::Date(l), Value::Date(h)) => Domain::Date { lo: l, hi: h },
            (DataType::DateTime { precision, .. }, Value::Time(l), Value::Time(h)) => Domain::Time { lo: l, hi: h, precision: *precision },
            (other, _, _) => return Err(format!("range is not supported for {other}")),
        };
        return Ok(Some(d));
    }
    if let Some(kind) = &cfg.kind {
        let k = TextKind::parse(kind).ok_or_else(|| format!("unknown kind '{kind}'"))?;
        return match ty.base() {
            DataType::String { max_len } => Ok(Some(Domain::Text { kind: k, max_len: *max_len, pool: None })),
            DataType::Uuid if k == TextKind::Uuid => Ok(Some(Domain::Uuid)),
            other => Err(format!("kind applies to text columns, not {other}")),
        };
    }
    if let Some([lo, hi]) = cfg.length {
        let mut d = default_domain(ty, "", "", window);
        match &mut d {
            Domain::Array { min, max, .. } | Domain::Map { min, max, .. } => {
                *min = lo;
                *max = hi.max(lo);
            }
            _ => return Err("length applies to Array and Map columns".into()),
        }
        return Ok(Some(d));
    }
    Ok(None)
}

fn build_table(
    config: &Config,
    schema: &Schema,
    ti: usize,
    rows: u64,
    window: (i64, i64),
    world: &mut World,
    found: &mut Findings,
) -> TableGen {
    let g = &config.generation;
    let t = &schema.tables[ti];
    let table_tag = tag(&t.name);
    let resolve = |n: &str| t.column_index(n);
    let compiler = Compiler { resolve: &resolve, rules: schema.dialect.type_rules() };

    // Checks dbgen can evaluate.
    let mut checks: Vec<(String, String, CExpr)> = Vec::new();
    for ch in &t.checks {
        match compiler.compile(&ch.expr) {
            Ok(e) => checks.push((ch.name.clone(), ch.sql.clone(), e)),
            Err(m) => found.warnings.push(format!("table {}: CHECK {} is not evaluated by dbgen ({m}); the database may reject rows", t.name, ch.name)),
        }
    }
    let mut check_cols = BTreeSet::new();
    checks.iter().for_each(|(_, _, e)| e.columns(&mut check_cols));
    let key_cols: BTreeSet<usize> = t
        .keys
        .iter()
        .flat_map(|k| k.parts.iter())
        .filter_map(|p| match p {
            KeyPart::Column(c) => t.column_index(c),
            KeyPart::Expr(_) => None,
        })
        .collect();
    let parent_ref_cols: BTreeSet<usize> = world.relations.iter().filter(|r| r.parent == ti).flat_map(|r| r.parent_cols.iter().copied()).collect();
    let child_ref: HashMap<usize, (usize, usize)> = world
        .relations
        .iter()
        .enumerate()
        .filter(|(_, r)| r.child == ti)
        .flat_map(|(ri, r)| r.child_cols.iter().enumerate().map(move |(part, &c)| (c, (ri, part))))
        .collect();

    let mut cols: Vec<ColGen> = Vec::with_capacity(t.columns.len());
    let mut configured: Vec<bool> = Vec::new();
    for (ci, col) in t.columns.iter().enumerate() {
        let cfg = config.column(&t.name, &col.name);
        let nullable = col.ty.is_nullable();
        let null_rate = if nullable { cfg.and_then(|c| c.null_rate).unwrap_or(g.null_rate) } else { 0.0 };
        let mut inserted = true;
        let mut is_configured = false;
        let kind = col.default.as_ref().map(|d| d.kind);
        let strategy = if !col.insertable() || kind == Some(DefaultKind::Ephemeral) {
            inserted = false;
            match col.default.as_ref().and_then(|d| d.expr.as_ref()) {
                Some(e) if check_cols.contains(&ci) && kind != Some(DefaultKind::Ephemeral) => match compiler.compile(e) {
                    Ok(c) => Strategy::Computed(c),
                    Err(_) => Strategy::Skip,
                },
                _ => Strategy::Skip,
            }
        } else if cfg.and_then(|c| c.generate) == Some(false) {
            inserted = false;
            Strategy::Skip
        } else if let Some(v) = col.fixed {
            Strategy::Constant(cast(Value::Int(v as i128), &col.ty))
        } else if let Some(v) = cfg.and_then(|c| c.value.as_ref()) {
            is_configured = true;
            match yaml_value(v, &col.ty) {
                Ok(v) => Strategy::Constant(v),
                Err(e) => {
                    found.problems.push(format!("columns.{}.{}: {e}", t.name, col.name));
                    Strategy::Skip
                }
            }
        } else if let Some(&(rel, part)) = child_ref.get(&ci) {
            Strategy::Ref { rel, part }
        } else if col.has_default()
            && !g.fill_defaults
            && cfg.is_none()
            && !check_cols.contains(&ci)
            && !key_cols.contains(&ci)
            && !parent_ref_cols.contains(&ci)
        {
            inserted = false;
            Strategy::Skip
        } else if let Some(u) = col.ty.unsupported() {
            if nullable {
                found.warnings.push(format!("{}.{}: type {u} is not generated; the column is left NULL", t.name, col.name));
                Strategy::Constant(Value::Null)
            } else if col.default.is_some() {
                inserted = false;
                Strategy::Skip
            } else {
                found.problems.push(format!("{}.{}: type {u} is not supported and the column has no default", t.name, col.name));
                Strategy::Skip
            }
        } else {
            let d = match cfg.map(|c| configured_domain(c, &col.ty, window)).transpose() {
                Ok(Some(Some(d))) => {
                    is_configured = true;
                    d
                }
                Ok(_) => default_domain(&col.ty, &t.name, &col.name, window),
                Err(e) => {
                    found.problems.push(format!("columns.{}.{}: {e}", t.name, col.name));
                    default_domain(&col.ty, &t.name, &col.name, window)
                }
            };
            Strategy::Random(d)
        };
        configured.push(is_configured);
        cols.push(ColGen {
            name: col.name.clone(),
            ty: col.ty.clone(),
            tag: tag(&col.name),
            strategy,
            inserted,
            nullable,
            null_rate,
            mutability: Mutability::Fixed,
            hints: Vec::new(),
            pure: true,
            describe: String::new(),
        });
    }
    // Unknown columns in the configuration.
    for key in config.columns.keys() {
        if let Some((tn, cn)) = key.rsplit_once('.')
            && tn == t.name && t.column(cn).is_none() {
                found.problems.push(format!("columns.{key}: table {} has no column {cn}{}", t.name, suggest(cn, t.columns.iter().map(|c| c.name.as_str()))));
            }
    }

    // What the checks say about single columns.
    for (_, _, e) in &checks {
        let mut hints = Vec::new();
        e.literal_hints(&mut hints);
        for (c, v) in hints {
            let v = cast(v, &cols[c].ty);
            if !v.is_null() && !cols[c].hints.contains(&v) {
                cols[c].hints.push(v);
            }
        }
        for conj in e.conjuncts() {
            if let Some((c, vals)) = conj.closed_domain()
                && let Strategy::Random(_) = cols[c].strategy
                    && !configured[c] {
                        let vals: Vec<Value> = vals.into_iter().map(|v| cast(v, &cols[c].ty)).filter(|v| !v.is_null()).collect();
                        if !vals.is_empty() {
                            cols[c].strategy = Strategy::Random(Domain::values(vals, None));
                        }
                    }
            for (c, op, v) in conj.bounds() {
                let col = &mut cols[c];
                if let Strategy::Random(d) = &mut col.strategy {
                    d.narrow(op, &v, &col.ty);
                }
            }
        }
    }
    // A text column compared with literals is categorical: draw from those literals.
    for (c, col) in cols.iter_mut().enumerate() {
        if configured[c] || col.hints.is_empty() {
            continue;
        }
        if let Strategy::Random(Domain::Text { .. }) = col.strategy {
            col.strategy = Strategy::Random(Domain::values(col.hints.clone(), None));
        }
    }

    // Orderings: `end > start` makes `end` follow `start` by a positive gap.
    let mut deps: HashMap<usize, Vec<usize>> = HashMap::new();
    let span = window.1 - window.0;
    for (ci, col) in t.columns.iter().enumerate() {
        let Some(cfg) = config.column(&t.name, &col.name) else { continue };
        let Some(after) = &cfg.after else { continue };
        let Some(base) = t.column_index(after) else {
            found.problems.push(format!("columns.{}.{}.after: no column {after}", t.name, col.name));
            continue;
        };
        let (min_gap, max_gap) = match &cfg.gap {
            Some([a, b]) => match (parse_duration(a), parse_duration(b)) {
                (Ok(a), Ok(b)) => (a, b.max(a)),
                (Err(e), _) | (_, Err(e)) => {
                    found.problems.push(format!("columns.{}.{}.gap: {e}", t.name, col.name));
                    (US_PER_MIN, US_PER_HOUR)
                }
            },
            None => (US_PER_MIN, (span / 20).max(US_PER_HOUR)),
        };
        if !col.ty.is_temporal() || !t.columns[base].ty.is_temporal() {
            found.problems.push(format!("columns.{}.{}.after: both columns must be dates or timestamps", t.name, col.name));
            continue;
        }
        set_after(&mut cols[ci], base, min_gap, max_gap);
        deps.insert(ci, vec![base]);
    }
    for (_, _, e) in &checks {
        for conj in e.conjuncts() {
            let Some((later, earlier, strict)) = conj.ordering() else { continue };
            if configured[later] || !matches!(cols[later].strategy, Strategy::Random(Domain::Time { .. } | Domain::Date { .. })) {
                continue;
            }
            if !cols[earlier].ty.is_temporal() || depends_on(&deps, earlier, later) {
                continue;
            }
            let unit = match cols[later].ty.base() {
                DataType::Date | DataType::Date32 => US_PER_DAY,
                _ => US_PER_SEC,
            };
            let min_gap = if strict { unit.max(US_PER_MIN) } else { US_PER_MIN };
            set_after(&mut cols[later], earlier, min_gap, (span / 20).max(min_gap * 2));
            deps.insert(later, vec![earlier]);
        }
    }
    // Equalities: `c = expr` (or a linear sum) computes `c` from the rest of the row.
    for (_, _, e) in &checks {
        for conj in e.conjuncts() {
            let mut candidates = conj.assignments();
            candidates.reverse();
            for (c, rhs) in candidates {
                if configured[c] || key_cols.contains(&c) || parent_ref_cols.contains(&c) || !matches!(cols[c].strategy, Strategy::Random(_)) {
                    continue;
                }
                let mut used = BTreeSet::new();
                rhs.columns(&mut used);
                if used.iter().any(|&u| u == c || depends_on(&deps, u, c)) {
                    continue;
                }
                cols[c].strategy = Strategy::Derived(rhs);
                deps.insert(c, used.into_iter().collect());
                break;
            }
        }
    }
    for (c, col) in cols.iter().enumerate() {
        if let Strategy::Computed(e) = &col.strategy {
            let mut used = BTreeSet::new();
            e.columns(&mut used);
            deps.insert(c, used.into_iter().collect());
        }
    }

    // Uniqueness: one column per key distinct by construction, or the key's combinations
    // enumerated without repeats when every part takes few values.
    for key in &t.keys {
        let parts: Vec<Option<usize>> = key
            .parts
            .iter()
            .map(|p| match p {
                KeyPart::Column(c) => t.column_index(c),
                KeyPart::Expr(_) => None,
            })
            .collect();
        let cols_in_key: Vec<usize> = parts.iter().flatten().copied().collect();
        let already = cols_in_key.iter().any(|&c| matches!(cols[c].strategy, Strategy::Unique(_)) || is_injective(world, &cols[c].strategy));
        if already || rows <= 1 {
            continue;
        }
        let mut candidates: Vec<usize> = cols_in_key
            .iter()
            .copied()
            .filter(|&c| matches!(cols[c].strategy, Strategy::Random(_)) && !configured[c] && !cols[c].nullable)
            .filter(|&c| make_unique(&cols[c], rows, g.seed, table_tag, window).is_some())
            .collect();
        candidates.sort_by_key(|&c| carrier_rank(&cols[c]));
        if let Some(&c) = candidates.first() {
            let u = make_unique(&cols[c], rows, g.seed, table_tag, window).expect("checked");
            cols[c].strategy = Strategy::Unique(u);
            continue;
        }
        // Enumerate the combinations of finite parts.
        if parts.iter().any(Option::is_none) {
            found.warnings.push(format!("table {}: {} has expression parts; dbgen cannot guarantee it is unique", t.name, key.name));
            continue;
        }
        let mut comps: Vec<(Component, Vec<usize>)> = Vec::new();
        let mut finite = true;
        for &c in &cols_in_key {
            match &cols[c].strategy {
                Strategy::Ref { rel, .. } => {
                    if !comps.iter().any(|(comp, _)| matches!(comp, Component::Relation(r) if r == rel)) {
                        comps.push((Component::Relation(*rel), world.relations[*rel].child_cols.clone()));
                    }
                }
                Strategy::Random(d) if !cols[c].nullable && domain_size(d).is_some_and(|s| s <= u32::MAX as u128) => {
                    let n = domain_size(d).unwrap() as u64;
                    comps.push((Component::Values((0..n).map(|k| domain_nth(d, k)).collect()), vec![c]));
                }
                Strategy::Constant(_) => {}
                _ => finite = false,
            }
        }
        if !finite || comps.is_empty() {
            found.warnings.push(format!("table {}: dbgen cannot guarantee that {} is unique", t.name, key.name));
            continue;
        }
        let radices: Vec<u64> = comps
            .iter()
            .map(|(comp, _)| match comp {
                Component::Relation(r) => world.relations[*r].parent_rows,
                Component::Values(v) => v.len() as u64,
            })
            .collect();
        let total = radices.iter().try_fold(1u128, |acc, &r| acc.checked_mul(r as u128));
        let combos = total.unwrap_or(u128::MAX);
        if (rows as u128) > combos {
            found.problems.push(format!("table {}: {} rows requested, but {} allows only {combos} distinct combinations", t.name, rows, key.name));
            continue;
        }
        let key_idx = world.products.len();
        world.products.push(ProductGen {
            perm: Perm::new(combos.min(u64::MAX as u128) as u64, hash(&[g.seed, table_tag, key_idx as u64])),
            radices,
            comps: comps.iter().map(|(c, _)| c.clone()).collect(),
        });
        for (comp_idx, (comp, cols_of)) in comps.iter().enumerate() {
            match comp {
                Component::Relation(r) => world.relations[*r].pick = Pick::Product { key: key_idx, comp: comp_idx },
                Component::Values(_) => {
                    for &c in cols_of {
                        cols[c].strategy = Strategy::Product { key: key_idx, comp: comp_idx, part: 0 };
                    }
                }
            }
        }
    }
    // Identifier-looking columns are unique, as identifiers usually are.
    for c in 0..cols.len() {
        let n = cols[c].name.to_ascii_lowercase();
        let id_like = n == "id" || n.ends_with("_id") || n.ends_with("_key") || n.ends_with("_uuid");
        if id_like && !configured[c] && !cols[c].nullable && matches!(cols[c].strategy, Strategy::Random(Domain::Int { .. } | Domain::Uuid)) && !check_cols.contains(&c)
            && let Some(u) = make_unique(&cols[c], rows, g.seed, table_tag, window) {
                cols[c].strategy = Strategy::Unique(u);
            }
        if config.column(&t.name, &cols[c].name).and_then(|cc| cc.unique) == Some(true) && !matches!(cols[c].strategy, Strategy::Unique(_)) {
            match make_unique(&cols[c], rows, g.seed, table_tag, window) {
                Some(u) => cols[c].strategy = Strategy::Unique(u),
                None => found.problems.push(format!("columns.{}.{}: cannot make {} distinct values of {}", t.name, cols[c].name, rows, cols[c].ty)),
            }
        }
    }
    // The partitioning column follows row order, so a batch touches few partitions.
    if let Some(p) = &t.partition_by
        && let Ok(e) = compiler.compile(&p.expr) {
            let mut used = BTreeSet::new();
            e.columns(&mut used);
            if used.len() == 1 {
                let c = *used.iter().next().unwrap();
                if !configured[c] {
                    if let Strategy::Random(Domain::Time { lo, hi, precision }) = cols[c].strategy {
                        cols[c].strategy = Strategy::Random(Domain::Ordered { lo, hi: hi + 1, rows, precision, date: false });
                    } else if let Strategy::Random(Domain::Date { lo, hi }) = cols[c].strategy {
                        cols[c].strategy = Strategy::Random(Domain::Ordered {
                            lo: lo as i64 * US_PER_DAY,
                            hi: (hi as i64 + 1) * US_PER_DAY,
                            rows,
                            precision: None,
                            date: true,
                        });
                    }
                }
            }
        }

    // Mutability, purity, repairable columns, evaluation order.
    for (c, col) in cols.iter_mut().enumerate() {
        col.mutability = match &col.strategy {
            Strategy::Random(_) | Strategy::After { .. } => Mutability::Full,
            Strategy::Ref { .. } if col.nullable => Mutability::NullOnly,
            _ => Mutability::Fixed,
        };
        col.pure = !check_cols.contains(&c)
            && !matches!(col.strategy, Strategy::After { .. } | Strategy::Derived(_) | Strategy::Computed(_));
        col.describe = describe(col, world, schema, ti);
    }
    let check_gens: Vec<CheckGen> = checks
        .into_iter()
        .map(|(name, sql, expr)| {
            let mut used = BTreeSet::new();
            expr.columns(&mut used);
            // Columns that feed a derived column used by the check can repair it too.
            let mut movable = BTreeSet::new();
            let mut stack: Vec<usize> = used.into_iter().collect();
            while let Some(c) = stack.pop() {
                if cols[c].mutability != Mutability::Fixed {
                    movable.insert(c);
                }
                if let Some(d) = deps.get(&c) {
                    stack.extend(d.iter().copied().filter(|x| !movable.contains(x)));
                }
            }
            CheckGen { name, sql, expr, movable: movable.into_iter().collect() }
        })
        .collect();
    let dependents = topo_order(&deps, cols.len());
    let insert: Vec<usize> = (0..cols.len()).filter(|&c| cols[c].inserted).collect();
    if insert.is_empty() {
        found.problems.push(format!("table {}: no column to insert", t.name));
    }
    TableGen {
        index: ti,
        name: t.name.clone(),
        rows,
        tag: table_tag,
        seed: g.seed,
        cols,
        insert,
        checks: check_gens,
        dependents,
        max_steps: g.max_repair_steps,
    }
}

fn set_after(col: &mut ColGen, base: usize, min_gap: i64, max_gap: i64) {
    let (precision, date) = match col.ty.base() {
        DataType::DateTime { precision, .. } => (*precision, false),
        _ => (None, true),
    };
    col.strategy = Strategy::After { base, min_gap, max_gap, precision, date };
}

fn depends_on(deps: &HashMap<usize, Vec<usize>>, from: usize, target: usize) -> bool {
    let mut stack = vec![from];
    let mut seen = HashSet::new();
    while let Some(c) = stack.pop() {
        if c == target {
            return true;
        }
        if seen.insert(c)
            && let Some(d) = deps.get(&c) {
                stack.extend(d.iter().copied());
            }
    }
    false
}

fn topo_order(deps: &HashMap<usize, Vec<usize>>, n: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut state = vec![0u8; n];
    fn visit(c: usize, deps: &HashMap<usize, Vec<usize>>, state: &mut [u8], out: &mut Vec<usize>) {
        if state[c] != 0 {
            return;
        }
        state[c] = 1;
        if let Some(d) = deps.get(&c) {
            for &x in d {
                visit(x, deps, state, out);
            }
        }
        state[c] = 2;
        if deps.contains_key(&c) {
            out.push(c);
        }
    }
    let mut keys: Vec<usize> = deps.keys().copied().collect();
    keys.sort_unstable();
    for c in keys {
        visit(c, deps, &mut state, &mut out);
    }
    out
}

fn is_injective(world: &World, s: &Strategy) -> bool {
    match s {
        Strategy::Ref { rel, .. } => matches!(world.relations[*rel].pick, Pick::Injective(_) | Pick::Product { .. }),
        Strategy::Product { .. } => true,
        _ => false,
    }
}

/// Preference among key columns for carrying uniqueness: opaque identifiers first.
fn carrier_rank(c: &ColGen) -> u8 {
    match &c.strategy {
        Strategy::Random(Domain::Uuid) => 0,
        Strategy::Random(Domain::Int { .. }) => 1,
        Strategy::Random(Domain::Text { .. } | Domain::Fixed { .. }) => 2,
        Strategy::Random(Domain::Time { .. } | Domain::Ordered { .. }) => 3,
        Strategy::Random(Domain::Decimal { .. }) => 4,
        Strategy::Random(Domain::Date { .. }) => 5,
        _ => 6,
    }
}

/// A unique-by-construction strategy for a column that has `rows` rows, if its type can
/// hold that many distinct values.
fn make_unique(col: &ColGen, rows: u64, seed: u64, table_tag: u64, window: (i64, i64)) -> Option<Unique> {
    let key = hash(&[seed, table_tag, col.tag]);
    let n = rows.max(1) as i128;
    let Strategy::Random(d) = &col.strategy else { return None };
    match (d, col.ty.base()) {
        (Domain::Uuid, _) => Some(Unique::Uuid { key }),
        (Domain::Int { lo, .. }, _) => {
            let (_, tmax) = col.ty.int_range()?;
            let start = (*lo).max(1).min(tmax);
            if start.checked_add(n - 1)? > tmax {
                let (tmin, _) = col.ty.int_range()?;
                return (tmax - tmin + 1 >= n).then_some(Unique::Int { start: tmin, step: 1 });
            }
            Some(Unique::Int { start, step: 1 })
        }
        (Domain::Decimal { lo, scale, .. }, DataType::Decimal { precision, .. }) => {
            let max = 10i128.checked_pow(*precision as u32)? - 1;
            let start = (*lo).max(1);
            (start + n - 1 <= max).then_some(Unique::Decimal { start, step: 1, scale: *scale })
        }
        (Domain::Float { lo, .. }, _) => Some(Unique::Float { start: lo.max(1.0), step: 1.0 }),
        (Domain::Text { kind, max_len, .. }, _) => {
            if let Some(m) = max_len
                && (*m as usize) < (rows.to_string().len() + 1) {
                    return None;
                }
            Some(Unique::Text { kind: kind.clone(), key, max_len: *max_len })
        }
        (Domain::Fixed { n: width }, _) => {
            let cap = 36f64.powi(*width as i32);
            (cap >= rows as f64).then_some(Unique::Fixed { n: *width })
        }
        (Domain::Time { lo, hi, precision } | Domain::Ordered { lo, hi, precision, date: false, .. }, _) => {
            let unit = match precision {
                None => US_PER_SEC,
                Some(p) if *p < 6 => 10i64.pow(6 - *p as u32),
                Some(_) => 1,
            };
            let span = (hi - lo).max(unit);
            let step = (span / rows.max(1) as i64).max(unit);
            let start = domain::truncate_time(*lo, *precision) + unit;
            Some(Unique::Time { start, step, precision: *precision, key })
        }
        (Domain::Date { lo, .. }, DataType::Date32) => Some(Unique::Date { start: *lo, step: 1 }),
        (Domain::Date { lo, .. }, DataType::Date) => ((*lo as i128 + n) <= 65_535).then_some(Unique::Date { start: *lo, step: 1 }),
        (Domain::Ip4, _) => (n < 1 << 24).then_some(Unique::Ip4 { start: 0x0A00_0001 }),
        (Domain::Ip6, _) => Some(Unique::Ip6 { start: (0xfd00u128 << 112) + 1 }),
        _ => {
            let _ = window;
            None
        }
    }
}

fn describe_unique(u: &Unique) -> String {
    match u {
        Unique::Int { start, step } => format!("unique sequence from {start} step {step}"),
        Unique::Decimal { start, scale, .. } => format!("unique sequence from {}", crate::value::format_decimal(*start, *scale)),
        Unique::Float { start, .. } => format!("unique sequence from {start}"),
        Unique::Uuid { .. } => "unique UUID".into(),
        Unique::Text { kind, .. } => format!("unique text ({})", kind_name(kind)),
        Unique::Fixed { n } => format!("unique base-36 code of {n} characters"),
        Unique::Time { start, step, .. } => format!("unique, increasing from {} every ~{}s", Rfc3339(*start), step / US_PER_SEC),
        Unique::Date { start, .. } => format!("unique dates from day {start}"),
        Unique::Ip4 { .. } | Unique::Ip6 { .. } => "unique addresses".into(),
    }
}

fn kind_name(k: &TextKind) -> String {
    match k {
        TextKind::Label(s) => format!("label \"{s} N\""),
        TextKind::Category(s) => format!("category {s}_N"),
        other => format!("{other:?}").to_lowercase(),
    }
}

fn describe(col: &ColGen, world: &World, schema: &Schema, ti: usize) -> String {
    let t = &schema.tables[ti];
    let mut s = match &col.strategy {
        Strategy::Skip if !col.inserted && t.column(&col.name).is_some_and(|c| !c.insertable()) => "computed by the database".to_string(),
        Strategy::Skip => "left to the database default".to_string(),
        Strategy::Computed(_) => "computed by the database (evaluated for CHECKs)".to_string(),
        Strategy::Constant(v) => format!("constant {v}"),
        Strategy::Unique(u) => describe_unique(u),
        Strategy::Ref { rel, part } => {
            let r = &world.relations[*rel];
            let p = &schema.tables[r.parent];
            let how = match r.pick {
                Pick::Uniform => "",
                Pick::Skewed { .. } => ", skewed",
                Pick::Injective(_) => ", one-to-one",
                Pick::Earlier => ", an earlier row",
                Pick::Product { .. } => ", combinations without repeats",
            };
            format!("references {}.{}{how}", p.name, p.columns[r.parent_cols[*part]].name)
        }
        Strategy::Product { .. } => "enumerated with the other key columns (no repeats)".to_string(),
        Strategy::Random(d) => describe_domain(d),
        Strategy::After { base, min_gap, max_gap, .. } => {
            format!("after {} by {} to {}", t.columns[*base].name, human_duration(*min_gap), human_duration(*max_gap))
        }
        Strategy::Derived(_) => "computed from other columns to satisfy a CHECK".to_string(),
    };
    if col.nullable && col.null_rate > 0.0 && !matches!(col.strategy, Strategy::Skip | Strategy::Constant(_) | Strategy::Computed(_)) {
        let _ = write!(s, ", {:.0}% NULL", col.null_rate * 100.0);
    }
    s
}

fn describe_domain(d: &Domain) -> String {
    match d {
        Domain::Values { values, .. } if values.len() <= 6 => {
            format!("one of {}", values.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", "))
        }
        Domain::Values { values, .. } => format!("one of {} values", values.len()),
        Domain::Int { lo, hi } => format!("random integer in [{lo}, {hi}]"),
        Domain::Float { lo, hi } => format!("random number in [{lo}, {hi}]"),
        Domain::Decimal { lo, hi, scale } => {
            format!("random decimal in [{}, {}]", crate::value::format_decimal(*lo, *scale), crate::value::format_decimal(*hi, *scale))
        }
        Domain::Bool { .. } => "random boolean".into(),
        Domain::Time { .. } => "random timestamp in the window".into(),
        Domain::Ordered { date: true, .. } => "dates spread over the window in row order (partition key)".into(),
        Domain::Ordered { .. } => "timestamps spread over the window in row order (partition key)".into(),
        Domain::Date { .. } => "random date".into(),
        Domain::Uuid => "random UUID".into(),
        Domain::Text { kind, pool: Some(p), .. } => format!("{} from {p} values", kind_name(kind)),
        Domain::Text { kind, .. } => kind_name(kind),
        Domain::Fixed { n } => format!("random {n} characters"),
        Domain::Ip4 | Domain::Ip6 => "random address".into(),
        Domain::Json => "small JSON object".into(),
        Domain::Array { min, max, .. } => format!("array of {min} to {max} elements"),
        Domain::Map { min, max, .. } => format!("map of {min} to {max} entries"),
        Domain::Tuple(_) => "tuple".into(),
        Domain::Null => "NULL (unsupported type)".into(),
    }
}

fn human_duration(us: i64) -> String {
    if us % US_PER_DAY == 0 {
        format!("{}d", us / US_PER_DAY)
    } else if us % US_PER_HOUR == 0 {
        format!("{}h", us / US_PER_HOUR)
    } else if us % US_PER_MIN == 0 {
        format!("{}m", us / US_PER_MIN)
    } else {
        format!("{}s", us / US_PER_SEC)
    }
}

/// Reference chains must end: a column whose value comes from a parent whose value comes
/// back from the child would never be computed.
fn detect_cycles(world: &World, schema: &Schema, problems: &mut Vec<String>) {
    // Table-level graph of references between generated tables through impure columns or
    // chains of references.
    let mut edges: HashMap<usize, BTreeSet<usize>> = HashMap::new();
    for r in &world.relations {
        if r.child != r.parent && world.tables[r.parent].is_some() {
            let parent = world.table(r.parent);
            let impure = r.parent_cols.iter().any(|&c| !parent.cols[c].pure || matches!(parent.cols[c].strategy, Strategy::Ref { .. } | Strategy::Product { .. }));
            if impure {
                edges.entry(r.child).or_default().insert(r.parent);
            }
        } else if r.child == r.parent {
            let t = world.table(r.child);
            if r.parent_cols.iter().any(|&c| !t.cols[c].pure || r.child_cols.contains(&c)) {
                problems.push(format!("table {}: a column references the table itself through a column computed from the row", schema.tables[r.child].name));
            }
        }
    }
    let mut state: HashMap<usize, u8> = HashMap::new();
    fn visit(n: usize, edges: &HashMap<usize, BTreeSet<usize>>, state: &mut HashMap<usize, u8>, path: &mut Vec<usize>) -> Option<Vec<usize>> {
        match state.get(&n) {
            Some(1) => {
                let start = path.iter().position(|&x| x == n).unwrap_or(0);
                return Some(path[start..].to_vec());
            }
            Some(2) => return None,
            _ => {}
        }
        state.insert(n, 1);
        path.push(n);
        if let Some(next) = edges.get(&n) {
            for &m in next {
                if let Some(c) = visit(m, edges, state, path) {
                    return Some(c);
                }
            }
        }
        path.pop();
        state.insert(n, 2);
        None
    }
    let mut keys: Vec<usize> = edges.keys().copied().collect();
    keys.sort_unstable();
    for k in keys {
        if let Some(cycle) = visit(k, &edges, &mut state, &mut Vec::new()) {
            let names: Vec<&str> = cycle.iter().map(|&t| schema.tables[t].name.as_str()).collect();
            problems.push(format!("relations form a cycle ({}): break it with ignore_relations", names.join(" -> ")));
            return;
        }
    }
}

/// Generated tables by dependency level, parents before children (cycles broken
/// arbitrarily).
fn load_levels(world: &World, tables: &[TablePlan]) -> Vec<Vec<usize>> {
    let generated: Vec<usize> = (0..tables.len()).filter(|&i| matches!(tables[i].mode, Mode::Generate(_))).collect();
    let mut level: HashMap<usize, usize> = generated.iter().map(|&i| (i, 0)).collect();
    for _ in 0..generated.len() {
        let mut changed = false;
        for r in &world.relations {
            if r.child == r.parent {
                continue;
            }
            if let (Some(&lp), Some(&lc)) = (level.get(&r.parent), level.get(&r.child))
                && lc <= lp && lp < generated.len() {
                    level.insert(r.child, lp + 1);
                    changed = true;
                }
        }
        if !changed {
            break;
        }
    }
    let mut levels: Vec<Vec<usize>> = Vec::new();
    for ti in generated {
        let l = level[&ti];
        if levels.len() <= l {
            levels.resize(l + 1, Vec::new());
        }
        levels[l].push(ti);
    }
    levels.retain(|l| !l.is_empty());
    levels
}

/// Result of generating sample rows of each table.
pub struct SampleReport {
    pub table: String,
    pub sampled: u64,
    pub repaired: u64,
    pub max_steps: u32,
    pub error: Option<String>,
}

/// Generates up to `n` rows spread over each generated table, to find CHECKs the
/// generator cannot satisfy before anything is written.
pub fn sample(plan: &Plan, n: u64) -> Vec<SampleReport> {
    let mut out = Vec::new();
    for &ti in &plan.order {
        let tg = plan.world.table(ti);
        let count = n.min(tg.rows);
        let mut report = SampleReport { table: tg.name.clone(), sampled: 0, repaired: 0, max_steps: 0, error: None };
        let mut row = Row::new();
        for k in 0..count {
            let i = if count == tg.rows { k } else { k * tg.rows / count };
            match tg.row(&plan.world, i, &mut row) {
                Ok(steps) => {
                    report.sampled += 1;
                    if steps > 0 {
                        report.repaired += 1;
                        report.max_steps = report.max_steps.max(steps);
                    }
                }
                Err(e) => {
                    report.error = Some(e.to_string());
                    break;
                }
            }
        }
        out.push(report);
    }
    out
}

impl Plan {
    /// The generated tables, parents first, with their schema index.
    pub fn generated(&self) -> impl Iterator<Item = (usize, &TableGen)> {
        self.order.iter().map(|&i| (i, self.world.table(i)))
    }

    /// Rows to generate across all tables.
    pub fn total_rows(&self) -> u64 {
        self.generated().map(|(_, t)| t.rows).sum()
    }

    /// Whether table `ti` has a column referencing the table itself.
    pub fn references_itself(&self, ti: usize) -> bool {
        self.world.relations.iter().any(|r| r.child == ti && r.parent == ti)
    }

    /// The generated tables that reference `ti`, directly or through other tables.
    pub fn dependents(&self, ti: usize) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        let mut stack = vec![ti];
        while let Some(t) = stack.pop() {
            for r in &self.world.relations {
                if r.parent == t && r.child != ti && !out.contains(&r.child) {
                    out.push(r.child);
                    stack.push(r.child);
                }
            }
        }
        out
    }

    /// Identifies the data this plan produces: same fingerprint, same rows.
    pub fn fingerprint(&self, schema: &Schema) -> String {
        let mut s = format!("v{MODEL_VERSION};seed={};window={}..{};", self.world.seed, self.window.0, self.window.1);
        for (ti, tg) in self.generated() {
            let t = &schema.tables[ti];
            let _ = write!(s, "{}={};", tg.name, tg.rows);
            for (c, col) in tg.cols.iter().enumerate() {
                let _ = write!(s, "{}:{}:{}:{}|", col.name, t.columns[c].ty, col.describe, col.inserted);
            }
            for ch in &tg.checks {
                let _ = write!(s, "check:{};", ch.sql);
            }
        }
        for r in &self.relations {
            let _ = write!(s, "rel:{}.{:?}->{}.{:?};", r.child, r.child_cols, r.parent, r.parent_cols);
        }
        format!("{:016x}", hash(&s.bytes().map(|b| b as u64).collect::<Vec<_>>()))
    }

    /// The plan as text: window, table modes, relations, column strategies, warnings.
    pub fn render(&self, schema: &Schema) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "Window: {} -> {}", Rfc3339(self.window.0), Rfc3339(self.window.1));
        let _ = writeln!(s, "\nTables:");
        for tp in &self.tables {
            let mode = match tp.mode {
                Mode::Generate(n) => format!("generate {}", crate::report::thousands(n)),
                Mode::Existing => "existing".into(),
                Mode::Skip => "skip".into(),
            };
            let _ = writeln!(s, "  {:<36} {mode}{}", tp.name, tp.reason.as_ref().map_or(String::new(), |r| format!(" ({r})")));
        }
        if !self.relations.is_empty() {
            let _ = writeln!(s, "\nRelations:");
            for r in &self.relations {
                let cc = if r.child_cols.len() == 1 { r.child_cols[0].clone() } else { format!("({})", r.child_cols.join(", ")) };
                let pc = if r.parent_cols.len() == 1 { r.parent_cols[0].clone() } else { format!("({})", r.parent_cols.join(", ")) };
                let _ = writeln!(s, "  {}.{cc} -> {}.{pc}   [{}]", r.child, r.parent, r.source);
            }
        }
        for (ti, tg) in self.generated() {
            let t = &schema.tables[ti];
            let _ = writeln!(s, "\n{} ({} rows):", tg.name, crate::report::thousands(tg.rows));
            for (c, col) in tg.cols.iter().enumerate() {
                let _ = writeln!(s, "  {:<32} {:<36} {}", col.name, t.columns[c].ty.to_string(), col.describe);
            }
            for ch in &tg.checks {
                let _ = writeln!(s, "  CHECK {}: {}", ch.name, ch.sql.split_whitespace().collect::<Vec<_>>().join(" "));
            }
        }
        if !self.warnings.is_empty() {
            let _ = writeln!(s, "\nWarnings:");
            for w in &self.warnings {
                let _ = writeln!(s, "  - {w}");
            }
        }
        s
    }
}
