"""The gates' self-test fixture lint (ADR-0106 D1).

A case that reads a fixture its group never built passes a bare `expect red` on
an absent path, so every `$work/<name>` a function reads must be created before
the read runs: earlier in the same function, or by a call it makes earlier.

Input, on stdin: bash's own text of each function (`declare -f`, which prints
nested functions and drops comments), each preceded by a \\036 byte and its
name on a line of its own. The text is split into words and operators as bash
splits it. Single-quoted text and heredoc bodies are fixture bytes, never a
read or a creation, and a word inside quotes is never a call.

A `$work/<name>` is created where it is:
  - the target of `>`, `>>`, `>|`, `&>` or `<>`;
  - an argument of mkdir, touch, mktemp or `git init`;
  - the last argument of cp, mv or ln;
  - the whole value of a variable assignment that stands alone or follows
    local, export, declare, readonly or typeset (`root="$work/x"`): the path is
    then created through the variable.
Anywhere else it is read; an assignment that prefixes a command hands that
command a path, so it reads.

A call creates and reads, at its own place, what the function it calls does:
a name that function writes whole, and a name one of its positional arguments
completes (`"$work/$1"`, `"$work/p-$name"` after `local name=$1`, `"$work/$1.s"`)
as the word the call passes there, written plainly. A call that passes `$N`
hands that name on to the calls of its own function. A name another variable
completes (a loop variable) is read only after the same function created the
same expression. `log` is the case helpers' own; a glob reads its family.

Order: a group's own lines run in order and a nested function runs at its first
call, so a read that precedes every creation of its name is red. Within one
call of a nested function the lint does not order. A function no call reaches
is checked against every creation of its block. Every read the lint cannot
follow to a name is red, as is a text it cannot split.
"""

import re
import sys

MAKE_REDIRECTS = (">", ">>", ">|", "&>", "&>>", "<>")
READ_REDIRECTS = ("<", "<<<", ">&", "<&")
HEREDOCS = ("<<", "<<-")
OPERATORS = ("&>>", "<<<", "<<-", "&&", "||", ";;", ";&", "|&", "<<", ">>", ">|", "&>", "<>",
             ">&", "<&", "|", "&", ";", "<", ">", "(", ")")
SEPARATORS = ("&&", "||", ";;", ";&", "|&", "|", "&", ";", "\n", "(", ")")
# Words that keep the next word in command position, and those a command starts past.
KEEP_COMMAND = ("then", "do", "else", "elif", "if", "while", "until", "!", "{", "time")
LEADING = KEEP_COMMAND + ("}", "fi", "done", "esac")
DECLARE = ("local", "export", "declare", "readonly", "typeset")
REF = re.compile(r"\$\{?work\}?\"?/([A-Za-z0-9._-]*)")
VARIABLE = re.compile(r"\$\{?([1-9]|[A-Za-z_][A-Za-z0-9_]*)\}?([A-Za-z0-9._-]*)")
PLAIN = re.compile(r"[A-Za-z0-9._-]+")
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
ASSIGN = re.compile(r"([A-Za-z_][A-Za-z0-9_]*)\+?=")
INF = float("inf")


class Token:
    """A word (kind "w"), an operator ("o") or the end of a `$( )` ("e").
    value: the word with its quotes removed when nothing in it expands, else
    None. live: the text in which `$work` expands, single-quoted text and
    `$( )` bodies masked. level: the `$( )` depth."""

    __slots__ = ("kind", "line", "level", "raw", "value", "live", "begin", "end")

    def __init__(self, kind, line, level, raw, value, live, begin, end):
        self.kind, self.line, self.level, self.raw = kind, line, level, raw
        self.value, self.live, self.begin, self.end = value, live, begin, end


class Lexer:
    """Splits one function's text; `error` names what left it unbalanced."""

    def __init__(self, text):
        self.text, self.at, self.line = text, 0, 1
        self.tokens, self.error, self.word, self.heredocs = [], None, None, []
        # A context: [mode "c" (code) or "d" (double quotes), open parentheses,
        # open case statements, the word a `$(` interrupted, command position].
        self.contexts = [["c", 0, 0, None, True]]
        self.run()

    def level(self):
        return sum(1 for c in self.contexts if c[0] == "c") - 1

    def begin_word(self):
        if self.word is None:
            self.word = [self.line, self.at, [], [], []]

    def add(self, raw, value, live):
        self.begin_word()
        self.word[2].append(raw)
        if value is None:
            self.word[3] = None
        elif self.word[3] is not None:
            self.word[3].append(value)
        self.word[4].append(live)

    def end_word(self):
        w, self.word = self.word, None
        if w is None:
            return
        raw = "".join(w[2])
        value = None if w[3] is None else "".join(w[3])
        self.tokens.append(Token("w", w[0], self.level(), raw, value, "".join(w[4]), w[1],
                                 self.at))
        top = self.contexts[-1]
        if top[4] and raw == "case":
            top[2] += 1
        elif top[4] and raw == "esac" and top[2] > 0:
            top[2] -= 1
        top[4] = raw in KEEP_COMMAND

    def matching(self, at, close):
        """The index of the `close` that ends what opens at `at`, or -1."""
        depth, text = 0, self.text
        while at < len(text):
            if text[at] == "\\":
                at += 2
                continue
            if close == "}" and text[at] == "{":
                depth += 1
            elif text[at] == close:
                depth -= 1
                if close != "}" or depth == 0:
                    return at
            at += 1
        return -1

    def opaque(self, close, live):
        """A backquote or `${ }`: a word part that expands, kept whole."""
        end = self.matching(self.at + 1, close)
        if end < 0:
            self.error = "an open " + ("backquote" if close == "`" else "${")
            return
        part = self.text[self.at:end + 1]
        self.add(part, None, part if live else "\x00")
        self.line += part.count("\n")
        self.at = end + 1

    def open_substitution(self):
        self.begin_word()
        self.contexts.append(["c", 0, 0, self.word, True])
        self.word = None
        self.at += 2

    def double_quoted(self, c, text):
        nxt = text[self.at + 1:self.at + 2]
        if c == "\"":
            self.add(c, "", c)
            self.contexts.pop()
            self.at += 1
        elif c == "\\" and nxt:
            self.add(c + nxt, nxt if nxt in "$`\"\\\n" else c + nxt, "\x00")
            self.line += nxt == "\n"
            self.at += 2
        elif c == "$" and nxt == "(":
            self.open_substitution()
        elif c == "$" and nxt == "{":
            self.opaque("}", True)
        elif c == "`":
            self.opaque("`", False)
        else:
            self.add(c, None if c == "$" else c, c)
            self.line += c == "\n"
            self.at += 1

    def newline(self):
        self.end_word()
        self.tokens.append(Token("o", self.line, self.level(), "\n", None, "", self.at,
                                 self.at + 1))
        self.contexts[-1][4] = True
        self.line += 1
        self.at += 1
        text = self.text
        for delimiter, tabs in self.heredocs:
            while True:
                if self.at >= len(text):
                    self.error = "a heredoc with no " + delimiter + " line"
                    return
                end = text.find("\n", self.at)
                end = len(text) if end < 0 else end
                body = text[self.at:end]
                self.at, self.line = end + 1, self.line + 1
                if (body.lstrip("\t") if tabs else body) == delimiter:
                    break
        self.heredocs = []

    def operator(self):
        self.end_word()
        text, top = self.text, self.contexts[-1]
        op = next(o for o in OPERATORS if text.startswith(o, self.at))
        begin = self.at
        self.at += len(op)
        if op == ")":
            if top[1] > 0:
                top[1] -= 1
            elif top[2] == 0 and len(self.contexts) > 1:
                # The `)` that ends a `$( )`: back to the word it interrupted.
                self.tokens.append(Token("e", self.line, self.level(), op, None, "", begin,
                                         self.at))
                saved = top[3]
                self.contexts.pop()
                self.word = saved
                self.add("$(\x00)", None, "\x00")
                return
            elif top[2] == 0:
                self.error = "an unmatched )"
                return
        elif op == "(":
            top[1] += 1
        elif op in HEREDOCS:
            m = re.match(r"[ \t]*\\?([\x27\"]?)([A-Za-z_][A-Za-z0-9_]*)\1", text[self.at:])
            if not m:
                self.error = "a heredoc with no delimiter"
                return
            self.heredocs.append((m.group(2), op == "<<-"))
            self.at += m.end()
        self.tokens.append(Token("o", self.line, self.level(), op, None, "", begin, self.at))
        top[4] = op in SEPARATORS

    def run(self):
        text = self.text
        while self.at < len(text) and self.error is None:
            c = text[self.at]
            nxt = text[self.at + 1:self.at + 2]
            if self.contexts[-1][0] == "d":
                self.double_quoted(c, text)
            elif c == "\n":
                self.newline()
            elif c in " \t":
                self.end_word()
                self.at += 1
            elif c == "\\" and nxt:
                if nxt == "\n":
                    self.line += 1
                else:
                    self.add(c + nxt, nxt, "\x00")
                self.at += 2
            elif c == "\x27":
                end = text.find("\x27", self.at + 1)
                if end < 0:
                    self.error = "an open single quote"
                    break
                self.add(text[self.at:end + 1], text[self.at + 1:end], "\x27\x00\x27")
                self.line += text.count("\n", self.at, end)
                self.at = end + 1
            elif c == "\"":
                self.add(c, "", c)
                self.contexts.append(["d"])
                self.at += 1
            elif c == "`":
                self.opaque("`", False)
            elif c == "$" and nxt == "(":
                self.open_substitution()
            elif c == "$" and nxt == "{":
                self.opaque("}", True)
            elif c in "|&;<>()":
                self.operator()
            else:
                self.add(c, None if c == "$" else c, c)
                self.at += 1
        self.end_word()
        if self.error is None and (len(self.contexts) > 1 or self.heredocs):
            self.error = "an open quote, $( or heredoc at its end"


def functions(tokens):
    """(name, header index, closing-brace index) of each function the text
    defines, the outermost first."""
    found = []
    for k in range(len(tokens) - 3):
        t = tokens[k]
        if t.kind != "w" or t.value != t.raw or not IDENT.fullmatch(t.raw):
            continue
        if tokens[k + 1].raw != "(" or tokens[k + 2].raw != ")":
            continue
        b = k + 3
        while b < len(tokens) and tokens[b].kind == "o" and tokens[b].raw in ("\n", ";"):
            b += 1
        if b >= len(tokens) or tokens[b].kind != "w" or tokens[b].raw != "{":
            continue
        depth, e = 0, b
        while e < len(tokens):
            u = tokens[e]
            if u.kind == "w" and u.level == t.level and u.raw in ("{", "}"):
                depth += 1 if u.raw == "{" else -1
                if depth == 0:
                    break
            e += 1
        found.append((t.raw, k, e))
    return found


def commands(tokens):
    """Each simple command: the indices of its words and redirection operators."""
    current, out = {}, []
    for k, t in enumerate(tokens):
        if t.kind == "w" or (t.kind == "o" and t.raw in MAKE_REDIRECTS + READ_REDIRECTS + HEREDOCS):
            current.setdefault(t.level, []).append(k)
        elif current.get(t.level):
            out.append(current.pop(t.level))
    out.extend(v for v in current.values() if v)
    return out


def positional(raw):
    m = re.fullmatch(r"\"?\$\{?([1-9])\}?\"?", raw)
    return int(m.group(1)) if m else None


class Block:
    """One function's text: its tokens, nested functions, refs and calls."""

    def __init__(self, name, body):
        self.name, self.lines = name, body.split("\n")
        lexer = Lexer(body)
        self.tokens, self.error = lexer.tokens, lexer.error
        self.refs, self.calls, self.nested = [], [], set()
        self.holder = [name] * len(self.tokens)
        if self.error is not None:
            return
        spans = functions(self.tokens)
        heads = {a for _, a, _ in spans}
        for fn, a, b in spans:  # the outermost first, so an inner one overwrites
            for k in range(a, b + 1):
                self.holder[k] = fn
            if fn != name:
                self.nested.add(fn)
        for command in commands(self.tokens):
            self.read_command(command, heads)

    def text_of(self, k):
        return self.lines[self.tokens[k].line - 1].strip()

    def read_command(self, command, heads):
        tokens, words, k = self.tokens, [], 0
        while k < len(command):
            t = tokens[command[k]]
            if t.kind == "o":
                nxt = command[k + 1] if k + 1 < len(command) else None
                if t.raw not in HEREDOCS and nxt is not None and tokens[nxt].kind == "w":
                    words.append((nxt, t.raw))  # a redirection's target
                    k += 2
                else:
                    k += 1
                continue
            nxt = tokens[command[k + 1]] if k + 1 < len(command) else None
            if (t.value is not None and t.value.isdigit() and nxt is not None
                    and nxt.kind == "o" and nxt.begin == t.end):
                k += 1  # a descriptor number: `2>`
                continue
            words.append((command[k], None))
            k += 1
        plain = [w for w, r in words if r is None]
        at = 0
        while at < len(plain) and tokens[plain[at]].raw in LEADING:
            at += 1
        assigns = []
        while at < len(plain) and ASSIGN.match(tokens[plain[at]].raw):
            assigns.append(plain[at])
            at += 1
        verb = tokens[plain[at]].value if at < len(plain) else None
        args = plain[at + 1:]
        if verb in DECLARE:
            assigns = [w for w in args if ASSIGN.match(tokens[w].raw)]
        elif at < len(plain):
            assigns = []  # it hands the command a path: a read
        created = []
        if verb in ("mkdir", "touch", "mktemp"):
            created = args
        elif verb == "git":
            init = [i for i, a in enumerate(args) if tokens[a].value == "init"]
            created = args[init[0] + 1:] if init else []
        elif verb in ("cp", "mv", "ln"):
            created = args[-1:]
        for w, redirect in words:
            live = tokens[w].live
            for m in REF.finditer(live):
                if redirect is not None:
                    make = redirect in MAKE_REDIRECTS
                elif w in assigns:
                    value = live[ASSIGN.match(live).end():]
                    make = re.fullmatch(r"\"?\$\{?work\}?\"?/[^\s\"]*\"?", value) is not None
                else:
                    make = w in created
                self.refs.append((w, "c" if make else "r", m.group(1), live[m.end():]))
        for j, w in enumerate(plain):
            t = tokens[w]
            if w in heads or (j > 0 and tokens[plain[j - 1]].raw == "function"):
                continue
            if t.value == t.raw and IDENT.fullmatch(t.raw):
                self.calls.append((w, t.raw, [tokens[a] for a in plain[j + 1:]]))

    def argument(self, holder, k, variable):
        """The positional argument a variable of `holder` names at token k, or
        None: `$N`, or a name `holder` assigns only from `$N`, with no shift
        before k."""
        own = [i for i in range(len(self.tokens)) if self.holder[i] == holder]
        if any(self.tokens[i].raw == "shift" and i < k for i in own):
            return None
        if variable.isdigit():
            return int(variable)
        sources = {positional(self.tokens[i].raw.split("=", 1)[1]) for i in own
                   if self.tokens[i].kind == "w" and self.tokens[i].raw.startswith(variable + "=")}
        return sources.pop() if len(sources) == 1 and None not in sources else None


def main():
    blocks = [Block(*b.partition("\n")[::2]) for b in sys.stdin.read().split("\x1e")[1:]]
    names = {b.name for b in blocks}
    errors = []
    # effects[(block or None, function)]: what a call of the function creates
    # ("c") or reads ("r"): a whole name (argument None), or prefix + the word
    # its caller passes at that argument + suffix.
    effects, made, reads, first = {}, {}, {}, {}

    def scope(b, holder):
        return (None, b.name) if holder == b.name else (b.name, holder)

    def place(b, k):
        holder = b.holder[k]
        return k if holder == b.name else first[b.name].get(holder, INF)

    def note(table, key, at, value):
        if at < table.get(key, (INF,))[0]:
            table[key] = (at,) + value
            return True
        return False

    for b in blocks:
        made[b.name], reads[b.name] = {"log": (-1,)}, {}
        if b.error is not None:
            errors.append(b.name + ": the lint cannot split its text (" + b.error + ")")
            continue
        runs = {}  # where each nested function first runs: a token of the block's own
        for _ in range(len(b.nested) + 1):
            for w, callee, _args in b.calls:
                if callee in b.nested:
                    holder = b.holder[w]
                    at = w if holder == b.name else runs.get(holder, INF)
                    if at < runs.get(callee, INF):
                        runs[callee] = at
        first[b.name] = runs
        expressions = set()
        for w, kind, key, post in sorted(b.refs):
            holder, at = b.holder[w], place(b, w)
            if not post.startswith("$"):
                if kind == "c":
                    note(made[b.name], key, at, ())
                    effects.setdefault(scope(b, holder), set()).add(("c", None, key, ""))
                elif key:
                    note(reads[b.name], key, at, (b.text_of(w), post[:1] in ("[", "*", "?")))
                continue
            tail = re.match(r"[^/\"\s\x27]*", post).group(0)
            m = VARIABLE.fullmatch(tail)
            if kind == "c":
                expressions.add((holder, key, tail))
            elif (holder, key, tail) in expressions:
                continue  # the same name this function created above, whatever it holds
            argument = b.argument(holder, w, m.group(1)) if m else None
            if argument is not None:
                effects.setdefault(scope(b, holder), set()).add((kind, argument, key,
                                                                 m.group(2)))
            elif kind == "r":
                errors.append(b.name + ": reads $work/" + key + tail
                              + ", a name the lint cannot follow to an argument: "
                              + b.text_of(w))

    followed, changed, rounds = set(), True, 0
    while changed and rounds <= len(blocks) + 1:
        changed, rounds = False, rounds + 1
        for b in blocks:
            if b.error is not None:
                continue
            for w, callee, args in b.calls:
                if callee in b.nested:
                    table = effects.get((b.name, callee), set())
                elif callee in names and callee != b.name:
                    table = effects.get((None, callee), set())
                else:
                    continue
                holder, at = b.holder[w], place(b, w)
                own = effects.setdefault(scope(b, holder), set())
                for kind, argument, prefix, suffix in sorted(table, key=str):
                    followed.add((b.name, w))
                    passed = None
                    if argument is not None and argument <= len(args):
                        passed = args[argument - 1]
                    if argument is None or (passed is not None and passed.value is not None
                                            and PLAIN.fullmatch(passed.value)):
                        full = prefix if argument is None else prefix + passed.value + suffix
                        if kind == "c":
                            changed |= note(made[b.name], full, at, ())
                            if ("c", None, full, "") not in own:
                                own.add(("c", None, full, ""))
                                changed = True
                        else:
                            changed |= note(reads[b.name], full, at, (b.text_of(w), False))
                        continue
                    handed = None
                    if passed is not None:
                        handed = positional(passed.raw)
                        m = re.fullmatch(r"\"?\$\{?([A-Za-z_][A-Za-z0-9_]*)\}?\"?", passed.raw)
                        if handed is None and m:
                            handed = b.argument(holder, w, m.group(1))
                    if handed is not None and (holder != b.name or not b.name.startswith("group_")):
                        if (kind, handed, prefix, suffix) not in own:
                            own.add((kind, handed, prefix, suffix))
                            changed = True
                    elif kind == "r":
                        error = (b.name + ": passes " + callee + " a name the lint cannot read ("
                                 + (passed.raw if passed is not None else "none")
                                 + "), which it reads as $work/" + prefix + "<name>" + suffix
                                 + ": " + b.text_of(w))
                        if error not in errors:
                            errors.append(error)

    for b in blocks:
        created = made[b.name]
        for key, (at, line, glob) in sorted(reads[b.name].items()):
            if created.get(key, (INF,))[0] <= at:
                continue
            if glob and any(k.startswith(key) and v[0] <= at for k, v in created.items()):
                continue
            ident = re.sub(r"[^A-Za-z0-9_]", "_", key)
            if key in created:
                errors.append(b.name + ": reads $work/" + key + " before it creates it or builds"
                              + " it with fx_build_" + ident + ": " + line)
            else:
                errors.append(b.name + ": reads $work/" + key + ", which it neither creates nor"
                              + " builds with fx_build_" + ident + ": " + line)
    for e in errors:
        print("SELFTEST LINT: " + e)
    print("selftest lint: %d functions read, %d calls followed, %d unbuilt fixture(s)"
          % (len(blocks), len(followed), len(errors)))
    return 1 if errors or not blocks else 0


if __name__ == "__main__":
    sys.exit(main())
