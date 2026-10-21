//! Values as text, byte for byte what the dialect's `as_text` gives
//! (`convert(nvarchar(max), x, 126)`), so what is shown can be compared
//! back; and the kinds of columns.
//!
//! The one exception is binary, which `as_text` cannot write: it shows as
//! `0x` and uppercase hex (what style 1 gives). `money` arrives as a float,
//! so values beyond about 10^14 lose their last digits.

use kuery_core::result_set::ColumnKind;
use tiberius::time::{DateTime2, Time};
use tiberius::{ColumnData, ColumnType};

/// 1900-01-01, where `datetime` and `smalldatetime` count from, in days
/// since 0001-01-01.
const DAY_1900: i64 = 693_595;

/// The text of a value, `None` for NULL. `money` and `smallmoney` arrive as
/// floats too; `column` tells them apart.
pub(crate) fn text(value: &ColumnData<'_>, column: ColumnType) -> Option<String> {
    Some(match value {
        ColumnData::U8(v) => v.as_ref()?.to_string(),
        ColumnData::I16(v) => v.as_ref()?.to_string(),
        ColumnData::I32(v) => v.as_ref()?.to_string(),
        ColumnData::I64(v) => v.as_ref()?.to_string(),
        ColumnData::Bit(v) => (if *v.as_ref()? { "1" } else { "0" }).into(),
        ColumnData::Numeric(v) => {
            let n = v.as_ref()?;
            // `Display` writes `5.0` for scale 0; the server writes `5`.
            if n.scale() == 0 {
                n.value().to_string()
            } else {
                n.to_string()
            }
        }
        ColumnData::F64(v) => {
            let v = *v.as_ref()?;
            match column {
                ColumnType::Money | ColumnType::Money4 => format!("{v:.4}"),
                _ => float(v),
            }
        }
        ColumnData::F32(v) => scientific(f64::from(*v.as_ref()?), 7),
        ColumnData::String(v) => v.as_ref()?.to_string(),
        ColumnData::Guid(v) => v.as_ref()?.to_string().to_uppercase(),
        ColumnData::Binary(v) => {
            let hex: String = v.as_ref()?.iter().map(|b| format!("{b:02X}")).collect();
            format!("0x{hex}")
        }
        ColumnData::Xml(v) => v.as_ref()?.as_ref().to_string(),
        ColumnData::Date(v) => date(v.as_ref()?.days() as i64),
        ColumnData::Time(v) => time(*v.as_ref()?),
        ColumnData::DateTime2(v) => datetime2(*v.as_ref()?, None),
        ColumnData::DateTimeOffset(v) => {
            let v = v.as_ref()?;
            datetime2(v.datetime2(), Some(v.offset()))
        }
        ColumnData::DateTime(v) => {
            let v = v.as_ref()?;
            // Fragments of 1/300 s, written as rounded milliseconds.
            let fragments = v.seconds_fragments() as u64;
            let (mut seconds, mut millis) = (fragments / 300, ((fragments % 300) * 10 + 1) / 3);
            if millis >= 1000 {
                seconds += 1;
                millis -= 1000;
            }
            format!(
                "{}T{}",
                date(DAY_1900 + v.days() as i64),
                clock(seconds, millis, 3)
            )
        }
        ColumnData::SmallDateTime(v) => {
            let v = v.as_ref()?;
            let minutes = v.seconds_fragments() as u64;
            format!(
                "{}T{}",
                date(DAY_1900 + v.days() as i64),
                clock(minutes * 60, 0, 0)
            )
        }
    })
}

/// What a column holds, for sorting and charts.
pub(crate) fn kind(column: ColumnType) -> ColumnKind {
    use ColumnType::*;
    match column {
        Int1 | Int2 | Int4 | Int8 | Intn | Bit | Bitn => ColumnKind::Integer,
        Float4 | Float8 | Floatn | Money | Money4 | Decimaln | Numericn => ColumnKind::Number,
        Daten => ColumnKind::Date,
        Datetime | Datetime4 | Datetimen | Datetime2 | DatetimeOffsetn => ColumnKind::Timestamp,
        Null => ColumnKind::Unknown,
        _ => ColumnKind::Text,
    }
}

/// A `real` the way style 126 writes it: `digits` decimals, and the
/// exponent as a sign and three digits (`1.0000000e-001`).
fn scientific(value: f64, digits: usize) -> String {
    let text = format!("{value:.digits$e}");
    let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:03}", exponent.abs())
}

/// A `float` the way style 126 writes it: correctly rounded to 17
/// significant digits first, then half up to 16 (`-3.3333333333333335`
/// gives `-3.333333333333334e+000`, not `…333e+000`).
fn float(value: f64) -> String {
    let text = format!("{value:.16e}");
    let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
    let mut exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: Vec<u8> = mantissa.bytes().filter(u8::is_ascii_digit).collect();
    let mut kept: Vec<u8> = digits[..16].iter().map(|d| d - b'0').collect();
    if digits[16] >= b'5' {
        let mut index = kept.len();
        loop {
            if index == 0 {
                // 9.999… rounded up to 10.
                kept.insert(0, 1);
                kept.pop();
                exponent += 1;
                break;
            }
            index -= 1;
            if kept[index] == 9 {
                kept[index] = 0;
            } else {
                kept[index] += 1;
                break;
            }
        }
    }
    let fraction: String = kept[1..].iter().map(|d| char::from(b'0' + d)).collect();
    let sign = if exponent < 0 { '-' } else { '+' };
    format!(
        "{}{}.{fraction}e{sign}{:03}",
        if negative { "-" } else { "" },
        kept[0],
        exponent.abs()
    )
}

/// `YYYY-MM-DD` of a day counted from 0001-01-01 (proleptic Gregorian).
fn date(days_since_year_1: i64) -> String {
    // Days since 1970-01-01, then the civil date (Howard Hinnant's method).
    let z = days_since_year_1 - 719_162 + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// `HH:MM:SS`, plus `.` and `scale` digits of `fraction` unless it is zero.
fn clock(seconds: u64, fraction: u64, scale: u8) -> String {
    let mut text = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    if scale > 0 && fraction != 0 {
        text.push_str(&format!(".{fraction:0width$}", width = scale as usize));
    }
    text
}

fn time(value: Time) -> String {
    let unit = 10u64.pow(value.scale() as u32);
    let increments = value.increments();
    clock(increments / unit, increments % unit, value.scale())
}

/// A `datetime2`, or with `offset` minutes a `datetimeoffset`, which arrives
/// in UTC and is written in its own local time.
fn datetime2(value: DateTime2, offset: Option<i16>) -> String {
    let scale = value.time().scale();
    let unit = 10i64.pow(scale as u32);
    let mut days = value.date().days() as i64;
    let mut increments = value.time().increments() as i64;
    let mut suffix = String::new();
    if let Some(offset) = offset {
        increments += i64::from(offset) * 60 * unit;
        let day = 86_400 * unit;
        days += increments.div_euclid(day);
        increments = increments.rem_euclid(day);
        let sign = if offset < 0 { '-' } else { '+' };
        let minutes = offset.unsigned_abs();
        suffix = format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60);
    }
    format!(
        "{}T{}{suffix}",
        date(days),
        clock(
            (increments / unit) as u64,
            (increments % unit) as u64,
            scale
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_dates_from_day_numbers() {
        assert_eq!(date(0), "0001-01-01");
        assert_eq!(date(DAY_1900), "1900-01-01");
        assert_eq!(date(719_162), "1970-01-01");
        assert_eq!(date(3_652_058), "9999-12-31");
    }

    #[test]
    fn writes_floats_like_style_126() {
        assert_eq!(float(0.1), "1.000000000000000e-001");
        assert_eq!(float(1.5e20), "1.500000000000000e+020");
        assert_eq!(float(0.0), "0.000000000000000e+000");
        assert_eq!(float(-3.3333333333333335), "-3.333333333333334e+000");
        assert_eq!(float(9.9999999999999995), "1.000000000000000e+001");
        assert_eq!(float(1e-300), "1.000000000000000e-300");
        assert_eq!(scientific(0.1, 7), "1.0000000e-001");
        assert_eq!(scientific(0.0, 7), "0.0000000e+000");
    }

    #[test]
    fn writes_fractions_only_when_not_zero() {
        assert_eq!(clock(49_509, 0, 7), "13:45:09");
        assert_eq!(clock(49_509, 120, 3), "13:45:09.120");
        assert_eq!(clock(49_509, 1_234_567, 7), "13:45:09.1234567");
    }

    #[test]
    fn tells_money_from_float_by_the_column() {
        let value = ColumnData::F64(Some(1234.5));
        assert_eq!(
            text(&value, ColumnType::Money).as_deref(),
            Some("1234.5000")
        );
        assert_eq!(
            text(&value, ColumnType::Float8).as_deref(),
            Some("1.234500000000000e+003")
        );
        assert_eq!(text(&ColumnData::I32(None), ColumnType::Intn), None);
    }
}
