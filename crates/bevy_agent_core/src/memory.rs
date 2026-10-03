//! Conservative accounting for retained JSON allocations.
use serde_json::Value;

/// Charges array capacity, strings, and map nodes rather than wire bytes alone.
/// Allocator bookkeeping is conservatively included for each map entry.
pub fn json_heap_bytes(value: &Value) -> Option<usize> {
    match value {
        Value::String(s) => Some(s.capacity()),
        Value::Array(values) => values.iter().try_fold(
            values
                .capacity()
                .checked_mul(std::mem::size_of::<Value>())?,
            |bytes, value| bytes.checked_add(json_heap_bytes(value)?),
        ),
        Value::Object(values) => values.iter().try_fold(0usize, |bytes, (key, value)| {
            bytes
                .checked_add(256)?
                .checked_add(key.capacity())?
                .checked_add(json_heap_bytes(value)?)
        }),
        _ => Some(0),
    }
}
