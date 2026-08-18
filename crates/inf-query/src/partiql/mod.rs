//! The PartiQL subset compiler (M4.5-S09, ADR-0080): statement text →
//! one access program, or a documented rejection. Total by
//! construction — there is no cost model, no plan search, and the
//! output type has exactly one access-step field (ADR-0024 D2).
//!
//! The compat contract — accepted productions, semantics, and every
//! rejection's exact string — is `infinitydb/docs/partiql-subset.md`;
//! the S09 table-driven suite pins it. `QlError`'s `Display` output IS
//! that contract's library layer (the RESP prefix is pinned at
//! S10/S11).

mod cache;
mod compile;
mod lex;
mod parse;

pub use cache::{STATEMENT_CACHE_DEFAULT_ENTRIES, StatementCache};

use inf_store::{IndexSpec, NsId};

use crate::access::{Access, AccessProgram};
use crate::predicate::PredicateVm;

/// Statement-size ceiling: DynamoDB's PartiQL statement cap, deliberate
/// register parity (ADR-0080 D6). Config-lowerable; raising it is a
/// successor ADR.
pub const STATEMENT_BYTES_CEILING: usize = 8192;

/// The compiler's catalog input (ADR-0080 D5): planning reads **catalog**
/// state only (ADR-0075 D3) — per-cell machine states never influence
/// compilation. `inf-server` implements this over the real namespace
/// catalog + `IndexRegistry` (S10/S11); tests implement it over
/// fixtures. Monomorphized — no data-plane dispatch.
pub trait CatalogView {
    /// Namespace name → id, or `None` (unknown names reject).
    fn resolve_ns(&self, name: &[u8]) -> Option<NsId>;
    /// Exact-name lookup on `ns` (the `FROM ns.index` form).
    fn index_by_name(&self, ns: NsId, name: &[u8]) -> Option<&IndexSpec>;
    /// Every declaration on `ns`, catalog order (path matching scans
    /// them; ≤ 64 per node — never a data-plane cost).
    fn indexes(&self, ns: NsId) -> impl Iterator<Item = &IndexSpec>;
    /// Monotone DDL epoch: changes iff a statement could now compile
    /// differently (index create/drop/state/rebuild + namespace DDL).
    /// The statement cache guards entries with it (ADR-0080 D5).
    fn catalog_epoch(&self) -> u64;
}

/// One compiled statement — the cache value (ADR-0080 D5): the frozen
/// serialized form, its decoded fields (executors read these), and the
/// residual's VM with pools pre-decoded (the S08 cold path lives here,
/// not per execution).
#[derive(Clone, Debug)]
pub struct CompiledStatement {
    pub program: AccessProgram,
    pub access: Access,
    pub vm: Option<PredicateVm>,
}

/// Compile a statement under the default size cap.
///
/// # Errors
/// A [`QlError`] whose `Display` string is the subset spec's documented
/// rejection (L8).
pub fn compile<C: CatalogView>(text: &[u8], catalog: &C) -> Result<CompiledStatement, QlError> {
    compile_with_max_bytes(text, catalog, STATEMENT_BYTES_CEILING)
}

/// Compile with a configured cap (clamped to the ceiling — config
/// lowers, never raises; the ParseLimits rule).
///
/// # Errors
/// See [`compile`].
pub fn compile_with_max_bytes<C: CatalogView>(
    text: &[u8],
    catalog: &C,
    max_bytes: usize,
) -> Result<CompiledStatement, QlError> {
    if text.len() > max_bytes.min(STATEMENT_BYTES_CEILING) {
        return Err(QlError { offset: 0, kind: QlErrorKind::StatementTooLong });
    }
    if let Err(e) = core::str::from_utf8(text) {
        return Err(QlError { offset: e.valid_up_to(), kind: QlErrorKind::InvalidUtf8 });
    }
    let statement = parse::parse(text)?;
    compile::compile_statement(statement, catalog)
}

/// A documented statement rejection: byte offset + typed kind. The
/// `Display` strings are the compat contract (subset spec §7) — golden-
/// tested verbatim; changing one is a compat break.
#[derive(Clone, Debug, PartialEq)]
pub struct QlError {
    pub offset: usize,
    pub kind: QlErrorKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum QlErrorKind {
    StatementTooLong,
    InvalidUtf8,
    UnexpectedChar,
    UnterminatedString,
    BadNumber,
    IntegerOutOfRange,
    NonFiniteNumber,
    /// `expected {what}` — the `{what}` spellings are enumerated in the
    /// spec (§7) and are contract text like everything else.
    Expected(&'static str),
    TrailingInput,
    TooDeep,
    DescendUnsupported,
    SliceUnionUnsupported,
    /// A path that lexes but fails the M3 path compiler.
    BadPath,
    InCount,
    LimitRange,
    UnknownFunction,
    BeginsWithArgs,
    ExistsArgs,
    UnknownPseudoPath,
    ColumnProjection,
    OrderByUnsupported,
    GroupByUnsupported,
    HavingUnsupported,
    JoinUnsupported,
    OffsetUnsupported,
    DmlUnsupported,
    IsNullUnsupported,
    LikeUnsupported,
    NullComparison,
    MixedInFamilies,
    MixedBetweenFamilies,
    UnknownNamespace(String),
    UnknownIndex(String),
    IndexNotReady {
        name: String,
        state: &'static str,
    },
    AmbiguousKeyCondition {
        first: String,
        second: String,
    },
    NoAccessPath,
    MultiValueRange(String),
    KeyTypeMismatch {
        name: String,
        key_type: &'static str,
    },
    UnconstrainedIndex(String),
    PkOp,
    PkPosition,
    PkType,
    PkDuplicate,
    PkWithScan,
    PkKeyLength,
    CountWithLimit,
    TooManyOps,
    TooManyPaths,
    TooManyConstants,
    ProgramTooLong,
}

impl core::fmt::Display for QlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        use QlErrorKind as K;
        let o = self.offset;
        match &self.kind {
            K::StatementTooLong => write!(f, "statement exceeds the size limit"),
            K::InvalidUtf8 => write!(f, "statement is not valid UTF-8 at offset {o}"),
            K::UnexpectedChar => write!(f, "unexpected character at offset {o}"),
            K::UnterminatedString => write!(f, "unterminated string literal at offset {o}"),
            K::BadNumber => write!(f, "invalid number literal at offset {o}"),
            K::IntegerOutOfRange => write!(f, "integer literal out of i64 range at offset {o}"),
            K::NonFiniteNumber => write!(f, "number literal overflows f64 at offset {o}"),
            K::Expected(what) => write!(f, "expected {what} at offset {o}"),
            K::TrailingInput => write!(f, "unexpected input after statement end at offset {o}"),
            K::TooDeep => write!(f, "WHERE nesting exceeds depth 32 at offset {o}"),
            K::DescendUnsupported => write!(
                f,
                "recursive descent ('..') is not allowed in statement paths at offset {o}"
            ),
            K::SliceUnionUnsupported => {
                write!(f, "slices and unions are not allowed in statement paths at offset {o}")
            }
            K::BadPath => write!(f, "invalid document path at offset {o}"),
            K::InCount => write!(f, "IN list must hold 1 to 100 members at offset {o}"),
            K::LimitRange => write!(f, "LIMIT must be between 1 and 4294967295 at offset {o}"),
            K::UnknownFunction => {
                write!(f, "unknown function at offset {o} (supported: begins_with, exists)")
            }
            K::BeginsWithArgs => write!(f, "begins_with takes (path, 'prefix') at offset {o}"),
            K::ExistsArgs => write!(f, "exists takes (path) at offset {o}"),
            K::UnknownPseudoPath => {
                write!(f, "unknown pseudo-path at offset {o} ($key is the only pseudo-path)")
            }
            K::ColumnProjection => {
                write!(f, "unsupported projection: only * and COUNT(*) are in the subset")
            }
            K::OrderByUnsupported => {
                write!(f, "unsupported: ORDER BY (results follow index-key order)")
            }
            K::GroupByUnsupported => write!(f, "unsupported: GROUP BY"),
            K::HavingUnsupported => write!(f, "unsupported: HAVING"),
            K::JoinUnsupported => write!(f, "unsupported: JOIN (statements read one namespace)"),
            K::OffsetUnsupported => write!(f, "unsupported: OFFSET (pages resume from cursors)"),
            K::DmlUnsupported => write!(
                f,
                "unsupported: INSERT/UPDATE/DELETE (mutations use the native command set)"
            ),
            K::IsNullUnsupported => write!(
                f,
                "unsupported: IS NULL / IS MISSING (null is never indexed; exists(path) tests \
                 presence)"
            ),
            K::LikeUnsupported => write!(
                f,
                "unsupported: LIKE (begins_with(path, 'prefix') is the indexed prefix test)"
            ),
            K::NullComparison => write!(
                f,
                "unsupported: comparison to NULL (null is never indexed; exists(path) tests \
                 presence)"
            ),
            K::MixedInFamilies => write!(f, "IN members must share one type family"),
            K::MixedBetweenFamilies => write!(f, "BETWEEN bounds must share one type family"),
            K::UnknownNamespace(ns) => write!(f, "unknown namespace '{ns}'"),
            K::UnknownIndex(name) => write!(f, "unknown index '{name}'"),
            K::IndexNotReady { name, state } => {
                write!(f, "index '{name}' is {state}; only ready indexes serve queries")
            }
            K::AmbiguousKeyCondition { first, second } => write!(
                f,
                "the WHERE clause matches more than one ready index ('{first}', '{second}'); \
                 name one: FROM ns.\"index\""
            ),
            K::NoAccessPath => write!(
                f,
                "no key condition names the primary key or a ready index; declare one (INF.IDX \
                 CREATE) or scan with explicit consent (FROM ns.SCAN)"
            ),
            K::MultiValueRange(name) => write!(
                f,
                "index '{name}' is over a multi-valued path; only equality is servable (ranges \
                 would page duplicates)"
            ),
            K::KeyTypeMismatch { name, key_type } => write!(
                f,
                "key condition value does not match index '{name}' declared type {key_type}"
            ),
            K::UnconstrainedIndex(name) => write!(
                f,
                "no key condition constrains index '{name}'; walking a whole index needs FROM \
                 ns.SCAN consent"
            ),
            K::PkOp => write!(
                f,
                "the primary key supports equality only ($key = 'k'); ranges need a declared \
                 index"
            ),
            K::PkPosition => {
                write!(f, "$key is only valid as a top-level AND conjunct '$key = <string>'")
            }
            K::PkType => write!(f, "$key compares to a string literal"),
            K::PkDuplicate => write!(f, "more than one $key condition"),
            K::PkWithScan => {
                write!(f, "$key does not combine with FROM ns.SCAN (point lookups use FROM ns)")
            }
            K::PkKeyLength => write!(f, "primary key literals must be 1 to 255 bytes"),
            K::CountWithLimit => write!(
                f,
                "LIMIT does not apply to COUNT(*) (each page is bounded by the page budget and \
                 returns a partial count)"
            ),
            K::TooManyOps => write!(f, "the WHERE clause exceeds 256 operations"),
            K::TooManyPaths => write!(f, "the WHERE clause exceeds 64 distinct paths"),
            K::TooManyConstants => write!(f, "the WHERE clause exceeds 512 constants"),
            K::ProgramTooLong => {
                write!(f, "the compiled statement exceeds the program size ceiling")
            }
        }
    }
}

impl core::error::Error for QlError {}
