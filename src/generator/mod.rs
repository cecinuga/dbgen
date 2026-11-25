//! Row generation.
//!
//! Every value is a pure function of `(seed, table, column, row index)`. A child row that
//! references a parent computes the parent row it points to and then that row's key, by
//! the same function the parent's own generator uses: the parent need not be in memory
//! or in the database, and batches can be generated in any order on any thread.
//!
//! CHECK constraints are satisfied per row: columns a check can derive are computed
//! (`c = expr`), the others are drawn from domains the checks narrowed, and a row that
//! still violates a check is repaired by a short local search over the columns involved
//! (resample a value, flip NULL-ness, try a literal the check names).

pub mod domain;
pub mod perm;
pub mod words;

use std::fmt;

use crate::expr::{eval, holds, CExpr, EvalCtx};
use crate::rng::{hash, unit, Rng};
use crate::schema::DataType;
use crate::value::Value;

use domain::{Domain, Unique};
use perm::Perm;

const NULL_STREAM: u64 = 0x6e75_6c6c;
const REPAIR_STREAM: u64 = 0x7265_7061;
const PICK_STREAM: u64 = 0x7069_636b;
const GAP_STREAM: u64 = 0x6761_7073;

/// How a column gets its value.
#[derive(Debug, Clone)]
pub enum Strategy {
    /// Not inserted: the database fills it (DEFAULT), or it is computed on its side.
    Skip,
    /// Not inserted, but evaluated so CHECKs that read it can be verified.
    Computed(CExpr),
    Constant(Value),
    /// Distinct for every row, by construction.
    Unique(Unique),
    /// Part `part` of the referenced key of relation `rel`.
    Ref { rel: usize, part: usize },
    /// Component `comp` (part `part`) of a composite key enumerated without repeats.
    Product { key: usize, comp: usize, part: usize },
    Random(Domain),
    /// A timestamp `gap` after column `base` of the same row.
    After { base: usize, min_gap: i64, max_gap: i64, precision: Option<u8>, date: bool },
    /// Computed from other columns so that an equality CHECK holds.
    Derived(CExpr),
}

/// What the CHECK solver may change in a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutability {
    /// The solver never changes it (keys, references that are not nullable...).
    Fixed,
    /// The solver may set it NULL or not (nullable references).
    NullOnly,
    /// The solver may redraw it.
    Full,
}

/// How one column of a generated table gets its values.
#[derive(Debug, Clone)]
pub struct ColGen {
    pub name: String,
    pub ty: DataType,
    pub tag: u64,
    pub strategy: Strategy,
    pub inserted: bool,
    pub nullable: bool,
    pub null_rate: f64,
    pub mutability: Mutability,
    /// Literals the checks compare this column with: candidate values for repairs.
    pub hints: Vec<Value>,
    /// Never changed by the solver and computable on its own (used by `value_at`).
    pub pure: bool,
    /// Human-readable description of the strategy, for `dbgen inspect`.
    pub describe: String,
}

/// A CHECK constraint the generator satisfies row by row.
#[derive(Debug, Clone)]
pub struct CheckGen {
    pub name: String,
    pub sql: String,
    pub expr: CExpr,
    /// Columns the solver may change to repair a violation.
    pub movable: Vec<usize>,
}

/// The generator of one table: row `i` is a function of the seed and `i`.
#[derive(Debug, Clone)]
pub struct TableGen {
    pub index: usize,
    pub name: String,
    pub rows: u64,
    pub tag: u64,
    pub seed: u64,
    pub cols: Vec<ColGen>,
    /// Inserted columns, in insert order.
    pub insert: Vec<usize>,
    pub checks: Vec<CheckGen>,
    /// Columns computed from others (After, Derived, Computed), in dependency order.
    pub dependents: Vec<usize>,
    pub max_steps: u32,
}

/// Rows of a table that is not generated but is referenced, read from the database.
#[derive(Debug, Clone)]
pub struct ExistingRows {
    /// Schema column index -> position in `rows[i]`.
    pub columns: Vec<usize>,
    pub rows: Vec<Vec<Value>>,
}

/// How a child row chooses the parent row it references.
#[derive(Debug, Clone)]
pub enum Pick {
    Uniform,
    /// Power-law: `floor(n * u^skew)`, scattered by a permutation.
    Skewed { skew: f64, perm: Perm },
    /// One-to-one: child row `i` -> parent row `perm(i)`.
    Injective(Perm),
    /// A row of the same table before the child row. Row 0 references itself when the
    /// column cannot be NULL.
    Earlier,
    /// Chosen by the enumeration of a composite key.
    Product { key: usize, comp: usize },
}

/// A relation between generated tables, or to rows read from the database.
#[derive(Debug, Clone)]
pub struct RelGen {
    pub child: usize,
    pub parent: usize,
    pub child_cols: Vec<usize>,
    pub parent_cols: Vec<usize>,
    pub parent_rows: u64,
    pub pick: Pick,
    pub tag: u64,
}

/// One part of a composite key enumerated without repeats.
#[derive(Debug, Clone)]
pub enum Component {
    /// A relation: the component index is the parent row.
    Relation(usize),
    /// A finite set of values for one column.
    Values(Vec<Value>),
}

/// A composite key whose combinations are enumerated without repeats: row `i` takes
/// combination `perm(i)`, read as a mixed-radix number.
#[derive(Debug, Clone)]
pub struct ProductGen {
    pub perm: Perm,
    pub radices: Vec<u64>,
    pub comps: Vec<Component>,
}

/// Everything needed to generate any row of any table.
#[derive(Debug, Clone)]
pub struct World {
    pub seed: u64,
    pub ctx: EvalCtx,
    pub tables: Vec<Option<TableGen>>,
    pub existing: Vec<Option<ExistingRows>>,
    pub relations: Vec<RelGen>,
    pub products: Vec<ProductGen>,
}

#[derive(Debug, Clone)]
enum ColState {
    Base { variant: u32, null: Option<bool> },
    Set(Value),
}

/// A row being built.
#[derive(Debug, Clone)]
pub struct Row {
    pub values: Vec<Value>,
    state: Vec<ColState>,
}

impl Row {
    /// An empty row, reused across rows to save allocations.
    pub fn new() -> Row {
        Row { values: Vec::new(), state: Vec::new() }
    }
}

impl Default for Row {
    fn default() -> Self {
        Row::new()
    }
}

/// A row whose CHECKs could not be satisfied, with its last attempt.
#[derive(Debug, Clone)]
pub struct RowError {
    pub table: String,
    pub row: u64,
    pub checks: Vec<String>,
    pub values: String,
}

impl fmt::Display for RowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "table {}, row {}: could not satisfy {} (last attempt: {})",
            self.table,
            self.row,
            self.checks.join(", "),
            self.values
        )
    }
}

impl World {
    /// The generator of table `index`, which must be generated.
    pub fn table(&self, index: usize) -> &TableGen {
        self.tables[index].as_ref().expect("generated table")
    }

    fn parent_row(&self, rel: &RelGen, i: u64) -> u64 {
        let n = rel.parent_rows.max(1);
        match &rel.pick {
            Pick::Uniform => Rng::at(&[self.seed, rel.tag, i, PICK_STREAM]).below(n),
            Pick::Skewed { skew, perm } => {
                let u = Rng::at(&[self.seed, rel.tag, i, PICK_STREAM]).f64();
                let k = ((n as f64) * u.powf(*skew)) as u64;
                perm.apply(k.min(n - 1))
            }
            Pick::Injective(perm) => perm.apply(i % n),
            Pick::Earlier if i == 0 => 0,
            Pick::Earlier => Rng::at(&[self.seed, rel.tag, i, PICK_STREAM]).below(i.min(n)),
            Pick::Product { key, comp } => self.product_index(*key, *comp, i),
        }
    }

    fn product_index(&self, key: usize, comp: usize, i: u64) -> u64 {
        let p = &self.products[key];
        let mut x = p.perm.apply(i % p.perm.len().max(1));
        for (k, radix) in p.radices.iter().enumerate() {
            if k == comp {
                return x % radix;
            }
            x /= radix;
        }
        0
    }

    fn parent_value(&self, rel: &RelGen, row: u64, part: usize) -> Value {
        let col = rel.parent_cols[part];
        if let Some(t) = &self.tables[rel.parent] {
            return t.value_at(self, row, col);
        }
        match &self.existing[rel.parent] {
            Some(ex) => {
                let pos = ex.columns.iter().position(|c| *c == col);
                match (pos, ex.rows.get(row as usize)) {
                    (Some(p), Some(r)) => r[p].clone(),
                    _ => Value::Null,
                }
            }
            None => Value::Null,
        }
    }
}

impl TableGen {
    fn null_draw(&self, col: usize, i: u64) -> bool {
        let c = &self.cols[col];
        c.nullable && c.null_rate > 0.0 && unit(hash(&[self.seed, self.tag, c.tag, i, NULL_STREAM])) < c.null_rate
    }

    /// The value of a column that does not depend on other columns of the row.
    fn base_value(&self, w: &World, col: usize, i: u64, state: &ColState) -> Value {
        let (variant, null) = match state {
            ColState::Set(v) => return v.clone(),
            ColState::Base { variant, null } => (*variant, *null),
        };
        let c = &self.cols[col];
        let is_null = || null.unwrap_or_else(|| self.null_draw(col, i));
        match &c.strategy {
            Strategy::Skip | Strategy::Computed(_) | Strategy::Derived(_) | Strategy::After { .. } => Value::Null,
            Strategy::Constant(v) => v.clone(),
            Strategy::Unique(u) => u.value(i),
            Strategy::Ref { rel, part } => {
                let r = &w.relations[*rel];
                // The first row of a table that references itself has no earlier row: it
                // is a root of the hierarchy, unless the column cannot be NULL.
                let root = i == 0 && matches!(r.pick, Pick::Earlier);
                if c.nullable && (root || is_null()) {
                    return Value::Null;
                }
                w.parent_value(r, w.parent_row(r, i), *part)
            }
            Strategy::Product { key, comp, part } => {
                let k = w.product_index(*key, *comp, i);
                match &w.products[*key].comps[*comp] {
                    Component::Relation(rel) => w.parent_value(&w.relations[*rel], k, *part),
                    Component::Values(vals) => vals.get(k as usize).cloned().unwrap_or(Value::Null),
                }
            }
            Strategy::Random(d) => {
                if c.nullable && is_null() {
                    return Value::Null;
                }
                d.sample(&mut Rng::at(&[self.seed, self.tag, c.tag, i, variant as u64]), i)
            }
        }
    }

    /// Computes the columns that depend on others, in dependency order.
    fn compute_dependents(&self, w: &World, i: u64, row: &mut Row) {
        for &col in &self.dependents {
            let c = &self.cols[col];
            let value = match (&c.strategy, &row.state[col]) {
                (_, ColState::Set(v)) => v.clone(),
                (Strategy::After { base, min_gap, max_gap, precision, date }, ColState::Base { variant, null }) => {
                    let is_null = null.unwrap_or_else(|| self.null_draw(col, i));
                    if c.nullable && is_null {
                        Value::Null
                    } else {
                        let mut rng = Rng::at(&[self.seed, self.tag, c.tag, i, *variant as u64, GAP_STREAM]);
                        let gap = log_uniform(&mut rng, *min_gap, *max_gap);
                        match row.values[*base].as_time() {
                            Some(t) if *date => Value::Date(crate::value::date_of_time(t + gap)),
                            Some(t) => Value::Time(domain::truncate_time(t + gap, *precision)),
                            None => Value::Null,
                        }
                    }
                }
                (Strategy::Derived(e) | Strategy::Computed(e), _) => {
                    let v = crate::expr::cast(eval(e, &row.values, &w.ctx), &c.ty);
                    fit(v, &c.ty)
                }
                _ => continue,
            };
            row.values[col] = value;
        }
    }

    fn failing(&self, w: &World, values: &[Value]) -> Vec<usize> {
        self.checks.iter().enumerate().filter(|(_, ch)| !holds(&ch.expr, values, &w.ctx)).map(|(k, _)| k).collect()
    }

    fn fresh(&self, w: &World, i: u64, row: &mut Row) {
        let n = self.cols.len();
        row.state.clear();
        row.state.resize(n, ColState::Base { variant: 0, null: None });
        row.values.clear();
        for col in 0..n {
            let v = self.base_value(w, col, i, &row.state[col]);
            row.values.push(v);
        }
        self.compute_dependents(w, i, row);
    }

    /// Generates row `i` into `row.values` (one value per schema column). Returns the
    /// number of repair steps it took.
    pub fn row(&self, w: &World, i: u64, row: &mut Row) -> Result<u32, RowError> {
        self.fresh(w, i, row);
        if self.checks.is_empty() {
            return Ok(0);
        }
        let mut failing = self.failing(w, &row.values);
        if failing.is_empty() {
            return Ok(0);
        }
        let mut rng = Rng::at(&[self.seed, self.tag, i, REPAIR_STREAM]);
        let mut scratch = row.clone();
        for step in 0..self.max_steps {
            let check = &self.checks[failing[rng.below(failing.len() as u64) as usize]];
            let moves = self.moves(check, row, step);
            if moves.is_empty() {
                break;
            }
            let chosen = if rng.chance(0.15) {
                rng.below(moves.len() as u64) as usize
            } else {
                // Greedy: the move that leaves the fewest checks failing; ties at random.
                let mut best = Vec::new();
                let mut best_score = usize::MAX;
                for (k, m) in moves.iter().enumerate() {
                    scratch.values.clone_from(&row.values);
                    scratch.state.clone_from(&row.state);
                    self.apply(w, i, &mut scratch, m);
                    let score = self.failing(w, &scratch.values).len();
                    if score < best_score {
                        best_score = score;
                        best.clear();
                    }
                    if score == best_score {
                        best.push(k);
                    }
                }
                best[rng.below(best.len() as u64) as usize]
            };
            self.apply(w, i, row, &moves[chosen]);
            failing = self.failing(w, &row.values);
            if failing.is_empty() {
                return Ok(step + 1);
            }
        }
        Err(RowError {
            table: self.name.clone(),
            row: i,
            checks: failing.iter().map(|&k| self.checks[k].name.clone()).collect(),
            values: failing
                .iter()
                .flat_map(|&k| {
                    let mut cols = std::collections::BTreeSet::new();
                    self.checks[k].expr.columns(&mut cols);
                    cols
                })
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|c| format!("{} = {}", self.cols[c].name, row.values[c]))
                .collect::<Vec<_>>()
                .join(", "),
        })
    }

    fn moves(&self, check: &CheckGen, row: &Row, step: u32) -> Vec<Move> {
        let mut out = Vec::new();
        for (k, &col) in check.movable.iter().enumerate() {
            let c = &self.cols[col];
            let is_null = row.values[col].is_null();
            match c.mutability {
                Mutability::Fixed => {}
                Mutability::NullOnly => out.push(Move::Null(col, !is_null)),
                Mutability::Full => {
                    if c.nullable {
                        out.push(Move::Null(col, !is_null));
                    }
                    out.push(Move::Redraw(col, 1 + step * 64 + k as u32));
                    for h in c.hints.iter().take(8) {
                        out.push(Move::Set(col, h.clone()));
                    }
                }
            }
        }
        out
    }

    fn apply(&self, w: &World, i: u64, row: &mut Row, m: &Move) {
        match m {
            Move::Null(col, null) => {
                let variant = match row.state[*col] {
                    ColState::Base { variant, .. } => variant,
                    ColState::Set(_) => 0,
                };
                row.state[*col] = ColState::Base { variant, null: Some(*null) };
                row.values[*col] = self.base_value(w, *col, i, &row.state[*col]);
            }
            Move::Redraw(col, variant) => {
                row.state[*col] = ColState::Base { variant: *variant, null: Some(false) };
                row.values[*col] = self.base_value(w, *col, i, &row.state[*col]);
            }
            Move::Set(col, v) => {
                row.state[*col] = ColState::Set(v.clone());
                row.values[*col] = v.clone();
            }
        }
        self.compute_dependents(w, i, row);
    }

    /// The value of column `col` in row `i`, as `row()` would produce it.
    pub fn value_at(&self, w: &World, i: u64, col: usize) -> Value {
        if self.cols[col].pure {
            return self.base_value(w, col, i, &ColState::Base { variant: 0, null: None });
        }
        let mut row = Row::new();
        match self.row(w, i, &mut row) {
            Ok(_) => row.values.swap_remove(col),
            Err(_) => Value::Null,
        }
    }
}

enum Move {
    Null(usize, bool),
    Redraw(usize, u32),
    Set(usize, Value),
}

fn log_uniform(rng: &mut Rng, lo: i64, hi: i64) -> i64 {
    let lo = lo.max(1) as f64;
    let hi = (hi as f64).max(lo);
    (lo * (hi / lo).powf(rng.f64())).round() as i64
}

/// Keeps a computed value only if it is representable in the column type (an integer
/// in range, a decimal within its precision); otherwise NULL, which the checks reject.
fn fit(v: Value, ty: &DataType) -> Value {
    match (&v, ty.base()) {
        (Value::Int(x), DataType::Int { .. }) => match ty.int_range() {
            Some((lo, hi)) if *x < lo || *x > hi => Value::Null,
            _ => v,
        },
        (Value::Decimal(x, _), DataType::Decimal { precision, .. }) => {
            let max = 10i128.checked_pow(*precision as u32).unwrap_or(i128::MAX);
            if x.abs() >= max {
                Value::Null
            } else {
                v
            }
        }
        _ => v,
    }
}
