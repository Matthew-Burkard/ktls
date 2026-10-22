//! Control flow and data flow of local variables in a function body: definite assignment
//! (`UNINITIALIZED_VARIABLE`, `VAL_REASSIGNMENT` of `val`s initialized later), values that
//! are never read (`UNUSED_VALUE`, `VARIABLE_WITH_REDUNDANT_INITIALIZER`,
//! `ASSIGNED_BUT_NEVER_ACCESSED_VARIABLE`), and code that no path reaches.
//!
//! The analysis walks the syntax tree in evaluation order. A state holds, for each local
//! of the body, whether it is assigned on every path and which assignments may still be
//! read; branches are joined (`if`, `when`, `&&`, `?:`, `try`), loops are walked twice
//! (the second time from the join with their back edges), and `return`, `throw`, `break`,
//! `continue` and calls returning `Nothing` end a path. Lambdas, local functions and
//! classes capture locals: assignments in them make a local's state unknown, and a local
//! they read or write is never reported for unused values.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::super::*;
use crate::analysis::ide::infer::{Def, Engine, LocalKind};
use crate::symbols::{SymbolKind, TypeRef};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    for root in cx.root.descendants().filter(is_root) {
        if in_error(&root) {
            continue;
        }
        let body: Vec<SyntaxNode> = match root.kind() {
            SyntaxKind::LAMBDA_EXPR => root.children().filter(|c| c.kind() == SyntaxKind::BLOCK).collect(),
            _ => root.children().filter(|c| c.kind() == SyntaxKind::BLOCK).chain(initializer(&root).filter(|_| root.kind() == SyntaxKind::FUN)).collect(),
        };
        if body.is_empty() {
            continue;
        }
        let mut a = Analysis::new(e, &root);
        if a.vars.is_empty() && !body.iter().any(|b| b.descendants().any(|d| matches!(d.kind(), SyntaxKind::RETURN_EXPR | SyntaxKind::THROW_EXPR | SyntaxKind::BREAK_EXPR | SyntaxKind::CONTINUE_EXPR | SyntaxKind::CALL_EXPR))) {
            continue;
        }
        let mut st = a.initial();
        for b in &body {
            st = a.walk(b, st);
        }
        a.finish(out);
    }
}

/// Bodies analysed on their own: functions, accessors, initializers, constructors, lambdas.
fn is_root(n: &SyntaxNode) -> bool {
    matches!(n.kind(), SyntaxKind::FUN | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::CLASS_INITIALIZER | SyntaxKind::SECONDARY_CONSTRUCTOR | SyntaxKind::LAMBDA_EXPR)
}

/// Code with its own flow inside a body: it captures locals rather than running in order.
fn is_nested(n: &SyntaxNode) -> bool {
    is_root(n) || matches!(n.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::OBJECT_LITERAL)
}

/// Whether a local is assigned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Init {
    Assigned,
    Unassigned,
    /// Assigned on some paths only.
    Maybe,
    /// Not known (assigned in a lambda, after a `when` that may be exhaustive).
    Unknown,
}

impl Init {
    fn join(self, other: Init) -> Init {
        use Init::*;
        match (self, other) {
            (a, b) if a == b => a,
            (Maybe, _) | (_, Maybe) => Maybe,
            (Assigned, Unassigned) | (Unassigned, Assigned) => Maybe,
            (Unassigned, Unknown) | (Unknown, Unassigned) => Maybe,
            _ => Unknown,
        }
    }

    /// Possibly unassigned on a path that certainly exists.
    fn unassigned(self) -> bool {
        matches!(self, Init::Unassigned | Init::Maybe)
    }
}

#[derive(Clone, Debug)]
struct State {
    dead: bool,
    init: Vec<Init>,
    /// Assignments (indexes into `Analysis::defs`) that may still be read, per local.
    defs: Vec<BTreeSet<usize>>,
}

impl State {
    fn join(&self, other: &State) -> State {
        if self.dead {
            return other.clone();
        }
        if other.dead {
            return self.clone();
        }
        State {
            dead: false,
            init: self.init.iter().zip(&other.init).map(|(a, b)| a.join(*b)).collect(),
            defs: self.defs.iter().zip(&other.defs).map(|(a, b)| a.union(b).copied().collect()).collect(),
        }
    }

    fn dead(&self) -> State {
        State { dead: true, ..self.clone() }
    }
}

/// A local declared in the body.
struct Var {
    name: String,
    name_range: TextRange,
    decl: SyntaxNode,
    val: bool,
    initialized: bool,
    /// Read, or captured by a lambda or local declaration.
    read: bool,
    captured: bool,
    assigned: bool,
}

/// A value stored in a local: its initializer or an assignment.
struct Store {
    var: usize,
    node: SyntaxNode,
    initializer: bool,
    used: bool,
}

/// A loop being walked: where `break` and `continue` leave from.
struct Frame {
    label: Option<String>,
    breaks: Vec<State>,
    continues: Vec<State>,
}

struct Analysis<'e, 'a> {
    e: &'e Engine<'a>,
    vars: Vec<Var>,
    /// Name references (by range) of the body's locals.
    refs: HashMap<TextRange, usize>,
    defs: Vec<Store>,
    /// Store by the node it stores (loops walk an assignment more than once).
    stores: HashMap<TextRange, usize>,
    loops: Vec<Frame>,
    /// Stores made inside the `try` blocks being walked (catch and finally may see them).
    tries: Vec<Vec<usize>>,
    /// Reads of unassigned locals: (range, local).
    uninitialized: HashMap<TextRange, usize>,
    reassigned: HashSet<TextRange>,
    unreachable: Vec<TextRange>,
    depth: u32,
}

impl<'e, 'a> Analysis<'e, 'a> {
    fn new(e: &'e Engine<'a>, root: &SyntaxNode) -> Self {
        let mut vars = Vec::new();
        // Locals of this body (not of nested bodies), with a single name.
        for p in root.descendants().filter(|d| d.kind() == SyntaxKind::PROPERTY && is_local(d) && own(root, d)) {
            if p.children().any(|c| matches!(c.kind(), SyntaxKind::DESTRUCTURING_DECLARATION | SyntaxKind::PROPERTY_DELEGATE)) || has_modifier(&p, "lateinit") {
                continue;
            }
            let Some(name) = name_token(&p) else { continue };
            vars.push(Var {
                name: ast::ident_text(&name),
                name_range: name.text_range(),
                val: ast::token(&p, SyntaxKind::VAL_KW).is_some(),
                initialized: initializer(&p).is_some(),
                decl: p,
                read: false,
                captured: false,
                assigned: false,
            });
        }
        let names: HashSet<&str> = vars.iter().map(|v| v.name.as_str()).collect();
        let mut refs = HashMap::new();
        for r in root.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF) {
            let Some(t) = r.first_token() else { continue };
            if !names.contains(ast::ident_text(&t).as_str()) {
                continue;
            }
            if let [Def::Local(l)] = e.resolve_name_ref(&r).as_slice()
                && matches!(l.kind, LocalKind::Val | LocalKind::Var)
                && let Some(i) = vars.iter().position(|v| v.name_range == l.name_range)
            {
                refs.insert(r.text_range(), i);
            }
        }
        Analysis { e, vars, refs, defs: Vec::new(), stores: HashMap::new(), loops: Vec::new(), tries: Vec::new(), uninitialized: HashMap::new(), reassigned: HashSet::new(), unreachable: Vec::new(), depth: 0 }
    }

    fn initial(&self) -> State {
        State { dead: false, init: vec![Init::Unassigned; self.vars.len()], defs: vec![BTreeSet::new(); self.vars.len()] }
    }

    fn var_of(&self, n: &SyntaxNode) -> Option<usize> {
        (n.kind() == SyntaxKind::NAME_REF).then(|| self.refs.get(&n.text_range()).copied()).flatten()
    }

    fn store(&mut self, var: usize, node: &SyntaxNode, initializer: bool, st: &mut State) {
        // One store per assignment, however often loops walk it.
        let id = match self.stores.get(&node.text_range()) {
            Some(id) => *id,
            None => {
                self.defs.push(Store { var, node: node.clone(), initializer, used: false });
                self.stores.insert(node.text_range(), self.defs.len() - 1);
                self.defs.len() - 1
            }
        };
        st.defs[var] = BTreeSet::from([id]);
        st.init[var] = Init::Assigned;
        for t in &mut self.tries {
            t.push(id);
        }
    }

    fn read(&mut self, var: usize, at: &SyntaxNode, st: &State) {
        self.vars[var].read = true;
        for d in &st.defs[var] {
            self.defs[*d].used = true;
        }
        if !self.vars[var].initialized && st.init[var].unassigned() {
            self.uninitialized.insert(at.text_range(), var);
        }
    }

    /// An assignment of `var` (the value already evaluated).
    fn assign(&mut self, var: usize, target: &SyntaxNode, node: &SyntaxNode, st: &mut State) {
        self.vars[var].assigned = true;
        // A `val` initialized later may be assigned once.
        if self.vars[var].val && !self.vars[var].initialized && matches!(st.init[var], Init::Assigned | Init::Maybe) {
            self.reassigned.insert(target.text_range());
        }
        self.store(var, node, false, st);
    }

    /// Locals used inside a lambda, local function or class: reads count as reads at any
    /// later time, and assignments make the local unknown.
    fn capture(&mut self, n: &SyntaxNode, st: &mut State) {
        for r in n.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF) {
            let Some(v) = self.var_of(&r) else { continue };
            self.vars[v].captured = true;
            self.vars[v].read = true;
            for d in &st.defs[v] {
                self.defs[*d].used = true;
            }
            if assignment_target(&r) {
                self.vars[v].assigned = true;
                if st.init[v] != Init::Assigned {
                    st.init[v] = Init::Unknown;
                }
            }
        }
    }

    /// Walk `n` from `st`, returning the state after it.
    fn walk(&mut self, n: &SyntaxNode, st: State) -> State {
        if st.dead {
            return st;
        }
        if is_nested(n) {
            let mut st = st;
            self.capture(n, &mut st);
            return st;
        }
        match n.kind() {
            SyntaxKind::BLOCK => self.block(n, st),
            SyntaxKind::PROPERTY => {
                let mut st = match initializer(n) {
                    Some(init) => self.walk(&init, st),
                    None => st,
                };
                // Delegates and accessors.
                for c in n.children().filter(|c| matches!(c.kind(), SyntaxKind::PROPERTY_DELEGATE)) {
                    st = self.walk(&c, st);
                }
                if let Some(v) = self.vars.iter().position(|v| v.decl == *n) {
                    st.init[v] = Init::Unassigned;
                    st.defs[v].clear();
                    if let Some(init) = initializer(n) {
                        self.store(v, &init, true, &mut st);
                    }
                }
                st
            }
            SyntaxKind::NAME_REF => {
                if let Some(v) = self.var_of(n) {
                    self.read(v, n, &st);
                }
                st
            }
            SyntaxKind::BINARY_EXPR => self.binary(n, st),
            SyntaxKind::PREFIX_EXPR | SyntaxKind::POSTFIX_EXPR => {
                let operand = n.children().find(|c| c.kind() != SyntaxKind::OPERATION_REFERENCE);
                let increment = binary_op(n).is_some_and(|o| matches!(o.kind(), SyntaxKind::PLUS2 | SyntaxKind::MINUS2));
                match (operand.as_ref().and_then(|o| self.var_of(o)), increment) {
                    (Some(v), true) => {
                        let o = operand.unwrap();
                        let mut st = st;
                        self.read(v, &o, &st);
                        self.assign(v, &o, n, &mut st);
                        st
                    }
                    _ => self.children(n, st),
                }
            }
            SyntaxKind::IF_EXPR => {
                let mut st = st;
                if let Some(c) = n.children().find(|c| c.kind() == SyntaxKind::CONDITION) {
                    st = self.walk(&c, st);
                }
                let then = n.children().find(|c| c.kind() == SyntaxKind::THEN_BRANCH).map_or(st.clone(), |t| self.walk(&t, st.clone()));
                let otherwise = n.children().find(|c| c.kind() == SyntaxKind::ELSE_BRANCH).map_or(st.clone(), |b| self.walk(&b, st.clone()));
                then.join(&otherwise)
            }
            SyntaxKind::WHEN_EXPR => self.when(n, st),
            SyntaxKind::WHILE_EXPR | SyntaxKind::DO_WHILE_EXPR | SyntaxKind::FOR_EXPR => self.repeat(n, None, st),
            SyntaxKind::LABELED_EXPR => {
                let label = n.children().find(|c| c.kind() == SyntaxKind::LABEL).map(|l| l.text().to_string().trim_end_matches('@').to_string());
                match n.children().find(|c| matches!(c.kind(), SyntaxKind::WHILE_EXPR | SyntaxKind::DO_WHILE_EXPR | SyntaxKind::FOR_EXPR)) {
                    Some(l) => self.repeat(&l, label, st),
                    None => self.children(n, st),
                }
            }
            SyntaxKind::BREAK_EXPR | SyntaxKind::CONTINUE_EXPR => {
                let label = n.children().find(|c| c.kind() == SyntaxKind::LABEL_QUALIFIER).map(|l| l.text().to_string().trim_start_matches('@').to_string());
                let frame = match &label {
                    Some(l) => self.loops.iter_mut().rev().find(|f| f.label.as_deref() == Some(l.as_str())),
                    None => self.loops.last_mut(),
                };
                // Without a loop to leave (an error of its own) the path goes on.
                let Some(f) = frame else { return st };
                if n.kind() == SyntaxKind::BREAK_EXPR { f.breaks.push(st.clone()) } else { f.continues.push(st.clone()) }
                st.dead()
            }
            SyntaxKind::RETURN_EXPR | SyntaxKind::THROW_EXPR => {
                let st = self.children(n, st);
                st.dead()
            }
            SyntaxKind::TRY_EXPR => self.try_expr(n, st),
            SyntaxKind::CALL_EXPR => {
                let st = self.children(n, st);
                if self.returns_nothing(n) { st.dead() } else { st }
            }
            _ => self.children(n, st),
        }
    }

    /// `error()`, `TODO()`, `fail()`: every candidate of the call returns `Nothing`.
    fn returns_nothing(&self, call: &SyntaxNode) -> bool {
        let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return false };
        let defs = self.e.resolve_name_ref(&callee);
        !defs.is_empty()
            && defs.iter().all(|d| match d {
                // `Nothing?` returns (null).
                Def::Symbol { sym, .. } => sym.kind == SymbolKind::Function && sym.ty.as_ref().is_some_and(|t| matches!(self.e.resolve_declared(sym, t), TypeRef::Named { ref fq, nullable: false, .. } if fq.as_deref() == Some("kotlin.Nothing"))),
                _ => false,
            })
    }

    fn children(&mut self, n: &SyntaxNode, mut st: State) -> State {
        for c in n.children() {
            st = self.walk(&c, st);
        }
        st
    }

    /// Statements in order; the first one no path reaches starts unreachable code.
    fn block(&mut self, n: &SyntaxNode, mut st: State) -> State {
        let stmts: Vec<SyntaxNode> = n.children().collect();
        for (i, s) in stmts.iter().enumerate() {
            if st.dead {
                // A jump right before is reported by the plain unreachable-code rule.
                let after_jump = i > 0 && matches!(stmts[i - 1].kind(), SyntaxKind::RETURN_EXPR | SyntaxKind::THROW_EXPR | SyntaxKind::BREAK_EXPR | SyntaxKind::CONTINUE_EXPR);
                let declarations = stmts[i..].iter().any(|s| matches!(s.kind(), SyntaxKind::FUN | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION));
                if !after_jump && !declarations && !self.unreachable.iter().any(|r| r.contains_range(s.text_range())) {
                    let last = stmts.last().unwrap();
                    self.unreachable.push(TextRange::new(s.text_range().start(), last.text_range().end()));
                }
                break;
            }
            st = self.walk(s, st);
        }
        st
    }

    fn binary(&mut self, n: &SyntaxNode, st: State) -> State {
        let (Some(lhs), Some(rhs)) = (n.first_child(), n.children().filter(|c| c.kind() != SyntaxKind::OPERATION_REFERENCE).nth(1)) else { return self.children(n, st) };
        if let Some(op) = assignment_op(n) {
            let target = self.var_of(&lhs);
            let compound = op.kind() != SyntaxKind::EQ;
            let mut st = st;
            match target {
                Some(v) => {
                    // `x += 1` reads `x`; on a `val` it is `plusAssign`, not an assignment.
                    if compound {
                        self.read(v, &lhs, &st);
                    }
                    st = self.walk(&rhs, st);
                    if !compound || !self.vars[v].val {
                        self.assign(v, &lhs, n, &mut st);
                    }
                    st
                }
                None => {
                    st = self.walk(&lhs, st);
                    self.walk(&rhs, st)
                }
            }
        } else if binary_op(n).is_some_and(|o| matches!(o.kind(), SyntaxKind::AMP2 | SyntaxKind::PIPE2 | SyntaxKind::ELVIS)) {
            // The right side runs on some paths only.
            let left = self.walk(&lhs, st);
            let right = self.walk(&rhs, left.clone());
            left.join(&right)
        } else {
            self.children(n, st)
        }
    }

    fn when(&mut self, n: &SyntaxNode, mut st: State) -> State {
        if let Some(s) = n.children().find(|c| c.kind() == SyntaxKind::WHEN_SUBJECT) {
            st = self.walk(&s, st);
        }
        let mut result: Option<State> = None;
        let mut exhaustive = false;
        for entry in n.children().filter(|c| c.kind() == SyntaxKind::WHEN_ENTRY) {
            if ast::token(&entry, SyntaxKind::ELSE_KW).is_some() {
                exhaustive = true;
            }
            // Conditions run in order until one matches.
            for c in entry.children().filter(|c| matches!(c.kind(), SyntaxKind::WHEN_CONDITION | SyntaxKind::WHEN_GUARD)) {
                st = self.walk(&c, st);
            }
            let body = entry.children().find(|c| c.kind() == SyntaxKind::BODY).map_or(st.clone(), |b| self.walk(&b, st.clone()));
            result = Some(match result {
                Some(r) => r.join(&body),
                None => body,
            });
        }
        // Without `else` the `when` may still be exhaustive (enums, sealed classes): locals
        // left unassigned by falling through are unknown.
        if !exhaustive {
            let mut fall = st.clone();
            for i in fall.init.iter_mut() {
                if *i == Init::Unassigned {
                    *i = Init::Unknown;
                }
            }
            result = Some(match result {
                Some(r) => r.join(&fall),
                None => fall,
            });
        }
        result.unwrap_or(st)
    }

    /// `while`, `do`-`while` and `for`: the body is walked from the state before the loop,
    /// then again from its join with the ends of the body (the back edge).
    fn repeat(&mut self, n: &SyntaxNode, label: Option<String>, st: State) -> State {
        let condition = n.children().find(|c| c.kind() == SyntaxKind::CONDITION);
        let body = n.children().find(|c| c.kind() == SyntaxKind::BODY);
        let forever = condition.as_ref().is_some_and(|c| c.text() == "true") && n.kind() != SyntaxKind::FOR_EXPR;
        let mut st = st;
        if let Some(r) = n.children().find(|c| c.kind() == SyntaxKind::LOOP_RANGE) {
            st = self.walk(&r, st);
        }
        let passes = if self.depth < 4 { 2 } else { 1 };
        self.depth += 1;
        let mut entry = st.clone();
        let mut after = st.dead();
        for pass in 0..passes {
            self.loops.push(Frame { label: label.clone(), breaks: Vec::new(), continues: Vec::new() });
            let (check, end) = if n.kind() == SyntaxKind::DO_WHILE_EXPR {
                let end = body.as_ref().map_or(entry.clone(), |b| self.walk(b, entry.clone()));
                let continues: Vec<State> = self.loops.last().unwrap().continues.clone();
                let before_check = continues.iter().fold(end, |a, c| a.join(c));
                let check = condition.as_ref().map_or(before_check.clone(), |c| self.walk(c, before_check));
                (check.clone(), check)
            } else {
                let check = condition.as_ref().map_or(entry.clone(), |c| self.walk(c, entry.clone()));
                let end = body.as_ref().map_or(check.clone(), |b| self.walk(b, check.clone()));
                (check, end)
            };
            let frame = self.loops.pop().unwrap();
            let back = frame.continues.iter().fold(end, |a, c| a.join(c));
            // Leaving the loop: the condition fails (never for `while (true)`), or a `break`.
            let exit = if forever { check.dead() } else { check.clone() };
            after = frame.breaks.iter().fold(exit, |a, b| a.join(b));
            if pass + 1 < passes {
                entry = st.join(&back);
            }
        }
        self.depth -= 1;
        after
    }

    fn try_expr(&mut self, n: &SyntaxNode, st: State) -> State {
        let block = n.children().find(|c| c.kind() == SyntaxKind::BLOCK);
        self.tries.push(Vec::new());
        let end = block.as_ref().map_or(st.clone(), |b| self.walk(b, st.clone()));
        let stored = self.tries.pop().unwrap();
        for t in &mut self.tries {
            t.extend(stored.iter().copied());
        }
        // A `catch` may start anywhere in the `try`: from before it, its end, or any store.
        let mut thrown = st.join(&end);
        for d in &stored {
            let v = self.defs[*d].var;
            thrown.defs[v].insert(*d);
            if thrown.init[v] == Init::Unassigned {
                thrown.init[v] = Init::Maybe;
            }
        }
        let mut result = end;
        for c in n.children().filter(|c| c.kind() == SyntaxKind::CATCH_CLAUSE) {
            let caught = c.children().find(|b| b.kind() == SyntaxKind::BLOCK).map_or(thrown.clone(), |b| self.walk(&b, thrown.clone()));
            result = result.join(&caught);
        }
        if let Some(f) = n.children().find(|c| c.kind() == SyntaxKind::FINALLY_CLAUSE).and_then(|f| f.children().find(|b| b.kind() == SyntaxKind::BLOCK)) {
            // Run on exceptional paths too (for reads), then on the normal one.
            self.walk(&f, thrown.join(&result));
            result = self.walk(&f, result);
        }
        result
    }

    fn finish(self, out: &mut Sink) {
        for (r, v) in &self.uninitialized {
            out.report(&UNINITIALIZED_VARIABLE, *r, format!("Variable '{}' must be initialized.", self.vars[*v].name), None);
        }
        for r in &self.reassigned {
            out.report(&VAL_REASSIGNMENT, *r, "'val' cannot be reassigned.", None);
        }
        for r in &self.unreachable {
            out.report(&UNREACHABLE_CODE, *r, "Unreachable code.", None);
        }
        for (i, v) in self.vars.iter().enumerate() {
            if v.captured || v.name == "_" {
                continue;
            }
            // Assigned but never read (a never used local is UNUSED_VARIABLE).
            if !v.read {
                if !v.val
                    && v.assigned
                    && let Some(name) = name_token(&v.decl)
                {
                    out.report(&ASSIGNED_BUT_NEVER_ACCESSED_VARIABLE, name.text_range(), format!("Variable '{}' is assigned but never accessed.", v.name), None);
                }
                continue;
            }
            for d in self.defs.iter().filter(|d| d.var == i && !d.used) {
                if d.initializer {
                    out.report(&VARIABLE_WITH_REDUNDANT_INITIALIZER, d.node.text_range(), format!("Initializer of variable '{}' is redundant: its value is never read.", v.name), None);
                } else {
                    out.report(&UNUSED_VALUE, d.node.text_range(), format!("The value assigned to '{}' is never read.", v.name), None);
                }
            }
        }
    }
}

/// `d` belongs to `root` itself, not to a nested body.
fn own(root: &SyntaxNode, d: &SyntaxNode) -> bool {
    !d.ancestors().skip(1).take_while(|a| a != root).any(|a| is_nested(&a))
}

/// `r` is the target of an assignment (`r = …`, `r += …`, `r++`).
fn assignment_target(r: &SyntaxNode) -> bool {
    let Some(p) = r.parent() else { return false };
    match p.kind() {
        SyntaxKind::BINARY_EXPR => assignment_op(&p).is_some() && p.first_child().as_ref() == Some(r),
        SyntaxKind::PREFIX_EXPR | SyntaxKind::POSTFIX_EXPR => binary_op(&p).is_some_and(|o| matches!(o.kind(), SyntaxKind::PLUS2 | SyntaxKind::MINUS2)),
        _ => false,
    }
}
