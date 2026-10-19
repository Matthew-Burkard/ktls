//! A tolerant parser for Gradle build scripts, covering the subset of Groovy and
//! Kotlin DSL syntax that declares plugins, repositories, dependencies and
//! project structure. Unknown constructs are skipped, never fatal.

#[derive(Clone, Debug, PartialEq)]
pub enum StrPart {
    Lit(String),
    /// `$name` or `${name.path}` — a simple reference.
    Ref(String),
    /// `${ arbitrary expression }`.
    Expr(Box<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Str(Vec<StrPart>),
    Num(String),
    Bool(bool),
    Null,
    Ident(String),
    Member(Box<Expr>, String),
    Call { target: Box<Expr>, args: Vec<Arg>, closure: Option<Vec<Stmt>> },
    /// `a op b` for operators and infix calls (`version`, `apply`, `to`...).
    Infix(Box<Expr>, String, Box<Expr>),
    Index(Box<Expr>, Box<Expr>),
    List(Vec<Expr>),
    Map(Vec<(String, Expr)>),
    Closure(Vec<Stmt>),
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Arg {
    pub name: Option<String>,
    pub value: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Expr(Expr),
    Assign { target: Expr, op: String, value: Expr },
    /// `val x = ...`, `def x = ...`, `val x by extra(...)`, `val x: String by project`
    Var { name: String, value: Option<Expr>, delegate: Option<Expr> },
    /// `if (cond) { then } else { otherwise }`
    If { cond: Expr, then: Vec<Stmt>, otherwise: Vec<Stmt> },
}

impl Expr {
    /// `a.b.c` → `Some("a.b.c")` for pure identifier chains.
    pub fn path(&self) -> Option<String> {
        match self {
            Expr::Ident(s) => Some(s.clone()),
            Expr::Member(t, n) => Some(format!("{}.{n}", t.path()?)),
            _ => None,
        }
    }

    /// The name of a called function (`foo(...)` → `foo`, `a.b(...)` → `a.b`).
    pub fn call_name(&self) -> Option<String> {
        match self {
            Expr::Call { target, .. } => target.path(),
            _ => None,
        }
    }

    pub fn literal(&self) -> Option<String> {
        match self {
            Expr::Str(parts) => {
                let mut s = String::new();
                for p in parts {
                    match p {
                        StrPart::Lit(l) => s.push_str(l),
                        _ => return None,
                    }
                }
                Some(s)
            }
            Expr::Num(n) => Some(n.clone()),
            _ => None,
        }
    }
}

// --------------------------------------------------------------- lexer

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(Vec<StrPart>),
    Num(String),
    Punct(&'static str),
    Newline,
}

const PUNCT: &[&str] = &[
    "?.", "?:", "::", "->", "==", "!=", "<=", ">=", "&&", "||", "+=", "-=", "..", "!!", "(", ")", "{", "}", "[", "]", ",",
    ".", "=", ":", ";", "+", "-", "*", "/", "%", "!", "<", ">", "?", "@", "&", "|", "#", "$",
];

fn lex(src: &str) -> Vec<Tok> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut toks = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            toks.push(Tok::Newline);
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                if chars[i] == '\n' {
                    toks.push(Tok::Newline);
                }
                i += 1;
            }
            i += 2;
            continue;
        }
        if c == '"' || c == '\'' {
            let triple = chars.get(i + 1) == Some(&c) && chars.get(i + 2) == Some(&c);
            let (parts, end) = lex_string(&chars, i, c, triple);
            toks.push(Tok::Str(parts));
            i = end;
            continue;
        }
        if c == '`' {
            let start = i + 1;
            let mut j = start;
            while j < chars.len() && chars[j] != '`' && chars[j] != '\n' {
                j += 1;
            }
            toks.push(Tok::Ident(chars[start..j].iter().collect()));
            i = j + 1;
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || (chars[i] == '.' && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()))) {
                i += 1;
            }
            toks.push(Tok::Num(chars[start..i].iter().collect()));
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            toks.push(Tok::Ident(chars[start..i].iter().collect()));
            continue;
        }
        let rest: String = chars[i..chars.len().min(i + 2)].iter().collect();
        match PUNCT.iter().find(|p| rest.starts_with(**p)) {
            Some(p) => {
                toks.push(Tok::Punct(p));
                i += p.chars().count();
            }
            None => i += 1,
        }
    }
    toks
}

/// Returns the string parts and the index after the closing quote.
fn lex_string(chars: &[char], start: usize, quote: char, triple: bool) -> (Vec<StrPart>, usize) {
    let mut i = start + if triple { 3 } else { 1 };
    let interpolate = quote == '"';
    let mut parts = Vec::new();
    let mut lit = String::new();
    while i < chars.len() {
        let c = chars[i];
        if triple {
            if c == quote && chars.get(i + 1) == Some(&quote) && chars.get(i + 2) == Some(&quote) {
                i += 3;
                break;
            }
        } else if c == quote {
            i += 1;
            break;
        } else if c == '\n' {
            break;
        }
        if c == '\\' && !triple {
            let n = chars.get(i + 1).copied().unwrap_or('\\');
            lit.push(match n {
                'n' => '\n',
                't' => '\t',
                other => other,
            });
            i += 2;
            continue;
        }
        if interpolate && c == '$' {
            if chars.get(i + 1) == Some(&'{') {
                // find matching brace
                let mut depth = 0;
                let mut j = i + 1;
                while j < chars.len() {
                    if chars[j] == '{' {
                        depth += 1;
                    } else if chars[j] == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    j += 1;
                }
                let inner: String = chars[i + 2..j.min(chars.len())].iter().collect();
                if !lit.is_empty() {
                    parts.push(StrPart::Lit(std::mem::take(&mut lit)));
                }
                let trimmed = inner.trim();
                if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.') {
                    parts.push(StrPart::Ref(trimmed.to_string()));
                } else {
                    let stmts = parse(&inner);
                    let e = match stmts.into_iter().next() {
                        Some(Stmt::Expr(e)) => e,
                        _ => Expr::Unknown,
                    };
                    parts.push(StrPart::Expr(Box::new(e)));
                }
                i = j + 1;
                continue;
            }
            if chars.get(i + 1).is_some_and(|c| c.is_alphabetic() || *c == '_') {
                let mut j = i + 1;
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_' || (chars[j] == '.' && chars.get(j + 1).is_some_and(|c| c.is_alphabetic()))) {
                    j += 1;
                }
                if !lit.is_empty() {
                    parts.push(StrPart::Lit(std::mem::take(&mut lit)));
                }
                parts.push(StrPart::Ref(chars[i + 1..j].iter().collect()));
                i = j;
                continue;
            }
        }
        lit.push(c);
        i += 1;
    }
    if !lit.is_empty() || parts.is_empty() {
        parts.push(StrPart::Lit(lit));
    }
    (parts, i)
}

// -------------------------------------------------------------- parser

pub fn parse(src: &str) -> Vec<Stmt> {
    let mut p = P { toks: lex(src), pos: 0, command_args: false };
    
    p.stmts(None)
}

struct P {
    toks: Vec<Tok>,
    pos: usize,
    /// Parsing Groovy command-call arguments: `id 'x' version 'y'` is `id('x').version('y')`.
    command_args: bool,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn peek_n(&self, n: usize) -> Option<&Tok> {
        self.toks.get(self.pos + n)
    }

    fn at_punct(&self, p: &str) -> bool {
        matches!(self.peek(), Some(Tok::Punct(x)) if *x == p)
    }

    fn at_ident(&self, s: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(x)) if x == s)
    }

    fn eat_punct(&mut self, p: &str) -> bool {
        if self.at_punct(p) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Some(Tok::Newline) | Some(Tok::Punct(";"))) {
            self.pos += 1;
        }
    }

    /// Skip newlines only if the token after them continues an expression (`.foo`).
    fn skip_newlines_before_dot(&mut self) {
        let mut j = self.pos;
        while matches!(self.toks.get(j), Some(Tok::Newline)) {
            j += 1;
        }
        if matches!(self.toks.get(j), Some(Tok::Punct(".")) | Some(Tok::Punct("?."))) {
            self.pos = j;
        }
    }

    fn stmts(&mut self, until: Option<&str>) -> Vec<Stmt> {
        let mut out = Vec::new();
        loop {
            self.skip_newlines();
            match self.peek() {
                None => break,
                Some(Tok::Punct(p)) if Some(*p) == until => break,
                Some(Tok::Punct("}")) if until.is_none() => {
                    self.pos += 1;
                    continue;
                }
                _ => {}
            }
            let before = self.pos;
            if let Some(s) = self.stmt() {
                out.push(s);
            }
            if self.pos == before {
                self.pos += 1; // guarantee progress
            }
        }
        out
    }

    fn block(&mut self) -> Vec<Stmt> {
        let outer = std::mem::replace(&mut self.command_args, false);
        let body = self.block_inner();
        self.command_args = outer;
        body
    }

    fn block_inner(&mut self) -> Vec<Stmt> {
        // at '{'
        self.pos += 1;
        // closure parameters: `{ a, b -> ...}` / `{ it -> }`
        let save = self.pos;
        let mut j = self.pos;
        while matches!(self.toks.get(j), Some(Tok::Ident(_)) | Some(Tok::Punct(",")) | Some(Tok::Punct(":")) | Some(Tok::Newline)) {
            j += 1;
        }
        if matches!(self.toks.get(j), Some(Tok::Punct("->"))) && j > save {
            self.pos = j + 1;
        }
        let body = self.stmts(Some("}"));
        self.eat_punct("}");
        body
    }

    fn stmt(&mut self) -> Option<Stmt> {
        match self.peek()? {
            Tok::Ident(kw) if kw == "import" || kw == "package" => {
                while !matches!(self.peek(), None | Some(Tok::Newline)) {
                    self.pos += 1;
                }
                return None;
            }
            Tok::Ident(kw) if (kw == "val" || kw == "var" || kw == "def") && matches!(self.peek_n(1), Some(Tok::Ident(_))) => {
                self.pos += 1;
                return Some(self.var_decl());
            }
            Tok::Ident(kw) if kw == "if" && matches!(self.peek_n(1), Some(Tok::Punct("("))) => {
                return Some(self.if_stmt());
            }
            // Groovy typed declaration: `String foo = "x"`
            Tok::Ident(t)
                if t.chars().next().is_some_and(|c| c.is_uppercase())
                    && matches!(self.peek_n(1), Some(Tok::Ident(_)))
                    && matches!(self.peek_n(2), Some(Tok::Punct("="))) =>
            {
                self.pos += 1;
                return Some(self.var_decl());
            }
            _ => {}
        }
        let target = self.expr(true)?;
        for op in ["=", "+=", "-="] {
            if self.at_punct(op) {
                self.pos += 1;
                self.skip_newlines_only();
                let value = self.expr(false).unwrap_or(Expr::Unknown);
                return Some(Stmt::Assign { target, op: op.into(), value });
            }
        }
        Some(Stmt::Expr(target))
    }

    fn skip_newlines_only(&mut self) {
        while matches!(self.peek(), Some(Tok::Newline)) {
            self.pos += 1;
        }
    }

    fn var_decl(&mut self) -> Stmt {
        let name = match self.peek() {
            Some(Tok::Ident(n)) => n.clone(),
            _ => String::new(),
        };
        self.pos += 1;
        if self.eat_punct(":") {
            // type annotation: skip a simple type
            self.type_ref();
        }
        if self.eat_punct("=") {
            self.skip_newlines_only();
            return Stmt::Var { name, value: self.expr(false), delegate: None };
        }
        if self.at_ident("by") {
            self.pos += 1;
            return Stmt::Var { name, value: None, delegate: self.expr(false) };
        }
        Stmt::Var { name, value: None, delegate: None }
    }

    fn type_ref(&mut self) {
        while let Some(Tok::Ident(_)) = self.peek() {
            self.pos += 1;
            if self.at_punct("<") {
                let mut depth = 0;
                while let Some(t) = self.peek() {
                    match t {
                        Tok::Punct("<") => depth += 1,
                        Tok::Punct(">") => {
                            depth -= 1;
                            if depth == 0 {
                                self.pos += 1;
                                break;
                            }
                        }
                        Tok::Newline => break,
                        _ => {}
                    }
                    self.pos += 1;
                }
            }
            self.eat_punct("?");
            if !self.eat_punct(".") {
                break;
            }
        }
    }

    fn if_stmt(&mut self) -> Stmt {
        self.pos += 1; // if
        self.pos += 1; // (
        let cond = self.expr(false).unwrap_or(Expr::Unknown);
        self.eat_punct(")");
        self.skip_newlines_only();
        let then = self.branch();
        let save = self.pos;
        self.skip_newlines_only();
        let otherwise = if self.at_ident("else") {
            self.pos += 1;
            self.skip_newlines_only();
            if self.at_ident("if") {
                vec![self.if_stmt()]
            } else {
                self.branch()
            }
        } else {
            self.pos = save;
            Vec::new()
        };
        Stmt::If { cond, then, otherwise }
    }

    fn branch(&mut self) -> Vec<Stmt> {
        if self.at_punct("{") {
            self.block()
        } else {
            self.stmt().into_iter().collect()
        }
    }

    fn can_start_arg(&self, n: usize) -> bool {
        match self.peek_n(n) {
            Some(Tok::Str(_)) | Some(Tok::Num(_)) => true,
            Some(Tok::Ident(s)) => !matches!(s.as_str(), "else" | "by" | "in" | "as" | "is"),
            Some(Tok::Punct("[")) => true,
            _ => false,
        }
    }

    /// `statement_start` enables Groovy command syntax (`implementation 'x'`).
    fn expr(&mut self, statement_start: bool) -> Option<Expr> {
        let mut lhs = self.postfix(statement_start)?;
        loop {
            match self.peek() {
                Some(Tok::Punct(op @ ("?:" | "+" | "-" | "*" | "/" | "==" | "!=" | "&&" | "||" | ".." | "<" | ">" | "<=" | ">="))) => {
                    let op = op.to_string();
                    self.pos += 1;
                    self.skip_newlines_only();
                    let rhs = self.postfix(false).unwrap_or(Expr::Unknown);
                    lhs = Expr::Infix(Box::new(lhs), op, Box::new(rhs));
                }
                // infix calls: `version "1.0"`, `apply false`, `to "x"`
                Some(Tok::Ident(name)) if !self.command_args && !matches!(name.as_str(), "else" | "by" | "as" | "is" | "in") && self.can_start_arg(1) => {
                    let name = name.clone();
                    self.pos += 1;
                    let rhs = self.postfix(false).unwrap_or(Expr::Unknown);
                    lhs = Expr::Infix(Box::new(lhs), name, Box::new(rhs));
                }
                Some(Tok::Ident(name)) if name == "is" || name == "in" => {
                    self.pos += 1;
                    self.type_ref();
                }
                Some(Tok::Ident(name)) if name == "as" => {
                    self.pos += 1;
                    self.type_ref();
                }
                _ => break,
            }
        }
        Some(lhs)
    }

    fn postfix(&mut self, statement_start: bool) -> Option<Expr> {
        let mut e = self.primary()?;
        let mut command_ok = statement_start;
        loop {
            self.skip_newlines_before_dot();
            match self.peek() {
                Some(Tok::Punct("." | "?.")) => {
                    self.pos += 1;
                    match self.peek() {
                        Some(Tok::Ident(n)) => {
                            let n = n.clone();
                            self.pos += 1;
                            e = Expr::Member(Box::new(e), n);
                        }
                        _ => break,
                    }
                }
                Some(Tok::Punct("::")) => {
                    self.pos += 1;
                    if let Some(Tok::Ident(n)) = self.peek() {
                        let n = n.clone();
                        self.pos += 1;
                        e = Expr::Member(Box::new(e), n);
                    }
                }
                Some(Tok::Punct("!!")) => self.pos += 1,
                Some(Tok::Punct("<")) if matches!(e, Expr::Ident(_) | Expr::Member(..)) && self.generic_args_ahead() => {
                    // `configure<Foo> { }`, `getByName<Type>("x")` — skip type args
                    self.skip_generic_args();
                }
                Some(Tok::Punct("(")) => {
                    let args = self.args();
                    let closure = if self.at_punct("{") { Some(self.block()) } else { None };
                    e = Expr::Call { target: Box::new(e), args, closure };
                }
                Some(Tok::Punct("{")) if matches!(e, Expr::Ident(_) | Expr::Member(..) | Expr::Call { .. }) => {
                    let closure = self.block();
                    e = match e {
                        Expr::Call { target, args, closure: None } => Expr::Call { target, args, closure: Some(closure) },
                        other => Expr::Call { target: Box::new(other), args: Vec::new(), closure: Some(closure) },
                    };
                }
                Some(Tok::Punct("[")) => {
                    self.pos += 1;
                    let idx = self.expr(false).unwrap_or(Expr::Unknown);
                    self.eat_punct("]");
                    e = Expr::Index(Box::new(e), Box::new(idx));
                }
                _ if command_ok && matches!(e, Expr::Ident(_) | Expr::Member(..)) && self.can_start_arg(0) => {
                    // Groovy command call: `implementation 'a:b:c'`, `apply plugin: 'x'`
                    let mut args = Vec::new();
                    let outer = std::mem::replace(&mut self.command_args, true);
                    loop {
                        args.push(self.arg());
                        if !self.eat_punct(",") {
                            break;
                        }
                        self.skip_newlines_only();
                    }
                    self.command_args = outer;
                    let closure = if self.at_punct("{") { Some(self.block()) } else { None };
                    e = Expr::Call { target: Box::new(e), args, closure };
                }
                _ => break,
            }
            command_ok = false;
        }
        Some(e)
    }

    fn generic_args_ahead(&self) -> bool {
        let mut depth = 0;
        let mut j = self.pos;
        while let Some(t) = self.toks.get(j) {
            match t {
                Tok::Punct("<") => depth += 1,
                Tok::Punct(">") => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(self.toks.get(j + 1), Some(Tok::Punct("(")) | Some(Tok::Punct("{")));
                    }
                }
                Tok::Ident(_) | Tok::Punct(".") | Tok::Punct(",") | Tok::Punct("?") | Tok::Punct("*") => {}
                _ => return false,
            }
            j += 1;
        }
        false
    }

    fn skip_generic_args(&mut self) {
        let mut depth = 0;
        while let Some(t) = self.peek() {
            match t {
                Tok::Punct("<") => depth += 1,
                Tok::Punct(">") => {
                    depth -= 1;
                    if depth == 0 {
                        self.pos += 1;
                        return;
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
    }

    fn arg(&mut self) -> Arg {
        // named: `name = value` (Kotlin) or `name: value` (Groovy)
        if let (Some(Tok::Ident(n)), Some(Tok::Punct(p))) = (self.peek(), self.peek_n(1))
            && (*p == "=" || *p == ":") {
                let name = n.clone();
                self.pos += 2;
                self.skip_newlines_only();
                return Arg { name: Some(name), value: self.expr(false).unwrap_or(Expr::Unknown) };
            }
        if let (Some(Tok::Str(parts)), Some(Tok::Punct(":"))) = (self.peek(), self.peek_n(1)) {
            // Groovy map entry with a string key
            let name = Expr::Str(parts.clone()).literal().unwrap_or_default();
            self.pos += 2;
            return Arg { name: Some(name), value: self.expr(false).unwrap_or(Expr::Unknown) };
        }
        Arg { name: None, value: self.expr(false).unwrap_or(Expr::Unknown) }
    }

    fn args(&mut self) -> Vec<Arg> {
        let outer = std::mem::replace(&mut self.command_args, false);
        let args = self.args_inner();
        self.command_args = outer;
        args
    }

    fn args_inner(&mut self) -> Vec<Arg> {
        self.pos += 1; // (
        let mut args = Vec::new();
        loop {
            self.skip_newlines_only();
            if self.eat_punct(")") || self.peek().is_none() {
                break;
            }
            let before = self.pos;
            args.push(self.arg());
            self.skip_newlines_only();
            if !self.eat_punct(",") {
                self.skip_newlines_only();
                if !self.eat_punct(")") {
                    // skip junk until ')'
                    let mut depth = 0;
                    while let Some(t) = self.peek() {
                        match t {
                            Tok::Punct("(") | Tok::Punct("{") | Tok::Punct("[") => depth += 1,
                            Tok::Punct(")") | Tok::Punct("}") | Tok::Punct("]") => {
                                if depth == 0 {
                                    break;
                                }
                                depth -= 1;
                            }
                            _ => {}
                        }
                        self.pos += 1;
                    }
                    self.eat_punct(")");
                }
                break;
            }
            if self.pos == before {
                self.pos += 1;
            }
        }
        args
    }

    fn primary(&mut self) -> Option<Expr> {
        let t = self.peek()?.clone();
        Some(match t {
            Tok::Str(parts) => {
                self.pos += 1;
                Expr::Str(parts)
            }
            Tok::Num(n) => {
                self.pos += 1;
                Expr::Num(n)
            }
            Tok::Ident(id) => {
                self.pos += 1;
                match id.as_str() {
                    "true" => Expr::Bool(true),
                    "false" => Expr::Bool(false),
                    "null" => Expr::Null,
                    "new" => return self.primary(),
                    _ => Expr::Ident(id),
                }
            }
            Tok::Punct("(") => {
                self.pos += 1;
                self.skip_newlines_only();
                let e = self.expr(false).unwrap_or(Expr::Unknown);
                self.skip_newlines_only();
                self.eat_punct(")");
                e
            }
            Tok::Punct("[") => {
                self.pos += 1;
                let mut items = Vec::new();
                let mut map = Vec::new();
                loop {
                    self.skip_newlines_only();
                    if self.eat_punct("]") || self.peek().is_none() {
                        break;
                    }
                    if self.at_punct(":") {
                        // `[:]` empty map
                        self.pos += 1;
                        continue;
                    }
                    let before = self.pos;
                    let a = self.arg();
                    match a.name {
                        Some(n) => map.push((n, a.value)),
                        None => items.push(a.value),
                    }
                    self.skip_newlines_only();
                    self.eat_punct(",");
                    if self.pos == before {
                        self.pos += 1;
                    }
                }
                if map.is_empty() { Expr::List(items) } else { Expr::Map(map) }
            }
            Tok::Punct("{") => Expr::Closure(self.block()),
            Tok::Punct("-") | Tok::Punct("!") => {
                self.pos += 1;
                self.primary()?;
                Expr::Unknown
            }
            Tok::Punct("@") => {
                // annotation: `@Suppress("x")` – skip
                self.pos += 1;
                if let Some(Tok::Ident(_)) = self.peek() {
                    self.pos += 1;
                }
                if self.at_punct("(") {
                    self.args();
                }
                Expr::Unknown
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_names(stmts: &[Stmt]) -> Vec<String> {
        stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::Expr(e) => e.call_name().or_else(|| match e {
                    Expr::Infix(l, op, _) => Some(format!("{}~{op}", l.call_name().unwrap_or_default())),
                    _ => None,
                }),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn kotlin_dsl() {
        let src = r#"
import org.jetbrains.kotlin.gradle.dsl.JvmTarget
plugins {
    kotlin("jvm") version "1.9.22"
    id("org.jetbrains.kotlin.plugin.serialization") version libs.versions.kotlin.get() apply false
    `java-library`
    alias(libs.plugins.ksp)
}
val ktorVersion: String by project
group = "com.example"
dependencies {
    implementation("io.ktor:ktor-server-core:$ktorVersion")
    implementation(libs.bundles.ktor)
    testImplementation(kotlin("test"))
    implementation(project(":core")) {
        exclude(group = "org.slf4j", module = "slf4j-api")
    }
    api(platform("org.jetbrains.kotlinx:kotlinx-coroutines-bom:1.8.0"))
}
kotlin { jvmToolchain(17) }
tasks.withType<Test> { useJUnitPlatform() }
"#;
        let stmts = parse(src);
        let Stmt::Expr(Expr::Call { closure: Some(plugins), .. }) = &stmts[0] else { panic!("{:?}", stmts[0]) };
        assert_eq!(call_names(plugins), vec!["kotlin~version", "~apply", "alias"]);
        assert!(matches!(&plugins[2], Stmt::Expr(Expr::Ident(id)) if id == "java-library"));
        assert!(matches!(&stmts[1], Stmt::Var { name, delegate: Some(_), .. } if name == "ktorVersion"));
        assert!(matches!(&stmts[2], Stmt::Assign { op, .. } if op == "="));
        let Stmt::Expr(Expr::Call { closure: Some(deps), .. }) = &stmts[3] else { panic!() };
        assert_eq!(call_names(deps), vec!["implementation", "implementation", "testImplementation", "implementation", "api"]);
        let Stmt::Expr(Expr::Call { args, .. }) = &deps[0] else { panic!() };
        assert_eq!(args[0].value, Expr::Str(vec![StrPart::Lit("io.ktor:ktor-server-core:".into()), StrPart::Ref("ktorVersion".into())]));
        assert_eq!(stmts.len(), 6);
    }

    #[test]
    fn groovy_dsl() {
        let src = r#"
plugins {
    id 'java'
    id 'org.jetbrains.kotlin.jvm' version '1.8.0'
}
ext.kotlin_version = '1.8.0'
ext {
    retrofit = "2.9.0"
}
repositories {
    mavenCentral()
    maven { url 'https://jitpack.io' }
}
dependencies {
    implementation "org.jetbrains.kotlin:kotlin-stdlib:$kotlin_version"
    implementation group: 'com.squareup.retrofit2', name: 'retrofit', version: "${retrofit}"
    implementation('com.google.guava:guava:31.1-jre') {
        exclude group: 'com.google.code.findbugs'
    }
    testImplementation 'junit:junit:4.13.2'
    implementation fileTree(dir: 'libs', include: ['*.jar'])
}
apply plugin: 'kotlin-kapt'
if (project.hasProperty('x')) {
    dependencies { implementation 'a:b:1' }
}
"#;
        let stmts = parse(src);
        let Stmt::Expr(Expr::Call { closure: Some(plugins), .. }) = &stmts[0] else { panic!() };
        assert_eq!(call_names(plugins), vec!["id", "id~version"]);
        let Stmt::Expr(Expr::Call { closure: Some(deps), .. }) = &stmts[4] else { panic!("{:?}", stmts[4]) };
        assert_eq!(call_names(deps), vec!["implementation", "implementation", "implementation", "testImplementation", "implementation"]);
        let Stmt::Expr(Expr::Call { args, .. }) = &deps[1] else { panic!() };
        assert_eq!(args.iter().map(|a| a.name.clone().unwrap()).collect::<Vec<_>>(), vec!["group", "name", "version"]);
        let Stmt::Expr(Expr::Call { closure, .. }) = &deps[2] else { panic!() };
        assert!(closure.is_some());
        assert!(matches!(&stmts[5], Stmt::Expr(Expr::Call { args, .. }) if args[0].name.as_deref() == Some("plugin")));
        assert!(matches!(&stmts[6], Stmt::If { then, .. } if then.len() == 1));
    }

    #[test]
    fn settings() {
        let src = r#"
rootProject.name = "demo"
include(":app", ":lib:core")
include 'feature'
project(":lib:core").projectDir = file("libs/core")
dependencyResolutionManagement {
    versionCatalogs { create("deps") { from(files("gradle/deps.toml")) } }
}
"#;
        let stmts = parse(src);
        assert_eq!(stmts.len(), 5);
        assert!(matches!(&stmts[0], Stmt::Assign { target, .. } if target.path().as_deref() == Some("rootProject.name")));
    }

    #[test]
    fn never_loops() {
        for src in ["{", "}", "(", ")", "a(", "a {", "\"abc", "val", "if (", "x = ", "a.b.", "[1, 2", "dependencies { implementation(", "@", "a<b"] {
            parse(src);
        }
    }
}
