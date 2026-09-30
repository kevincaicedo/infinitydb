//! Runtime bounds owned by the server. Every crossing names its behavior here.

/// Bytes read by the process supervisor for one CPU sample. An oversized
/// procfs record is refused and the read board retains its preceding sample.
pub const PROCESS_STAT_BYTES_MAX: u64 = 16 * 1024;

/// Bytes of the longest reply root `JSON.SET` and `JSON.MERGE`'s root
/// create write after their store call: `+OK\r\n`, or the NX/XX null
/// (`$-1\r\n`; RESP3 `_\r\n`). A command whose reply budget has fewer
/// bytes left answers `ERR reply too large` before the store call
/// (ADR-0099 A1).
#[cfg(feature = "doc")]
pub const JSON_STATUS_REPLY_BYTES_MAX: usize = 5;

/// Bytes of the reply a root `JSON.DEL`/`JSON.FORGET` writes after its
/// delete: `:0\r\n` or `:1\r\n`. Below it the command refuses before the
/// delete (ADR-0099 A1).
#[cfg(feature = "doc")]
pub const JSON_COUNT_REPLY_BYTES_MAX: usize = 4;

/// Bytes of the longest reply the in-place `JSON.TOGGLE` writes after its
/// patch: legacy `$5\r\nfalse\r\n`; `$` mode `*1\r\n$-1\r\n`, `*1\r\n:1\r\n`
/// or `*0\r\n`. Below it the command refuses before the probe, and so does
/// its general path, which the probe decides (ADR-0099 A1).
#[cfg(feature = "doc")]
pub const JSON_TOGGLE_REPLY_BYTES_MAX: usize = 11;

/// The fixed-shape column's maximum, and the `Number` row: the in-place
/// `JSON.NUMINCRBY`/`JSON.NUMMULTBY` reply, a RESP3 `*1\r\n` and one
/// double (the RESP2 bulk is at most 41 B). Two crossings (ADR-0099 A1):
/// a fixed-shape command whose reply budget has fewer bytes left than its
/// shape's maximum answers `ERR reply too large` before its effect; and a
/// `doc-max-reply-bytes` setter, when one exists, refuses a value below
/// this constant, because below it a numeric patch can never be served.
#[cfg(feature = "doc")]
pub const JSON_FIXED_REPLY_BYTES_MAX: usize = 4 + inf_wire::limits::DOUBLE_REPLY_BYTES_MAX;

/// The shapes of a `JSON.*` reply known only after its effect. The command
/// reserves the shape's maximum before the effect and then makes its one
/// write (ADR-0099 A1).
#[cfg(feature = "doc")]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FixedShape {
    /// Root `SET`, `MERGE`'s root create.
    Status,
    /// Root `DEL`/`FORGET`.
    Count,
    /// In-place `TOGGLE`.
    Toggle,
    /// In-place `NUMINCRBY`/`NUMMULTBY`.
    Number,
}

#[cfg(feature = "doc")]
impl FixedShape {
    /// Every row, for the table's assertions and its oracle.
    pub const ALL: [FixedShape; 4] =
        [FixedShape::Status, FixedShape::Count, FixedShape::Toggle, FixedShape::Number];

    /// The row's maximum reply bytes, M(shape).
    pub const fn reply_bytes_max(self) -> usize {
        match self {
            FixedShape::Status => JSON_STATUS_REPLY_BYTES_MAX,
            FixedShape::Count => JSON_COUNT_REPLY_BYTES_MAX,
            FixedShape::Toggle => JSON_TOGGLE_REPLY_BYTES_MAX,
            FixedShape::Number => JSON_FIXED_REPLY_BYTES_MAX,
        }
    }
}

#[cfg(feature = "doc")]
const _: () = {
    let mut row = 0;
    while row < FixedShape::ALL.len() {
        let fits = FixedShape::ALL[row].reply_bytes_max() <= JSON_FIXED_REPLY_BYTES_MAX;
        assert!(fits, "every fixed shape fits the column's maximum");
        row += 1;
    }
    assert!(JSON_FIXED_REPLY_BYTES_MAX == 348, "the fixed column's maximum is 348 bytes");
};
