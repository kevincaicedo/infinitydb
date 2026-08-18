//! JSONPath subset → path programs → deterministic evaluation
//! (M3-S08/S09; ADR-0040; grammar: `infinitydb/docs/jsonpath-subset.md`).
//!
//! `compile` parses text and encodes bytecode v1; `PathProgram` carries
//! the exact bytes `DocDelta` records store; `eval` walks a document's
//! cursors and yields matches identified by **location paths** (entry-
//! ordinal steps) — the form-agnostic identity that gives document-order
//! sorting, dedup, and §3.4 R5 overlap detection on both physical forms.

pub mod ast;
mod cache;
mod eval;
mod parse;
mod program;

pub use ast::{Member, PathAst, Segment, SliceSpec};
pub use cache::{PROGRAM_CACHE_DEFAULT_ENTRIES, ProgramCache};
pub use eval::{
    CanonicalMatches, EvalError, EvalLimits, EvalState, EvalStep, Matches, VisitEnd, VisitOutcome,
    eval, eval_budgeted, eval_visit, resolve,
};
pub(crate) use program::SimpleStep;
pub use program::{PathProgram, PathStep, PathSteps};

/// Hard ceiling on path text and program bytes (u16-shaped lengths in
/// the S10 cache and delta accounting; ADR-0040 D6). Config lowers via
/// [`compile_with_max_bytes`]; only a successor ADR raises it.
pub const PATH_BYTES_CEILING: usize = 0xFFFF;
/// Default text cap (`doc_max_path_bytes` — S11 wires the config key).
pub const PATH_BYTES_DEFAULT: usize = 4096;
/// Segment cap — aligned with the document depth cap (ADR-0036 D6): a
/// deeper path cannot match anything a valid document holds.
pub const SEGMENTS_MAX: usize = 128;
/// Union member cap (ADR-0040 D2).
pub const UNION_MEMBERS_MAX: usize = 16;
pub(crate) const PROGRAM_BYTES_CEILING: usize = PATH_BYTES_CEILING;

/// Typed path error with the byte offset of the offending input (text
/// offsets from `compile`; program-byte offsets from
/// `PathProgram::from_bytes`). RESP phrasing is pinned at S11 against
/// the oracle; the library lines below are the stable substrate.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PathError {
    pub offset: usize,
    pub kind: PathErrorKind,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PathErrorKind {
    /// `?(` — the documented M4.5 cut line (ADR-0024, milestone §2).
    FilterUnsupported,
    UnexpectedChar,
    Unterminated,
    BadEscape,
    BadNumber,
    BadSlice,
    BadUnionMember,
    TrailingDescend,
    PathTooLong,
    PathTooDeep,
    InvalidUtf8,
    // Program-byte validation (`PathProgram::from_bytes`).
    Truncated,
    BadVersion,
    BadFlags,
    BadOpcode,
    MissingRoot,
}

impl core::fmt::Display for PathError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.kind {
            PathErrorKind::FilterUnsupported => {
                write!(f, "filter expressions are not supported (planned for M4.5)")
            }
            PathErrorKind::UnexpectedChar => {
                write!(f, "unexpected character in path at offset {}", self.offset)
            }
            PathErrorKind::Unterminated => {
                write!(f, "unterminated bracket or string in path at offset {}", self.offset)
            }
            PathErrorKind::BadEscape => {
                write!(f, "invalid escape in path at offset {}", self.offset)
            }
            PathErrorKind::BadNumber => {
                write!(f, "invalid number in path at offset {}", self.offset)
            }
            PathErrorKind::BadSlice => write!(f, "slice step cannot be zero"),
            PathErrorKind::BadUnionMember => {
                write!(f, "invalid union member in path at offset {}", self.offset)
            }
            PathErrorKind::TrailingDescend => write!(f, "recursive descent needs a selector"),
            PathErrorKind::PathTooLong => write!(f, "path is too long"),
            PathErrorKind::PathTooDeep => write!(f, "path has too many segments"),
            PathErrorKind::InvalidUtf8 => {
                write!(f, "invalid UTF-8 in path at offset {}", self.offset)
            }
            PathErrorKind::Truncated => write!(f, "truncated path program"),
            PathErrorKind::BadVersion => write!(f, "unsupported path program version"),
            PathErrorKind::BadFlags => write!(f, "unsupported path program flags"),
            PathErrorKind::BadOpcode => write!(f, "invalid path program opcode"),
            PathErrorKind::MissingRoot => write!(f, "path program does not begin at root"),
        }
    }
}

impl core::error::Error for PathError {}

/// Compile path text (either mode, auto-detected) with the default cap.
pub fn compile(text: &[u8]) -> Result<PathProgram, PathError> {
    compile_with_max_bytes(text, PATH_BYTES_DEFAULT)
}

/// Compile with a configured text cap (clamped to the ceiling — config
/// lowers, never raises; the ParseLimits rule).
pub fn compile_with_max_bytes(text: &[u8], max_bytes: usize) -> Result<PathProgram, PathError> {
    if text.len() > max_bytes.min(PATH_BYTES_CEILING) {
        return Err(PathError { offset: 0, kind: PathErrorKind::PathTooLong });
    }
    if !inf_simd::utf8_is_valid(text) {
        let offset = core::str::from_utf8(text).map_err(|e| e.valid_up_to()).err().unwrap_or(0);
        return Err(PathError { offset, kind: PathErrorKind::InvalidUtf8 });
    }
    let ast = parse::parse(text)?;
    Ok(program::encode(&ast))
}

/// Parse without encoding (tests and the printer property).
pub fn parse_ast(text: &[u8]) -> Result<PathAst, PathError> {
    if text.len() > PATH_BYTES_CEILING {
        return Err(PathError { offset: 0, kind: PathErrorKind::PathTooLong });
    }
    if !inf_simd::utf8_is_valid(text) {
        let offset = core::str::from_utf8(text).map_err(|e| e.valid_up_to()).err().unwrap_or(0);
        return Err(PathError { offset, kind: PathErrorKind::InvalidUtf8 });
    }
    parse::parse(text)
}

/// Encode a (test-constructed) AST — the round-trip property's second leg.
pub fn encode_ast(ast: &PathAst) -> PathProgram {
    program::encode(ast)
}
