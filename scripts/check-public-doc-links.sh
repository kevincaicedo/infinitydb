#!/usr/bin/env bash
# The public documentation is self-contained: no published document in this
# repository may link outside it, link to a file that is not published with
# it, or point into the planning documents that are not published with it
# (ADRs, milestone plans, design records, reviews, tickets, the master plan,
# the claim ledger's source — the site publishes a snapshot of the ledger).
# A reader of the public repository cannot follow such a pointer, so it is a
# defect, not a citation. Bare decision identifiers ("ADR-0087 D2") are
# text, not pointers, and stay legal in the engineering references; the
# copy a newcomer reads first carries none. Review, finding and batch
# identifiers are history, not rules: no published document carries one.
#
# This header is the one list of the gate's spellings (L13). The decision
# record (ADR-0166) states the rules and their scope, docs/INFINITY_STYLE.md
# states the rule in a few sentences, and the self-test points here.
#
# Scope (ADR-0106 house style): every file git tracks plus every untracked,
# unignored one — a new document is checked before its first commit — in
# three declared classes: Markdown (`*.md`), HTML (`*.html`, the website)
# and the published tables (`docs/**/*.tsv`, `docs/**/*.toml`, this gate's
# own table included). Scope failures are red, never a skip: a root that is
# not its own git work tree, a shallow clone ((e) reads the whole history),
# a declared class with no file, and Markdown without a single relative
# link. The OK line names what was scanned, per class and per tier, and
# what was exempt.
#
# Tiers (TIERS, closed): every scanned document is in exactly one tier; a
# document in none, or in two, is red.
#   first-read  — README, CONTRIBUTING, the pull-request template, the docs
#                 index docs/README.md, docs/ARCHITECTURE.md,
#                 docs/roadmap.md, crate and binary READMEs, website/;
#   engineering — INFINITY_STYLE.md, the interface documents, validation.md,
#                 deployment.md, the compatibility matrix, the json-*
#                 references, the JSONPath and PartiQL subsets, the tables
#                 under docs/ and the SAFETY.md files;
#   evidence    — EVIDENCE_RECORDS: the published claim-ledger snapshot, a
#                 website file judged by the engineering tier's rules: it
#                 keeps its decision and story identifiers, and carries no
#                 review identifier. A carve-out pending the owner's ruling
#                 (ADR-0166 D2) with an owner and an expiry: red once it
#                 expires, and judged as first-read copy from then on; red
#                 too while it waives no identifier (stale).
#
# Checks. Each refuses the spellings listed here and only those; any other
# spelling passes the gate, and review holds it.
#   (a) links: every relative target — Markdown `[t](x)` and `[t]: x`
#       outside fenced and inline code, and `href` / `src`, quoted or not —
#       resolves to a published path: a file git tracks (or an unignored
#       new one), or a directory holding one. A file git ignores (run output
#       included) is not published and is red. The base is the renderer's.
#       The site serves the HTML pages under website/site: their relative
#       and root-absolute targets resolve inside website/site (the deployed
#       site's root), and a target that leaves it is red. The repository
#       renders every other file, Markdown under website/site included: its
#       root-absolute targets resolve from the repository root. A
#       code-host file URL into this repository's own remote (origin in a
#       CI checkout) — a file, directory (tree), raw, edit, blame or history
#       (commits) view, or the github.dev / vscode.dev editor's view — is
#       the same link written absolute: its path resolves the same way,
#       anywhere on a line;
#   (b) paths: every planning-path token — `docs/adr/`, `docs/milestones/`,
#       `docs/drr/`, `reviews/`, `tickets/20…`, `../../docs/…`,
#       `infinity-master-plan`, `claim-ledger.md` — anywhere on a line,
#       code included, and inside a code-host file URL (GitHub and its
#       web editors, GitLab, Bitbucket, Gitea), names an existing path of
#       the repository
#       (`docs/milestones/m0-gates.toml` does; a milestone plan does not);
#   (c) prose: no phrase names a planning document — "master plan",
#       "milestone(s) plan", "the M3 plan", "plan AC", "design (review)
#       record", "DRR", "the plan's", "in the plan", "per the plan", "the
#       plan requires/says/…", "internal plan", "review ledger",
#       "planning/governance/parent/outer/private repository/checkout/docs"
#       — in published text (see "Text" below: the attribute values a
#       reader sees included) and across a line break. Any other phrase
#       ("see the plan", "the S37 plan row", "the M4.5 milestone document")
#       passes, and review holds it;
#   (d) sections: a numeric `§N` names a numbered heading of the document
#       the line names before it (a `.md` path), else of the file itself —
#       a section of an unpublished plan resolves to nothing; after `RFC n`
#       or an ADR identifier it is that source's section and is not judged;
#   (e) removed documents: no name of a document git history (or the
#       working tree) deleted — `name.md`, or its hyphenated stem bare, or
#       the file an own-repository URL names — unless a file of that name
#       still exists;
#   (f) the parent's repository: when this work tree sits inside a parent
#       work tree, no URL names a repository the parent's remotes name;
#   (g) identifiers: no published file carries a review identifier —
#       a finding (F-L12-05, FCR-DOC-01, "review finding N4", "the F2
#       finding"), a review by date, item, commit, milestone or story
#       ("review 2026-08-30", "owner review, 2026-09-22", "the 2026-08-30
#       review", "(review C9)" — an item only before ")" or ",", so "review
#       L2 misses" is prose —, "the review of `2cb6074`", "the review of
#       commit 2cb6074", "the M0 review", "the S37 review", "the M4-S27
#       review", "the E4.7 review", "full-codebase review") or a
#       remediation batch ("remediation batch 37", "batch 43 amendment",
#       "the batch-12 row", "(batch 14)", "(batch 34, …", "(2026-09-15,
#       batch 66)", "…, batch 66)", "since batch 21", "in batch 35",
#       "before batch 12", "until batch 15", "batches 18-23", "batches
#       57/58", "batch 14 of the 2026-08-30 …"; "batch 32 per tick" and "a
#       batch-64 pipeline" are batch sizes, not judged), on its line or
#       across a line break like (c); a document states the
#       rule the review produced instead. A story's finding ("the S27
#       finding") is a story identifier, not a review's. A bare label in
#       parentheses ("(N5)") is not judged: it shares its spelling with
#       law, decision and claim citations ("(L10)", "(D16)", "(C7)"). The
#       first-read tier also carries no decision or story identifier: ADR
#       numbers (ADR-0087, ADR 0087, adr-0087), story and epic IDs (M4-S27,
#       S19, E4.7, ARCH-W0.2, W0.3b; not "S3-compatible" or "an S3
#       bucket"), dot milestones (M4.5).
# Text. (b), (e) and (f) read the raw line. (c), (d) and (g) read the line
# after these steps, and only these: entities decoded (&#8209;, &nbsp;,
# &sect;); a comment closed on its line removed without a space, its body
# kept as text (an HTML comment ships with the page), so ADR-<!-- -->0087
# reads ADR-0087; an inline tag (a, b, code, em, span, sub, …) removed
# without a space, so ADR-<b>0087</b> reads ADR-0087; in HTML every other
# tag removed; the attribute values a reader sees added (content, alt,
# title, label, placeholder, aria-*, data-*; quoted or not); in Markdown,
# outside inline code (the scanner's code spans, below; a fence has none), a
# backslash escape read as its character, `*` runs and word-edge `_` runs
# (emphasis) removed, and a link `[t](x)` read as "t (x)", `[t][r]` as "t";
# U+00A0 and every run of whitespace read as one space, and U+2010-U+2015 as
# "-". Any other markup is judged as written.
#
# Deviations. Each has an owner (an @handle) and an expiry no later than
# MAX_DEVIATION_DAYS after today — renewal is a dated edit — and is red
# once it expires or goes stale.
#   marker: `doc-link-allow(<checks>) <@owner> <YYYY-MM-DD>: <why>` on the
#     line, <checks> one or more of c, d, e (MARKER_CHECKS), comma-separated:
#     quoted text, such as an engine message quoted verbatim, may carry a
#     phrase, a section number or a removed name it cannot restate. Quoted
#     text is mechanical: the text inside <pre> or <code> (open across
#     lines), an inline code span, or a Markdown fence. One scanner
#     (markup_scan) reads a line's markup left to right, as a renderer does,
#     and is the only reader of inline code: markers, (a)'s links, quoted text
#     and "Text" all take its code spans (a marker's or a row's text, judged
#     by itself, is read with nothing open). A <pre> or <code> tag, and in
#     Markdown a code span, is markup only outside an HTML comment, another
#     tag (its attribute values, a quoted one holding ">" included) and the
#     raw text of <script>, <style>, <textarea> and <title>; a tag also
#     outside inline code. In Markdown neither is behind a backslash escape;
#     a code span is a backtick run and the next run of its length on the
#     line, each run whole; a tag, opening or closing, is whole on its line
#     and well-formed as CommonMark reads an opening tag (a name of letters,
#     digits and "-"; attributes each a name or name=value, an unquoted value
#     holding no space, quote, "=", "<", ">" or backtick). A comment and raw
#     text stay open across lines, and in HTML a tag does too. <code-x> is
#     another tag; a table has no markup. Any other spelling of the tag, and
#     any other backtick, is text and opens nothing. A code span is read on
#     its line only: one a line break splits is no code span, and on its
#     second line the closing run pairs with the next run there, so the text
#     between reads as inline code (open: ADR-0166 D3). A marker waives hits
#     in its line's quoted text only; the rest of the line is judged with no
#     waiver. A marker never waives (a), (b), (f) or (g), in any tier: a dead
#     link is fixed, and paths, the parent's repositories and identifiers are
#     removed, not waived. It waives only its checks, only on its own line. A
#     line carries at most one marker: a second is red. Every marker, from
#     `doc-link-allow` to its comment's end (or the line's), is no part of the
#     line it waives on, so it cannot satisfy its own waiver; each is parsed,
#     printed and judged by itself, with no waiver. One spelled inside inline
#     code is text about markers, not a marker. A named check that does not
#     fire on the line's quoted text makes the marker stale. A hit that
#     straddles the edge of the quoted text is whole in neither part and is
#     judged by neither: on a marker line whose named check fires nowhere else
#     the marker is stale, and otherwise review holds it.
#   row: a line another source owns — a generated file, or a table column a
#     gate matches verbatim against code — cannot carry a marker, so it is a
#     row of docs/doc-link-generated.tsv: the file, the text the line
#     contains, the owning source, the owner and the expiry. The owning
#     source's first token is a published path outside the scanned classes
#     (code or a script, never a document or a table). The row's file is an
#     engineering reference that is a table under docs/ or carries a
#     GENERATED banner ("GENERATED — do not edit") in its first five lines.
#     The row exempts only its text, and only from (b)-(g); the rest of the
#     line is judged. Red: a row on any other file or on the table itself; a
#     row matching no line, or more than one; a row whose text, judged by
#     itself, fires none of (b)-(g) (stale). In the table only the text
#     column of a live row is exempt; its comments and other columns are
#     judged.
# INF_CHECK_TODAY=YYYY-MM-DD replaces the clock (the self-test pins it).
set -euo pipefail
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"

python3 - <<'PY'
import datetime
import fnmatch
import html
import os
import re
import subprocess
import sys
from pathlib import Path, PurePosixPath
from urllib.parse import unquote

root = Path.cwd().resolve()
errors = []


def scope_fail(message):
    print(f"public-doc-links SCOPE: {message}")
    print("public-doc-links FAILED: scope not established")
    sys.exit(1)


def git(*args, cwd=root):
    return subprocess.run(
        ["git", "-C", str(cwd), *args], check=True, capture_output=True,
    ).stdout.decode(errors="replace")


def toplevel(path):
    try:
        return Path(git("rev-parse", "--show-toplevel", cwd=path).strip()).resolve()
    except (OSError, subprocess.CalledProcessError):
        return None


top = toplevel(root)
if top is None:
    scope_fail(f"{root} is not a git work tree; the file set is git's")
if top != root:
    scope_fail(f"{root} is inside the work tree {top}, not its root")
if git("rev-parse", "--is-shallow-repository").strip() == "true":
    scope_fail(f"{root} is a shallow clone — (e) reads the whole history (fetch-depth: 0)")

EXPIRY = re.compile(r"^\d{4}-\d{2}-\d{2}$")
clock = os.environ.get("INF_CHECK_TODAY", "")
if clock and not EXPIRY.match(clock):
    scope_fail(f"INF_CHECK_TODAY={clock!r} is not YYYY-MM-DD")
TODAY = (
    datetime.date.fromisoformat(clock) if clock
    else datetime.datetime.now(datetime.timezone.utc).date()
)
MAX_DEVIATION_DAYS = 30  # a marker's or row's expiry is at most this far out
HORIZON = TODAY + datetime.timedelta(days=MAX_DEVIATION_DAYS)

TABLE = "docs/doc-link-generated.tsv"
SITE = "website/site"  # the deployed site's root
CLASSES = {
    "md": [":(icase)*.md"],
    "html": [":(icase)*.html"],
    "table": [":(glob)docs/**/*.tsv", ":(glob)docs/**/*.toml"],
}
CLASS_NAMES = {"md": "Markdown", "html": "HTML", "table": "table"}
# The published set: tracked files plus unignored new ones. A path outside
# it — ignored run output, a local note — does not exist on the published
# repository, so a link to it is a dead link there.
present = {
    p for p in git("ls-files", "-z", "--cached", "--others", "--exclude-standard").split("\0")
    if p and (root / p).is_file()
}
present_dirs = {str(parent) for p in present for parent in PurePosixPath(p).parents}
files = {}
for kind, specs in CLASSES.items():
    listing = git("ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", *specs)
    for name in listing.split("\0"):
        if name in present:
            files.setdefault(name, kind)
counts = {kind: sum(1 for k in files.values() if k == kind) for kind in CLASSES}
if not counts["md"]:
    scope_fail("no Markdown files found — the scan would check nothing")

# Tiers: closed. fnmatch's `*` crosses `/`.
TIERS = {
    "first-read": [
        "README.md", "CONTRIBUTING.md", ".github/PULL_REQUEST_TEMPLATE.md", "docs/README.md",
        "docs/ARCHITECTURE.md", "docs/roadmap.md", "crates/*/README.md", "bins/*/README.md",
        "website/*",
    ],
    "engineering": [
        "docs/INFINITY_STYLE.md", "docs/interfaces-m*.md", "docs/validation.md",
        "docs/deployment.md", "docs/compat-matrix.md", "docs/json-*.md",
        "docs/jsonpath-subset.md", "docs/partiql-subset.md", "docs/*.tsv", "docs/*.toml",
        "*/SAFETY.md",
    ],
}
# The evidence record: a website file judged by the engineering tier's rules
# (ADR-0166 D2, Proposed): file -> (owner, expiry). Once expired it is judged
# by its directory's tier.
EVIDENCE_RECORDS = {"website/site/_ledger-snapshot.md": ("@kevincaicedo", "2026-10-14")}
evidence_live = {}  # record -> the carve-out is live (set below, before the scan)
evidence_waived = {record: 0 for record in EVIDENCE_RECORDS}  # first-read ids kept


def tiers_of(name):
    if evidence_live.get(name):
        return ["evidence"]
    return [t for t, globs in TIERS.items() if any(fnmatch.fnmatchcase(name, g) for g in globs)]


OUTPUT_DIRS = {".artifacts", "artifacts", "target"}
FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})(.*)$")
# An inline code span: a backtick run and the next run of the same length on
# the line, each run whole (never part of a longer one). Only markup_scan reads it.
INLINE_CODE = re.compile(r"(?P<ticks>`+)(?!`).+?(?<!`)(?P=ticks)(?!`)")
MD_ESCAPE = r"\\([!-/:-@\[-`{-~])"  # a Markdown backslash escape: the character it keeps
MD_LINKS = [
    re.compile(r"\]\(\s*(?:<([^>]+)>|([^\s)]+))"),
    re.compile(r"^ {0,3}\[[^\]]+\]:\s*(?:<([^>]+)>|(\S+))"),
]
HTML_LINKS = [re.compile(r"""\b(?:href|src)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'<>`=]+))""")]
# The attribute values a reader sees (a tooltip, alt text, a label), quoted or not.
ATTR_TEXT = re.compile(
    r"""(?<![\w-])(?:content|alt|title|label|placeholder|aria-[\w-]+|data-[\w-]+)"""
    r"""\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'<>`=]+))""",
    re.I,
)
# An inline tag renders no space: ADR-<b>0087</b> reads ADR-0087.
INLINE_TAG = re.compile(
    r"</?(?:a|abbr|b|bdi|bdo|cite|code|data|del|dfn|em|i|ins|kbd|mark|q|s|samp|small|span"
    r"|strong|sub|sup|time|u|var)(?:\s[^<>]*)?/?>",
    re.I,
)
SCHEME = re.compile(r"(?:[a-zA-Z][a-zA-Z0-9+.-]*:|//)")
URL = re.compile(r"""[a-zA-Z][a-zA-Z0-9+.-]*://[^\s"'<>)\]`]+""")
# A file inside a hosted repository: the path after the ref is a repository
# path. GitHub (blob/tree/raw/edit/blame/commits/history and
# raw.githubusercontent), its web editors (github.dev `/blob|tree/`,
# vscode.dev `/github/…/blob|tree/`), GitLab
# (`/-/blob|tree|raw|blame|commits/`), Bitbucket (`/src/<ref>/`), Gitea and
# Codeberg (`/src|commits/branch|commit|tag/<ref>/`).
CODE_HOST_FILE = re.compile(
    r"^https?://(?:www\.)?(?:"
    r"github\.com/[^/]+/[^/]+/(?:blob|tree|raw|edit|blame|commits|history)/[^/]+"
    r"|github\.dev/[^/]+/[^/]+/(?:blob|tree)/[^/]+"
    r"|vscode\.dev/github/[^/]+/[^/]+/(?:blob|tree)/[^/]+"
    r"|raw\.githubusercontent\.com/[^/]+/[^/]+/[^/]+"
    r"|[^/]+/(?:[^/]+/)+-/(?:blob|tree|raw|blame|commits)/[^/]+"
    r"|bitbucket\.org/[^/]+/[^/]+/src/[^/]+"
    r"|[^/]+/[^/]+/[^/]+/(?:src|commits)/(?:branch|commit|tag)/[^/]+"
    r")/(\S+)$"
)
TAG = re.compile(r"<[^>]+>")
COMMENT_MARK = re.compile(r"<!--|-->")  # a comment's body is published text
INLINE_COMMENT = re.compile(r"<!--(.*?)-->")  # closed on its line: renders no space
# Markdown inline syntax, outside inline code: a backslash escape (kept as its
# character), a `*` run, a word-edge `_` run (emphasis, removed).
MD_INLINE = re.compile(MD_ESCAPE + r"|\*+|(?<![\w/.])_+(?=\w)|(?<=\w)_+(?![\w/.-])")
MD_INLINE_LINK = re.compile(r"!?\[([^\[\]]*)\]\(")  # [t](x) reads "t (x)"
MD_REF_LINK = re.compile(r"!?\[([^\[\]]*)\]\[[^\[\]]*\]")  # [t][r] reads "t"
# Quoted text: inside <pre> or <code>, open across lines, or an inline code
# span. A <pre>/<code> tag and a code span count only where they are markup
# (the header's marker rule): markup_scan reads a line's comments, tags, escapes
# and backtick runs left to right, as a renderer does, and is the one reader of
# inline code — for markers, (a), quoted text and the published text alike.
TAG_START = r"<(?P<close>/?)(?P<name>[A-Za-z][\w:.-]*)"
MARKUP = {  # per class, the constructs a line's scan starts; a table has no markup
    "md": re.compile(
        rf"(?P<comment><!--)|{TAG_START}|(?P<escape>{MD_ESCAPE})|(?P<span>{INLINE_CODE.pattern})"
        r"|`+"  # a backtick run no run on the line closes: text, skipped whole
    ),
    "html": re.compile(rf"(?P<comment><!--)|{TAG_START}"),
}
# A Markdown tag, opening or closing, is whole on its line and well-formed as
# CommonMark reads an opening tag: a name of letters, digits and "-";
# attributes, each a name or name=value, the value quoted or unquoted (no
# space, quote, "=", "<", ">" or backtick); "/"? and ">". Else its "<" is text.
MD_ATTR = r"""\s+[A-Za-z_:][\w.:-]*(?:\s*=\s*(?:[^\s"'=<>`]+|"[^"]*"|'[^']*'))?"""
MD_TAG = re.compile(rf"</?[A-Za-z][A-Za-z0-9-]*(?:{MD_ATTR})*\s*/?>", re.A)
TAG_REST = re.compile(r"""[^"'>]*""")  # a tag's rest, to a quoted value or its ">"
CODE_TAGS = {"pre", "code"}
RAW_TEXT = {  # an element whose content is text, up to its end tag
    name: re.compile(rf"</{name}(?=[\s/>]|$)", re.I)
    for name in ("script", "style", "textarea", "title")
}
DASHES = {ord(c): "-" for c in "‐‑‒–—―"}
PLANNING = re.compile(
    r"(?<![\w.#-])"
    r"((?:\.\./)*(?:[\w.-]+/)*"
    r"(?:docs/(?:adr|milestones|drr)/|reviews/|tickets/20|\.\./docs/)"
    r"[\w./#-]*"
    r"|[\w./-]*(?:infinity-master-plan[\w./#-]*|claim-ledger\.md))"
)
PROSE = re.compile(
    r"(?i)\b(master[ -]plans?|milestones? plans?|design (?:review )?records?|DRRs?"
    r"|the plan's|in the plan|per the (?:M\d+(?:\.\d+)? )?plans?"
    r"|the plan (?:requires|says|states|owns|records|decides|names)"
    r"|M\d+(?:\.\d+)? plans?|plan ACs?|internal plans?|review ledgers?"
    r"|(?:planning|governance|parent|outer|private)"
    r" (?:repo|repos|repository|repositories|checkouts?|docs|documents|records))\b"
)
SECTION = re.compile(r"§\s?(\d+[a-z]?(?:\.\d+)*[a-z]?)")
SECTION_SOURCE = re.compile(r"((?:[\w.-]+/)*[\w.-]+\.md)\b|\bRFC\s?\d+|\bADR-?\d{4}")
MD_HEADING = re.compile(r"^ {0,3}#{1,6}\s+(?:§\s*)?(\d+[a-z]?(?:\.\d+)*[a-z]?)(?=[.):\s]|$)")
HTML_HEADING = re.compile(r"<h[1-6][^>]*>\s*(?:§\s*)?(\d+[a-z]?(?:\.\d+)*[a-z]?)(?=[.):\s<])")
MD_NAME = re.compile(r"(?<![\w.-])((?:[\w.-]+/)*([\w.-]+\.md))\b")
IDENTIFIER = re.compile(
    r"\b((?i:ADR)[ -]?\d{1,4}"                        # a decision record, case-folded
    r"|M\d+(?:\.\d+)?-(?!M\d)[A-Z]{1,4}\d+[\w.]*"     # a story or epic: M4-S27, not M0-M3
    r"|M\d+\.\d+"                                     # a dot milestone: M4.5
    r"|(?!S3(?:-compatible| buckets?\b))S\d{1,2}[a-z]?"  # a bare story: S19
    r"|E\d+\.\d+"                                     # a bare epic: E4.7
    r"|(?:ARCH-)?W\d+\.\d+[a-z]?|ARCH-W\d+[a-z]?)\b"   # a wave: ARCH-W0.2, W0.3b
)
REVIEW_IDENTIFIER = re.compile(
    r"\b(F-L\d{2}-\d{2}|FCR-[A-Z0-9]+-\d+"                     # a review finding
    r"|(?:[Rr]eview )?[Ff]inding [A-Z]{1,3}\d{1,3}[a-z]?"         # "review finding N4"
    r"|(?!S\d)[A-Z]{1,2}\d{1,2}[a-z]? finding"                    # "the F2 finding", not S27's
    r"|(?:[Ff]ull-codebase )?[Rr]eview,? (?:of )?\d{4}-\d{2}-\d{2}"  # "review, 2026-09-22"
    r"|\d{4}-\d{2}-\d{2} review"                                # "the 2026-08-30 review"
    r"|[Rr]eview of (?:commit )?`?(?=[0-9a-f]*\d)[0-9a-f]{7,40}"  # a review by commit
    r"|M\d+(?:\.\d+)? review"                                     # "the M0 review"
    r"|(?:M\d+(?:\.\d+)?-)?(?:S\d{1,3}[a-z]?|E\d+(?:\.\d+)?) review"  # a story or epic review
    r"|(?:[Ff]ull-codebase )?[Rr]eview [A-Z]{1,2}\d{1,2}[a-z]?(?=[),])"  # an item: (review C9)
    r"|[Ff]ull-codebase review"                                   # the review itself
    r"|[Rr]emediation [Bb]atch(?:es)?[ -]\d{1,3}(?:-\d{1,3})?"    # a remediation batch
    r"|[Bb]atch(?:es)?[ -]\d{1,3}(?:-\d{1,3})? (?:amendments?|rows?|tails?)"
    r"|(?<=\()[Bb]atch(?:es)? \d{1,3}(?:[-/]\d{1,3})?"          # (batch 14), (batch 34, …
    r"|(?<=, )[Bb]atch(?:es)? \d{1,3}(?:[-/]\d{1,3})?(?=\))"      # …, batch 66)
    r"|\d{4}-\d{2}-\d{2}, [Bb]atch(?:es)? \d{1,3}"                # 2026-09-15, batch 66
    r"|(?:[Ss]ince|[Ii]n|[Bb]efore|[Uu]ntil) [Bb]atch(?:es)? \d{1,3}"  # since batch 21
    r"|[Bb]atches \d{1,3}[-/]\d{1,3}"                              # batches 18-23, 57/58
    r"|[Bb]atch \d{1,3} of the \d{4}-\d{2}-\d{2})\b"                 # batch 14 of the 2026-…
)
MARKER = re.compile(r"doc-link-allow\b")
ALLOW_FORM = re.compile(r"^\(([a-g](?:,[a-g])*)\)\s+(\S+)\s+(\S+):(.*)$")
OWNER = re.compile(r"^@[\w.-]+$")
# What a marker may waive: quoted text. Never (a), (b), (f) or (g).
MARKER_CHECKS = {"c", "d", "e"}
GENERATED_BANNER = re.compile(r"\bGENERATED\b.*\bdo not edit\b")
WHITESPACE = re.compile(r"\s+")


def inside(path, base=root):
    try:
        path.relative_to(base)
    except ValueError:
        return False
    return True


def resolve(source, target):
    # The resolved path and the root it must stay in, by renderer: the site
    # serves an HTML page under website/site, so its links resolve and stay
    # inside the site; the repository renders every other file.
    target = unquote(target.split("#", 1)[0].split("?", 1)[0])
    site = root / SITE
    base = site if source.suffix.lower() == ".html" and inside(source, site) else root
    if target.startswith("/"):
        return (base / target.lstrip("/")).resolve(), base
    return (source.parent / target).resolve(), base


def names_file(source, token):
    # `../x` and `./x` are file-relative only; a bare path may be written
    # from the repository root or from the file's directory.
    candidates = [(source.parent / token).resolve()]
    if not token.startswith(("./", "../")):
        candidates.append((root / token).resolve())
    return next((c for c in candidates if inside(c) and c.exists()), None)


def deviation_live(where, what, fix, owner, expiry_text):
    # A deviation's expiry: a date, not past, and within the horizon.
    try:
        expiry = datetime.date.fromisoformat(expiry_text) if EXPIRY.match(expiry_text) else None
    except ValueError:
        expiry = None
    if expiry is None:
        errors.append(f"DOC LINK: {where}: expiry {expiry_text!r} is not YYYY-MM-DD")
        return False
    if expiry < TODAY:
        errors.append(
            f"DOC LINK: {where}: {what} expired on {expiry} — {owner} fixes {fix} "
            "or renews it with a reason"
        )
        return False
    if expiry > HORIZON:
        errors.append(
            f"DOC LINK: {where}: {what} expires {expiry}, past the {MAX_DEVIATION_DAYS}-day "
            f"horizon ({HORIZON}) — renewal is a dated edit, not a far date"
        )
        return False
    return True


# The evidence-record carve-out is a deviation too: an owner and an expiry.
# Once expired, the record is judged by its directory's tier.
for record, (owner, expiry) in EVIDENCE_RECORDS.items():
    if record not in files:
        errors.append(f"DOC LINK: the evidence record {record} names no scanned file")
    if not OWNER.match(owner):
        errors.append(f"DOC LINK: {record}: the carve-out's owner {owner!r} is no @handle")
    evidence_live[record] = deviation_live(
        record, "the evidence-record carve-out", "the snapshot's tier", owner, expiry
    )

# Owned-line rows: file, text the line contains, owning source, owner, expiry.
generated = []
live_rows = set()
table_text = {}  # the table's line number -> its live row
if (root / TABLE).is_file():
    for number, row in enumerate((root / TABLE).read_text().splitlines(), 1):
        if not row.strip() or row.startswith("#"):
            continue
        where = f"{TABLE}:{number}"
        cells = row.split("\t")
        if len(cells) != 5 or not all(c.strip() for c in cells):
            errors.append(f"DOC LINK: {where}: a row is file, text, owning source, owner, expiry")
            continue
        cells = tuple(cells)
        target, source, owner = cells[0].strip(), cells[2].split()[0], cells[3].strip()
        live = deviation_live(where, f"the row for {target}", source, owner, cells[4].strip())
        if not OWNER.match(owner):
            errors.append(f"DOC LINK: {where}: the row's owner {owner!r} is no @handle")
            live = False
        if source not in present:
            errors.append(f"DOC LINK: {where}: owning source {source} is no published file")
            live = False
        elif source in files:
            errors.append(
                f"DOC LINK: {where}: owning source {source} is a scanned document — a row "
                "exempts only text code or a script owns; state the rule in that document"
            )
            live = False
        if target == TABLE or tiers_of(target) != ["engineering"]:
            errors.append(
                f"DOC LINK: {where}: {target} is not an engineering reference — a row exempts "
                "only text a generator or a gate owns; state the rule in the file instead"
            )
            live = False
        elif files.get(target) != "table" and not any(
            GENERATED_BANNER.search(text)
            for text in (root / target).read_text(errors="replace").splitlines()[:5]
        ):
            errors.append(
                f"DOC LINK: {where}: {target} is neither a docs table nor a generated file "
                "(no GENERATED banner in its first five lines) — state the rule in it instead"
            )
            live = False
        generated.append(cells)
        if live:
            live_rows.add(cells)
            table_text[number] = cells
generated_hits = {row: 0 for row in generated}

# (e) documents removed from the repository, by name.
removed = set()
try:
    removed.update(git("log", "--diff-filter=D", "--name-only", "--format=", "--", "*.md").split())
    removed.update(git("diff", "--name-only", "--diff-filter=D", "HEAD", "--", "*.md").split())
except subprocess.CalledProcessError:
    pass  # no commit yet: nothing was removed
removed = {Path(p).name for p in removed} - {Path(p).name for p in present}
# A hyphenated stem is distinctive enough to judge bare ("ops-tiered-storage");
# a one-word stem ("architecture") is ordinary prose.
REMOVED_STEMS = [
    re.compile(rf"(?<![\w./-]){re.escape(Path(n).stem)}(?![\w-]|\.md\b)", re.I)
    for n in sorted(removed) if "-" in Path(n).stem
]

def remote_repos(cwd):
    repos = set()
    for url in git("remote", "-v", cwd=cwd).split():
        match = re.search(r"[:/]([\w.-]+/[\w.-]+?)(?:\.git)?/?$", url)
        if match and "/" in url:
            repos.add(match[1].lower())
    return repos


# (a) this repository's own remotes: a code-host URL into one is a link.
own = remote_repos(root)
OWN_URL = [
    re.compile(rf"^https?://[^/]+/(?:[^/]+/)*{re.escape(repo)}/", re.I) for repo in sorted(own)
]

# (f) repositories the parent work tree's remotes name.
parent_repos = set()
parent_top = toplevel(root.parent)
if parent_top is not None:
    parent_repos = remote_repos(parent_top) - own

headings_cache = {}


def headings(path):
    if path not in headings_cache:
        pattern = HTML_HEADING if path.suffix == ".html" else MD_HEADING
        found = set()
        if path.suffix in (".md", ".html"):
            for text in path.read_text(errors="replace").splitlines():
                found.update(pattern.findall(text))
        headings_cache[path] = found
    return headings_cache[path]


counters = dict(links=0, tokens=0, sections=0, own_urls=0)
allowed = []
waived = {}  # the current line's marker: check -> hits it waived
probe = None  # while a text is judged by itself: the checks it fires


def flag(check, message):
    # A marker waives only the checks it names, on its own line.
    if probe is not None:
        probe.append(check)
    elif check in waived:
        waived[check] += 1
    else:
        errors.append(message)


def check_links(path, where, text, patterns):
    for pattern in patterns:
        for groups in pattern.findall(text):
            target = next((g for g in groups if g), "").strip()
            if not target or target.startswith("#") or SCHEME.match(target):
                continue
            counters["links"] += 1
            resolved, base = resolve(path, target)
            if not inside(resolved):
                flag("a", f"DOC LINK: {where}: link {target} leaves the repository")
                continue
            if not inside(resolved, base):
                flag("a", f"DOC LINK: {where}: link {target} leaves the site's root {SITE}")
                continue
            if not resolved.exists():
                flag("a", f"DOC LINK: {where}: link {target} names no file")
                continue
            rel = resolved.relative_to(root)
            if OUTPUT_DIRS.intersection(rel.parts):
                flag("a", f"DOC LINK: {where}: link {target} is ignored run output")
            elif resolved.is_dir():
                if rel.as_posix() not in present_dirs:
                    flag(
                        "a",
                        f"DOC LINK: {where}: link {target} names a directory with no "
                        "published file",
                    )
            elif rel.as_posix() not in present:
                flag(
                    "a",
                    f"DOC LINK: {where}: link {target} names a file git ignores — "
                    "it is not published",
                )


def check_own_url(where, url, path):
    # The URL is a link written absolute: it resolves from the repository
    # root against the published set, exactly like (a), and (e) judges the
    # file it names.
    counters["own_urls"] += 1
    target = unquote(path.split("#", 1)[0].split("?", 1)[0]).rstrip("/")
    resolved = (root / target).resolve()
    if PurePosixPath(target).name in removed:
        flag("a", f"DOC LINK: {where}: {url} names a document removed from the repository")
    if not inside(resolved):
        flag("a", f"DOC LINK: {where}: {url} leaves the repository")
        return
    rel = resolved.relative_to(root).as_posix()
    if OUTPUT_DIRS.intersection(PurePosixPath(rel).parts):
        flag("a", f"DOC LINK: {where}: {url} is ignored run output")
    elif rel not in present and rel not in present_dirs:
        flag(
            "a",
            f"DOC LINK: {where}: {url} names no published file of this repository "
            "(missing, or ignored by git)",
        )


def check_own_urls(where, line):
    for url in URL.findall(line):
        url = url.rstrip(".,;:")
        match = CODE_HOST_FILE.match(url)
        if match and any(own_url.match(url) for own_url in OWN_URL):
            check_own_url(where, url, match[1])


def check_urls(where, line):
    for url in URL.findall(line):
        url = url.rstrip(".,;:")
        lowered = url.lower()
        for repo in parent_repos:
            if re.search(rf"[/:]{re.escape(repo)}(?:\.git)?(?:[/#?]|$)", lowered):
                flag("f", f"DOC LINK: {where}: {url} names the parent's repository")
        match = CODE_HOST_FILE.match(url)
        if match:
            for token in PLANNING.findall(match[1]):
                counters["tokens"] += 1
                if not names_file(root / "README.md", token.split("#", 1)[0]):
                    flag(
                        "b",
                        f"DOC LINK: {where}: {url} points into planning documents "
                        "that are not part of this repository",
                    )


def check_prose(where, text, carried):
    # A phrase is judged on its line and across the break from the line
    # before it; a phrase wholly on the previous line was judged there.
    joined = f"{carried} {text}" if carried else text
    offset = len(carried) + 1 if carried else 0
    for match in PROSE.finditer(joined):
        if match.end() <= offset:
            continue
        flag(
            "c",
            f"DOC LINK: {where}: \"{' '.join(match[1].split())}\" names a planning document "
            "that is not part of this repository — state the rule instead",
        )


def check_review(tier, where, text, carried):
    # Judged like (c): on the line and across the break from the line before.
    # Every tier, the evidence record included.
    before, line = URL.sub("", carried), URL.sub("", text).translate(DASHES)
    joined = f"{before} {line}" if before else line
    offset = len(before) + 1 if before else 0
    for match in REVIEW_IDENTIFIER.finditer(joined):
        if match.end() <= offset:
            continue
        flag(
            "g",
            f"DOC LINK: {where}: {' '.join(match[1].split())} — a published document carries "
            "no review, finding or batch identifier; state the rule the review produced",
        )


def check_content(path, tier, where, raw, text):
    for token in PLANNING.findall(URL.sub("", raw)):
        token = token.rstrip(".,;:").split("#", 1)[0]
        counters["tokens"] += 1
        if not names_file(path, token):
            flag(
                "b",
                f"DOC LINK: {where}: {token} points into planning documents "
                "that are not part of this repository",
            )
    for match in SECTION.finditer(text):
        counters["sections"] += 1
        sources = list(SECTION_SOURCE.finditer(text, 0, match.start()))
        source = sources[-1] if sources else None
        if source is not None and source[1] is None:
            continue  # an RFC's or an ADR's own section
        document = path if source is None else names_file(path, source[1])
        if document is None or match[1] not in headings(document):
            named = "this file" if source is None else source[1]
            flag(
                "d",
                f"DOC LINK: {where}: §{match[1]} is no numbered section of {named} — "
                "a section of an unpublished plan; state the rule instead",
            )
    bare = URL.sub("", raw)
    for full, base in MD_NAME.findall(bare):
        if base in removed:
            flag("e", f"DOC LINK: {where}: {full} names a document removed from the repository")
    for stem in REMOVED_STEMS:
        for hit in stem.findall(bare):
            flag("e", f"DOC LINK: {where}: {hit} names a document removed from the repository")
    idents = IDENTIFIER.findall(URL.sub("", text).translate(DASHES))
    if tier == "evidence":
        evidence_waived[path.relative_to(root).as_posix()] += len(idents)
    elif tier == "first-read":
        for ident in idents:
            flag(
                "g",
                f"DOC LINK: {where}: {ident} — first-read copy carries no decision, story or "
                "review identifier; state the decision or name the big milestone",
            )


def marker_spans(line, code):
    # Every marker, from `doc-link-allow` to its comment's end (or the
    # line's): [(start, end), …]. One inside inline code (the line's code
    # spans, markup_scan's) is text about markers.
    spans = []
    for marker in MARKER.finditer(line):
        if any(start <= marker.start() < end for start, end in code):
            continue
        end = line.find("-->", marker.end())
        spans.append((marker.start(), len(line) if end < 0 else end))
    return spans


def blank(line, spans, fill=" "):
    # The line with each span (they may overlap) replaced by fill.
    out, at = [], 0
    for start, end in sorted(spans):
        if start >= at:
            out += [line[at:start], fill]
        at = max(at, end)
    return "".join(out + [line[at:]])


def runs(line, spans, code):
    # The line's spans joined by one space, and the code spans inside them
    # re-based onto the joined text.
    out, inner, at = [], [], 0
    for start, end in sorted(spans):
        inner += [(at + s - start, at + e - start) for s, e in code if start <= s and e <= end]
        out.append(line[start:end])
        at += end - start + 1
    return " ".join(out), inner


def markup_scan(kind, line, state):
    # The line's <pre>/<code> tags that are markup, as (closing, start, end),
    # its inline code spans, as (start, end), and what the line leaves open:
    # None (text), ("comment",), ("raw", name) or, in HTML, ("tag", name,
    # closing, quote): a tag, and the quote of the attribute value it is inside
    # ("" outside one). A tag carried in starts at 0.
    tags, code, at, start = [], [], 0, 0
    scan = MARKUP.get(kind)
    while scan is not None:
        if state is None:
            found = scan.search(line, at)
            if found is None:
                break
            at = found.end()
            if found["comment"]:
                state = ("comment",)
            elif found["name"] and (kind == "html" or MD_TAG.match(line, found.start())):
                state = ("tag", found["name"].lower(), bool(found["close"]), "")
                start = found.start()
            elif kind == "md" and found["span"]:
                code.append(found.span())
            # An escape or a backtick run no run closes is text, skipped whole.
        elif state[0] == "comment":
            end = line.find("-->", at)
            if end < 0:
                break
            state, at = None, end + 3
        elif state[0] == "raw":
            end = RAW_TEXT[state[1]].search(line, at)
            if end is None:
                break
            state, at = None, end.start()  # the end tag is read as a tag
        elif state[3]:  # inside a quoted attribute value
            end = line.find(state[3], at)
            if end < 0:
                break
            state, at = (*state[:3], ""), end + 1
        else:
            at = TAG_REST.match(line, at).end()
            if at == len(line):
                break
            if line[at] != ">":
                state, at = (*state[:3], line[at]), at + 1
                continue
            _, name, closing, _ = state
            state, at = None, at + 1
            if name in CODE_TAGS:
                tags.append((closing, start, at))
            elif name in RAW_TEXT and not closing and line[at - 2] != "/":
                state = ("raw", name)
    return tags, code, state


def quoted_spans(kind, line, depth, state, fenced_line):
    # The line's quoted text — inside <pre> or <code>, an inline code span or
    # a Markdown fence — as spans, and its inline code spans; after the line,
    # the <pre>/<code> depth and the markup it leaves open (markup_scan).
    if fenced_line:
        return [(0, len(line))], [], depth, state
    spans, opened = [], 0 if depth else None
    tags, code, state = markup_scan(kind, line, state)
    for closing, start, end in tags:
        if not closing:
            if not depth:
                opened = end
            depth += 1
        elif depth:
            depth -= 1
            if not depth:
                spans.append((opened, start))
    if depth:
        spans.append((opened, len(line)))
    spans += code
    merged = []  # an inline span inside <code> is one quoted run, not two
    for start, end in sorted(spans):
        if merged and start <= merged[-1][1]:
            merged[-1] = (merged[-1][0], max(merged[-1][1], end))
        else:
            merged.append((start, end))
    return merged, code, depth, state


def marker_waiver(where, marker):
    # `doc-link-allow(<checks>) <@owner> <YYYY-MM-DD>: <why>`; the checks it
    # waives on this line, or None.
    form = ALLOW_FORM.match(MARKER.sub("", marker, 1).strip())
    if not form or not OWNER.match(form[2]):
        errors.append(
            f"DOC LINK: {where}: a marker is doc-link-allow(<checks c-e>) <@owner> "
            "<YYYY-MM-DD>: <why>"
        )
        return None
    checks, owner, expiry, reason = form[1].split(","), form[2], form[3], form[4].strip()
    ok = deviation_live(where, "the doc-link-allow marker", "the line", owner, expiry)
    for check in sorted(set(checks) - MARKER_CHECKS):
        why = "fix the link" if check == "a" else "remove it; only quoted text is waived (c-e)"
        errors.append(f"DOC LINK: {where}: doc-link-allow never waives ({check}) — {why}")
        ok = False
    if not reason:
        errors.append(f"DOC LINK: {where}: doc-link-allow without a reason")
        ok = False
    if not ok:
        return None
    allowed.append(f"{where}: ({form[1]}) {owner}, expires {expiry}: {reason}")
    return checks


def markdown_inline(text):
    # Links read as their text (and target), emphasis goes, escapes resolve.
    text = MD_REF_LINK.sub(r"\1", MD_INLINE_LINK.sub(r"\1 (", text))
    return MD_INLINE.sub(lambda m: m[1] or "", text)


def published_text(kind, line, code=()):
    # (c), (d) and (g) read what a reader sees; see the header's "Text".
    # code: the line's inline code spans (markup_scan's).
    attrs = []

    def drop(match, space):
        attrs.extend(v for groups in ATTR_TEXT.findall(match[0]) for v in groups if v)
        return space

    def comment(match):
        attrs.append(match[1])
        return ""

    # Markdown's inline syntax renders outside inline code only.
    parts, at = [], 0
    for start, end in [*code, (len(line), len(line))]:
        text, span = (INLINE_COMMENT.sub(comment, part) for part in (line[at:start], line[start:end]))
        parts += [markdown_inline(text) if kind == "md" else text, span]
        at = end
    body = INLINE_TAG.sub(lambda m: drop(m, ""), "".join(parts))
    if kind == "html":
        body = TAG.sub(lambda m: drop(m, " "), COMMENT_MARK.sub(" ", body))
    text = html.unescape(" ".join([body, *attrs])).replace("\xa0", " ")
    return WHITESPACE.sub(" ", text).strip().translate(DASHES)


def judge(path, kind, tier, where, raw, carried, code=()):
    # Checks (b)-(g) on a line's raw text, whose inline code spans are code;
    # the published text it judged.
    text = published_text(kind, raw, code)
    check_urls(where, raw)
    check_prose(where, text, carried)
    check_review(tier, where, text, carried)
    check_content(path, tier, where, raw, text)
    return text


def fires(path, kind, text):
    # The checks among (b)-(g) that a row's text fires by itself.
    global probe
    probe, saved = [], dict(counters)
    try:
        judge(path, kind, "engineering", "", text, "", markup_scan(kind, text, None)[1])
        return list(probe)
    finally:
        probe = None
        counters.update(saved)


tier_counts = {}
for name in sorted(files):
    kind = files[name]
    path = root / name
    tiers = tiers_of(name)
    if len(tiers) != 1:
        errors.append(
            f"DOC LINK: {name}: in {len(tiers)} tiers {tiers} — the gate's TIERS table puts "
            "every document in exactly one; add it there"
        )
    tier = tiers[0] if len(tiers) == 1 else "first-read"  # unassigned: judged strictest
    tier_counts[tier] = tier_counts.get(tier, 0) + 1
    fenced = None  # the opening fence's run while inside a fenced block
    depth = 0      # open <pre>/<code> tags: quoted text runs across lines
    markup = None  # a comment, raw text or (HTML) a tag open across lines
    carried = ""   # the previous checked line's tail, for phrases across a break
    for number, line in enumerate(path.read_text(errors="replace").splitlines(), 1):
        where = f"{name}:{number}"
        fence_line = False
        if kind == "md":
            fence = FENCE.match(line)
            if fence and fenced is None:
                run, info = fence.group(1), fence.group(2)
                if not (run[0] == "`" and "`" in info):  # a backtick fence's info has none
                    fenced, fence_line = run, True
            elif fence and fence.group(1)[0] == fenced[0] and len(fence.group(1)) >= len(fenced) \
                    and not fence.group(2).strip():
                fenced, fence_line = None, True
        in_fence = fence_line or fenced is not None
        # The whole line's inline code, for markers and (a): markup_scan from
        # the state the line starts in. The checked line is scanned below; only
        # that scan's state carries to the next line. A fence has no inline code.
        code = [] if in_fence else markup_scan(kind, line, markup)[1]
        spans = marker_spans(line, code)
        if len(spans) > 1:
            errors.append(
                f"DOC LINK: {where}: {len(spans)} doc-link-allow markers on one line — one "
                "marker names every check it waives"
            )
        checks = set()
        for start, end in spans:
            # Each marker is judged by itself, with no waiver, and is no part
            # of the line it waives on: it cannot satisfy its own waiver.
            marker = line[start:end]
            judge(
                path, kind, tier, f"{where} (doc-link-allow marker)", marker, "",
                markup_scan(kind, marker, None)[1],
            )
            checks.update(marker_waiver(where, marker) or [])
        # (a) judges the whole line; neither a marker nor a row waives it.
        if kind == "md" and not in_fence:
            check_links(path, where, blank(line, code, ""), MD_LINKS + HTML_LINKS)
        elif kind == "html":
            check_links(path, where, line, HTML_LINKS)
        check_own_urls(where, line)
        # A live row exempts only its own text; the rest of the line is judged.
        checked = blank(line, spans)
        for row in generated:
            if row[0] == name and row[1] in line:
                generated_hits[row] += 1
                if row in live_rows:
                    checked = checked.replace(row[1], " ")
        if name == TABLE and number in table_text:
            cells = line.split("\t")
            checked = "\t".join([cells[0], " ", *cells[2:]])
        quoted, code, depth, markup = quoted_spans(kind, checked, depth, markup, in_fence)
        if checks:
            # A marker waives hits in the line's quoted text only; the rest
            # of the line (no inline code left in it) is judged with no waiver.
            text = published_text(kind, checked, code)
            judge(path, kind, tier, where, blank(checked, quoted), carried)
            waived = {check: 0 for check in sorted(checks)}
            inside_text, inside_code = runs(checked, quoted, code)
            judge(path, kind, tier, where, inside_text, "", inside_code)
        else:
            text = judge(path, kind, tier, where, checked, carried, code)
        carried = " ".join(text.split()[-4:])
        for check, hits in waived.items():
            if not hits:
                errors.append(
                    f"DOC LINK: {where}: doc-link-allow({check}) waives nothing on this line — "
                    "delete the marker"
                )
        waived = {}

for row in sorted(live_rows):
    if not fires(root / row[0], files.get(row[0], "md"), row[1]):
        errors.append(
            f"DOC LINK: {TABLE} row for {row[0]} exempts nothing: its text fires none of "
            f"(b)-(g) — delete the row: {row[1]!r}"
        )
for record, kept in evidence_waived.items():
    if evidence_live.get(record) and not kept:
        errors.append(
            f"DOC LINK: {record}: the evidence-record carve-out waives nothing (no decision or "
            "story identifier) — delete it; the record is first-read copy"
        )
for row, hits in generated_hits.items():
    if hits != 1:
        errors.append(
            f"DOC LINK: {TABLE} row for {row[0]} matched {hits} lines, not 1 — "
            f"if {row[2].split()[0]} changed, delete the row: {row[1]!r}"
        )
scope_problems = [
    f"no {CLASS_NAMES[kind]} files found — a declared class that checks nothing"
    for kind in CLASSES if not counts[kind]
]
if not counters["links"]:
    scope_problems.append(
        f"{counts['md']} Markdown files hold no relative link — the link check judged nothing"
    )
scope = (
    f"{counts['md']} Markdown, {counts['html']} HTML, {counts['table']} table files "
    f"({', '.join(f'{n} {t}' for t, n in sorted(tier_counts.items()))}); "
    f"{counters['links']} relative links, {counters['tokens']} planning-path tokens, "
    f"{counters['sections']} § references, {len(removed)} removed document names, "
    f"{counters['own_urls']} own-repository URLs "
    f"{'(' + ', '.join(sorted(own)) + ')' if own else '(no remote: none resolved)'}, "
    f"{len(parent_repos)} parent repositories"
    f"{'' if parent_top is not None else ' (standalone: no parent work tree)'}, "
    f"{len(allowed)} doc-link-allow sites, {len(live_rows)} of {len(generated)} owned-line rows "
    f"live (exempt: each live row's text, and {TABLE}'s text column); "
    f"deviations expire by {HORIZON}{' (INF_CHECK_TODAY)' if clock else ''}; "
    "not scanned: Rust sources, scripts, workflows, configs outside docs/"
)
for site in allowed:
    print(f"allowed: {site}")
for name, text, source, owner, expiry in generated:
    print(f"owned line: {name}: {text!r} ({source}; owner {owner}, expires {expiry})")
for record, (owner, expiry) in EVIDENCE_RECORDS.items():
    state = f"keeps {evidence_waived[record]} decision or story identifiers" \
        if evidence_live.get(record) else "expired: judged as first-read copy"
    print(f"evidence record: {record} (owner {owner}, expires {expiry}; {state})")
if errors:
    print("\n".join(errors))
for problem in scope_problems:
    print(f"public-doc-links SCOPE: {problem}")
if scope_problems:
    print(f"public-doc-links FAILED: scope not established; {scope}")
    sys.exit(1)
if errors:
    print(f"public-doc-links FAILED: {len(errors)} violation(s); {scope}")
    sys.exit(1)
print(f"public-doc-links OK: {scope}")
PY
