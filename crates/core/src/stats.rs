//! Column statistics for a result. [`overview`] gives every column a cheap
//! summary: counts, distinct values and the shape of a small chart.
//! [`analyze`] looks at one column in depth, the way its [`Options`] say:
//! as numbers, categories, a timeline or text.
//!
//! Values are the text PostgreSQL sent; a column's [`ColumnKind`] says how to
//! read them. When the server did not report it, it is inferred from the
//! values. Everything here is plain computation over a [`ResultSet`], meant to
//! run off the UI thread.

use std::collections::HashMap;

use crate::result_set::{ColumnKind, ResultSet};

/// Bars in a histogram unless the options say otherwise.
pub const BINS: usize = 20;
/// Bars in an overview's small chart.
const SPARK: usize = 16;
/// Columns with at least this many values, all different, read as keys.
const KEY_MIN: usize = 10;
/// Integers with at most this many distinct values read as categories.
const INTEGER_CATEGORIES: usize = 12;
/// Text with at most this many distinct values reads as categories.
const TEXT_CATEGORIES: usize = 50;
/// Timeline buckets above which a coarser granularity is used.
const MAX_BUCKETS: usize = 400;

/// The cheap summary of a column, for the list of columns.
#[derive(Debug, Clone, PartialEq)]
pub struct Overview {
    pub name: String,
    /// The kind used: the server's, or the inferred one.
    pub kind: ColumnKind,
    /// Whether `kind` was inferred from the values.
    pub inferred: bool,
    pub rows: usize,
    pub nulls: usize,
    pub distinct: usize,
    /// Every value is different (and there are enough to tell): a likely key.
    pub unique: bool,
    /// Bar heights from 0 to 1: a histogram for numbers and dates, the most
    /// frequent values for everything else.
    pub spark: Vec<f32>,
}

impl Overview {
    /// Non-NULL values.
    pub fn values(&self) -> usize {
        self.rows - self.nulls
    }
}

/// How to look at a column.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Picked from the kind and the values.
    #[default]
    Auto,
    Numeric,
    Categorical,
    Temporal,
    Text,
}

impl Mode {
    pub const ALL: [Mode; 5] = [
        Mode::Auto,
        Mode::Numeric,
        Mode::Categorical,
        Mode::Temporal,
        Mode::Text,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Mode::Auto => "Auto",
            Mode::Numeric => "Numeric",
            Mode::Categorical => "Categorical",
            Mode::Temporal => "Temporal",
            Mode::Text => "Text",
        }
    }

    /// Whether a column of `kind` can be read this way.
    pub fn applies_to(self, kind: ColumnKind) -> bool {
        match self {
            Mode::Auto | Mode::Categorical | Mode::Text => true,
            Mode::Numeric => kind.is_numeric(),
            Mode::Temporal => kind.is_temporal(),
        }
    }
}

/// How a timeline groups its values. The last two fold every value onto a
/// cycle (the day of the week, the hour of the day) to show patterns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Granularity {
    /// Picked from the span of the values.
    #[default]
    Auto,
    Hour,
    Day,
    Week,
    Month,
    Year,
    Weekday,
    HourOfDay,
}

impl Granularity {
    pub const TIMELINE: [Granularity; 6] = [
        Granularity::Auto,
        Granularity::Hour,
        Granularity::Day,
        Granularity::Week,
        Granularity::Month,
        Granularity::Year,
    ];
    pub const CYCLES: [Granularity; 2] = [Granularity::Weekday, Granularity::HourOfDay];

    pub fn label(self) -> &'static str {
        match self {
            Granularity::Auto => "Auto",
            Granularity::Hour => "Hour",
            Granularity::Day => "Day",
            Granularity::Week => "Week",
            Granularity::Month => "Month",
            Granularity::Year => "Year",
            Granularity::Weekday => "Weekday",
            Granularity::HourOfDay => "Hour of day",
        }
    }

    /// The next coarser timeline granularity.
    fn coarser(self) -> Option<Granularity> {
        match self {
            Granularity::Hour => Some(Granularity::Day),
            Granularity::Day => Some(Granularity::Week),
            Granularity::Week => Some(Granularity::Month),
            Granularity::Month => Some(Granularity::Year),
            _ => None,
        }
    }
}

/// What [`analyze`] does with a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Options {
    pub mode: Mode,
    /// Histogram bars; `None` picks them (one per value for small integer
    /// ranges).
    pub bins: Option<usize>,
    /// Bar heights on a log scale, so small counts stay visible.
    pub log_scale: bool,
    /// Leaves values beyond 1.5 × IQR out of the histogram (not the stats).
    pub hide_outliers: bool,
    pub granularity: Granularity,
    /// Most frequent values listed.
    pub top: usize,
    /// Lists NULL among the categories.
    pub include_nulls: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mode: Mode::Auto,
            bins: None,
            log_scale: false,
            hide_outliers: false,
            granularity: Granularity::Auto,
            top: 10,
            include_nulls: false,
        }
    }
}

/// One column, in depth.
#[derive(Debug, Clone, PartialEq)]
pub struct Analysis {
    pub overview: Overview,
    pub summary: Summary,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Summary {
    Numeric(NumericSummary),
    Temporal(TemporalSummary),
    Categorical(CategoricalSummary),
    Text(TextSummary),
    Boolean {
        trues: usize,
        falses: usize,
    },
    /// Every value is different: only the range says anything.
    Key {
        min: String,
        max: String,
    },
    /// No values to summarize (all NULL, or none parsed).
    Empty,
}

impl Summary {
    /// What the summary shows, for "Auto · …".
    pub fn label(&self) -> &'static str {
        match self {
            Summary::Numeric(_) => "numeric",
            Summary::Temporal(_) => "temporal",
            Summary::Categorical(_) => "categorical",
            Summary::Text(_) => "text",
            Summary::Boolean { .. } => "boolean",
            Summary::Key { .. } => "key",
            Summary::Empty => "empty",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NumericSummary {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub stddev: f64,
    pub p25: f64,
    pub p50: f64,
    pub p75: f64,
    pub p90: f64,
    pub p99: f64,
    pub histogram: Vec<Bin>,
    /// Values left out of the histogram as outliers.
    pub outliers: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TemporalSummary {
    /// The earliest and latest values, as written.
    pub min: String,
    pub max: String,
    /// The granularity used (Auto resolved, or coarser than asked when that
    /// would make too many buckets).
    pub granularity: Granularity,
    /// In order, empty ones included.
    pub buckets: Vec<(String, usize)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CategoricalSummary {
    /// Most frequent first; ties by value. `None` is NULL.
    pub top: Vec<(Option<String>, usize)>,
    /// Values outside `top`.
    pub others: usize,
    /// Values counted (NULLs only when listed).
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextSummary {
    pub min_length: usize,
    pub max_length: usize,
    pub mean_length: f64,
    /// Lengths in characters.
    pub lengths: Vec<Bin>,
    /// Empty or whitespace-only values.
    pub blank: usize,
    pub top: Vec<(String, usize)>,
}

/// `[start, end)`, except the last bin, which includes `end`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bin {
    pub start: f64,
    pub end: f64,
    pub count: usize,
}

/// The overview of every column of `set`.
pub fn overview(set: &ResultSet) -> Vec<Overview> {
    set.columns()
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let kind = set.kinds().get(index).copied().unwrap_or_default();
            overview_column(name, kind, || set.column_values(index))
        })
        .collect()
}

/// Values read to infer a column's kind when the server did not report it.
const KIND_SAMPLE: usize = 1_000;

/// Every column's kind, and whether it was inferred: the server's when it
/// reported one, else what the first non-NULL values fit. Cheap enough to
/// run before anything is analyzed.
pub fn kinds(set: &ResultSet) -> Vec<(ColumnKind, bool)> {
    (0..set.columns().len())
        .map(
            |index| match set.kinds().get(index).copied().unwrap_or_default() {
                ColumnKind::Unknown => (
                    infer_kind(set.column_values(index).flatten().take(KIND_SAMPLE)),
                    true,
                ),
                kind => (kind, false),
            },
        )
        .collect()
}

/// Analyzes column `column` of `set`.
pub fn analyze(set: &ResultSet, column: usize, options: &Options) -> Analysis {
    let kind = set.kinds().get(column).copied().unwrap_or_default();
    let name = set.columns().get(column).map_or("", String::as_str);
    analyze_column(name, kind, options, || set.column_values(column))
}

/// Counts and kind of a column, shared by the overview and the analysis.
struct Counted<'a> {
    kind: ColumnKind,
    inferred: bool,
    rows: usize,
    nulls: usize,
    counts: HashMap<&'a str, usize>,
}

impl Counted<'_> {
    fn values(&self) -> usize {
        self.rows - self.nulls
    }

    fn unique(&self) -> bool {
        self.values() >= KEY_MIN && self.counts.len() == self.values()
    }

    /// Value counts, most frequent first; ties by value.
    fn ranked(&self) -> Vec<(&str, usize)> {
        let mut ranked: Vec<(&str, usize)> = self.counts.iter().map(|(v, c)| (*v, *c)).collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        ranked
    }

    fn overview(&self, name: &str, spark: Vec<f32>) -> Overview {
        Overview {
            name: name.to_string(),
            kind: self.kind,
            inferred: self.inferred,
            rows: self.rows,
            nulls: self.nulls,
            distinct: self.counts.len(),
            unique: self.unique(),
            spark,
        }
    }
}

fn count<'a, I>(kind: ColumnKind, values: &impl Fn() -> I) -> Counted<'a>
where
    I: Iterator<Item = Option<&'a str>>,
{
    let (kind, inferred) = match kind {
        ColumnKind::Unknown => (infer_kind(values().flatten()), true),
        kind => (kind, false),
    };
    let mut rows = 0;
    let mut nulls = 0;
    let mut counts = HashMap::new();
    for value in values() {
        rows += 1;
        match value {
            Some(value) => *counts.entry(value).or_default() += 1,
            None => nulls += 1,
        }
    }
    Counted {
        kind,
        inferred,
        rows,
        nulls,
        counts,
    }
}

/// The overview of one column. `values` is called up to three times.
pub fn overview_column<'a, I>(name: &str, kind: ColumnKind, values: impl Fn() -> I) -> Overview
where
    I: Iterator<Item = Option<&'a str>>,
{
    let counted = count(kind, &values);
    let counts: Vec<usize> = if counted.kind.is_numeric() {
        let numbers: Vec<f64> = values().flatten().filter_map(parse_number).collect();
        bin_counts(&numbers, SPARK)
    } else if counted.kind.is_temporal() {
        let seconds: Vec<f64> = values().flatten().filter_map(wall_clock).collect();
        bin_counts(&seconds, SPARK)
    } else {
        counted.ranked().iter().take(8).map(|(_, c)| *c).collect()
    };
    let tallest = counts.iter().copied().max().unwrap_or(0).max(1) as f32;
    let spark = counts.iter().map(|c| *c as f32 / tallest).collect();
    counted.overview(name, spark)
}

fn bin_counts(values: &[f64], bins: usize) -> Vec<usize> {
    let (min, max) = values
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(*v), hi.max(*v))
        });
    if values.is_empty() {
        return Vec::new();
    }
    histogram(values, min, max, bins, false)
        .into_iter()
        .map(|bin| bin.count)
        .collect()
}

/// One column in depth. `values` is called a few times.
pub fn analyze_column<'a, I>(
    name: &str,
    kind: ColumnKind,
    options: &Options,
    values: impl Fn() -> I,
) -> Analysis
where
    I: Iterator<Item = Option<&'a str>>,
{
    let counted = count(kind, &values);
    let mode = if options.mode.applies_to(counted.kind) {
        options.mode
    } else {
        Mode::Auto
    };
    let summary = if counted.values() == 0 {
        Summary::Empty
    } else {
        match mode {
            Mode::Auto => auto(&counted, options, &values),
            Mode::Numeric => numeric(&values, counted.kind, options),
            Mode::Temporal => temporal(&values, counted.kind, options.granularity),
            Mode::Categorical => categorical(&counted, options),
            Mode::Text => text(&counted, options),
        }
    };
    Analysis {
        overview: counted.overview(name, Vec::new()),
        summary,
    }
}

/// The summary that suits the column: keys get their range, booleans a
/// split, integers and text with few values their categories.
fn auto<'a, I>(counted: &Counted<'a>, options: &Options, values: &impl Fn() -> I) -> Summary
where
    I: Iterator<Item = Option<&'a str>>,
{
    let distinct = counted.counts.len();
    match counted.kind {
        ColumnKind::Integer | ColumnKind::Text | ColumnKind::Unknown if counted.unique() => {
            key(counted)
        }
        ColumnKind::Boolean => boolean(counted),
        ColumnKind::Integer if distinct <= INTEGER_CATEGORIES => categorical(counted, options),
        ColumnKind::Integer | ColumnKind::Number => numeric(values, counted.kind, options),
        ColumnKind::Date | ColumnKind::Timestamp => {
            temporal(values, counted.kind, options.granularity)
        }
        ColumnKind::Text | ColumnKind::Unknown if distinct <= TEXT_CATEGORIES => {
            categorical(counted, options)
        }
        ColumnKind::Text | ColumnKind::Unknown => text(counted, options),
    }
}

/// The narrowest kind every value fits: integer, number, boolean, date,
/// timestamp, else text. No values: text.
pub fn infer_kind<'a>(values: impl Iterator<Item = &'a str>) -> ColumnKind {
    let mut fits = [true; 5]; // integer, number, boolean, date, timestamp
    let mut any = false;
    for value in values {
        any = true;
        fits[0] &= value.parse::<i64>().is_ok();
        fits[1] &= parse_number(value).is_some();
        fits[2] &= matches!(value, "t" | "f" | "true" | "false");
        fits[3] &= value.len() == 10 && parse_date(value).is_some();
        fits[4] &= value.len() > 10 && parse_timestamp(value).is_some();
        if !fits.iter().any(|fit| *fit) {
            return ColumnKind::Text;
        }
    }
    if !any {
        return ColumnKind::Text;
    }
    [
        ColumnKind::Integer,
        ColumnKind::Number,
        ColumnKind::Boolean,
        ColumnKind::Date,
        ColumnKind::Timestamp,
    ]
    .into_iter()
    .zip(fits)
    .find_map(|(kind, fit)| fit.then_some(kind))
    .unwrap_or(ColumnKind::Text)
}

pub fn parse_number(value: &str) -> Option<f64> {
    value.parse::<f64>().ok().filter(|n| n.is_finite())
}

fn numeric<'a, I>(values: &impl Fn() -> I, kind: ColumnKind, options: &Options) -> Summary
where
    I: Iterator<Item = Option<&'a str>>,
{
    let mut values: Vec<f64> = values().flatten().filter_map(parse_number).collect();
    if values.is_empty() {
        return Summary::Empty;
    }
    values.sort_by(f64::total_cmp);
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    let (min, max) = (values[0], values[values.len() - 1]);
    let (p25, p75) = (percentile(&values, 0.25), percentile(&values, 0.75));
    let shown: &[f64] = if options.hide_outliers {
        let fence = 1.5 * (p75 - p25);
        let low = values.partition_point(|v| *v < p25 - fence);
        let high = values.partition_point(|v| *v <= p75 + fence);
        &values[low..high]
    } else {
        &values
    };
    let histogram = match (shown.first(), shown.last()) {
        (Some(first), Some(last)) => histogram(
            shown,
            *first,
            *last,
            options.bins.unwrap_or(BINS),
            options.bins.is_none() && kind == ColumnKind::Integer,
        ),
        _ => Vec::new(),
    };
    Summary::Numeric(NumericSummary {
        min,
        max,
        mean,
        stddev: variance.sqrt(),
        p25,
        p50: percentile(&values, 0.50),
        p75,
        p90: percentile(&values, 0.90),
        p99: percentile(&values, 0.99),
        histogram,
        outliers: values.len() - shown.len(),
    })
}

/// Linear interpolation between the closest ranks of sorted `values`.
pub(crate) fn percentile(sorted: &[f64], q: f64) -> f64 {
    let position = q * (sorted.len() - 1) as f64;
    let (low, high) = (position.floor() as usize, position.ceil() as usize);
    let fraction = position - low as f64;
    sorted[low] + (sorted[high] - sorted[low]) * fraction
}

/// `bins` equal-width bins over `[min, max]`. With `per_value`, an integer
/// range smaller than `bins` gets one bin per value, so a count like 1..=5
/// reads as five bars.
fn histogram(values: &[f64], min: f64, max: f64, bins: usize, per_value: bool) -> Vec<Bin> {
    if min == max {
        return vec![Bin {
            start: min,
            end: max,
            count: values.len(),
        }];
    }
    let bins = bins.max(1);
    let (bins, width) = if per_value && max - min < bins as f64 {
        ((max - min) as usize + 1, 1.0)
    } else {
        (bins, (max - min) / bins as f64)
    };
    let mut counts = vec![0; bins];
    for value in values {
        let index = (((value - min) / width) as usize).min(bins - 1);
        counts[index] += 1;
    }
    counts
        .into_iter()
        .enumerate()
        .map(|(index, count)| Bin {
            start: min + index as f64 * width,
            end: min + (index + 1) as f64 * width,
            count,
        })
        .collect()
}

fn temporal<'a, I>(values: &impl Fn() -> I, kind: ColumnKind, asked: Granularity) -> Summary
where
    I: Iterator<Item = Option<&'a str>>,
{
    let mut parsed: Vec<(f64, &str)> = values()
        .flatten()
        .filter_map(|value| Some((wall_clock(value)?, value)))
        .collect();
    if parsed.is_empty() {
        return Summary::Empty;
    }
    parsed.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (first, last) = (parsed[0], parsed[parsed.len() - 1]);
    let buckets = Buckets::new(first.0, last.0, kind == ColumnKind::Date, asked);
    let mut counts = vec![0; buckets.len];
    for (seconds, _) in &parsed {
        counts[buckets.index(*seconds)] += 1;
    }
    let granularity = buckets.granularity;
    let buckets = counts
        .into_iter()
        .enumerate()
        .map(|(index, count)| (buckets.label(index), count))
        .collect();
    Summary::Temporal(TemporalSummary {
        min: first.1.to_string(),
        max: last.1.to_string(),
        granularity,
        buckets,
    })
}

/// The buckets that wall-clock times between `first` and `last` fall into:
/// a timeline from the first bucket to the last, or a whole cycle.
pub(crate) struct Buckets {
    /// The granularity used: Auto resolved from the span, and coarser than
    /// asked when that would make too many buckets.
    pub granularity: Granularity,
    start: i64,
    pub len: usize,
}

impl Buckets {
    pub(crate) fn new(first: f64, last: f64, dates_only: bool, asked: Granularity) -> Self {
        let mut granularity = match asked {
            Granularity::Auto => {
                let days = (last - first) / 86_400.0;
                match days {
                    d if d <= 2.0 && !dates_only => Granularity::Hour,
                    d if d <= 62.0 => Granularity::Day,
                    d if d <= 366.0 => Granularity::Week,
                    d if d <= 3_660.0 => Granularity::Month,
                    _ => Granularity::Year,
                }
            }
            Granularity::Hour if dates_only => Granularity::Day,
            Granularity::HourOfDay if dates_only => Granularity::Weekday,
            asked => asked,
        };
        while let Some(coarser) = granularity.coarser()
            && bucket(last, granularity) - bucket(first, granularity) >= MAX_BUCKETS as i64
        {
            granularity = coarser;
        }
        let (start, len) = match granularity {
            Granularity::Weekday => (0, 7),
            Granularity::HourOfDay => (0, 24),
            _ => {
                let start = bucket(first, granularity);
                (start, (bucket(last, granularity) - start + 1) as usize)
            }
        };
        Self {
            granularity,
            start,
            len,
        }
    }

    /// The bucket of `seconds`, which must lie between `first` and `last`.
    pub(crate) fn index(&self, seconds: f64) -> usize {
        ((bucket(seconds, self.granularity) - self.start) as usize).min(self.len - 1)
    }

    pub(crate) fn label(&self, index: usize) -> String {
        bucket_label(self.start + index as i64, self.granularity)
    }
}

/// The bucket of a wall-clock time: a running number for timelines (hours,
/// days, Monday-based weeks, months or years since the epoch), the position
/// in the cycle for the others (0 is Monday, or midnight).
fn bucket(seconds: f64, granularity: Granularity) -> i64 {
    let hours = (seconds / 3_600.0).floor() as i64;
    let days = hours.div_euclid(24);
    match granularity {
        Granularity::Hour => hours,
        Granularity::Auto | Granularity::Day => days,
        // 1970-01-01 was a Thursday.
        Granularity::Week => (days + 3).div_euclid(7),
        Granularity::Month => {
            let (year, month, _) = civil_from_days(days);
            year * 12 + month as i64 - 1
        }
        Granularity::Year => civil_from_days(days).0,
        Granularity::Weekday => (days + 3).rem_euclid(7),
        Granularity::HourOfDay => hours.rem_euclid(24),
    }
}

fn bucket_label(key: i64, granularity: Granularity) -> String {
    let date = |days: i64| {
        let (year, month, day) = civil_from_days(days);
        format!("{year:04}-{month:02}-{day:02}")
    };
    match granularity {
        Granularity::Hour => format!("{} {:02}:00", date(key.div_euclid(24)), key.rem_euclid(24)),
        Granularity::Auto | Granularity::Day => date(key),
        Granularity::Week => date(key * 7 - 3),
        Granularity::Month => format!("{:04}-{:02}", key.div_euclid(12), key.rem_euclid(12) + 1),
        Granularity::Year => format!("{key:04}"),
        Granularity::Weekday => {
            ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][key as usize].into()
        }
        Granularity::HourOfDay => format!("{key:02}h"),
    }
}

fn categorical(counted: &Counted, options: &Options) -> Summary {
    let mut top: Vec<(Option<String>, usize)> = counted
        .ranked()
        .into_iter()
        .map(|(value, count)| (Some(value.to_string()), count))
        .collect();
    let mut total = counted.values();
    if options.include_nulls && counted.nulls > 0 {
        let at = top.partition_point(|(_, count)| *count >= counted.nulls);
        top.insert(at, (None, counted.nulls));
        total += counted.nulls;
    }
    top.truncate(options.top.max(1));
    let shown: usize = top.iter().map(|(_, count)| count).sum();
    Summary::Categorical(CategoricalSummary {
        top,
        others: total - shown,
        total,
    })
}

fn text(counted: &Counted, options: &Options) -> Summary {
    let (mut min_length, mut max_length, mut length_sum, mut blank) = (usize::MAX, 0, 0, 0);
    let mut lengths = Vec::with_capacity(counted.values());
    for (value, count) in &counted.counts {
        let length = value.chars().count();
        min_length = min_length.min(length);
        max_length = max_length.max(length);
        length_sum += length * count;
        if value.trim().is_empty() {
            blank += count;
        }
        lengths.extend(std::iter::repeat_n(length as f64, *count));
    }
    let top = counted
        .ranked()
        .into_iter()
        .take(options.top.max(1))
        .map(|(value, count)| (value.to_string(), count))
        .collect();
    Summary::Text(TextSummary {
        min_length,
        max_length,
        mean_length: length_sum as f64 / counted.values() as f64,
        lengths: histogram(&lengths, min_length as f64, max_length as f64, BINS, true),
        blank,
        top,
    })
}

fn boolean(counted: &Counted) -> Summary {
    let count = |values: [&str; 2]| -> usize {
        values
            .iter()
            .map(|value| counted.counts.get(value).copied().unwrap_or(0))
            .sum()
    };
    Summary::Boolean {
        trues: count(["t", "true"]),
        falses: count(["f", "false"]),
    }
}

fn key(counted: &Counted) -> Summary {
    let range = if counted.kind.is_numeric() {
        let numbers = counted
            .counts
            .keys()
            .filter_map(|value| Some((parse_number(value)?, *value)));
        let min = numbers.clone().min_by(|a, b| a.0.total_cmp(&b.0));
        let max = numbers.max_by(|a, b| a.0.total_cmp(&b.0));
        min.zip(max).map(|(min, max)| (min.1, max.1))
    } else {
        counted
            .counts
            .keys()
            .min()
            .zip(counted.counts.keys().max())
            .map(|(a, b)| (*a, *b))
    };
    match range {
        Some((min, max)) => Summary::Key {
            min: min.to_string(),
            max: max.to_string(),
        },
        None => Summary::Empty,
    }
}

/// A date or timestamp as seconds since the epoch on its own clock: the
/// offset is ignored, so hours read as written.
pub(crate) fn wall_clock(value: &str) -> Option<f64> {
    if value.len() == 10 {
        parse_date(value)
    } else {
        timestamp_parts(value).map(|(wall, _)| wall)
    }
}

/// `YYYY-MM-DD` as seconds since the Unix epoch (midnight UTC).
pub fn parse_date(value: &str) -> Option<f64> {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i64 = value[0..4].parse().ok()?;
    let month: u32 = value[5..7].parse().ok()?;
    let day: u32 = value[8..10].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) as f64 * 86_400.0)
}

/// `YYYY-MM-DD HH:MM:SS[.fff][±HH[:MM]]` (or with `T`) as seconds since the
/// Unix epoch; the offset, when present, is applied.
pub fn parse_timestamp(value: &str) -> Option<f64> {
    timestamp_parts(value).map(|(wall, offset)| wall - offset)
}

/// A timestamp's wall-clock seconds and its offset in seconds.
fn timestamp_parts(value: &str) -> Option<(f64, f64)> {
    // Timestamps are ASCII; anything else could split a character when sliced.
    if value.len() < 19 || !value.is_ascii() {
        return None;
    }
    let date = parse_date(&value[..10])?;
    if !matches!(value.as_bytes()[10], b' ' | b'T') {
        return None;
    }
    let time = &value[11..];
    let hours: f64 = time.get(0..2)?.parse().ok()?;
    let minutes: f64 = time.get(3..5)?.parse().ok()?;
    let seconds_end = time[6..]
        .find(['+', '-', 'Z'])
        .map_or(time.len(), |index| index + 6);
    let seconds: f64 = time.get(6..seconds_end)?.parse().ok()?;
    let offset = match time.get(seconds_end..) {
        None | Some("") | Some("Z") => 0.0,
        Some(zone) => {
            let sign = if zone.starts_with('-') { -1.0 } else { 1.0 };
            let digits: String = zone[1..].chars().filter(char::is_ascii_digit).collect();
            let hours: f64 = digits.get(0..2)?.parse().ok()?;
            let minutes: f64 = digits.get(2..4).map_or(Some(0.0), |m| m.parse().ok())?;
            sign * (hours * 3_600.0 + minutes * 60.0)
        }
    };
    Some((date + hours * 3_600.0 + minutes * 60.0 + seconds, offset))
}

/// Days from 1970-01-01 to the given civil date (proleptic Gregorian).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = month as i64;
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The civil date of a day count from 1970-01-01 (proleptic Gregorian).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze_values(kind: ColumnKind, options: Options, values: &[Option<&str>]) -> Analysis {
        analyze_column("c", kind, &options, || values.iter().copied())
    }

    fn as_numbers(analysis: Analysis) -> NumericSummary {
        match analysis.summary {
            Summary::Numeric(numbers) => numbers,
            other => panic!("numeric, got {other:?}"),
        }
    }

    fn owned(values: impl Iterator<Item = String>) -> Vec<String> {
        values.collect()
    }

    fn borrowed(values: &[String]) -> Vec<Option<&str>> {
        values.iter().map(|v| Some(v.as_str())).collect()
    }

    #[test]
    fn numeric_columns_get_ranges_percentiles_and_a_histogram() {
        let values = owned((1..=100).map(|n| format!("{n}.5")));
        let mut column = borrowed(&values);
        column.push(None);
        let analysis = analyze_values(ColumnKind::Number, Options::default(), &column);
        let overview = &analysis.overview;
        assert_eq!(
            (overview.rows, overview.nulls, overview.distinct),
            (101, 1, 100)
        );
        let numbers = as_numbers(analysis);
        assert_eq!((numbers.min, numbers.max), (1.5, 100.5));
        assert!((numbers.mean - 51.0).abs() < 1e-9);
        assert!((numbers.p50 - 51.0).abs() < 1e-9);
        assert!((numbers.p90 - 90.6).abs() < 1e-9);
        assert_eq!(numbers.histogram.len(), BINS);
        assert_eq!(
            numbers.histogram.iter().map(|b| b.count).sum::<usize>(),
            100
        );
    }

    #[test]
    fn bins_and_outliers_follow_the_options() {
        let mut values = owned((1..=99).map(|n| (n % 10).to_string()));
        values.push("1000".into());
        let column = borrowed(&values);
        let options = Options {
            mode: Mode::Numeric,
            bins: Some(5),
            hide_outliers: true,
            ..Options::default()
        };
        let numbers = as_numbers(analyze_values(ColumnKind::Integer, options, &column));
        assert_eq!(numbers.histogram.len(), 5);
        assert_eq!(numbers.outliers, 1);
        assert_eq!(numbers.max, 1000.0);
        assert!((numbers.histogram.last().unwrap().end - 9.0).abs() < 1e-9);
    }

    #[test]
    fn small_integer_ranges_get_a_bar_per_value() {
        let options = Options {
            mode: Mode::Numeric,
            ..Options::default()
        };
        let numbers = as_numbers(analyze_values(
            ColumnKind::Integer,
            options,
            &[Some("1"), Some("2"), Some("2"), Some("5")],
        ));
        let counts: Vec<usize> = numbers.histogram.iter().map(|b| b.count).collect();
        assert_eq!(counts, [1, 2, 0, 0, 1]);
    }

    #[test]
    fn a_single_value_is_one_bin() {
        let numbers = as_numbers(analyze_values(
            ColumnKind::Number,
            Options::default(),
            &[Some("2.5"), Some("2.5")],
        ));
        assert_eq!(
            numbers.histogram,
            [Bin {
                start: 2.5,
                end: 2.5,
                count: 2
            }]
        );
        assert_eq!(numbers.stddev, 0.0);
    }

    #[test]
    fn auto_picks_a_summary_from_kind_and_values() {
        let ids = owned((1..=20).map(|n| n.to_string()));
        let analysis = analyze_values(ColumnKind::Integer, Options::default(), &borrowed(&ids));
        assert!(analysis.overview.unique);
        assert_eq!(
            analysis.summary,
            Summary::Key {
                min: "1".into(),
                max: "20".into()
            }
        );

        let codes = owned((1..=40).map(|n| (n % 3).to_string()));
        let analysis = analyze_values(ColumnKind::Integer, Options::default(), &borrowed(&codes));
        assert_eq!(analysis.summary.label(), "categorical");

        let analysis = analyze_values(
            ColumnKind::Boolean,
            Options::default(),
            &[Some("t"), Some("f"), Some("t"), None],
        );
        assert_eq!(
            analysis.summary,
            Summary::Boolean {
                trues: 2,
                falses: 1
            }
        );

        let words = owned((0..60).map(|n| format!("word {}", n % 55)));
        let analysis = analyze_values(ColumnKind::Text, Options::default(), &borrowed(&words));
        assert_eq!(analysis.summary.label(), "text");
    }

    #[test]
    fn modes_that_do_not_apply_fall_back_to_auto() {
        let options = Options {
            mode: Mode::Temporal,
            ..Options::default()
        };
        let analysis = analyze_values(ColumnKind::Text, options, &[Some("a"), Some("a")]);
        assert_eq!(analysis.summary.label(), "categorical");
    }

    #[test]
    fn categories_list_the_most_frequent_values_and_nulls_on_request() {
        let column = [
            Some("paid"),
            Some("paid"),
            Some("pending"),
            Some("refunded"),
            None,
            None,
            None,
        ];
        let options = Options {
            mode: Mode::Categorical,
            top: 2,
            include_nulls: true,
            ..Options::default()
        };
        let Summary::Categorical(categories) =
            analyze_values(ColumnKind::Text, options, &column).summary
        else {
            panic!("categorical");
        };
        assert_eq!(categories.top, [(None, 3), (Some("paid".to_string()), 2)]);
        assert_eq!((categories.others, categories.total), (2, 7));
    }

    #[test]
    fn text_gets_lengths_blanks_and_top_values() {
        let options = Options {
            mode: Mode::Text,
            ..Options::default()
        };
        let Summary::Text(text) = analyze_values(
            ColumnKind::Text,
            options,
            &[Some("ab"), Some("ab"), Some("abcd"), Some(" ")],
        )
        .summary
        else {
            panic!("text");
        };
        assert_eq!((text.min_length, text.max_length, text.blank), (1, 4, 1));
        assert!((text.mean_length - 2.25).abs() < 1e-9);
        assert_eq!(text.top[0], ("ab".to_string(), 2));
        assert_eq!(
            text.lengths.iter().map(|b| b.count).collect::<Vec<_>>(),
            [1, 2, 0, 1]
        );
    }

    #[test]
    fn timelines_group_by_the_granularity_asked() {
        let column = [
            Some("2026-01-15 10:00:00+02"),
            Some("2026-01-31 23:59:00+02"),
            Some("2026-03-02 08:30:00+02"),
        ];
        let summary = |granularity| {
            let options = Options {
                granularity,
                ..Options::default()
            };
            match analyze_values(ColumnKind::Timestamp, options, &column).summary {
                Summary::Temporal(timeline) => timeline,
                other => panic!("temporal, got {other:?}"),
            }
        };
        let months = summary(Granularity::Month);
        assert_eq!(
            months.buckets,
            [
                ("2026-01".to_string(), 2),
                ("2026-02".to_string(), 0),
                ("2026-03".to_string(), 1)
            ]
        );
        assert_eq!(summary(Granularity::Auto).granularity, Granularity::Day);
        let hours = summary(Granularity::HourOfDay);
        assert_eq!(hours.buckets.len(), 24);
        assert_eq!(hours.buckets[23], ("23h".to_string(), 1));
        let weekdays = summary(Granularity::Weekday);
        // 2026-01-15 was a Thursday, 2026-01-31 a Saturday, 2026-03-02 a Monday.
        let counts: Vec<usize> = weekdays.buckets.iter().map(|(_, c)| *c).collect();
        assert_eq!(counts, [1, 0, 0, 1, 0, 1, 0]);
        assert_eq!(summary(Granularity::Week).buckets[0].0, "2026-01-12");
        // Hours over two months would be too many buckets.
        assert_eq!(summary(Granularity::Hour).granularity, Granularity::Day);
    }

    #[test]
    fn overviews_shape_a_small_chart() {
        let numbers = owned((1..=100).map(|n| n.to_string()));
        let overview = overview_column("n", ColumnKind::Integer, || {
            numbers.iter().map(|v| Some(v.as_str()))
        });
        assert_eq!(overview.spark.len(), SPARK);
        assert!(
            overview
                .spark
                .iter()
                .all(|share| *share > 0.0 && *share <= 1.0)
        );
        let words = ["a", "b", "a", "c", "a"];
        let overview = overview_column("w", ColumnKind::Text, || words.iter().map(|w| Some(*w)));
        assert_eq!(overview.spark, [1.0, 1.0 / 3.0, 1.0 / 3.0]);
        assert!(!overview.unique);
    }

    #[test]
    fn infers_kinds_from_values() {
        let infer = |values: &[&str]| infer_kind(values.iter().copied());
        assert_eq!(infer(&["1", "-2", "30"]), ColumnKind::Integer);
        assert_eq!(infer(&["1", "2.5"]), ColumnKind::Number);
        assert_eq!(infer(&["t", "f"]), ColumnKind::Boolean);
        assert_eq!(infer(&["2026-10-15", "2025-01-01"]), ColumnKind::Date);
        assert_eq!(
            infer(&["2026-10-15 17:42:00+02", "2026-10-15 09:00:00.5"]),
            ColumnKind::Timestamp
        );
        assert_eq!(infer(&["1", "abc"]), ColumnKind::Text);
        assert_eq!(infer(&[]), ColumnKind::Text);
        let analysis = analyze_values(
            ColumnKind::Unknown,
            Options::default(),
            &[Some("3"), Some("4")],
        );
        assert_eq!(
            (analysis.overview.kind, analysis.overview.inferred),
            (ColumnKind::Integer, true)
        );
    }

    #[test]
    fn parses_dates_and_timestamps() {
        assert_eq!(parse_date("1970-01-01"), Some(0.0));
        assert_eq!(parse_date("2000-03-01"), Some(951_868_800.0));
        assert_eq!(parse_date("2026-13-01"), None);
        assert_eq!(parse_timestamp("1970-01-01 01:00:00"), Some(3_600.0));
        assert_eq!(parse_timestamp("1970-01-01 01:00:00+01"), Some(0.0));
        assert_eq!(
            parse_timestamp("1970-01-01T00:00:01.5-00:30"),
            Some(1_801.5)
        );
        assert_eq!(parse_timestamp("not a timestamp at all"), None);
        for days in [-800_000, -1, 0, 59, 10_957, 20_741, 800_000] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days);
        }
        assert_eq!(civil_from_days(20_741), (2026, 10, 15));
    }

    #[test]
    fn text_that_is_not_ascii_is_no_timestamp() {
        for value in [
            "Crème brûlée recipe",
            "東京都の天気予報です",
            "2026-10-1é 10:00:00",
            "2026-10-15 1é:00:00",
            "2026-10-15 10:00:é0+01",
            "2026-10-15 10:00:00+é1",
        ] {
            assert_eq!(parse_timestamp(value), None, "{value}");
            assert_eq!(wall_clock(value), None, "{value}");
        }
        assert_eq!(
            infer_kind(["東京都の天気予報です"].into_iter()),
            ColumnKind::Text
        );
    }

    #[test]
    fn all_null_columns_have_no_summary() {
        let analysis = analyze_values(ColumnKind::Integer, Options::default(), &[None, None]);
        assert_eq!(
            (analysis.overview.nulls, analysis.summary),
            (2, Summary::Empty)
        );
    }
}
