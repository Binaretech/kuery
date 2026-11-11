//! Two columns against each other: a measure summarized by the groups of
//! another column, two categories crossed, a measure or a category over
//! time, or two measures correlated. Like [`crate::stats`], plain
//! computation over a [`ResultSet`] meant to run off the UI thread.

use std::collections::HashMap;

use crate::result_set::{ColumnKind, ResultSet};
use crate::stats::{self, Buckets, Granularity};

/// Points a scatter plot draws; more pairs are sampled down to this.
pub const MAX_POINTS: usize = 4_000;

/// Categories a stacked chart shows before folding the rest into "Other".
pub const MAX_STACKED: usize = 8;

/// A way to look at two columns together. Indices are columns of the set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Comparison {
    /// A measure summarized per group of another column.
    Groups { measure: usize, group: usize },
    /// Two categorical columns crossed: counts per pair of values.
    CrossTab { rows: usize, columns: usize },
    /// A measure aggregated per period.
    Series { time: usize, measure: usize },
    /// How a category splits within each period.
    Breakdown { time: usize, category: usize },
    /// Two measures against each other.
    Correlation { x: usize, y: usize },
}

impl Comparison {
    pub fn label(self) -> &'static str {
        match self {
            Comparison::Groups { .. } => "By group",
            Comparison::CrossTab { .. } => "Cross tab",
            Comparison::Series { .. } => "Over time",
            Comparison::Breakdown { .. } => "Breakdown over time",
            Comparison::Correlation { .. } => "Correlation",
        }
    }
}

fn numeric(kind: ColumnKind) -> bool {
    kind.is_numeric()
}

fn categorical(kind: ColumnKind) -> bool {
    matches!(
        kind,
        ColumnKind::Text
            | ColumnKind::Json
            | ColumnKind::Id
            | ColumnKind::Boolean
            | ColumnKind::Integer
            | ColumnKind::Unknown
    )
}

fn temporal(kind: ColumnKind) -> bool {
    kind.is_temporal()
}

/// The comparisons that fit columns `x` and `y` of these kinds, the most
/// telling first. Integers count both as measures and as categories.
pub fn comparisons(x: usize, x_kind: ColumnKind, y: usize, y_kind: ColumnKind) -> Vec<Comparison> {
    let mut found = Vec::new();
    for (a, a_kind, b, b_kind) in [(x, x_kind, y, y_kind), (y, y_kind, x, x_kind)] {
        if temporal(a_kind) && numeric(b_kind) {
            found.push(Comparison::Series {
                time: a,
                measure: b,
            });
        }
    }
    for (a, a_kind, b, b_kind) in [(x, x_kind, y, y_kind), (y, y_kind, x, x_kind)] {
        if temporal(a_kind) && categorical(b_kind) {
            found.push(Comparison::Breakdown {
                time: a,
                category: b,
            });
        }
    }
    if numeric(x_kind) && numeric(y_kind) {
        found.push(Comparison::Correlation { x, y });
    }
    for (a, a_kind, b, b_kind) in [(x, x_kind, y, y_kind), (y, y_kind, x, x_kind)] {
        if numeric(a_kind) && categorical(b_kind) {
            found.push(Comparison::Groups {
                measure: a,
                group: b,
            });
        }
    }
    if categorical(x_kind) && categorical(y_kind) {
        found.push(Comparison::CrossTab {
            rows: x,
            columns: y,
        });
    }
    found
}

/// How a measure is reduced to one value per period.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Aggregate {
    Count,
    Sum,
    #[default]
    Avg,
    Min,
    Max,
    Median,
}

impl Aggregate {
    pub const ALL: [Aggregate; 6] = [
        Aggregate::Count,
        Aggregate::Sum,
        Aggregate::Avg,
        Aggregate::Min,
        Aggregate::Max,
        Aggregate::Median,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Aggregate::Count => "count",
            Aggregate::Sum => "sum",
            Aggregate::Avg => "avg",
            Aggregate::Min => "min",
            Aggregate::Max => "max",
            Aggregate::Median => "median",
        }
    }

    /// `values` must be sorted for the median.
    fn apply(self, sorted: &[f64]) -> Option<f64> {
        if sorted.is_empty() {
            return (self == Aggregate::Count).then_some(0.0);
        }
        Some(match self {
            Aggregate::Count => sorted.len() as f64,
            Aggregate::Sum => sorted.iter().sum(),
            Aggregate::Avg => sorted.iter().sum::<f64>() / sorted.len() as f64,
            Aggregate::Min => sorted[0],
            Aggregate::Max => sorted[sorted.len() - 1],
            Aggregate::Median => stats::percentile(sorted, 0.5),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompareOptions {
    /// For [`Comparison::Series`].
    pub aggregate: Aggregate,
    /// For the comparisons over time.
    pub granularity: Granularity,
    /// Groups or categories shown before folding the rest into "Other".
    pub groups: usize,
    /// Cross tabs as shares of each row instead of counts.
    pub percent: bool,
}

impl Default for CompareOptions {
    fn default() -> Self {
        Self {
            aggregate: Aggregate::Avg,
            granularity: Granularity::Auto,
            groups: 10,
            percent: false,
        }
    }
}

/// A group or category.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Value(String),
    Null,
    /// The values beyond the ones shown, folded together.
    Other,
}

impl Key {
    pub fn text(&self) -> &str {
        match self {
            Key::Value(value) => value,
            Key::Null => "NULL",
            Key::Other => "Other",
        }
    }

    fn of(value: Option<&str>) -> Key {
        value.map_or(Key::Null, |v| Key::Value(v.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Groups(GroupTable),
    CrossTab(CrossTab),
    Series(Series),
    Breakdown(Breakdown),
    Correlation(Correlation),
    /// No row has a usable value in both columns.
    Empty,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupStats {
    pub key: Key,
    pub n: usize,
    pub sum: f64,
    pub mean: f64,
    pub min: f64,
    pub p25: f64,
    pub median: f64,
    pub p75: f64,
    pub max: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupTable {
    /// Largest groups first; the last one is [`Key::Other`] when groups
    /// were folded.
    pub groups: Vec<GroupStats>,
    /// Every value together.
    pub overall: GroupStats,
    /// Distinct groups before folding.
    pub distinct: usize,
    /// The share of the measure's variance the groups explain (η²).
    pub eta_squared: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CrossTab {
    pub rows: Vec<Key>,
    pub columns: Vec<Key>,
    /// `counts[row][column]`.
    pub counts: Vec<Vec<usize>>,
    pub row_totals: Vec<usize>,
    pub column_totals: Vec<usize>,
    pub total: usize,
    /// The strength of the association, 0 to 1 (Cramér's V); `None` when
    /// either column has a single value.
    pub cramers_v: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub granularity: Granularity,
    pub aggregate: Aggregate,
    /// Every period from the first to the last; `value` is `None` for
    /// periods without values (except for counts, which are 0).
    pub points: Vec<Point>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub label: String,
    pub n: usize,
    pub value: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Breakdown {
    pub granularity: Granularity,
    pub periods: Vec<String>,
    /// Largest first; [`Key::Other`] last when categories were folded.
    pub categories: Vec<Key>,
    /// `counts[period][category]`.
    pub counts: Vec<Vec<usize>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Correlation {
    /// Rows with a number in both columns.
    pub n: usize,
    /// `None` when either column does not vary.
    pub pearson: Option<f64>,
    pub spearman: Option<f64>,
    /// The least-squares line `y = slope · x + intercept`.
    pub slope: Option<f64>,
    pub intercept: Option<f64>,
    /// Up to [`MAX_POINTS`] of the pairs, evenly spread over the rows, for a
    /// scatter plot.
    pub points: Vec<(f64, f64)>,
    /// The smallest and largest of each column, over every pair.
    pub x_range: (f64, f64),
    pub y_range: (f64, f64),
}

/// Numeric columns a [`CorrelationMatrix`] compares: the first ones that
/// vary, when there are more.
pub const MAX_MATRIX: usize = 24;

/// How every numeric column moves with every other: Pearson's r by pair.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CorrelationMatrix {
    /// The columns compared, by their index in the results. Columns that
    /// hold one number only are not here: nothing varies with them.
    pub columns: Vec<usize>,
    /// Numeric columns that vary but were left out, beyond [`MAX_MATRIX`].
    pub left_out: usize,
    /// `r[i][j]` for `columns[i]` and `columns[j]`, over the rows with a
    /// number in both. `None` on the diagonal, and when fewer than three
    /// rows have both or one of the two does not vary over them.
    pub r: Vec<Vec<Option<f64>>>,
}

impl CorrelationMatrix {
    /// Every measured pair as `(column, column, r)`, the strongest first
    /// (ties in the order of the columns).
    pub fn strongest(&self) -> Vec<(usize, usize, f64)> {
        let mut pairs: Vec<(usize, usize, f64)> = (0..self.columns.len())
            .flat_map(|i| (0..i).map(move |j| (j, i)))
            .filter_map(|(j, i)| Some((self.columns[j], self.columns[i], self.r[i][j]?)))
            .collect();
        pairs.sort_by(|a, b| {
            b.2.abs()
                .total_cmp(&a.2.abs())
                .then((a.0, a.1).cmp(&(b.0, b.1)))
        });
        pairs
    }
}

/// Pearson's r between every two numeric columns of `set`, whose kinds must
/// be known (see [`stats::kinds`]).
pub fn correlations(set: &ResultSet) -> CorrelationMatrix {
    // Each column as numbers around its own mean (NaN where there is none),
    // so the sums below stay small.
    let mut varying: Vec<(usize, Vec<f64>)> = Vec::new();
    let mut left_out = 0;
    for (index, kind) in set.kinds().iter().enumerate() {
        if !kind.is_numeric() {
            continue;
        }
        let mut values: Vec<f64> = set
            .column_values(index)
            .map(|value| value.and_then(stats::parse_number).unwrap_or(f64::NAN))
            .collect();
        let (mut n, mut sum, mut low, mut high) = (0usize, 0.0, f64::INFINITY, f64::NEG_INFINITY);
        for value in values.iter().filter(|v| !v.is_nan()) {
            n += 1;
            sum += value;
            low = low.min(*value);
            high = high.max(*value);
        }
        if n == 0 || low == high {
            continue;
        }
        if varying.len() == MAX_MATRIX {
            left_out += 1;
            continue;
        }
        let mean = sum / n as f64;
        values.iter_mut().for_each(|value| *value -= mean);
        varying.push((index, values));
    }
    let size = varying.len();
    let mut r = vec![vec![None; size]; size];
    for i in 0..size {
        for j in 0..i {
            let (mut n, mut sx, mut sy, mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
            for (x, y) in varying[i].1.iter().zip(&varying[j].1) {
                if x.is_nan() || y.is_nan() {
                    continue;
                }
                n += 1.0;
                sx += x;
                sy += y;
                sxx += x * x;
                syy += y * y;
                sxy += x * y;
            }
            let (vx, vy) = (sxx - sx * sx / n, syy - sy * sy / n);
            let found = (n >= 3.0 && vx > 0.0 && vy > 0.0)
                .then(|| ((sxy - sx * sy / n) / (vx * vy).sqrt()).clamp(-1.0, 1.0));
            r[i][j] = found;
            r[j][i] = found;
        }
    }
    CorrelationMatrix {
        columns: varying.into_iter().map(|(index, _)| index).collect(),
        left_out,
        r,
    }
}

/// Runs `comparison` over `set`, whose kinds must be known (see
/// [`stats::kinds`]).
pub fn compare(set: &ResultSet, comparison: Comparison, options: &CompareOptions) -> Outcome {
    let kind = |column: usize| set.kinds().get(column).copied().unwrap_or_default();
    let pairs = |a: usize, b: usize| set.column_values(a).zip(set.column_values(b));
    match comparison {
        Comparison::Groups { measure, group } => groups(pairs(measure, group), options),
        Comparison::CrossTab { rows, columns } => cross_tab(pairs(rows, columns), options),
        Comparison::Series { time, measure } => series(
            pairs(time, measure),
            kind(time) == ColumnKind::Date,
            options,
        ),
        Comparison::Breakdown { time, category } => breakdown(
            pairs(time, category),
            kind(time) == ColumnKind::Date,
            options,
        ),
        Comparison::Correlation { x, y } => correlation(pairs(x, y)),
    }
}

/// Value counts, most frequent first; ties by value, NULL after values.
fn ranked<'a>(counts: &HashMap<Option<&'a str>, usize>) -> Vec<(Option<&'a str>, usize)> {
    let mut ranked: Vec<(Option<&str>, usize)> = counts.iter().map(|(k, c)| (*k, *c)).collect();
    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.is_none().cmp(&b.0.is_none()))
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked
}

/// The keys shown, and each value's index among them (values beyond `keep`
/// share the last index, [`Key::Other`]).
fn fold<'a>(
    counts: &HashMap<Option<&'a str>, usize>,
    keep: usize,
) -> (Vec<Key>, HashMap<Option<&'a str>, usize>) {
    let ranked = ranked(counts);
    let keep = keep.max(1);
    let folds = ranked.len() > keep;
    let mut keys = Vec::new();
    let mut index = HashMap::new();
    for (position, (value, _)) in ranked.iter().enumerate() {
        if folds && position >= keep {
            index.insert(*value, keep);
        } else {
            keys.push(Key::of(*value));
            index.insert(*value, position);
        }
    }
    if folds {
        keys.push(Key::Other);
    }
    (keys, index)
}

fn group_stats(key: Key, values: &mut [f64]) -> GroupStats {
    values.sort_by(f64::total_cmp);
    let sum: f64 = values.iter().sum();
    GroupStats {
        key,
        n: values.len(),
        sum,
        mean: sum / values.len() as f64,
        min: values[0],
        p25: stats::percentile(values, 0.25),
        median: stats::percentile(values, 0.5),
        p75: stats::percentile(values, 0.75),
        max: values[values.len() - 1],
    }
}

fn groups<'a>(
    pairs: impl Iterator<Item = (Option<&'a str>, Option<&'a str>)>,
    options: &CompareOptions,
) -> Outcome {
    let mut by_group: HashMap<Option<&str>, Vec<f64>> = HashMap::new();
    for (measure, group) in pairs {
        if let Some(value) = measure.and_then(stats::parse_number) {
            by_group.entry(group).or_default().push(value);
        }
    }
    if by_group.is_empty() {
        return Outcome::Empty;
    }
    let all: Vec<f64> = by_group.values().flatten().copied().collect();
    let grand_mean = all.iter().sum::<f64>() / all.len() as f64;
    let total_ss: f64 = all.iter().map(|v| (v - grand_mean).powi(2)).sum();
    let between_ss: f64 = by_group
        .values()
        .map(|values| {
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            values.len() as f64 * (mean - grand_mean).powi(2)
        })
        .sum();
    let counts: HashMap<Option<&str>, usize> =
        by_group.iter().map(|(k, v)| (*k, v.len())).collect();
    let (keys, index) = fold(&counts, options.groups);
    let mut values: Vec<Vec<f64>> = vec![Vec::new(); keys.len()];
    for (group, group_values) in by_group {
        values[index[&group]].extend(group_values);
    }
    let mut all = all;
    Outcome::Groups(GroupTable {
        groups: keys
            .into_iter()
            .zip(values.iter_mut())
            .map(|(key, values)| group_stats(key, values))
            .collect(),
        overall: group_stats(Key::Other, &mut all),
        distinct: counts.len(),
        eta_squared: if total_ss > 0.0 {
            between_ss / total_ss
        } else {
            0.0
        },
    })
}

fn cross_tab<'a>(
    pairs: impl Iterator<Item = (Option<&'a str>, Option<&'a str>)>,
    options: &CompareOptions,
) -> Outcome {
    let mut cells: HashMap<(Option<&str>, Option<&str>), usize> = HashMap::new();
    let mut row_counts: HashMap<Option<&str>, usize> = HashMap::new();
    let mut column_counts: HashMap<Option<&str>, usize> = HashMap::new();
    let mut total = 0;
    for (row, column) in pairs {
        *cells.entry((row, column)).or_default() += 1;
        *row_counts.entry(row).or_default() += 1;
        *column_counts.entry(column).or_default() += 1;
        total += 1;
    }
    if total == 0 {
        return Outcome::Empty;
    }
    // Over the whole table, before folding: χ² = N·(Σ O²/(Rᵢ·Cⱼ) − 1).
    let sum: f64 = cells
        .iter()
        .map(|((row, column), count)| {
            (*count as f64).powi(2) / (row_counts[row] as f64 * column_counts[column] as f64)
        })
        .sum();
    let chi_squared = total as f64 * (sum - 1.0);
    let smaller = row_counts.len().min(column_counts.len());
    let cramers_v =
        (smaller > 1).then(|| (chi_squared / (total as f64 * (smaller - 1) as f64)).sqrt());

    let (rows, row_index) = fold(&row_counts, options.groups);
    let (columns, column_index) = fold(&column_counts, options.groups);
    let mut counts = vec![vec![0; columns.len()]; rows.len()];
    let mut row_totals = vec![0; rows.len()];
    let mut column_totals = vec![0; columns.len()];
    for ((row, column), count) in cells {
        let (r, c) = (row_index[&row], column_index[&column]);
        counts[r][c] += count;
        row_totals[r] += count;
        column_totals[c] += count;
    }
    Outcome::CrossTab(CrossTab {
        rows,
        columns,
        counts,
        row_totals,
        column_totals,
        total,
        cramers_v,
    })
}

fn series<'a>(
    pairs: impl Iterator<Item = (Option<&'a str>, Option<&'a str>)>,
    dates_only: bool,
    options: &CompareOptions,
) -> Outcome {
    let points: Vec<(f64, f64)> = pairs
        .filter_map(|(time, measure)| {
            Some((stats::wall_clock(time?)?, stats::parse_number(measure?)?))
        })
        .collect();
    let Some(buckets) = buckets_of(points.iter().map(|p| p.0), dates_only, options) else {
        return Outcome::Empty;
    };
    let mut values: Vec<Vec<f64>> = vec![Vec::new(); buckets.len];
    for (seconds, value) in points {
        values[buckets.index(seconds)].push(value);
    }
    Outcome::Series(Series {
        granularity: buckets.granularity,
        aggregate: options.aggregate,
        points: values
            .iter_mut()
            .enumerate()
            .map(|(index, values)| {
                values.sort_by(f64::total_cmp);
                Point {
                    label: buckets.label(index),
                    n: values.len(),
                    value: options.aggregate.apply(values),
                }
            })
            .collect(),
    })
}

fn buckets_of(
    seconds: impl Iterator<Item = f64>,
    dates_only: bool,
    options: &CompareOptions,
) -> Option<Buckets> {
    let (first, last) = seconds.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), s| {
        (lo.min(s), hi.max(s))
    });
    (first <= last).then(|| Buckets::new(first, last, dates_only, options.granularity))
}

fn breakdown<'a>(
    pairs: impl Iterator<Item = (Option<&'a str>, Option<&'a str>)>,
    dates_only: bool,
    options: &CompareOptions,
) -> Outcome {
    let points: Vec<(f64, Option<&str>)> = pairs
        .filter_map(|(time, category)| Some((stats::wall_clock(time?)?, category)))
        .collect();
    let Some(buckets) = buckets_of(points.iter().map(|p| p.0), dates_only, options) else {
        return Outcome::Empty;
    };
    let mut category_counts: HashMap<Option<&str>, usize> = HashMap::new();
    for (_, category) in &points {
        *category_counts.entry(*category).or_default() += 1;
    }
    let (categories, index) = fold(&category_counts, options.groups.min(MAX_STACKED));
    let mut counts = vec![vec![0; categories.len()]; buckets.len];
    for (seconds, category) in points {
        counts[buckets.index(seconds)][index[&category]] += 1;
    }
    Outcome::Breakdown(Breakdown {
        granularity: buckets.granularity,
        periods: (0..buckets.len).map(|i| buckets.label(i)).collect(),
        categories,
        counts,
    })
}

fn correlation<'a>(pairs: impl Iterator<Item = (Option<&'a str>, Option<&'a str>)>) -> Outcome {
    let points: Vec<(f64, f64)> = pairs
        .filter_map(|(x, y)| Some((stats::parse_number(x?)?, stats::parse_number(y?)?)))
        .collect();
    if points.len() < 2 {
        return Outcome::Empty;
    }
    let xs: Vec<f64> = points.iter().map(|p| p.0).collect();
    let ys: Vec<f64> = points.iter().map(|p| p.1).collect();
    let line = least_squares(&xs, &ys);
    let range = |values: &[f64]| {
        values
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
                (lo.min(*v), hi.max(*v))
            })
    };
    let sample = if points.len() > MAX_POINTS {
        (0..MAX_POINTS)
            .map(|i| points[i * points.len() / MAX_POINTS])
            .collect()
    } else {
        points.clone()
    };
    Outcome::Correlation(Correlation {
        x_range: range(&xs),
        y_range: range(&ys),
        points: sample,
        n: points.len(),
        pearson: pearson(&xs, &ys),
        spearman: pearson(&ranks(&xs), &ranks(&ys)),
        slope: line.map(|(slope, _)| slope),
        intercept: line.map(|(_, intercept)| intercept),
    })
}

fn moments(xs: &[f64], ys: &[f64]) -> (f64, f64, f64, f64, f64) {
    let n = xs.len() as f64;
    let (mx, my) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (x, y) in xs.iter().zip(ys) {
        sxy += (x - mx) * (y - my);
        sxx += (x - mx).powi(2);
        syy += (y - my).powi(2);
    }
    (mx, my, sxy, sxx, syy)
}

fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    let (_, _, sxy, sxx, syy) = moments(xs, ys);
    (sxx > 0.0 && syy > 0.0).then(|| sxy / (sxx * syy).sqrt())
}

fn least_squares(xs: &[f64], ys: &[f64]) -> Option<(f64, f64)> {
    let (mx, my, sxy, sxx, _) = moments(xs, ys);
    (sxx > 0.0).then(|| {
        let slope = sxy / sxx;
        (slope, my - slope * mx)
    })
}

/// Ranks from 1, ties sharing their average rank.
fn ranks(values: &[f64]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|a, b| values[*a].total_cmp(&values[*b]));
    let mut ranks = vec![0.0; values.len()];
    let mut start = 0;
    while start < order.len() {
        let mut end = start;
        while end + 1 < order.len() && values[order[end + 1]] == values[order[start]] {
            end += 1;
        }
        let rank = (start + end) as f64 / 2.0 + 1.0;
        for position in start..=end {
            ranks[order[position]] = rank;
        }
        start = end + 1;
    }
    ranks
}

#[cfg(test)]
mod tests {
    use super::*;

    type Rows<'a> = &'a [(Option<&'a str>, Option<&'a str>)];

    fn pairs<'a>(rows: Rows<'a>) -> impl Iterator<Item = (Option<&'a str>, Option<&'a str>)> {
        rows.iter().copied()
    }

    #[test]
    fn offers_comparisons_that_fit_the_kinds() {
        use ColumnKind::*;
        assert_eq!(
            comparisons(0, Number, 1, Text),
            [Comparison::Groups {
                measure: 0,
                group: 1
            }]
        );
        assert_eq!(
            comparisons(0, Timestamp, 1, Number),
            [Comparison::Series {
                time: 0,
                measure: 1
            }]
        );
        let integers = comparisons(0, Integer, 1, Integer);
        assert_eq!(integers[0], Comparison::Correlation { x: 0, y: 1 });
        assert!(integers.contains(&Comparison::CrossTab {
            rows: 0,
            columns: 1
        }));
        assert_eq!(
            comparisons(0, Text, 1, Date),
            [Comparison::Breakdown {
                time: 1,
                category: 0
            }]
        );
        assert!(comparisons(0, Date, 1, Timestamp).is_empty());
    }

    #[test]
    fn groups_summarize_a_measure_and_fold_small_groups() {
        let rows = [
            (Some("10"), Some("a")),
            (Some("20"), Some("a")),
            (Some("30"), Some("a")),
            (Some("100"), Some("b")),
            (Some("200"), Some("b")),
            (Some("5"), None),
            (None, Some("c")),
            (Some("x"), Some("c")),
        ];
        let options = CompareOptions {
            groups: 1,
            ..CompareOptions::default()
        };
        let Outcome::Groups(table) = groups(pairs(&rows), &options) else {
            panic!("groups");
        };
        assert_eq!(table.distinct, 3);
        assert_eq!(table.groups.len(), 2);
        let a = &table.groups[0];
        assert_eq!(
            (&a.key, a.n, a.mean, a.median),
            (&Key::Value("a".into()), 3, 20.0, 20.0)
        );
        let other = &table.groups[1];
        assert_eq!((&other.key, other.n, other.sum), (&Key::Other, 3, 305.0));
        assert_eq!(table.overall.n, 6);
        assert!(table.eta_squared > 0.8 && table.eta_squared <= 1.0);
    }

    #[test]
    fn cross_tabs_count_pairs_and_measure_association() {
        let mut rows = Vec::new();
        for _ in 0..10 {
            rows.push((Some("paid"), Some("card")));
            rows.push((Some("refunded"), Some("cash")));
        }
        let Outcome::CrossTab(table) = cross_tab(pairs(&rows), &CompareOptions::default()) else {
            panic!("cross tab");
        };
        assert_eq!(table.total, 20);
        assert_eq!(table.counts, [[10, 0], [0, 10]]);
        assert!((table.cramers_v.unwrap() - 1.0).abs() < 1e-9);

        let independent = [
            (Some("a"), Some("x")),
            (Some("a"), Some("y")),
            (Some("b"), Some("x")),
            (Some("b"), Some("y")),
        ];
        let Outcome::CrossTab(table) = cross_tab(pairs(&independent), &CompareOptions::default())
        else {
            panic!("cross tab");
        };
        assert!(table.cramers_v.unwrap().abs() < 1e-9);
    }

    #[test]
    fn series_aggregate_a_measure_per_period() {
        let rows = [
            (Some("2026-01-01"), Some("10")),
            (Some("2026-01-01"), Some("30")),
            (Some("2026-01-03"), Some("5")),
            (None, Some("99")),
        ];
        let options = CompareOptions {
            granularity: Granularity::Day,
            aggregate: Aggregate::Sum,
            ..CompareOptions::default()
        };
        let Outcome::Series(series) = series(pairs(&rows), true, &options) else {
            panic!("series");
        };
        let values: Vec<Option<f64>> = series.points.iter().map(|p| p.value).collect();
        assert_eq!(values, [Some(40.0), None, Some(5.0)]);
        assert_eq!(series.points[1].label, "2026-01-02");
        assert_eq!(Aggregate::Count.apply(&[]), Some(0.0));
        assert_eq!(Aggregate::Median.apply(&[1.0, 2.0, 10.0]), Some(2.0));
    }

    #[test]
    fn breakdowns_split_each_period_by_category() {
        let rows = [
            (Some("2026-01-05"), Some("web")),
            (Some("2026-01-06"), Some("app")),
            (Some("2026-02-01"), Some("web")),
            (Some("2026-02-02"), None),
        ];
        let options = CompareOptions {
            granularity: Granularity::Month,
            ..CompareOptions::default()
        };
        let Outcome::Breakdown(breakdown) = breakdown(pairs(&rows), true, &options) else {
            panic!("breakdown");
        };
        assert_eq!(breakdown.periods, ["2026-01", "2026-02"]);
        assert_eq!(
            breakdown.categories,
            [
                Key::Value("web".into()),
                Key::Value("app".into()),
                Key::Null
            ]
        );
        assert_eq!(breakdown.counts, [[1, 1, 0], [1, 0, 1]]);
    }

    #[test]
    fn correlations_report_pearson_spearman_and_a_line() {
        let rows = [
            (Some("1"), Some("3")),
            (Some("2"), Some("5")),
            (Some("3"), Some("7")),
            (Some("4"), Some("9")),
            (Some("5"), None),
        ];
        let Outcome::Correlation(found) = correlation(pairs(&rows)) else {
            panic!("correlation");
        };
        assert_eq!(found.n, 4);
        assert!((found.pearson.unwrap() - 1.0).abs() < 1e-9);
        assert!((found.spearman.unwrap() - 1.0).abs() < 1e-9);
        assert!((found.slope.unwrap() - 2.0).abs() < 1e-9);
        assert!((found.intercept.unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(ranks(&[10.0, 20.0, 10.0]), [1.5, 3.0, 1.5]);
        let flat = [(Some("1"), Some("2")), (Some("1"), Some("3"))];
        let Outcome::Correlation(flat) = correlation(pairs(&flat)) else {
            panic!("correlation");
        };
        assert_eq!((flat.pearson, flat.slope), (None, None));
        assert_eq!((flat.x_range, flat.y_range), ((1.0, 1.0), (2.0, 3.0)));
    }

    #[test]
    fn a_matrix_correlates_every_two_numeric_columns_that_vary() {
        use crate::result_set::{BatchBuilder, ColumnKind};
        let names = ["x", "double", "down", "flat", "word", "sparse"];
        let mut set = ResultSet::new(names.iter().map(|n| n.to_string()).collect());
        set.set_kinds(vec![
            ColumnKind::Integer,
            ColumnKind::Number,
            ColumnKind::Integer,
            ColumnKind::Integer,
            ColumnKind::Text,
            ColumnKind::Integer,
        ]);
        let rows: Vec<[Option<String>; 6]> = (1..=6)
            .map(|n: i64| {
                [
                    Some(n.to_string()),
                    Some((n * 2).to_string()),
                    Some((100 - n * n).to_string()),
                    Some("7".into()),
                    Some("w".into()),
                    // Two rows with a number: too few to measure.
                    (n <= 2).then(|| n.to_string()),
                ]
            })
            .collect();
        let mut builder = BatchBuilder::new(6, rows.len());
        for row in &rows {
            builder.push_row(|index| row[index].as_deref());
        }
        set.push(builder.finish());

        let matrix = correlations(&set);
        // The flat column and the text one are not compared.
        assert_eq!(matrix.columns, [0, 1, 2, 5]);
        assert_eq!(matrix.left_out, 0);
        assert!((matrix.r[1][0].unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(matrix.r[0][1], matrix.r[1][0]);
        assert!(matrix.r[2][0].unwrap() < -0.9);
        assert_eq!(matrix.r[0][0], None);
        assert_eq!(matrix.r[3][0], None);

        // It agrees with the comparison of two columns.
        let Outcome::Correlation(pair) = compare(
            &set,
            Comparison::Correlation { x: 0, y: 2 },
            &CompareOptions::default(),
        ) else {
            panic!("correlation");
        };
        assert!((matrix.r[2][0].unwrap() - pair.pearson.unwrap()).abs() < 1e-9);

        let strongest = matrix.strongest();
        assert_eq!((strongest[0].0, strongest[0].1), (0, 1));
        assert_eq!(strongest.len(), 3);
        assert!(strongest[1].2.abs() >= strongest[2].2.abs());
    }

    #[test]
    fn scatter_points_are_sampled_evenly() {
        let values: Vec<String> = (0..10_000).map(|n| n.to_string()).collect();
        let rows: Vec<(Option<&str>, Option<&str>)> = values
            .iter()
            .map(|v| (Some(v.as_str()), Some(v.as_str())))
            .collect();
        let Outcome::Correlation(found) = correlation(pairs(&rows)) else {
            panic!("correlation");
        };
        assert_eq!(found.n, 10_000);
        assert_eq!(found.points.len(), MAX_POINTS);
        assert_eq!(found.points[0], (0.0, 0.0));
        assert_eq!(found.points[MAX_POINTS - 1], (9_997.0, 9_997.0));
        assert_eq!(found.x_range, (0.0, 9_999.0));
    }
}
