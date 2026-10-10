//! Cell-owned query resource limits and their refusal boundaries (ADR-0146).

/// Statement cache entries per cell. Larger requests are refused before
/// allocation; zero disables caching (ADR-0146 D1).
pub const STATEMENT_CACHE_ENTRIES_MAX: u16 = 4096;
const DEFAULT_ENTRIES: u16 = 1024;
/// Default retained entries per cell; construction remains fallible.
pub const STATEMENT_CACHE_DEFAULT_ENTRIES: usize = DEFAULT_ENTRIES as usize;
const _: () = assert!(DEFAULT_ENTRIES <= STATEMENT_CACHE_ENTRIES_MAX);
/// Retained payload bytes admitted per configured cache entry.
pub const STATEMENT_CACHE_ENTRY_SHARE_BYTES: usize = crate::partiql::STATEMENT_BYTES_CEILING;

/// A cache entry count and its checked bucket/payload geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatementCacheCapacity {
    entries: u16,
    buckets: usize,
    budget_bytes: usize,
}

/// The requested cache exceeds its cell-owned resource ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatementCacheCapacityError {
    pub requested: usize,
}

impl core::fmt::Display for StatementCacheCapacityError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "statement cache capacity {} exceeds {} entries per cell",
            self.requested, STATEMENT_CACHE_ENTRIES_MAX
        )
    }
}

impl core::error::Error for StatementCacheCapacityError {}

impl TryFrom<usize> for StatementCacheCapacity {
    type Error = StatementCacheCapacityError;

    fn try_from(requested: usize) -> Result<Self, Self::Error> {
        let refused = StatementCacheCapacityError { requested };
        if requested > usize::from(STATEMENT_CACHE_ENTRIES_MAX) {
            return Err(refused);
        }
        let entries = u16::try_from(requested).map_err(|_| refused)?;
        let buckets = if requested == 0 {
            0
        } else {
            requested.checked_mul(2).and_then(usize::checked_next_power_of_two).ok_or(refused)?
        };
        let budget_bytes =
            requested.checked_mul(STATEMENT_CACHE_ENTRY_SHARE_BYTES).ok_or(refused)?;
        Ok(Self { entries, buckets, budget_bytes })
    }
}

impl StatementCacheCapacity {
    pub fn entries(self) -> usize {
        usize::from(self.entries)
    }

    pub(crate) fn slots(self) -> u32 {
        u32::from(self.entries)
    }

    pub(crate) fn buckets(self) -> usize {
        self.buckets
    }

    pub(crate) fn budget_bytes(self) -> usize {
        self.budget_bytes
    }

    const DEFAULT: Self = Self {
        entries: DEFAULT_ENTRIES,
        buckets: STATEMENT_CACHE_DEFAULT_ENTRIES
            .checked_mul(2)
            .expect("statement cache default bucket multiplication fits usize")
            .checked_next_power_of_two()
            .expect("statement cache default bucket rounding fits usize"),
        budget_bytes: STATEMENT_CACHE_DEFAULT_ENTRIES
            .checked_mul(STATEMENT_CACHE_ENTRY_SHARE_BYTES)
            .expect("statement cache default payload budget fits usize"),
    };
}

impl Default for StatementCacheCapacity {
    fn default() -> Self {
        Self::DEFAULT
    }
}
