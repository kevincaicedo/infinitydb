//! Driver-op file positions (ADR-0167 D1).
//!
//! A positional driver op (a cold-tier read, a log or tier write) hands the
//! kernel a `loff_t`. io_uring reads position `u64::MAX` as `−1`, "use and
//! advance the file's current position"; any other value above `i64::MAX` is
//! negative (`EINVAL`), and a resubmission near the top of `u64` wraps.
//! [`FileOffset`] is the only position a driver op carries, and its
//! constructors keep the op's whole span inside `0..=i64::MAX`.

use core::fmt;

use crate::limits::FILE_OFFSET_BYTES_MAX;

/// A byte position in a file that a driver op of up to
/// [`DRIVER_OP_BYTES_MAX`](crate::limits::DRIVER_OP_BYTES_MAX) bytes may
/// start at: `0..=FILE_OFFSET_BYTES_MAX`, so the op's span end is at most
/// `i64::MAX`. The field is private: [`new`](Self::new) (the range check) and
/// the total [`from_u32_bytes`](Self::from_u32_bytes) are the only
/// constructors, and there is no `From`/`TryFrom`.
///
/// The field cannot be set from outside this module:
///
/// ```compile_fail,E0423
/// use inf_foundation::FileOffset;
/// let _ = FileOffset(u64::MAX);
/// ```
///
/// Its twin, the same path through a constructor, compiles:
///
/// ```
/// use inf_foundation::FileOffset;
/// let _ = FileOffset::from_u32_bytes(1);
/// ```
///
/// A `u64` does not convert without the range check:
///
/// ```compile_fail,E0277
/// use inf_foundation::FileOffset;
/// let _: FileOffset = u64::MAX.into();
/// ```
///
/// Its twin compiles:
///
/// ```
/// use inf_foundation::FileOffset;
/// let _: FileOffset = FileOffset::from_u32_bytes(7);
/// ```
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct FileOffset(u64);

/// A position [`FileOffset::new`] refused: above
/// [`FILE_OFFSET_BYTES_MAX`](crate::limits::FILE_OFFSET_BYTES_MAX). Built
/// only by that refusal, so it always names a value the kernel cannot
/// address.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct FileOffsetRefused {
    offset_bytes: u64,
}

impl FileOffset {
    /// The range check.
    ///
    /// # Errors
    /// [`FileOffsetRefused`] carrying `offset_bytes` when it exceeds
    /// [`FILE_OFFSET_BYTES_MAX`](crate::limits::FILE_OFFSET_BYTES_MAX).
    #[inline]
    pub fn new(offset_bytes: u64) -> Result<FileOffset, FileOffsetRefused> {
        if offset_bytes <= FILE_OFFSET_BYTES_MAX {
            Ok(FileOffset(offset_bytes))
        } else {
            Err(FileOffsetRefused { offset_bytes })
        }
    }

    /// A position below 4 GiB (a log segment cursor, a checkpoint header's
    /// 0). Total: every `u32` is at most `FILE_OFFSET_BYTES_MAX`
    /// (const-asserted in `limits`).
    #[must_use]
    #[inline]
    pub fn from_u32_bytes(offset_bytes: u32) -> FileOffset {
        FileOffset(u64::from(offset_bytes))
    }

    /// The position in bytes.
    #[must_use]
    #[inline]
    pub fn bytes(self) -> u64 {
        self.0
    }

    /// The position `done_bytes` past this one: an SQE's position after a
    /// short transfer, or a merged read's span end. At most `i64::MAX`, and
    /// the add cannot wrap: `self ≤ FILE_OFFSET_BYTES_MAX` and `done_bytes ≤
    /// DRIVER_OP_BYTES_MAX`, whose sum is `i64::MAX` (const-asserted).
    #[must_use]
    #[inline]
    pub fn bytes_after(self, done_bytes: u32) -> u64 {
        self.0 + u64::from(done_bytes)
    }

    /// [`bytes_after`](Self::bytes_after) as the `off_t` that `pread` and
    /// `pwrite` take. The value is in `0..=i64::MAX`, so the reinterpretation
    /// is exact.
    #[must_use]
    #[inline]
    pub fn position_after(self, done_bytes: u32) -> i64 {
        self.bytes_after(done_bytes).cast_signed()
    }
}

impl FileOffsetRefused {
    /// The refused position, in bytes.
    #[must_use]
    #[inline]
    pub fn offset_bytes(self) -> u64 {
        self.offset_bytes
    }
}

impl fmt::Display for FileOffsetRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "file position {} is above {FILE_OFFSET_BYTES_MAX}, the largest a driver op may \
             carry within the kernel's loff_t range",
            self.offset_bytes
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::DRIVER_OP_BYTES_MAX;

    /// ADR-0167's hostile inputs for `new`: the bound, its neighbours and
    /// the integer maxima, each accepted or refused carrying its value.
    #[test]
    fn file_offset_boundaries() {
        let bound = FILE_OFFSET_BYTES_MAX;
        assert_eq!(bound, 0x7FFF_FFFF_0000_0000, "i64::MAX − u32::MAX");
        assert_eq!(DRIVER_OP_BYTES_MAX, u64::from(u32::MAX));
        for accepted in [0, 1, u64::from(u32::MAX), bound - 1, bound] {
            let position = FileOffset::new(accepted).expect("addressable");
            assert_eq!(position.bytes(), accepted);
        }
        let top = FileOffset::new(bound).expect("the bound is addressable");
        assert_eq!(top.bytes_after(u32::MAX), i64::MAX.cast_unsigned(), "span end at the bound");
        assert_eq!(top.position_after(u32::MAX), i64::MAX, "never negative, never −1");
        let refused_inputs = [
            bound + 1,
            i64::MAX.cast_unsigned(),
            i64::MAX.cast_unsigned() + 1,
            u64::MAX - 1,
            u64::MAX,
        ];
        for refused in refused_inputs {
            let err = FileOffset::new(refused).expect_err("above the bound");
            assert_eq!(err.offset_bytes(), refused, "the refusal carries the value");
        }
        let cursor = FileOffset::from_u32_bytes(u32::MAX);
        assert_eq!(cursor.bytes(), u64::from(u32::MAX));
        assert_eq!(cursor.position_after(0), i64::from(u32::MAX));
    }
}
