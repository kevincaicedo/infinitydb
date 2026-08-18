//! Statement parser (M4.5-S09): hand-rolled, iterative — the WHERE
//! expression machine runs on an explicit frame stack with an explicit
//! depth bound (L9; the M3-S08 pattern). Grammar authority:
//! `infinitydb/docs/partiql-subset.md`; every rejection is one of the
//! spec's documented strings.

use inf_doc::path::PathProgram;

use super::lex::{Tok, Token, lex};
use super::{QlError, QlErrorKind};
use crate::access::Projection;
use crate::predicate::{CmpOp, NESTING_DEPTH_MAX};

pub(crate) struct Statement {
    pub projection: Projection,
    pub target: Target,
    /// Offset of the WHERE keyword — residual build errors (op/pool
    /// overflows) anchor here.
    pub where_at: usize,
    pub condition: Option<Cond>,
    pub limit: Option<u32>,
}

pub(crate) enum Target {
    Ns { ns: Vec<u8> },
    Index { ns: Vec<u8>, index: Vec<u8> },
    Scan { ns: Vec<u8> },
}

pub(crate) enum Cond {
    And(Vec<Cond>),
    Or(Vec<Cond>),
    Not(Box<Cond>),
    Leaf(Leaf),
}

pub(crate) struct Leaf {
    /// Statement offset of the leaf's path/function — resolution errors
    /// point at it.
    pub at: usize,
    pub kind: LeafKind,
}

pub(crate) enum LeafKind {
    Cmp {
        path: StmtPath,
        op: CmpOp,
        lit: Lit,
    },
    Between {
        path: StmtPath,
        lo: Lit,
        hi: Lit,
    },
    BeginsWith {
        path: StmtPath,
        prefix: String,
    },
    In {
        path: StmtPath,
        members: Vec<Lit>,
    },
    Exists {
        path: StmtPath,
    },
    /// `$key = '…'` — position rules are enforced at compile (§4).
    KeyEq {
        key: String,
    },
}

/// A statement path, already compiled to the canonical M3 program —
/// resolution against index declarations is byte equality (ADR-0040).
pub(crate) struct StmtPath {
    pub program: PathProgram,
    pub at: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Lit {
    I64(i64),
    F64(f64),
    Bool(bool),
    Str(String),
}

fn err<T>(offset: usize, kind: QlErrorKind) -> Result<T, QlError> {
    Err(QlError { offset, kind })
}

/// Offset of a leading INSERT/UPDATE/DELETE keyword, if any.
fn leading_dml_keyword(text: &[u8]) -> Option<usize> {
    let start = text.iter().position(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))?;
    let end = text[start..]
        .iter()
        .position(|&b| !(b.is_ascii_alphanumeric() || b == b'_'))
        .map_or(text.len(), |i| start + i);
    let word = &text[start..end];
    ["INSERT", "UPDATE", "DELETE"]
        .iter()
        .any(|kw| word.eq_ignore_ascii_case(kw.as_bytes()))
        .then_some(start)
}

pub(crate) fn parse(text: &[u8]) -> Result<Statement, QlError> {
    // DML detection precedes lexing: an INSERT body holds syntax this
    // lexer rightly cannot tokenize, and the documented rejection must
    // name the real production, not the first strange byte.
    if let Some(at) = leading_dml_keyword(text) {
        return err(at, QlErrorKind::DmlUnsupported);
    }
    let tokens = lex(text)?;
    let mut p = Parser { tokens, i: 0 };
    if !p.take_kw("SELECT") {
        return err(p.at(), QlErrorKind::Expected("SELECT"));
    }
    let projection = p.parse_projection()?;
    if !p.take_kw("FROM") {
        return err(p.at(), QlErrorKind::Expected("FROM"));
    }
    let target = p.parse_target()?;
    p.reject_clause_keywords()?;
    let mut where_at = 0;
    let mut condition = None;
    if p.take_kw("WHERE") {
        where_at = p.prev_at();
        condition = Some(p.parse_condition()?);
        p.reject_clause_keywords()?;
    }
    let mut limit = None;
    if p.take_kw("LIMIT") {
        limit = Some(p.parse_limit()?);
        p.reject_clause_keywords()?;
    }
    if matches!(p.kind(), Tok::Semi) {
        p.bump();
    }
    if !matches!(p.kind(), Tok::End) {
        return err(p.at(), QlErrorKind::TrailingInput);
    }
    Ok(Statement { projection, target, where_at, condition, limit })
}

struct Parser<'s> {
    tokens: Vec<Token<'s>>,
    i: usize,
}

impl Parser<'_> {
    fn kind(&self) -> &Tok<'_> {
        &self.tokens[self.i].kind
    }

    fn at(&self) -> usize {
        self.tokens[self.i].at
    }

    fn prev_at(&self) -> usize {
        self.tokens[self.i - 1].at
    }

    fn bump(&mut self) {
        debug_assert!(self.i + 1 < self.tokens.len(), "End is never consumed");
        self.i += 1;
    }

    fn is_kw(&self, word: &str) -> bool {
        matches!(self.kind(), Tok::Ident(s) if s.eq_ignore_ascii_case(word))
    }

    fn take_kw(&mut self, word: &str) -> bool {
        if self.is_kw(word) {
            self.bump();
            return true;
        }
        false
    }

    /// The clause boundary: every recognized-but-unsupported clause
    /// keyword gets its documented rejection here (L8) instead of a
    /// generic syntax error.
    fn reject_clause_keywords(&self) -> Result<(), QlError> {
        let kind = if self.is_kw("ORDER") {
            QlErrorKind::OrderByUnsupported
        } else if self.is_kw("GROUP") {
            QlErrorKind::GroupByUnsupported
        } else if self.is_kw("HAVING") {
            QlErrorKind::HavingUnsupported
        } else if self.is_kw("OFFSET") {
            QlErrorKind::OffsetUnsupported
        } else if self.is_kw("JOIN") || matches!(self.kind(), Tok::Comma) {
            QlErrorKind::JoinUnsupported
        } else {
            return Ok(());
        };
        err(self.at(), kind)
    }

    fn parse_projection(&mut self) -> Result<Projection, QlError> {
        let projection = match self.kind() {
            Tok::Star => {
                self.bump();
                Projection::Documents
            }
            Tok::Ident(s) if s.eq_ignore_ascii_case("COUNT") => {
                let count_at = self.at();
                self.bump();
                if !matches!(self.kind(), Tok::LParen) {
                    // `SELECT count FROM …` — a column named count.
                    return err(count_at, QlErrorKind::ColumnProjection);
                }
                self.bump();
                if !matches!(self.kind(), Tok::Star) {
                    return err(self.at(), QlErrorKind::ColumnProjection);
                }
                self.bump();
                if !matches!(self.kind(), Tok::RParen) {
                    return err(self.at(), QlErrorKind::Expected("')'"));
                }
                self.bump();
                Projection::Count
            }
            Tok::Ident(_) | Tok::Quoted(_) | Tok::Str(_) | Tok::Int(_) | Tok::Float(_) => {
                return err(self.at(), QlErrorKind::ColumnProjection);
            }
            _ => return err(self.at(), QlErrorKind::Expected("* or COUNT(*)")),
        };
        if matches!(self.kind(), Tok::Comma) {
            return err(self.at(), QlErrorKind::ColumnProjection);
        }
        Ok(projection)
    }

    /// `ns | ns.index | ns.SCAN` — quoted parts for dotted names; a
    /// quoted `"SCAN"` is an index name, never the consent keyword.
    fn parse_target(&mut self) -> Result<Target, QlError> {
        let Some(ns) = self.take_name() else {
            return err(self.at(), QlErrorKind::Expected("a namespace"));
        };
        if !matches!(self.kind(), Tok::Dot) {
            return Ok(Target::Ns { ns });
        }
        self.bump();
        match self.kind() {
            Tok::Ident(s) if s.eq_ignore_ascii_case("SCAN") => {
                self.bump();
                Ok(Target::Scan { ns })
            }
            _ => match self.take_name() {
                Some(index) => Ok(Target::Index { ns, index }),
                None => err(self.at(), QlErrorKind::Expected("an index name or SCAN")),
            },
        }
    }

    fn take_name(&mut self) -> Option<Vec<u8>> {
        let name = match self.kind() {
            Tok::Ident(s) => s.as_bytes().to_vec(),
            Tok::Quoted(s) => s.clone().into_bytes(),
            _ => return None,
        };
        self.bump();
        Some(name)
    }

    fn parse_limit(&mut self) -> Result<u32, QlError> {
        let Tok::Int(v) = *self.kind() else {
            return err(self.at(), QlErrorKind::Expected("an integer"));
        };
        if !(1..=i64::from(u32::MAX)).contains(&v) {
            return err(self.at(), QlErrorKind::LimitRange);
        }
        self.bump();
        Ok(v as u32)
    }

    /// The WHERE expression machine: precedence OR < AND < NOT, with
    /// parentheses — one loop, one explicit frame stack, depth-bounded
    /// (ADR-0080 D6: statement nesting ≤ `NESTING_DEPTH_MAX`).
    fn parse_condition(&mut self) -> Result<Cond, QlError> {
        let mut stack: Vec<Frame> = Vec::new();
        let mut current: Option<Cond> = None;
        loop {
            let Some(mut cond) = current.take() else {
                // Operand position. Frames open ≈ tree levels above the
                // coming leaf; the leaf itself is a level (ADR-0079 D7).
                if stack.len() >= NESTING_DEPTH_MAX {
                    return err(self.at(), QlErrorKind::TooDeep);
                }
                if self.take_kw("NOT") {
                    stack.push(Frame::Not);
                } else if matches!(self.kind(), Tok::LParen) {
                    self.bump();
                    stack.push(Frame::Paren);
                } else {
                    current = Some(self.parse_leaf()?);
                }
                continue;
            };
            // NOT binds tightest: fold prefix negations immediately, so
            // a Not frame never sits below an And/Or frame.
            while matches!(stack.last(), Some(Frame::Not)) {
                stack.pop();
                cond = Cond::Not(Box::new(cond));
            }
            if self.take_kw("AND") {
                match stack.last_mut() {
                    Some(Frame::And(v)) => v.push(cond),
                    _ => stack.push(Frame::And(vec![cond])),
                }
            } else if self.take_kw("OR") {
                if matches!(stack.last(), Some(Frame::And(_))) {
                    let Some(Frame::And(mut v)) = stack.pop() else { unreachable!("just matched") };
                    v.push(cond);
                    cond = Cond::And(v);
                }
                match stack.last_mut() {
                    Some(Frame::Or(v)) => v.push(cond),
                    _ => stack.push(Frame::Or(vec![cond])),
                }
            } else if matches!(self.kind(), Tok::RParen)
                && stack.iter().any(|f| matches!(f, Frame::Paren))
            {
                loop {
                    match stack.pop() {
                        Some(Frame::And(mut v)) => {
                            v.push(cond);
                            cond = Cond::And(v);
                        }
                        Some(Frame::Or(mut v)) => {
                            v.push(cond);
                            cond = Cond::Or(v);
                        }
                        Some(Frame::Paren) => break,
                        Some(Frame::Not) | None => unreachable!("folded above; Paren was found"),
                    }
                }
                self.bump();
                current = Some(cond);
            } else {
                // End of the condition (an unmatched `)` ends it too —
                // the statement loop reports it as trailing input).
                loop {
                    match stack.pop() {
                        Some(Frame::And(mut v)) => {
                            v.push(cond);
                            cond = Cond::And(v);
                        }
                        Some(Frame::Or(mut v)) => {
                            v.push(cond);
                            cond = Cond::Or(v);
                        }
                        Some(Frame::Paren) => return err(self.at(), QlErrorKind::Expected("')'")),
                        Some(Frame::Not) => unreachable!("folded above"),
                        None => return Ok(cond),
                    }
                }
            }
        }
    }

    fn parse_leaf(&mut self) -> Result<Cond, QlError> {
        match self.kind() {
            Tok::Pseudo(_) => self.parse_key_leaf(),
            Tok::Ident(s) if matches!(self.tokens[self.i + 1].kind, Tok::LParen) => {
                if s.eq_ignore_ascii_case("begins_with") {
                    self.parse_begins_with()
                } else if s.eq_ignore_ascii_case("exists") {
                    self.parse_exists()
                } else {
                    err(self.at(), QlErrorKind::UnknownFunction)
                }
            }
            Tok::Ident(_) | Tok::LBracket => self.parse_path_leaf(),
            _ => err(self.at(), QlErrorKind::Expected("a condition")),
        }
    }

    /// `$key = '…'` — anything else spelled on `$key` is a documented
    /// primary-key rejection (§4).
    fn parse_key_leaf(&mut self) -> Result<Cond, QlError> {
        let at = self.at();
        let Tok::Pseudo(name) = self.kind() else { unreachable!("caller matched") };
        if !name.eq_ignore_ascii_case("key") {
            return err(at, QlErrorKind::UnknownPseudoPath);
        }
        self.bump();
        if !matches!(self.kind(), Tok::Eq) {
            return err(at, QlErrorKind::PkOp);
        }
        self.bump();
        match self.parse_literal()? {
            Lit::Str(key) => Ok(Cond::Leaf(Leaf { at, kind: LeafKind::KeyEq { key } })),
            _ => err(at, QlErrorKind::PkType),
        }
    }

    fn parse_begins_with(&mut self) -> Result<Cond, QlError> {
        let at = self.at();
        self.bump(); // begins_with
        self.bump(); // (
        let path = self.parse_path()?;
        if !matches!(self.kind(), Tok::Comma) {
            return err(at, QlErrorKind::BeginsWithArgs);
        }
        self.bump();
        let Tok::Str(prefix) = self.kind() else {
            return err(at, QlErrorKind::BeginsWithArgs);
        };
        let prefix = prefix.clone();
        self.bump();
        if !matches!(self.kind(), Tok::RParen) {
            return err(at, QlErrorKind::BeginsWithArgs);
        }
        self.bump();
        Ok(Cond::Leaf(Leaf { at, kind: LeafKind::BeginsWith { path, prefix } }))
    }

    fn parse_exists(&mut self) -> Result<Cond, QlError> {
        let at = self.at();
        self.bump(); // exists
        self.bump(); // (
        let path = self.parse_path()?;
        if !matches!(self.kind(), Tok::RParen) {
            return err(at, QlErrorKind::ExistsArgs);
        }
        self.bump();
        Ok(Cond::Leaf(Leaf { at, kind: LeafKind::Exists { path } }))
    }

    fn parse_path_leaf(&mut self) -> Result<Cond, QlError> {
        let at = self.at();
        let path = self.parse_path()?;
        if let Some(op) = self.take_cmp() {
            let lit = self.parse_literal()?;
            return Ok(Cond::Leaf(Leaf { at, kind: LeafKind::Cmp { path, op, lit } }));
        }
        if self.take_kw("BETWEEN") {
            return self.parse_between(at, path, false);
        }
        if self.take_kw("IN") {
            return self.parse_in(at, path, false);
        }
        if self.is_kw("IS") {
            return err(self.at(), QlErrorKind::IsNullUnsupported);
        }
        if self.is_kw("LIKE") {
            return err(self.at(), QlErrorKind::LikeUnsupported);
        }
        if self.take_kw("NOT") {
            if self.take_kw("BETWEEN") {
                return self.parse_between(at, path, true);
            }
            if self.take_kw("IN") {
                return self.parse_in(at, path, true);
            }
            if self.is_kw("LIKE") {
                return err(self.at(), QlErrorKind::LikeUnsupported);
            }
            return err(self.at(), QlErrorKind::Expected("BETWEEN or IN"));
        }
        err(self.at(), QlErrorKind::Expected("a condition"))
    }

    fn parse_between(&mut self, at: usize, path: StmtPath, negated: bool) -> Result<Cond, QlError> {
        let lo = self.parse_literal()?;
        if !self.take_kw("AND") {
            return err(self.at(), QlErrorKind::Expected("AND"));
        }
        let hi = self.parse_literal()?;
        let leaf = Leaf { at, kind: LeafKind::Between { path, lo, hi } };
        Ok(negate(leaf, negated))
    }

    fn parse_in(&mut self, at: usize, path: StmtPath, negated: bool) -> Result<Cond, QlError> {
        if !matches!(self.kind(), Tok::LParen) {
            return err(self.at(), QlErrorKind::Expected("'('"));
        }
        self.bump();
        let mut members = vec![self.parse_literal()?];
        while matches!(self.kind(), Tok::Comma) {
            self.bump();
            members.push(self.parse_literal()?);
        }
        if !matches!(self.kind(), Tok::RParen) {
            return err(self.at(), QlErrorKind::Expected("',' or ')'"));
        }
        self.bump();
        if members.len() > crate::predicate::IN_MEMBERS_MAX {
            return err(at, QlErrorKind::InCount);
        }
        let leaf = Leaf { at, kind: LeafKind::In { path, members } };
        Ok(negate(leaf, negated))
    }

    fn take_cmp(&mut self) -> Option<CmpOp> {
        let op = match self.kind() {
            Tok::Eq => CmpOp::Eq,
            Tok::Ne => CmpOp::Ne,
            Tok::Lt => CmpOp::Lt,
            Tok::Le => CmpOp::Le,
            Tok::Gt => CmpOp::Gt,
            Tok::Ge => CmpOp::Ge,
            _ => return None,
        };
        self.bump();
        Some(op)
    }

    fn parse_literal(&mut self) -> Result<Lit, QlError> {
        let lit = match self.kind() {
            Tok::Str(s) => Lit::Str(s.clone()),
            Tok::Int(v) => Lit::I64(*v),
            Tok::Float(v) => Lit::F64(*v),
            Tok::Ident(s) if s.eq_ignore_ascii_case("TRUE") => Lit::Bool(true),
            Tok::Ident(s) if s.eq_ignore_ascii_case("FALSE") => Lit::Bool(false),
            Tok::Ident(s) if s.eq_ignore_ascii_case("NULL") => {
                return err(self.at(), QlErrorKind::NullComparison);
            }
            _ => return err(self.at(), QlErrorKind::Expected("a literal")),
        };
        self.bump();
        Ok(lit)
    }

    /// Assemble a fence-shaped path from tokens and compile it through
    /// the one path grammar (`inf_doc::path::compile` — ADR-0079 D1):
    /// resolution identity is the canonical program bytes.
    fn parse_path(&mut self) -> Result<StmtPath, QlError> {
        let start = self.at();
        let mut text = String::from("$");
        match self.kind() {
            Tok::Ident(s) => {
                text.push('.');
                text.push_str(s);
                self.bump();
            }
            Tok::LBracket => self.path_bracket(&mut text)?,
            _ => return err(self.at(), QlErrorKind::Expected("a document path")),
        }
        loop {
            match self.kind() {
                Tok::Dot if matches!(self.tokens[self.i + 1].kind, Tok::Dot) => {
                    return err(self.at(), QlErrorKind::DescendUnsupported);
                }
                Tok::Dot => {
                    self.bump();
                    let Tok::Ident(s) = self.kind() else {
                        return err(self.at(), QlErrorKind::Expected("a document path"));
                    };
                    text.push('.');
                    text.push_str(s);
                    self.bump();
                }
                Tok::LBracket => self.path_bracket(&mut text)?,
                _ => break,
            }
        }
        match inf_doc::path::compile(text.as_bytes()) {
            Ok(program) => {
                debug_assert!(!program.is_legacy(), "assembled text is rooted at $");
                Ok(StmtPath { program, at: start })
            }
            Err(_) => err(start, QlErrorKind::BadPath),
        }
    }

    /// `'[' ( integer | name | * ) ']'` — slice/union spellings get
    /// their own documented rejection instead of a generic one.
    fn path_bracket(&mut self, text: &mut String) -> Result<(), QlError> {
        self.bump(); // [
        match self.kind() {
            Tok::Int(i) => {
                text.push_str(&format!("[{i}]"));
                self.bump();
            }
            Tok::Star => {
                text.push_str("[*]");
                self.bump();
            }
            Tok::Str(name) | Tok::Quoted(name) => {
                push_quoted_member(text, name);
                self.bump();
            }
            Tok::Colon => return err(self.at(), QlErrorKind::SliceUnionUnsupported),
            _ => return err(self.at(), QlErrorKind::Expected("a document path")),
        }
        match self.kind() {
            Tok::RBracket => {
                self.bump();
                Ok(())
            }
            Tok::Comma | Tok::Colon => err(self.at(), QlErrorKind::SliceUnionUnsupported),
            _ => err(self.at(), QlErrorKind::Expected("']'")),
        }
    }
}

/// `NOT BETWEEN` / `NOT IN` are sugar for tree-level negation. A
/// negated leaf is a plain residual `NOT` — it never becomes a key
/// condition (complement ranges are not servable, spec §5).
fn negate(leaf: Leaf, negated: bool) -> Cond {
    let cond = Cond::Leaf(leaf);
    if negated { Cond::Not(Box::new(cond)) } else { cond }
}

enum Frame {
    And(Vec<Cond>),
    Or(Vec<Cond>),
    Not,
    Paren,
}

/// Rebuild a quoted bracket member in the M3 path grammar's spelling
/// (single quotes, JSON-style escapes) — the assembled text goes
/// through `inf_doc::path::compile`, whose canonical program is the
/// resolution identity, so spelling variants cannot fork.
fn push_quoted_member(text: &mut String, name: &str) {
    text.push_str("['");
    for ch in name.chars() {
        match ch {
            '\'' => text.push_str("\\'"),
            '\\' => text.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                text.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => text.push(c),
        }
    }
    text.push_str("']");
}
