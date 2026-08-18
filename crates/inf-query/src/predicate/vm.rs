//! Predicate VM evaluator (M4.5-S08): ADR-0079 D4–D6 implemented
//! verbatim over validated bytecode. One iterative pass over the op
//! tape (`read_op`, ~2–4 predictable branches per op), a fixed
//! [`NESTING_DEPTH_MAX`]-slot connective stack, path resolution through
//! the M3 streaming entry (`inf_doc::path::eval_visit` — the ADR-0040
//! evaluator, never a second implementation), and the imported ADR-0074
//! numeric truth (`inf-store::index_key::compare_i64_f64` — the
//! two-importer rule; a third derivation anywhere is a review reject).
//! The hot path allocates nothing: constants and paths are decoded once
//! at [`PredicateVm::new`], per-eval state is the connective array plus
//! the walk's fixed-capacity frames.
//!
//! Semantics refinements this module pins (both disclosed in the S08
//! ledger entry, both mirrored by the differential reference):
//!
//! - **Leaves are atomic:** a comparison tests *every* value its path
//!   resolves to, so flags are a pure function of (match multiset,
//!   constant) — independent of walk order or short-circuit strategy.
//!   Connective short-circuit (D3) still skips whole operands; skipped
//!   operands contribute no flags, exactly as the ADR's skip-by-decode
//!   implies. `EXISTS` alone stops at the first match (it has no flags
//!   to lose; fuel stays deterministic).
//! - **IN classifies once per value:** members are family-homogeneous
//!   (D2.4), so comparability is a function of the value's type alone —
//!   the first incomparable member proves every member incomparable:
//!   one fuel unit, the flag once, false. Comparable values test
//!   members left to right, one fuel unit each, stopping at `Equal`.

use std::cmp::Ordering;
use std::ops::ControlFlow;

use inf_doc::DocValue;
use inf_doc::path::{PathProgram, VisitEnd, eval_visit};
use inf_store::compare_i64_f64;

use super::NESTING_DEPTH_MAX;
use super::program::{
    CmpOp, Constant, InMembersRef, Op, PredicateProgram, decode_constants, decode_paths, read_op,
};

/// Monotone observability flags (ADR-0079 D5): set by evaluated work,
/// never cleared; `NOT` flips the verdict only. They feed the S12
/// `query_*` counters and the S15 oracle's truth model — deterministic
/// (L7), part of the observable contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EvalFlags {
    /// Some evaluated comparison's path resolved to zero values
    /// (∃ over ∅ — the leaf was false, D4). `EXISTS` never sets it.
    pub missing: bool,
    /// Some evaluated comparison was forced false by the D4 table (or a
    /// `BEGINS_WITH`/`IN`/`BETWEEN` value-class mismatch).
    pub type_mismatch: bool,
}

/// One completed evaluation: the two-valued verdict, the flags, and the
/// exact fuel consumed (deterministic — the DST equivalence oracle
/// consumes all three).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvalOutcome {
    pub verdict: bool,
    pub flags: EvalFlags,
    /// D6 units: op decodes (skipped included) + document nodes visited
    /// during path resolution + IN members tested.
    pub fuel_used: u64,
}

/// Evaluation failure — an operating condition, never a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PredicateEvalError {
    /// The fuel budget ran out (D6). S11 surfaces this as the
    /// documented `INF.QL` error — never a truncated result, never a
    /// silently-false verdict (L8/L10).
    FuelExhausted,
}

impl core::fmt::Display for PredicateEvalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PredicateEvalError::FuelExhausted => write!(f, "predicate fuel exhausted"),
        }
    }
}

impl core::error::Error for PredicateEvalError {}

/// A predicate prepared for evaluation: the program's pools decoded
/// once (S09 caches this alongside the compiled statement), the op tape
/// read in place per eval. Construction is the cold path; `eval` is the
/// §4.1 hot path.
#[derive(Clone, Debug)]
pub struct PredicateVm {
    program: PredicateProgram,
    paths: Vec<PathProgram>,
    constants: Vec<Constant>,
    expr_at: u32,
}

/// Fuel meter (ADR-0079 D6): one budget, counted up, checked on every
/// charge. `remaining` hands the walk its node budget — the same unit.
struct Fuel {
    used: u64,
    budget: u64,
}

impl Fuel {
    #[inline]
    fn charge(&mut self, units: u64) -> Result<(), PredicateEvalError> {
        match self.used.checked_add(units) {
            Some(next) if next <= self.budget => {
                self.used = next;
                Ok(())
            }
            _ => Err(PredicateEvalError::FuelExhausted),
        }
    }

    #[inline]
    fn remaining(&self) -> u64 {
        self.budget - self.used
    }
}

/// One open connective: how many operands are still unevaluated,
/// counting the one currently under evaluation.
#[derive(Clone, Copy)]
enum Pending {
    And { remaining: u8 },
    Or { remaining: u8 },
    Not,
}

impl PredicateVm {
    /// Decode the pools of a validated program (cold path — allocates;
    /// the per-statement cache holds the result). Path programs inside
    /// predicates are compiled once here, never re-parsed per eval.
    pub fn new(program: &PredicateProgram) -> PredicateVm {
        let bytes = program.as_bytes();
        let mut at = 2; // version, flags
        let paths = decode_paths(bytes, &mut at);
        let constants = decode_constants(bytes, &mut at);
        debug_assert!(at < bytes.len(), "validated program ends with an expression");
        PredicateVm { program: program.clone(), paths, constants, expr_at: at as u32 }
    }

    /// The bytes this VM evaluates (the S07 canonical form).
    pub fn program(&self) -> &PredicateProgram {
        &self.program
    }

    /// Evaluate against `root` under a fuel budget (S11 wires the
    /// cell's value; tests pass `u64::MAX`). Deterministic in verdict,
    /// flags, and fuel (L7); zero heap allocation on this path (the
    /// §4.1 row's gate). A predicate needs at least one unit — a zero
    /// budget is exhausted before the first op decodes.
    pub fn eval(
        &self,
        root: DocValue<'_>,
        fuel_budget: u64,
    ) -> Result<EvalOutcome, PredicateEvalError> {
        let bytes = self.program.as_bytes();
        let mut fuel = Fuel { used: 0, budget: fuel_budget };
        let mut flags = EvalFlags::default();
        // The D7 depth bound as a fixed array: nesting ≤ 32 counting
        // the leaf level, so connective frames stay strictly below it.
        let mut stack = [Pending::Not; NESTING_DEPTH_MAX];
        let mut depth: usize = 0;
        let mut pc = self.expr_at as usize;
        loop {
            // Every op decode charges one unit, evaluated or skipped (D6).
            fuel.charge(1)?;
            let (op, next) = read_op(bytes, pc);
            pc = next;
            let leaf_verdict = match op {
                Op::And { arity } => {
                    debug_assert!(depth < NESTING_DEPTH_MAX, "validated nesting depth");
                    stack[depth] = Pending::And { remaining: arity };
                    depth += 1;
                    continue;
                }
                Op::Or { arity } => {
                    debug_assert!(depth < NESTING_DEPTH_MAX, "validated nesting depth");
                    stack[depth] = Pending::Or { remaining: arity };
                    depth += 1;
                    continue;
                }
                Op::Not => {
                    debug_assert!(depth < NESTING_DEPTH_MAX, "validated nesting depth");
                    stack[depth] = Pending::Not;
                    depth += 1;
                    continue;
                }
                leaf => self.eval_leaf(leaf, root, &mut fuel, &mut flags)?,
            };
            if let Some(verdict) =
                fold(leaf_verdict, bytes, &mut pc, &mut stack, &mut depth, &mut fuel)?
            {
                debug_assert_eq!(pc, bytes.len(), "the expression ends exactly at end-of-program");
                return Ok(EvalOutcome { verdict, flags, fuel_used: fuel.used });
            }
        }
    }

    /// One predicate leaf under the D4 table. Each arm resolves the
    /// leaf's path once (streaming, existential) and folds flags per
    /// the module rules above.
    fn eval_leaf(
        &self,
        op: Op<'_>,
        root: DocValue<'_>,
        fuel: &mut Fuel,
        flags: &mut EvalFlags,
    ) -> Result<bool, PredicateEvalError> {
        match op {
            Op::Cmp { op, path, constant } => {
                let constant = &self.constants[constant as usize];
                self.leaf(path, root, fuel, flags, |value, _fuel, flags| {
                    Ok(match relation(&value, constant) {
                        Some(ordering) => cmp_matches(op, ordering),
                        None => {
                            flags.type_mismatch = true;
                            false
                        }
                    })
                })
            }
            Op::Between { path, lo, hi } => {
                let lo = &self.constants[lo as usize];
                let hi = &self.constants[hi as usize];
                self.leaf(path, root, fuel, flags, |value, _fuel, flags| {
                    // Inclusive both ends; reversed bounds fall out as
                    // always-false — a value, not an error (D3).
                    Ok(match (relation(&value, lo), relation(&value, hi)) {
                        (Some(versus_lo), Some(versus_hi)) => {
                            versus_lo != Ordering::Less && versus_hi != Ordering::Greater
                        }
                        _ => {
                            flags.type_mismatch = true;
                            false
                        }
                    })
                })
            }
            Op::BeginsWith { path, prefix } => {
                let Constant::Utf8(prefix) = &self.constants[prefix as usize] else {
                    unreachable!("validated BEGINS_WITH operand is utf8")
                };
                self.leaf(path, root, fuel, flags, |value, _fuel, flags| {
                    // Byte-prefix on utf8 ≡ code-point prefix (D4); the
                    // empty prefix matches every string.
                    Ok(match value {
                        DocValue::Str(s) => s.as_bytes().starts_with(prefix.as_bytes()),
                        _ => {
                            flags.type_mismatch = true;
                            false
                        }
                    })
                })
            }
            Op::In { path, members } => self.leaf(path, root, fuel, flags, |value, fuel, flags| {
                self.in_test(&value, members, fuel, flags)
            }),
            Op::Exists { path } => self.exists(path, root, fuel),
            Op::And { .. } | Op::Or { .. } | Op::Not => {
                unreachable!("connectives handled by the main loop")
            }
        }
    }

    /// Existential comparison leaf: stream the path's values, test each
    /// (leaf-atomic — every value, flags order-independent), settle the
    /// node fuel, fold the MISSING flag. The walk's budget is the fuel
    /// remaining at leaf entry; member fuel charged inside `test` can
    /// push the settled total past the budget — that leaf is then
    /// `FuelExhausted`, deterministically.
    fn leaf<'a>(
        &self,
        path: u32,
        root: DocValue<'a>,
        fuel: &mut Fuel,
        flags: &mut EvalFlags,
        mut test: impl FnMut(
            DocValue<'a>,
            &mut Fuel,
            &mut EvalFlags,
        ) -> Result<bool, PredicateEvalError>,
    ) -> Result<bool, PredicateEvalError> {
        let mut any_match = false;
        let mut satisfied = false;
        let mut failed: Option<PredicateEvalError> = None;
        let budget_nodes = fuel.remaining();
        let outcome = eval_visit(&self.paths[path as usize], root, budget_nodes, |value| {
            any_match = true;
            match test(value, fuel, flags) {
                Ok(true) => {
                    satisfied = true;
                    ControlFlow::Continue(())
                }
                Ok(false) => ControlFlow::Continue(()),
                Err(error) => {
                    failed = Some(error);
                    ControlFlow::Break(())
                }
            }
        });
        fuel.charge(outcome.nodes_visited)?;
        if let Some(error) = failed {
            return Err(error);
        }
        if outcome.end == VisitEnd::Exhausted {
            return Err(PredicateEvalError::FuelExhausted);
        }
        if !any_match {
            flags.missing = true;
        }
        Ok(satisfied)
    }

    /// `EXISTS` tests presence, not value truthiness (D3): true iff the
    /// path resolves at all — explicit null and containers exist; never
    /// sets MISSING (absence is its domain, not its anomaly, D5). Stops
    /// at the first match: with no flags to accumulate, the early break
    /// is observation-free and fuel stays deterministic.
    fn exists(
        &self,
        path: u32,
        root: DocValue<'_>,
        fuel: &mut Fuel,
    ) -> Result<bool, PredicateEvalError> {
        let mut found = false;
        let outcome = eval_visit(&self.paths[path as usize], root, fuel.remaining(), |_| {
            found = true;
            ControlFlow::Break(())
        });
        fuel.charge(outcome.nodes_visited)?;
        if outcome.end == VisitEnd::Exhausted {
            return Err(PredicateEvalError::FuelExhausted);
        }
        Ok(found)
    }

    /// One value against an IN list — each membership test is an EQ
    /// under D4, numeric coercion included (`10 IN (10.0)` is true).
    /// Fuel per member tested; classify-once on incomparability (see
    /// the module doc).
    fn in_test(
        &self,
        value: &DocValue<'_>,
        members: InMembersRef<'_>,
        fuel: &mut Fuel,
        flags: &mut EvalFlags,
    ) -> Result<bool, PredicateEvalError> {
        for member in members.iter() {
            fuel.charge(1)?;
            match relation(value, &self.constants[member as usize]) {
                Some(Ordering::Equal) => return Ok(true),
                Some(_) => {}
                None => {
                    // Members share a family (D2.4): incomparable with
                    // one ⇒ incomparable with all.
                    flags.type_mismatch = true;
                    return Ok(false);
                }
            }
        }
        Ok(false)
    }
}

/// Fold a completed operand's verdict into the open connectives.
/// `Some(v)` ⇒ the root expression completed; `None` ⇒ evaluation
/// continues at `pc` (the next unevaluated sibling). Short-circuit is
/// skip-by-decode (ADR-0079 D3): a settled AND/OR decodes past its
/// unevaluated siblings, `pc` strictly increases, no jump exists.
fn fold(
    mut verdict: bool,
    bytes: &[u8],
    pc: &mut usize,
    stack: &mut [Pending; NESTING_DEPTH_MAX],
    depth: &mut usize,
    fuel: &mut Fuel,
) -> Result<Option<bool>, PredicateEvalError> {
    loop {
        if *depth == 0 {
            return Ok(Some(verdict));
        }
        match &mut stack[*depth - 1] {
            Pending::Not => {
                // Flips the verdict only; flags accumulate through (D5).
                *depth -= 1;
                verdict = !verdict;
            }
            Pending::And { remaining } => {
                if !verdict {
                    skip_operands(bytes, pc, *remaining - 1, fuel)?;
                    *depth -= 1; // settled false
                } else if *remaining == 1 {
                    *depth -= 1; // last operand: settled true
                } else {
                    *remaining -= 1;
                    return Ok(None);
                }
            }
            Pending::Or { remaining } => {
                if verdict {
                    skip_operands(bytes, pc, *remaining - 1, fuel)?;
                    *depth -= 1; // settled true
                } else if *remaining == 1 {
                    *depth -= 1; // settled false
                } else {
                    *remaining -= 1;
                    return Ok(None);
                }
            }
        }
    }
}

/// Decode past `count` complete operand subtrees without evaluating
/// them. Skipped ops still charge decode fuel (D6) — fuel stays
/// proportional to program size and deterministic. Bounded: each
/// iteration decodes one op forward and the validated tape holds at
/// most `OPS_MAX` ops (the fuel charge is the second bound).
fn skip_operands(
    bytes: &[u8],
    pc: &mut usize,
    count: u8,
    fuel: &mut Fuel,
) -> Result<(), PredicateEvalError> {
    let mut open_operands = u32::from(count);
    while open_operands > 0 {
        fuel.charge(1)?;
        let (op, next) = read_op(bytes, *pc);
        *pc = next;
        open_operands -= 1;
        match op {
            Op::And { arity } | Op::Or { arity } => open_operands += u32::from(arity),
            Op::Not => open_operands += 1,
            _ => {}
        }
    }
    Ok(())
}

/// The ADR-0079 D4 table cell for one resolved value against one typed
/// constant: `Some(ordering)` on the comparable cells, `None` on the
/// false-with-flag cells. The numeric pair routes through the imported
/// ADR-0074 compare (§3.1 one-truth-table rule — never re-derived
/// here). A NaN document scalar — unreachable from JSON — falls out as
/// `None`: the D4 incomparable stance, asserted in debug, never a
/// panic.
fn relation(value: &DocValue<'_>, constant: &Constant) -> Option<Ordering> {
    match (value, constant) {
        (DocValue::I64(v), Constant::I64(c)) => Some(v.cmp(c)),
        (DocValue::I64(v), Constant::F64(c)) => Some(compare_i64_f64(*v, *c)),
        (DocValue::F64(v), Constant::I64(c)) => {
            debug_assert!(!v.is_nan(), "NaN cannot reach the VM from JSON (ADR-0079 D4)");
            if v.is_nan() {
                return None;
            }
            Some(compare_i64_f64(*c, *v).reverse())
        }
        (DocValue::F64(v), Constant::F64(c)) => {
            debug_assert!(!v.is_nan(), "NaN cannot reach the VM from JSON (ADR-0079 D4)");
            // Total here: `c` is validated finite, and a NaN `v` is
            // exactly the `None` cell — partial_cmp is the table.
            v.partial_cmp(c)
        }
        (DocValue::Bool(v), Constant::Bool(c)) => Some(v.cmp(c)), // false < true
        (DocValue::Str(v), Constant::Utf8(c)) => Some(v.as_bytes().cmp(c.as_bytes())),
        // Null, Arr, Obj, and every remaining cross-type cell: false
        // with the flag (the DynamoDB sparse posture — no invented
        // cross-type order, D4).
        _ => None,
    }
}

/// Comparator verdict from an ordering. `Ne` is true on any ordering
/// other than `Equal` — the false-with-flag cells never reach here
/// (they are `None` in [`relation`], false for every comparator
/// including `Ne`).
fn cmp_matches(op: CmpOp, ordering: Ordering) -> bool {
    match op {
        CmpOp::Eq => ordering == Ordering::Equal,
        CmpOp::Ne => ordering != Ordering::Equal,
        CmpOp::Lt => ordering == Ordering::Less,
        CmpOp::Le => ordering != Ordering::Greater,
        CmpOp::Gt => ordering == Ordering::Greater,
        CmpOp::Ge => ordering != Ordering::Less,
    }
}

#[cfg(test)]
mod tests {
    use inf_doc::{JsonParser, TapeDoc, path};

    use super::super::encode;
    use super::super::program::Predicate;
    use super::*;

    fn p(text: &str) -> PathProgram {
        path::compile(text.as_bytes()).expect("fixture path compiles")
    }

    fn vm_of(predicate: &Predicate) -> PredicateVm {
        PredicateVm::new(&encode(predicate).expect("fixture encodes"))
    }

    fn cmp(op: CmpOp, path: &str, constant: Constant) -> Predicate {
        Predicate::Cmp { op, path: p(path), constant }
    }

    /// Evaluate a predicate against a JSON fixture with unbounded fuel.
    fn eval_on(predicate: &Predicate, json: &str) -> EvalOutcome {
        let vm = vm_of(predicate);
        let bytes = JsonParser::new().parse(json.as_bytes()).expect("fixture parses");
        let tape = TapeDoc::from_bytes(&bytes).expect("parser emits valid idoc");
        vm.eval(DocValue::from(tape.root()), u64::MAX).expect("unbounded fuel completes")
    }

    fn flags(missing: bool, type_mismatch: bool) -> EvalFlags {
        EvalFlags { missing, type_mismatch }
    }

    #[track_caller]
    fn assert_case(predicate: &Predicate, json: &str, verdict: bool, expected: EvalFlags) {
        let outcome = eval_on(predicate, json);
        assert_eq!(outcome.verdict, verdict, "verdict for {predicate:?} over {json}");
        assert_eq!(outcome.flags, expected, "flags for {predicate:?} over {json}");
    }

    // -- The D4 table -------------------------------------------------

    #[test]
    fn numeric_pair_is_the_one_coercing_cell() {
        // 3 == 3.0 (the §3.1 one-truth-table rule).
        assert_case(
            &cmp(CmpOp::Eq, "$.n", Constant::F64(3.0)),
            r#"{"n":3}"#,
            true,
            flags(false, false),
        );
        // The 2^53 edge: 2^53 + 1 as i64 vs 2^53 as f64 — exact, not
        // rounded (the reason the compare is imported, never a cast).
        let above = &cmp(CmpOp::Gt, "$.n", Constant::F64(9_007_199_254_740_992.0));
        assert_case(above, r#"{"n":9007199254740993}"#, true, flags(false, false));
        let equal = &cmp(CmpOp::Eq, "$.n", Constant::F64(9_007_199_254_740_992.0));
        assert_case(equal, r#"{"n":9007199254740993}"#, false, flags(false, false));
        // i64::MAX vs an f64 above 2^63.
        let below = &cmp(CmpOp::Lt, "$.n", Constant::F64(9.3e18));
        assert_case(below, r#"{"n":9223372036854775807}"#, true, flags(false, false));
        // A fractional f64 document value against i64 bounds.
        let half =
            &Predicate::Between { path: p("$.n"), lo: Constant::I64(0), hi: Constant::I64(1) };
        assert_case(half, r#"{"n":0.5}"#, true, flags(false, false));
    }

    #[test]
    fn cross_type_cells_are_false_with_flag() {
        let doc = r#"{"s":"x","b":true,"nul":null,"arr":[7],"obj":{"k":1},"n":1}"#;
        // Every flag cell is false for EVERY comparator — Ne included
        // (a type-mismatched value can never satisfy a predicate).
        for op in [CmpOp::Eq, CmpOp::Ne, CmpOp::Lt, CmpOp::Ge] {
            assert_case(&cmp(op, "$.s", Constant::I64(1)), doc, false, flags(false, true));
        }
        assert_case(&cmp(CmpOp::Eq, "$.b", Constant::I64(1)), doc, false, flags(false, true));
        assert_case(
            &cmp(CmpOp::Eq, "$.n", Constant::Utf8("1".into())),
            doc,
            false,
            flags(false, true),
        );
        assert_case(&cmp(CmpOp::Eq, "$.n", Constant::Bool(true)), doc, false, flags(false, true));
        // Null compares with nothing (null-absent, ADR-0074 D2).
        assert_case(&cmp(CmpOp::Eq, "$.nul", Constant::I64(1)), doc, false, flags(false, true));
        assert_case(
            &cmp(CmpOp::Ne, "$.nul", Constant::Utf8("x".into())),
            doc,
            false,
            flags(false, true),
        );
        // Containers: deep equality is a named absence (D8).
        assert_case(&cmp(CmpOp::Eq, "$.arr", Constant::I64(7)), doc, false, flags(false, true));
        assert_case(&cmp(CmpOp::Eq, "$.obj", Constant::I64(1)), doc, false, flags(false, true));
        // Comparable cells right next to them stay unflagged.
        assert_case(&cmp(CmpOp::Eq, "$.b", Constant::Bool(true)), doc, true, flags(false, false));
        assert_case(
            &cmp(CmpOp::Gt, "$.s", Constant::Utf8("w".into())),
            doc,
            true,
            flags(false, false),
        );
    }

    #[test]
    fn existential_multi_match_and_ne_vs_not_eq() {
        let doc = r#"{"tags":[1,2]}"#;
        let eq_one = cmp(CmpOp::Eq, "$.tags[*]", Constant::I64(1));
        assert_case(&eq_one, doc, true, flags(false, false));
        assert_case(
            &cmp(CmpOp::Eq, "$.tags[*]", Constant::I64(3)),
            doc,
            false,
            flags(false, false),
        );
        // The D4 disclosure: NE quantifies existentially too, so on a
        // multi-match path NE ≢ NOT(EQ) — both expressible, different.
        assert_case(&cmp(CmpOp::Ne, "$.tags[*]", Constant::I64(1)), doc, true, flags(false, false));
        assert_case(&Predicate::Not(Box::new(eq_one)), doc, false, flags(false, false));
        // Leaf atomicity: a satisfied leaf still tests every value, so
        // the mismatching sibling raises the flag (order-independent).
        let mixed = r#"{"tags":[1,"x"]}"#;
        assert_case(
            &cmp(CmpOp::Gt, "$.tags[*]", Constant::I64(0)),
            mixed,
            true,
            flags(false, true),
        );
    }

    #[test]
    fn missing_path_semantics() {
        let doc = r#"{"a":1}"#;
        let missing_eq = cmp(CmpOp::Eq, "$.b", Constant::I64(1));
        // ∃ over ∅: false with MISSING.
        assert_case(&missing_eq, doc, false, flags(true, false));
        // The D5 golden: NOT(missing = 5) is TRUE with MISSING set —
        // flags accumulate, NOT flips the verdict only.
        assert_case(&Predicate::Not(Box::new(missing_eq.clone())), doc, true, flags(true, false));
        // Evaluated operands contribute flags…
        let and = Predicate::And(vec![cmp(CmpOp::Eq, "$.a", Constant::I64(1)), missing_eq.clone()]);
        assert_case(&and, doc, false, flags(true, false));
        // …but short-circuit-skipped operands contribute nothing: the
        // OR settles on its first operand and never resolves `$.b`.
        let or = Predicate::Or(vec![cmp(CmpOp::Eq, "$.a", Constant::I64(1)), missing_eq]);
        assert_case(&or, doc, true, flags(false, false));
    }

    #[test]
    fn exists_semantics() {
        let doc = r#"{"nul":null,"obj":{},"arr":[],"n":0}"#;
        // Presence, not truthiness: explicit null and empty containers
        // exist (DynamoDB attribute_exists posture).
        for present in ["$.nul", "$.obj", "$.arr", "$.n"] {
            assert_case(&Predicate::Exists { path: p(present) }, doc, true, flags(false, false));
        }
        // Absence is EXISTS's domain, not its anomaly: no MISSING flag.
        let gone = Predicate::Exists { path: p("$.gone") };
        assert_case(&gone, doc, false, flags(false, false));
        assert_case(&Predicate::Not(Box::new(gone)), doc, true, flags(false, false));
        // A wildcard EXISTS over an empty container finds nothing.
        assert_case(&Predicate::Exists { path: p("$.arr[*]") }, doc, false, flags(false, false));
    }

    #[test]
    fn between_semantics() {
        let between = |lo, hi| Predicate::Between { path: p("$.n"), lo, hi };
        // Inclusive on both ends (SQL, PartiQL, DynamoDB agree).
        assert_case(
            &between(Constant::I64(5), Constant::I64(10)),
            r#"{"n":5}"#,
            true,
            flags(false, false),
        );
        assert_case(
            &between(Constant::I64(1), Constant::I64(5)),
            r#"{"n":5}"#,
            true,
            flags(false, false),
        );
        assert_case(
            &between(Constant::I64(6), Constant::I64(10)),
            r#"{"n":5}"#,
            false,
            flags(false, false),
        );
        // Reversed bounds are valid and unsatisfiable — no flag, no
        // error (an empty range is a value, D3).
        assert_case(
            &between(Constant::I64(10), Constant::I64(1)),
            r#"{"n":5}"#,
            false,
            flags(false, false),
        );
        // Numeric family mixes freely across the pair.
        assert_case(
            &between(Constant::I64(1), Constant::F64(9.5)),
            r#"{"n":5}"#,
            true,
            flags(false, false),
        );
        // A value outside the bounds' family is the flag cell.
        assert_case(
            &between(Constant::I64(1), Constant::I64(9)),
            r#"{"n":"x"}"#,
            false,
            flags(false, true),
        );
        // Utf8 family orders by raw bytes.
        let s = Predicate::Between {
            path: p("$.s"),
            lo: Constant::Utf8("a".into()),
            hi: Constant::Utf8("z".into()),
        };
        assert_case(&s, r#"{"s":"m"}"#, true, flags(false, false));
    }

    #[test]
    fn begins_with_semantics() {
        let begins = |prefix: &str| Predicate::BeginsWith { path: p("$.s"), prefix: prefix.into() };
        assert_case(&begins("ab"), r#"{"s":"abc"}"#, true, flags(false, false));
        // The empty prefix matches every string (D2.4).
        assert_case(&begins(""), r#"{"s":""}"#, true, flags(false, false));
        assert_case(&begins("abc"), r#"{"s":"abc"}"#, true, flags(false, false));
        assert_case(&begins("abcd"), r#"{"s":"abc"}"#, false, flags(false, false));
        assert_case(&begins("b"), r#"{"s":"abc"}"#, false, flags(false, false));
        // Byte-prefix ≡ code-point prefix on utf8.
        assert_case(&begins("hé"), r#"{"s":"héllo"}"#, true, flags(false, false));
        // Non-string value: the value-class flag cell.
        assert_case(&begins("1"), r#"{"s":1}"#, false, flags(false, true));
    }

    #[test]
    fn in_semantics() {
        let members = vec![Constant::I64(1), Constant::F64(10.0)];
        let in_list = Predicate::In { path: p("$.n"), members };
        // Membership is EQ under D4, numeric coercion included.
        assert_case(&in_list, r#"{"n":10}"#, true, flags(false, false));
        assert_case(&in_list, r#"{"n":2}"#, false, flags(false, false));
        // A value outside the members' family: classified once, flagged.
        assert_case(&in_list, r#"{"n":"10"}"#, false, flags(false, true));
        let strings = Predicate::In {
            path: p("$.s"),
            members: vec![Constant::Utf8("a".into()), Constant::Utf8("b".into())],
        };
        assert_case(&strings, r#"{"s":"b"}"#, true, flags(false, false));
        assert_case(&strings, r#"{"s":true}"#, false, flags(false, true));
    }

    // -- Fuel (D6) ----------------------------------------------------

    /// Hand-derived fuel goldens: op decodes (skipped included) + nodes
    /// visited + IN members tested. A change here is a fuel-schedule
    /// event (successor-ADR territory), not a refactor.
    #[test]
    fn fuel_accounting_goldens() {
        // EXISTS $.a: 1 op + 1 node (early break after the match).
        let outcome = eval_on(&Predicate::Exists { path: p("$.a") }, r#"{"a":1}"#);
        assert_eq!(outcome.fuel_used, 2);
        // AND(GE, BEGINS_WITH), both evaluated: 3 ops + 2 nodes.
        let both = Predicate::And(vec![
            cmp(CmpOp::Ge, "$.price", Constant::I64(10)),
            Predicate::BeginsWith { path: p("$.name"), prefix: "ab".into() },
        ]);
        let outcome = eval_on(&both, r#"{"price":20,"name":"about"}"#);
        assert!(outcome.verdict);
        assert_eq!(outcome.fuel_used, 5);
        // Short-circuit: the skipped sibling still pays its op decode
        // (1 unit) but none of its path work.
        let skipped = Predicate::And(vec![
            cmp(CmpOp::Lt, "$.price", Constant::I64(0)),
            Predicate::BeginsWith { path: p("$.name"), prefix: "ab".into() },
        ]);
        let outcome = eval_on(&skipped, r#"{"price":20,"name":"about"}"#);
        assert!(!outcome.verdict);
        assert_eq!(outcome.fuel_used, 4);
        // IN: one unit per member tested — miss tests all three…
        let miss = Predicate::In {
            path: p("$.n"),
            members: vec![Constant::I64(1), Constant::I64(2), Constant::I64(3)],
        };
        assert_eq!(eval_on(&miss, r#"{"n":5}"#).fuel_used, 5);
        // …a first-member hit tests one.
        let hit =
            Predicate::In { path: p("$.n"), members: vec![Constant::I64(5), Constant::I64(9)] };
        assert_eq!(eval_on(&hit, r#"{"n":5}"#).fuel_used, 3);
    }

    /// The S08 pitfall pinned: fuel counts path-resolution nodes, so a
    /// wide `[*]` cannot do unbounded work under a small budget.
    #[test]
    fn wide_wildcard_work_is_budgeted() {
        let elements: Vec<String> = (0..100).map(|i| i.to_string()).collect();
        let json = format!(r#"{{"items":[{}]}}"#, elements.join(","));
        let wide = cmp(CmpOp::Eq, "$.items[*]", Constant::I64(-1));
        let vm = vm_of(&wide);
        let bytes = JsonParser::new().parse(json.as_bytes()).expect("fixture parses");
        let tape = TapeDoc::from_bytes(&bytes).expect("valid idoc");
        let root = DocValue::from(tape.root());
        let outcome = vm.eval(root, u64::MAX).expect("completes");
        assert!(!outcome.verdict);
        assert!(outcome.fuel_used > 100, "node visits dominate: {}", outcome.fuel_used);
        assert_eq!(vm.eval(root, 10), Err(PredicateEvalError::FuelExhausted));
    }

    /// Exhaustion is exact and monotone: the recorded consumption is
    /// precisely the budget that succeeds, one less fails, zero fails.
    #[test]
    fn fuel_exhaustion_boundary_is_exact() {
        let predicate = Predicate::And(vec![
            cmp(CmpOp::Ge, "$.price", Constant::I64(10)),
            Predicate::In {
                path: p("$.tag"),
                members: vec![Constant::Utf8("a".into()), Constant::Utf8("b".into())],
            },
        ]);
        let vm = vm_of(&predicate);
        let bytes = JsonParser::new().parse(br#"{"price":20,"tag":"b"}"#).expect("fixture parses");
        let tape = TapeDoc::from_bytes(&bytes).expect("valid idoc");
        let root = DocValue::from(tape.root());
        let unbounded = vm.eval(root, u64::MAX).expect("completes");
        assert_eq!(vm.eval(root, unbounded.fuel_used), Ok(unbounded));
        assert_eq!(vm.eval(root, unbounded.fuel_used - 1), Err(PredicateEvalError::FuelExhausted));
        assert_eq!(vm.eval(root, 0), Err(PredicateEvalError::FuelExhausted));
    }

    // -- Structure ----------------------------------------------------

    #[test]
    fn connective_stack_boundary_and_or_skip() {
        // 31 NOT frames put the leaf at depth 32 — the encoder/validator
        // maximum evaluates on the fixed stack.
        let mut tree = Predicate::Exists { path: p("$.a") };
        for _ in 0..31 {
            tree = Predicate::Not(Box::new(tree));
        }
        let outcome = eval_on(&tree, r#"{"a":1}"#);
        assert!(!outcome.verdict, "31 negations flip true to false");
        // OR settles on its middle operand and skips the tail.
        let or = Predicate::Or(vec![
            cmp(CmpOp::Eq, "$.a", Constant::I64(0)),
            cmp(CmpOp::Eq, "$.a", Constant::I64(1)),
            cmp(CmpOp::Eq, "$.missing", Constant::I64(1)),
        ]);
        assert_case(&or, r#"{"a":1}"#, true, flags(false, false));
        // Nested: AND(OR(false, true), NOT(false)) — both settle true.
        let nested = Predicate::And(vec![
            Predicate::Or(vec![
                cmp(CmpOp::Eq, "$.a", Constant::I64(0)),
                cmp(CmpOp::Eq, "$.a", Constant::I64(1)),
            ]),
            Predicate::Not(Box::new(cmp(CmpOp::Eq, "$.a", Constant::I64(2)))),
        ]);
        assert_case(&nested, r#"{"a":1}"#, true, flags(false, false));
    }

    /// A member chain past the walk's hot frame capacity spills to the
    /// heap and stays correct (the zero-allocation gate covers the hot
    /// shapes; this covers the cold one).
    #[test]
    fn deep_member_chain_spills_correctly() {
        let depth = 40;
        let mut json = String::from(r#"{"leaf":7}"#);
        let mut text = String::from("$");
        for _ in 0..depth {
            json = format!(r#"{{"a":{json}}}"#);
            text.push_str(".a");
        }
        text.push_str(".leaf");
        let predicate =
            Predicate::Cmp { op: CmpOp::Eq, path: p(&text), constant: Constant::I64(7) };
        assert_case(&predicate, &json, true, flags(false, false));
    }

    /// Determinism (L7): verdict, flags, and fuel agree across runs.
    #[test]
    fn evaluation_is_deterministic() {
        let predicate = Predicate::And(vec![
            cmp(CmpOp::Gt, "$.items[*]", Constant::F64(2.5)),
            Predicate::Not(Box::new(Predicate::Exists { path: p("$.gone") })),
        ]);
        let vm = vm_of(&predicate);
        let bytes = JsonParser::new().parse(br#"{"items":[1,2,3]}"#).expect("fixture parses");
        let tape = TapeDoc::from_bytes(&bytes).expect("valid idoc");
        let root = DocValue::from(tape.root());
        let first = vm.eval(root, u64::MAX).expect("completes");
        let second = vm.eval(root, u64::MAX).expect("completes");
        assert_eq!(first, second);
        assert!(first.verdict);
    }
}
