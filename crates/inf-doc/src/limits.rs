//! Document format ceilings (ADR-0036 D2/D6) and cell-owned cache limits
//! (ADR-0146). Namespace configuration may lower the format ceilings;
//! cache capacity has its own checked construction boundary.

/// Maximum nesting depth, in containers: the reader bound of every stored
/// document and every delta operand, and the bound a path mutation's
/// composed output meets (ADR-0169 D2/D3). Crossing ⇒ `DepthExceeded`,
/// `ERR document nesting too deep`, nothing changed. Not RedisJSON's
/// bound: it refuses a parsed value's 128th container and never bounds a
/// composed document — both differences are recorded deviations (ADR-0042
/// A1).
pub const DEPTH_MAX: usize = 128;

/// Document byte cap: 16 MiB − 1. This is a *ceiling built into field
/// widths*, not a checked constant: the store record `vlen` is u24 (§7.2)
/// and container skip-lengths are u24 (ADR-0036 D3), so a larger document
/// is unrepresentable. M4 blob extents lift where bytes live, not this.
pub const DOC_BYTES_MAX: usize = 0xFF_FFFF;

// The cap must fit the u24 skip-length fields — if this ever fails to
// compile, the format changed without its ADR.
const _: () = assert!(DOC_BYTES_MAX <= 0xFF_FFFF);

/// The document bounds one writer applies: nesting depth in containers and
/// body bytes (the header excluded). The fields are private and `new`
/// clamps each to [`DocLimits::FORMAT`], so configuration lowers a bound
/// and never raises it (ADR-0039 D5's clamp law; ADR-0169 D2).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DocLimits {
    depth_max: usize,
    body_bytes_max: usize,
}

impl DocLimits {
    /// The format ceilings: [`DEPTH_MAX`] and [`DOC_BYTES_MAX`].
    pub const FORMAT: DocLimits = DocLimits { depth_max: DEPTH_MAX, body_bytes_max: DOC_BYTES_MAX };

    pub const fn new(depth_max: usize, body_bytes_max: usize) -> DocLimits {
        DocLimits {
            depth_max: if depth_max < DEPTH_MAX { depth_max } else { DEPTH_MAX },
            body_bytes_max: if body_bytes_max < DOC_BYTES_MAX {
                body_bytes_max
            } else {
                DOC_BYTES_MAX
            },
        }
    }

    /// Containers a document may nest.
    pub const fn depth_max(self) -> usize {
        self.depth_max
    }

    /// Body bytes a document may hold, its header excluded.
    pub const fn body_bytes_max(self) -> usize {
        self.body_bytes_max
    }
}

/// Path cache entries per cell. Larger requests are refused before
/// allocation; zero disables caching (ADR-0146 D1).
pub const PROGRAM_CACHE_ENTRIES_MAX: u16 = 4096;
const DEFAULT_ENTRIES: u16 = 1024;
/// Default retained entries per cell; construction remains fallible.
pub const PROGRAM_CACHE_DEFAULT_ENTRIES: usize = DEFAULT_ENTRIES as usize;
const _: () = assert!(DEFAULT_ENTRIES <= PROGRAM_CACHE_ENTRIES_MAX);
/// Retained payload bytes admitted per configured cache entry.
pub const PROGRAM_CACHE_ENTRY_SHARE_BYTES: usize = 4096;

/// A cache entry count and its checked bucket/payload geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgramCacheCapacity {
    entries: u16,
    buckets: usize,
    budget_bytes: usize,
}

/// The requested cache exceeds its cell-owned resource ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgramCacheCapacityError {
    pub requested: usize,
}

impl core::fmt::Display for ProgramCacheCapacityError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "path cache capacity {} exceeds {} entries per cell",
            self.requested, PROGRAM_CACHE_ENTRIES_MAX
        )
    }
}

impl core::error::Error for ProgramCacheCapacityError {}

impl TryFrom<usize> for ProgramCacheCapacity {
    type Error = ProgramCacheCapacityError;

    fn try_from(requested: usize) -> Result<Self, Self::Error> {
        let refused = ProgramCacheCapacityError { requested };
        if requested > usize::from(PROGRAM_CACHE_ENTRIES_MAX) {
            return Err(refused);
        }
        let entries = u16::try_from(requested).map_err(|_| refused)?;
        let buckets = if requested == 0 {
            0
        } else {
            requested.checked_mul(2).and_then(usize::checked_next_power_of_two).ok_or(refused)?
        };
        let budget_bytes = requested.checked_mul(PROGRAM_CACHE_ENTRY_SHARE_BYTES).ok_or(refused)?;
        Ok(Self { entries, buckets, budget_bytes })
    }
}

impl ProgramCacheCapacity {
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
        buckets: PROGRAM_CACHE_DEFAULT_ENTRIES
            .checked_mul(2)
            .expect("path cache default bucket multiplication fits usize")
            .checked_next_power_of_two()
            .expect("path cache default bucket rounding fits usize"),
        budget_bytes: PROGRAM_CACHE_DEFAULT_ENTRIES
            .checked_mul(PROGRAM_CACHE_ENTRY_SHARE_BYTES)
            .expect("path cache default payload budget fits usize"),
    };
}

impl Default for ProgramCacheCapacity {
    fn default() -> Self {
        Self::DEFAULT
    }
}
