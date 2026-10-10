//! Runtime bounds owned by the server. Every crossing names its behavior here.

/// Checkpoint-board slots one cell's sweep visits per MAINTAIN turn
/// (ADR-0159 D4, A1.4). Owner: the cell's `BoardSweep`, charged as one
/// Maintenance unit per turn as MAINTAIN's first budgeted step. Crossing:
/// pacing — the sweep resumes at its cursor next turn, so a board of `N`
/// cells completes in `ceil(N / 64)` turns (256 at 16,384 cells) and a
/// publication is observed within two completed sweeps.
pub const CKPT_BOARD_VISITS_PER_TURN: usize = 64;

/// The recovery step budget's price of one byte a tiered boot replay
/// machine wrote to a tier file (ADR-0174 R10; D6's gauge reads the
/// largest step's charge): one budget byte, the device bytes the step
/// moved. Owner: the recovery driver (`RecoverConfig::step_bytes` is the
/// budget it charges). Crossing: a step whose bytes read plus its charge
/// reach the budget yields at the next frame or checkpoint section (the
/// non-yielding unit, ADR-0174 OD-1), or after the settle step that
/// reached it. The bytes the end settle walks and its reads are priced by
/// the store, whose walk yields on them
/// ([`ReplayWork::settle_charge_bytes`](inf_store::ReplayWork::settle_charge_bytes)).
pub const REPLAY_TIER_BYTE_CHARGE: u64 = 1;

/// The recovery step budget's price (ADR-0174 R10) of one boot barrier
/// (a demote step's flush, a gap or capacity seal, the hand-over's
/// drain): 4 MiB, about 4 ms of the flush barrier a device that moves
/// 1 GiB/s pays. Owner and crossing: as [`REPLAY_TIER_BYTE_CHARGE`].
pub const REPLAY_BARRIER_CHARGE_BYTES: u64 = 4 << 20;

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

/// Declares [`FixedShape`], [`FixedShape::ALL`] and
/// [`FixedShape::reply_bytes_max`] from one list of rows, so a new shape is
/// a row of `ALL` by construction and the column assertion below sees it.
macro_rules! fixed_shapes {
    ($($(#[$row:meta])* $shape:ident => $reply_bytes_max:expr,)+) => {
        /// The shapes of a `JSON.*` reply known only after its effect. The
        /// command reserves the shape's maximum before the effect and then
        /// makes its one write (ADR-0099 A1).
        #[cfg(feature = "doc")]
        #[derive(Copy, Clone, PartialEq, Eq, Debug)]
        pub enum FixedShape {
            $($(#[$row])* $shape,)+
        }

        #[cfg(feature = "doc")]
        impl FixedShape {
            /// Every row, for the column's assertion.
            pub const ALL: &'static [FixedShape] = &[$(FixedShape::$shape,)+];

            /// The row's maximum reply bytes, M(shape).
            pub const fn reply_bytes_max(self) -> usize {
                match self {
                    $(FixedShape::$shape => $reply_bytes_max,)+
                }
            }
        }
    };
}

fixed_shapes! {
    /// Root `SET`, `MERGE`'s root create.
    Status => JSON_STATUS_REPLY_BYTES_MAX,
    /// Root `DEL`/`FORGET`.
    Count => JSON_COUNT_REPLY_BYTES_MAX,
    /// In-place `TOGGLE`.
    Toggle => JSON_TOGGLE_REPLY_BYTES_MAX,
    /// In-place `NUMINCRBY`/`NUMMULTBY`.
    Number => JSON_FIXED_REPLY_BYTES_MAX,
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
