use chrono::{DateTime, Utc};
fn aligned_half_hour(now: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp(now.timestamp().div_euclid(1800) * 1800, 0).expect("half-hour")
}
pub const HANDOVER_MARGIN: chrono::Duration = chrono::Duration::minutes(5);

/// Last command before Axle: the half-hour boundary strictly before its start.
pub fn handover_at(start: DateTime<Utc>) -> DateTime<Utc> {
    aligned_half_hour(start - chrono::Duration::nanoseconds(1))
}

/// Keep the end buffer, then resume on a tariff boundary (rounding up).
pub fn resume_at(end: DateTime<Utc>) -> DateTime<Utc> {
    let buffered = end + HANDOVER_MARGIN;
    let boundary = aligned_half_hour(buffered);
    if boundary == buffered {
        boundary
    } else {
        boundary + chrono::Duration::minutes(30)
    }
}
