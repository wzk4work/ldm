//! A small, sandboxed interpreter for the JavaScript subset PAC files use (own code).
//!
//! Supported: `function` declarations, `var`/`let`/`const`, `if`/`else`, `return`, `while`,
//! C-style `for`, `break`/`continue`, blocks; expressions with literals (number, string, bool,
//! null, undefined, arrays), `+ - * / %`, comparisons, `== != === !==`, `&& || !`, `?:`,
//! `typeof`, assignment (`= += -=`), `++`/`--`, calls, `.length`, indexing, and common string /
//! array methods. Not supported (parse error): regular-expression literals, objects, closures over
//! local variables, `new`, `eval`, `this`, exceptions. The script has no I/O except the PAC helper
//! functions the host provides; every evaluation runs under a step, depth and size budget.
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

pub type JsResult<T> = std::result::Result<T, String>;

const MAX_STEPS: u64 = 2_000_000;
const MAX_DEPTH: usize = 64;
const MAX_STR: usize = 1 << 20;
const MAX_ARR: usize = 100_000;

// ---------------------------------------------------------------- lexer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    P(&'static str),
    Eof,
}

const PUNCT: &[&str] = &[
    "===", "!==", ">>>", "==", "!=", "<=", ">=", "&&", "||", "++", "--", "+=", "-=", "*=", "/=", "%=", "<<", ">>", "{", "}", "(", ")", "[",
    "]", ";", ",", ".", "?", ":", "+", "-", "*", "/", "%", "<", ">", "=", "!", "&", "|", "^", "~",
];

fn lex(src: &str) -> JsResult<Vec<(Tok, usize)>> {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut line = 1;
    let mut out = vec![];
    while i < b.len() {
        let c = b[i];
        if c == '\n' {
            line += 1;
            i += 1;
        } else if c.is_whitespace() || c == '\u{feff}' {
            i += 1;
        } else if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i < b.len() && !(b[i] == '*' && b.get(i + 1) == Some(&'/')) {
                if b[i] == '\n' {
                    line += 1;
                }
                i += 1;
            }
            if i >= b.len() {
                return Err(format!("line {line}: unterminated comment"));
            }
            i += 2;
        } else if c.is_ascii_digit() || (c == '.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let s = i;
            if c == '0' && matches!(b.get(i + 1), Some('x' | 'X')) {
                i += 2;
                while i < b.len() && b[i].is_ascii_hexdigit() {
                    i += 1;
                }
                let h: String = b[s + 2..i].iter().collect();
                out.push((Tok::Num(i64::from_str_radix(&h, 16).map_err(|_| format!("line {line}: bad number"))? as f64), line));
                continue;
            }
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == '.') {
                i += 1;
            }
            if i < b.len() && matches!(b[i], 'e' | 'E') {
                i += 1;
                if i < b.len() && matches!(b[i], '+' | '-') {
                    i += 1;
                }
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let t: String = b[s..i].iter().collect();
            out.push((Tok::Num(t.parse().map_err(|_| format!("line {line}: bad number {t}"))?), line));
        } else if c.is_alphabetic() || c == '_' || c == '$' {
            let s = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '$') {
                i += 1;
            }
            out.push((Tok::Ident(b[s..i].iter().collect()), line));
        } else if c == '"' || c == '\'' {
            i += 1;
            let mut s = String::new();
            loop {
                let Some(&d) = b.get(i) else { return Err(format!("line {line}: unterminated string")) };
                i += 1;
                if d == c {
                    break;
                }
                if d == '\n' {
                    return Err(format!("line {line}: newline in string"));
                }
                if d != '\\' {
                    s.push(d);
                    continue;
                }
                let Some(&e) = b.get(i) else { return Err(format!("line {line}: unterminated string")) };
                i += 1;
                match e {
                    'n' => s.push('\n'),
                    't' => s.push('\t'),
                    'r' => s.push('\r'),
                    '0' => s.push('\0'),
                    'b' => s.push('\u{8}'),
                    'f' => s.push('\u{c}'),
                    'v' => s.push('\u{b}'),
                    '\n' => line += 1,
                    'x' | 'u' => {
                        let n = if e == 'x' { 2 } else { 4 };
                        let h: String = b.get(i..i + n).ok_or(format!("line {line}: bad escape"))?.iter().collect();
                        let v = u32::from_str_radix(&h, 16).map_err(|_| format!("line {line}: bad escape"))?;
                        s.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                        i += n;
                    }
                    o => s.push(o),
                }
            }
            out.push((Tok::Str(s), line));
        } else {
            let rest: String = b[i..(i + 3).min(b.len())].iter().collect();
            let p = PUNCT.iter().find(|p| rest.starts_with(**p)).ok_or_else(|| format!("line {line}: unexpected character {c:?}"))?;
            i += p.chars().count();
            out.push((Tok::P(p), line));
        }
    }
    out.push((Tok::Eof, line));
    Ok(out)
}

// ---------------------------------------------------------------- AST + parser

#[derive(Debug)]
pub struct Func {
    pub name: String,
    params: Vec<String>,
    body: Vec<Stmt>,
}

#[derive(Debug)]
enum Expr {
    Lit(Lit),
    Ident(String),
    Array(Vec<Expr>),
    Unary(&'static str, Box<Expr>),
    Bin(&'static str, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Assign(&'static str, Box<Expr>, Box<Expr>),
    Update { prefix: bool, delta: f64, target: Box<Expr> },
    Call(Box<Expr>, Vec<Expr>),
    Member(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone)]
enum Lit {
    Num(f64),
    Str(Arc<str>),
    Bool(bool),
    Null,
    Undef,
}

#[derive(Debug)]
enum Stmt {
    Var(Vec<(String, Option<Expr>)>),
    Expr(Expr),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    Block(Vec<Stmt>),
    Return(Option<Expr>),
    While(Expr, Box<Stmt>),
    DoWhile(Box<Stmt>, Expr),
    For(Option<Box<Stmt>>, Option<Expr>, Option<Expr>, Box<Stmt>),
    Break,
    Continue,
    Func(Arc<Func>),
    Empty,
}

/// A parsed script, shareable between threads.
#[derive(Debug)]
pub struct Program {
    body: Vec<Stmt>,
}

struct Parser {
    t: Vec<(Tok, usize)>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.t[self.i].0
    }
    fn line(&self) -> usize {
        self.t[self.i].1
    }
    fn next(&mut self) -> Tok {
        let t = self.t[self.i].0.clone();
        if self.i + 1 < self.t.len() {
            self.i += 1;
        }
        t
    }
    fn is(&self, p: &str) -> bool {
        matches!(self.peek(), Tok::P(x) if *x == p)
    }
    fn is_kw(&self, k: &str) -> bool {
        matches!(self.peek(), Tok::Ident(x) if x == k)
    }
    fn eat(&mut self, p: &str) -> bool {
        if self.is(p) {
            self.next();
            true
        } else {
            false
        }
    }
    fn err<T>(&self, what: &str) -> JsResult<T> {
        Err(format!("line {}: {what} (found {:?})", self.line(), self.peek()))
    }
    fn expect(&mut self, p: &str) -> JsResult<()> {
        if self.eat(p) {
            Ok(())
        } else {
            self.err(&format!("expected '{p}'"))
        }
    }
    fn ident(&mut self) -> JsResult<String> {
        match self.next() {
            Tok::Ident(s) if !RESERVED.contains(&s.as_str()) => Ok(s),
            t => Err(format!("line {}: expected a name, found {t:?}", self.line())),
        }
    }
    /// Optional `;` (automatic semicolon insertion is approximated by making it optional).
    fn semi(&mut self) {
        self.eat(";");
    }

    fn stmt(&mut self) -> JsResult<Stmt> {
        if self.eat(";") {
            return Ok(Stmt::Empty);
        }
        if self.eat("{") {
            let mut v = vec![];
            while !self.eat("}") {
                if *self.peek() == Tok::Eof {
                    return self.err("expected '}'");
                }
                v.push(self.stmt()?);
            }
            return Ok(Stmt::Block(v));
        }
        if let Tok::Ident(k) = self.peek().clone() {
            match k.as_str() {
                "function" => {
                    self.next();
                    return Ok(Stmt::Func(Arc::new(self.func()?)));
                }
                "var" | "let" | "const" => {
                    self.next();
                    let s = self.var_decl()?;
                    self.semi();
                    return Ok(s);
                }
                "if" => {
                    self.next();
                    self.expect("(")?;
                    let c = self.expr()?;
                    self.expect(")")?;
                    let a = self.stmt()?;
                    let b = if self.is_kw("else") {
                        self.next();
                        Some(Box::new(self.stmt()?))
                    } else {
                        None
                    };
                    return Ok(Stmt::If(c, Box::new(a), b));
                }
                "return" => {
                    self.next();
                    let line = self.t[self.i - 1].1;
                    let v = if self.is(";") || self.is("}") || self.line() != line || *self.peek() == Tok::Eof {
                        None
                    } else {
                        Some(self.expr()?)
                    };
                    self.semi();
                    return Ok(Stmt::Return(v));
                }
                "while" => {
                    self.next();
                    self.expect("(")?;
                    let c = self.expr()?;
                    self.expect(")")?;
                    return Ok(Stmt::While(c, Box::new(self.stmt()?)));
                }
                "do" => {
                    self.next();
                    let body = self.stmt()?;
                    if !self.is_kw("while") {
                        return self.err("expected 'while'");
                    }
                    self.next();
                    self.expect("(")?;
                    let c = self.expr()?;
                    self.expect(")")?;
                    self.semi();
                    return Ok(Stmt::DoWhile(Box::new(body), c));
                }
                "for" => {
                    self.next();
                    self.expect("(")?;
                    let init = if self.is(";") {
                        None
                    } else if self.is_kw("var") || self.is_kw("let") || self.is_kw("const") {
                        self.next();
                        Some(Box::new(self.var_decl()?))
                    } else {
                        Some(Box::new(Stmt::Expr(self.expr()?)))
                    };
                    if self.is_kw("in") || self.is_kw("of") {
                        return self.err("for-in/for-of loops are not supported");
                    }
                    self.expect(";")?;
                    let cond = if self.is(";") { None } else { Some(self.expr()?) };
                    self.expect(";")?;
                    let step = if self.is(")") { None } else { Some(self.expr()?) };
                    self.expect(")")?;
                    return Ok(Stmt::For(init, cond, step, Box::new(self.stmt()?)));
                }
                "break" => {
                    self.next();
                    self.semi();
                    return Ok(Stmt::Break);
                }
                "continue" => {
                    self.next();
                    self.semi();
                    return Ok(Stmt::Continue);
                }
                "switch" | "try" | "throw" | "new" | "class" | "with" | "this" | "eval" | "import" | "export" => {
                    return self.err(&format!("'{k}' is not supported in PAC scripts"));
                }
                _ => {}
            }
        }
        let e = self.expr()?;
        self.semi();
        Ok(Stmt::Expr(e))
    }

    fn func(&mut self) -> JsResult<Func> {
        let name = self.ident()?;
        self.expect("(")?;
        let mut params = vec![];
        if !self.eat(")") {
            loop {
                params.push(self.ident()?);
                if self.eat(")") {
                    break;
                }
                self.expect(",")?;
            }
        }
        self.expect("{")?;
        let mut body = vec![];
        while !self.eat("}") {
            if *self.peek() == Tok::Eof {
                return self.err("expected '}'");
            }
            body.push(self.stmt()?);
        }
        Ok(Func { name, params, body })
    }

    fn var_decl(&mut self) -> JsResult<Stmt> {
        let mut v = vec![];
        loop {
            let n = self.ident()?;
            let init = if self.eat("=") { Some(self.assign()?) } else { None };
            v.push((n, init));
            if !self.eat(",") {
                break;
            }
        }
        Ok(Stmt::Var(v))
    }

    fn expr(&mut self) -> JsResult<Expr> {
        let mut e = self.assign()?;
        // comma operator: evaluate left to right, keep the last value
        while self.eat(",") {
            let r = self.assign()?;
            e = Expr::Bin(",", Box::new(e), Box::new(r));
        }
        Ok(e)
    }

    fn assign(&mut self) -> JsResult<Expr> {
        let lhs = self.cond()?;
        for op in ["=", "+=", "-=", "*=", "/=", "%="] {
            if self.is(op) {
                if !matches!(lhs, Expr::Ident(_) | Expr::Index(..)) {
                    return self.err("invalid assignment target");
                }
                self.next();
                let rhs = self.assign()?;
                return Ok(Expr::Assign(op, Box::new(lhs), Box::new(rhs)));
            }
        }
        Ok(lhs)
    }

    fn cond(&mut self) -> JsResult<Expr> {
        let c = self.or()?;
        if self.eat("?") {
            let a = self.assign()?;
            self.expect(":")?;
            let b = self.assign()?;
            return Ok(Expr::Cond(Box::new(c), Box::new(a), Box::new(b)));
        }
        Ok(c)
    }

    fn or(&mut self) -> JsResult<Expr> {
        let mut l = self.and()?;
        while self.eat("||") {
            l = Expr::Or(Box::new(l), Box::new(self.and()?));
        }
        Ok(l)
    }

    fn and(&mut self) -> JsResult<Expr> {
        let mut l = self.bin(0)?;
        while self.eat("&&") {
            l = Expr::And(Box::new(l), Box::new(self.bin(0)?));
        }
        Ok(l)
    }

    fn bin(&mut self, level: usize) -> JsResult<Expr> {
        const LEVELS: &[&[&str]] = &[
            &["|"],
            &["^"],
            &["&"],
            &["===", "!==", "==", "!="],
            &["<=", ">=", "<", ">"],
            &["<<", ">>>", ">>"],
            &["+", "-"],
            &["*", "/", "%"],
        ];
        if level == LEVELS.len() {
            return self.unary();
        }
        let mut l = self.bin(level + 1)?;
        'outer: loop {
            for op in LEVELS[level] {
                if self.is(op) {
                    self.next();
                    let r = self.bin(level + 1)?;
                    l = Expr::Bin(op, Box::new(l), Box::new(r));
                    continue 'outer;
                }
            }
            return Ok(l);
        }
    }

    fn unary(&mut self) -> JsResult<Expr> {
        for op in ["!", "-", "+", "~"] {
            if self.is(op) {
                self.next();
                return Ok(Expr::Unary(op, Box::new(self.unary()?)));
            }
        }
        if self.is_kw("typeof") {
            self.next();
            return Ok(Expr::Unary("typeof", Box::new(self.unary()?)));
        }
        for (op, d) in [("++", 1.0), ("--", -1.0)] {
            if self.is(op) {
                self.next();
                let t = self.unary()?;
                if !matches!(t, Expr::Ident(_) | Expr::Index(..)) {
                    return self.err("invalid update target");
                }
                return Ok(Expr::Update { prefix: true, delta: d, target: Box::new(t) });
            }
        }
        let e = self.postfix()?;
        for (op, d) in [("++", 1.0), ("--", -1.0)] {
            if self.is(op) {
                if !matches!(e, Expr::Ident(_) | Expr::Index(..)) {
                    return self.err("invalid update target");
                }
                self.next();
                return Ok(Expr::Update { prefix: false, delta: d, target: Box::new(e) });
            }
        }
        Ok(e)
    }

    fn postfix(&mut self) -> JsResult<Expr> {
        let mut e = self.primary()?;
        loop {
            if self.eat(".") {
                let name = match self.next() {
                    Tok::Ident(s) => s,
                    t => return Err(format!("line {}: expected a property name, found {t:?}", self.line())),
                };
                e = Expr::Member(Box::new(e), name);
            } else if self.eat("[") {
                let i = self.expr()?;
                self.expect("]")?;
                e = Expr::Index(Box::new(e), Box::new(i));
            } else if self.eat("(") {
                let mut args = vec![];
                if !self.eat(")") {
                    loop {
                        args.push(self.assign()?);
                        if self.eat(")") {
                            break;
                        }
                        self.expect(",")?;
                    }
                }
                e = Expr::Call(Box::new(e), args);
            } else {
                return Ok(e);
            }
        }
    }

    fn primary(&mut self) -> JsResult<Expr> {
        match self.next() {
            Tok::Num(n) => Ok(Expr::Lit(Lit::Num(n))),
            Tok::Str(s) => Ok(Expr::Lit(Lit::Str(s.into()))),
            Tok::P("(") => {
                let e = self.expr()?;
                self.expect(")")?;
                Ok(e)
            }
            Tok::P("[") => {
                let mut v = vec![];
                while !self.eat("]") {
                    v.push(self.assign()?);
                    if !self.eat(",") {
                        self.expect("]")?;
                        break;
                    }
                }
                Ok(Expr::Array(v))
            }
            Tok::P("/") => Err(format!("line {}: regular expressions are not supported", self.line())),
            Tok::P("{") => Err(format!("line {}: object literals are not supported", self.line())),
            Tok::Ident(s) => Ok(match s.as_str() {
                "true" => Expr::Lit(Lit::Bool(true)),
                "false" => Expr::Lit(Lit::Bool(false)),
                "null" => Expr::Lit(Lit::Null),
                "undefined" => Expr::Lit(Lit::Undef),
                "function" | "new" | "this" | "eval" | "delete" | "void" | "in" | "instanceof" => {
                    return Err(format!("line {}: '{s}' is not supported in PAC scripts", self.line()))
                }
                _ => Expr::Ident(s),
            }),
            t => Err(format!("line {}: unexpected {t:?}", self.line())),
        }
    }
}

const RESERVED: &[&str] = &[
    "var",
    "let",
    "const",
    "function",
    "if",
    "else",
    "return",
    "while",
    "do",
    "for",
    "break",
    "continue",
    "true",
    "false",
    "null",
    "undefined",
    "typeof",
    "new",
    "this",
    "switch",
    "try",
    "throw",
    "class",
    "with",
    "in",
    "of",
    "instanceof",
    "delete",
    "void",
];

pub fn parse(src: &str) -> JsResult<Program> {
    let mut p = Parser { t: lex(src)?, i: 0 };
    let mut body = vec![];
    while *p.peek() != Tok::Eof {
        body.push(p.stmt()?);
    }
    Ok(Program { body })
}

impl Program {
    pub fn has_function(&self, name: &str) -> bool {
        self.body.iter().any(|s| matches!(s, Stmt::Func(f) if f.name == name))
    }
}

// ---------------------------------------------------------------- values + interpreter

#[derive(Debug, Clone)]
pub enum Val {
    Undef,
    Null,
    Bool(bool),
    Num(f64),
    Str(Rc<str>),
    Arr(Rc<RefCell<Vec<Val>>>),
    Func(Arc<Func>),
    Native(&'static str),
}

impl Val {
    pub fn str(s: &str) -> Val {
        Val::Str(s.into())
    }
    pub fn truthy(&self) -> bool {
        match self {
            Val::Undef | Val::Null => false,
            Val::Bool(b) => *b,
            Val::Num(n) => *n != 0.0 && !n.is_nan(),
            Val::Str(s) => !s.is_empty(),
            _ => true,
        }
    }
    pub fn num(&self) -> f64 {
        match self {
            Val::Undef => f64::NAN,
            Val::Null => 0.0,
            Val::Bool(b) => *b as u8 as f64,
            Val::Num(n) => *n,
            Val::Str(s) => {
                let t = s.trim();
                if t.is_empty() {
                    0.0
                } else if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                    i64::from_str_radix(h, 16).map(|v| v as f64).unwrap_or(f64::NAN)
                } else {
                    t.parse().unwrap_or(f64::NAN)
                }
            }
            Val::Arr(a) => {
                let a = a.borrow();
                match a.len() {
                    0 => 0.0,
                    1 => a[0].num(),
                    _ => f64::NAN,
                }
            }
            _ => f64::NAN,
        }
    }
    pub fn to_str(&self) -> String {
        match self {
            Val::Undef => "undefined".into(),
            Val::Null => "null".into(),
            Val::Bool(b) => b.to_string(),
            Val::Num(n) => fmt_num(*n),
            Val::Str(s) => s.to_string(),
            Val::Arr(a) => a
                .borrow()
                .iter()
                .map(|v| if matches!(v, Val::Undef | Val::Null) { String::new() } else { v.to_str() })
                .collect::<Vec<_>>()
                .join(","),
            Val::Func(f) => format!("function {}() {{ [code] }}", f.name),
            Val::Native(n) => format!("function {n}() {{ [native code] }}"),
        }
    }
    fn type_name(&self) -> &'static str {
        match self {
            Val::Undef => "undefined",
            Val::Null | Val::Arr(_) => "object",
            Val::Bool(_) => "boolean",
            Val::Num(_) => "number",
            Val::Str(_) => "string",
            Val::Func(_) | Val::Native(_) => "function",
        }
    }
}

fn fmt_num(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else if n == n.trunc() && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn strict_eq(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Undef, Val::Undef) | (Val::Null, Val::Null) => true,
        (Val::Bool(x), Val::Bool(y)) => x == y,
        (Val::Num(x), Val::Num(y)) => x == y,
        (Val::Str(x), Val::Str(y)) => x == y,
        (Val::Arr(x), Val::Arr(y)) => Rc::ptr_eq(x, y),
        (Val::Func(x), Val::Func(y)) => Arc::ptr_eq(x, y),
        (Val::Native(x), Val::Native(y)) => x == y,
        _ => false,
    }
}

fn loose_eq(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Undef | Val::Null, Val::Undef | Val::Null) => true,
        (Val::Undef | Val::Null, _) | (_, Val::Undef | Val::Null) => false,
        (Val::Num(_), Val::Str(_)) | (Val::Str(_), Val::Num(_)) | (Val::Bool(_), _) | (_, Val::Bool(_)) => a.num() == b.num(),
        (Val::Arr(_), Val::Str(_) | Val::Num(_)) => loose_eq(&Val::str(&a.to_str()), b),
        (Val::Str(_) | Val::Num(_), Val::Arr(_)) => loose_eq(a, &Val::str(&b.to_str())),
        _ => strict_eq(a, b),
    }
}

/// Built-in functions supplied by the embedder (the PAC helpers).
pub trait Natives {
    /// Names of the globals the embedder provides.
    fn names(&self) -> &'static [&'static str];
    fn call(&self, name: &str, args: &[Val]) -> JsResult<Val>;
}

enum Flow {
    Normal,
    Return(Val),
    Break,
    Continue,
}

pub struct Interp<'a> {
    globals: HashMap<String, Val>,
    frames: Vec<HashMap<String, Val>>,
    steps: u64,
    natives: &'a dyn Natives,
}

impl<'a> Interp<'a> {
    /// Run the top level of `prog` (function declarations and global variables).
    pub fn new(prog: &Program, natives: &'a dyn Natives) -> JsResult<Self> {
        let mut it = Interp { globals: HashMap::new(), frames: vec![], steps: 0, natives };
        for n in natives.names() {
            it.globals.insert(n.to_string(), Val::Native(n));
        }
        for n in ["parseInt", "parseFloat", "isNaN", "String", "Number", "Boolean"] {
            it.globals.insert(n.to_string(), Val::Native(n));
        }
        it.hoist(&prog.body);
        match it.block(&prog.body)? {
            Flow::Normal => Ok(it),
            _ => Err("return/break outside a function".into()),
        }
    }

    pub fn call_global(&mut self, name: &str, args: Vec<Val>) -> JsResult<Val> {
        let f = self.globals.get(name).cloned().ok_or_else(|| format!("{name} is not defined"))?;
        self.call_val(&f, args)
    }

    fn tick(&mut self) -> JsResult<()> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err("script exceeded its time budget".into());
        }
        Ok(())
    }

    fn hoist(&mut self, body: &[Stmt]) {
        for s in body {
            if let Stmt::Func(f) = s {
                self.set_local(&f.name, Val::Func(f.clone()));
            }
        }
    }

    fn set_local(&mut self, name: &str, v: Val) {
        match self.frames.last_mut() {
            Some(f) => f.insert(name.to_string(), v),
            None => self.globals.insert(name.to_string(), v),
        };
    }

    fn lookup(&self, name: &str) -> JsResult<Val> {
        if let Some(v) = self.frames.last().and_then(|f| f.get(name)) {
            return Ok(v.clone());
        }
        self.globals.get(name).cloned().ok_or_else(|| format!("{name} is not defined"))
    }

    fn assign_var(&mut self, name: &str, v: Val) {
        if let Some(f) = self.frames.last_mut() {
            if let Some(slot) = f.get_mut(name) {
                *slot = v;
                return;
            }
        }
        // undeclared assignment creates a global (sloppy mode)
        self.globals.insert(name.to_string(), v);
    }

    fn block(&mut self, body: &[Stmt]) -> JsResult<Flow> {
        for s in body {
            match self.stmt(s)? {
                Flow::Normal => {}
                f => return Ok(f),
            }
        }
        Ok(Flow::Normal)
    }

    fn stmt(&mut self, s: &Stmt) -> JsResult<Flow> {
        self.tick()?;
        Ok(match s {
            Stmt::Empty | Stmt::Func(_) => Flow::Normal,
            Stmt::Var(v) => {
                for (n, init) in v {
                    let val = match init {
                        Some(e) => self.expr(e)?,
                        None => match self.frames.last().and_then(|f| f.get(n)).or_else(|| {
                            if self.frames.is_empty() {
                                self.globals.get(n)
                            } else {
                                None
                            }
                        }) {
                            Some(old) => old.clone(),
                            None => Val::Undef,
                        },
                    };
                    self.set_local(n, val);
                }
                Flow::Normal
            }
            Stmt::Expr(e) => {
                self.expr(e)?;
                Flow::Normal
            }
            Stmt::If(c, a, b) => {
                if self.expr(c)?.truthy() {
                    self.stmt(a)?
                } else if let Some(b) = b {
                    self.stmt(b)?
                } else {
                    Flow::Normal
                }
            }
            Stmt::Block(v) => {
                self.hoist(v);
                self.block(v)?
            }
            Stmt::Return(e) => Flow::Return(match e {
                Some(e) => self.expr(e)?,
                None => Val::Undef,
            }),
            Stmt::Break => Flow::Break,
            Stmt::Continue => Flow::Continue,
            Stmt::While(c, body) => {
                while self.expr(c)?.truthy() {
                    match self.stmt(body)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        _ => {}
                    }
                }
                Flow::Normal
            }
            Stmt::DoWhile(body, c) => {
                loop {
                    match self.stmt(body)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        _ => {}
                    }
                    if !self.expr(c)?.truthy() {
                        break;
                    }
                }
                Flow::Normal
            }
            Stmt::For(init, cond, step, body) => {
                if let Some(i) = init {
                    self.stmt(i)?;
                }
                loop {
                    if let Some(c) = cond {
                        if !self.expr(c)?.truthy() {
                            break;
                        }
                    }
                    match self.stmt(body)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        _ => {}
                    }
                    if let Some(s) = step {
                        self.expr(s)?;
                    }
                }
                Flow::Normal
            }
        })
    }

    fn call_val(&mut self, f: &Val, args: Vec<Val>) -> JsResult<Val> {
        match f {
            Val::Func(f) => {
                if self.frames.len() >= MAX_DEPTH {
                    return Err("too much recursion".into());
                }
                let mut frame = HashMap::new();
                for (i, p) in f.params.iter().enumerate() {
                    frame.insert(p.clone(), args.get(i).cloned().unwrap_or(Val::Undef));
                }
                frame.insert("arguments".into(), Val::Arr(Rc::new(RefCell::new(args))));
                self.frames.push(frame);
                self.hoist(&f.body);
                let r = self.block(&f.body);
                self.frames.pop();
                Ok(match r? {
                    Flow::Return(v) => v,
                    _ => Val::Undef,
                })
            }
            Val::Native(n) => self.native(n, &args),
            v => Err(format!("{} is not a function", v.type_name())),
        }
    }

    fn native(&mut self, n: &str, a: &[Val]) -> JsResult<Val> {
        let arg = |i: usize| a.get(i).cloned().unwrap_or(Val::Undef);
        Ok(match n {
            "parseInt" => {
                let s = arg(0).to_str();
                let radix = a.get(1).map(|r| r.num()).filter(|r| !r.is_nan() && *r != 0.0).unwrap_or(10.0) as u32;
                let t = s.trim_start();
                let (neg, t) = match t.strip_prefix('-') {
                    Some(r) => (true, r),
                    None => (false, t.strip_prefix('+').unwrap_or(t)),
                };
                let (radix, t) = match (radix, t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))) {
                    (16, Some(r)) | (10, Some(r)) if a.get(1).is_none() || radix == 16 => (16, r),
                    _ => (radix, t),
                };
                if !(2..=36).contains(&radix) {
                    return Ok(Val::Num(f64::NAN));
                }
                let digits: String = t.chars().take_while(|c| c.is_digit(radix)).collect();
                match i64::from_str_radix(&digits, radix) {
                    Ok(v) => Val::Num(if neg { -(v as f64) } else { v as f64 }),
                    Err(_) => Val::Num(f64::NAN),
                }
            }
            "parseFloat" => {
                let s = arg(0).to_str();
                let t = s.trim_start();
                let end = (1..=t.len()).rev().find(|&i| t.is_char_boundary(i) && t[..i].parse::<f64>().is_ok()).unwrap_or(0);
                Val::Num(if end == 0 { f64::NAN } else { t[..end].parse().unwrap_or(f64::NAN) })
            }
            "isNaN" => Val::Bool(arg(0).num().is_nan()),
            "String" => Val::str(&arg(0).to_str()),
            "Number" => Val::Num(if a.is_empty() { 0.0 } else { arg(0).num() }),
            "Boolean" => Val::Bool(arg(0).truthy()),
            _ => self.natives.call(n, a)?,
        })
    }

    fn checked_str(s: String) -> JsResult<Val> {
        if s.len() > MAX_STR {
            return Err("string too long".into());
        }
        Ok(Val::Str(s.into()))
    }

    fn expr(&mut self, e: &Expr) -> JsResult<Val> {
        self.tick()?;
        Ok(match e {
            Expr::Lit(l) => match l {
                Lit::Num(n) => Val::Num(*n),
                Lit::Str(s) => Val::str(s),
                Lit::Bool(b) => Val::Bool(*b),
                Lit::Null => Val::Null,
                Lit::Undef => Val::Undef,
            },
            Expr::Ident(n) => self.lookup(n)?,
            Expr::Array(v) => {
                let mut out = Vec::with_capacity(v.len());
                for x in v {
                    out.push(self.expr(x)?);
                }
                Val::Arr(Rc::new(RefCell::new(out)))
            }
            Expr::Unary(op, x) => {
                if *op == "typeof" {
                    if let Expr::Ident(n) = &**x {
                        return Ok(Val::str(self.lookup(n).map(|v| v.type_name()).unwrap_or("undefined")));
                    }
                    return Ok(Val::str(self.expr(x)?.type_name()));
                }
                let v = self.expr(x)?;
                match *op {
                    "!" => Val::Bool(!v.truthy()),
                    "-" => Val::Num(-v.num()),
                    "+" => Val::Num(v.num()),
                    _ => Val::Num(!(to_i32(v.num())) as f64),
                }
            }
            Expr::And(a, b) => {
                let l = self.expr(a)?;
                if !l.truthy() {
                    l
                } else {
                    self.expr(b)?
                }
            }
            Expr::Or(a, b) => {
                let l = self.expr(a)?;
                if l.truthy() {
                    l
                } else {
                    self.expr(b)?
                }
            }
            Expr::Cond(c, a, b) => {
                if self.expr(c)?.truthy() {
                    self.expr(a)?
                } else {
                    self.expr(b)?
                }
            }
            Expr::Bin(op, a, b) => {
                let l = self.expr(a)?;
                let r = self.expr(b)?;
                self.binop(op, l, r)?
            }
            Expr::Assign(op, t, x) => {
                let v = self.expr(x)?;
                let v = if *op == "=" {
                    v
                } else {
                    let old = self.expr(t)?;
                    self.binop(&op[..1], old, v)?
                };
                self.store(t, v.clone())?;
                v
            }
            Expr::Update { prefix, delta, target } => {
                let old = self.expr(target)?.num();
                let new = old + delta;
                self.store(target, Val::Num(new))?;
                Val::Num(if *prefix { new } else { old })
            }
            Expr::Member(o, name) => {
                let o = self.expr(o)?;
                match (&o, name.as_str()) {
                    (Val::Str(s), "length") => Val::Num(s.encode_utf16().count() as f64),
                    (Val::Arr(a), "length") => Val::Num(a.borrow().len() as f64),
                    (Val::Undef | Val::Null, _) => return Err(format!("cannot read property '{name}' of {}", o.to_str())),
                    _ => Val::Undef,
                }
            }
            Expr::Index(o, i) => {
                let o = self.expr(o)?;
                let i = self.expr(i)?;
                match &o {
                    Val::Arr(a) => {
                        let n = i.num();
                        if n >= 0.0 && n == n.trunc() {
                            a.borrow().get(n as usize).cloned().unwrap_or(Val::Undef)
                        } else if i.to_str() == "length" {
                            Val::Num(a.borrow().len() as f64)
                        } else {
                            Val::Undef
                        }
                    }
                    Val::Str(s) => {
                        let n = i.num();
                        let u: Vec<u16> = s.encode_utf16().collect();
                        if n >= 0.0 && n == n.trunc() && (n as usize) < u.len() {
                            Val::str(&String::from_utf16_lossy(&u[n as usize..n as usize + 1]))
                        } else if i.to_str() == "length" {
                            Val::Num(u.len() as f64)
                        } else {
                            Val::Undef
                        }
                    }
                    Val::Undef | Val::Null => return Err(format!("cannot index {}", o.to_str())),
                    _ => Val::Undef,
                }
            }
            Expr::Call(callee, args) => {
                let mut av = Vec::with_capacity(args.len());
                if let Expr::Member(obj, m) = &**callee {
                    let o = self.expr(obj)?;
                    for x in args {
                        av.push(self.expr(x)?);
                    }
                    return self.method(&o, m, &av);
                }
                let f = self.expr(callee)?;
                for x in args {
                    av.push(self.expr(x)?);
                }
                self.call_val(&f, av)?
            }
        })
    }

    fn store(&mut self, t: &Expr, v: Val) -> JsResult<()> {
        match t {
            Expr::Ident(n) => {
                self.assign_var(n, v);
                Ok(())
            }
            Expr::Index(o, i) => {
                let o = self.expr(o)?;
                let i = self.expr(i)?.num();
                match o {
                    Val::Arr(a) if i >= 0.0 && i == i.trunc() && (i as usize) < MAX_ARR => {
                        let mut a = a.borrow_mut();
                        let i = i as usize;
                        if i >= a.len() {
                            a.resize(i + 1, Val::Undef);
                        }
                        a[i] = v;
                        Ok(())
                    }
                    _ => Err("unsupported assignment target".into()),
                }
            }
            _ => Err("invalid assignment target".into()),
        }
    }

    fn binop(&mut self, op: &str, l: Val, r: Val) -> JsResult<Val> {
        Ok(match op {
            "," => r,
            "+" => {
                let lp = matches!(l, Val::Str(_) | Val::Arr(_));
                let rp = matches!(r, Val::Str(_) | Val::Arr(_));
                if lp || rp {
                    let mut s = l.to_str();
                    s.push_str(&r.to_str());
                    Self::checked_str(s)?
                } else {
                    Val::Num(l.num() + r.num())
                }
            }
            "-" => Val::Num(l.num() - r.num()),
            "*" => Val::Num(l.num() * r.num()),
            "/" => Val::Num(l.num() / r.num()),
            "%" => Val::Num(l.num() % r.num()),
            "==" => Val::Bool(loose_eq(&l, &r)),
            "!=" => Val::Bool(!loose_eq(&l, &r)),
            "===" => Val::Bool(strict_eq(&l, &r)),
            "!==" => Val::Bool(!strict_eq(&l, &r)),
            "<" | ">" | "<=" | ">=" => {
                let ord = match (&l, &r) {
                    (Val::Str(a), Val::Str(b)) => Some(a.encode_utf16().cmp(b.encode_utf16())),
                    _ => l.num().partial_cmp(&r.num()),
                };
                Val::Bool(match ord {
                    None => false,
                    Some(o) => match op {
                        "<" => o.is_lt(),
                        ">" => o.is_gt(),
                        "<=" => o.is_le(),
                        _ => o.is_ge(),
                    },
                })
            }
            "&" => Val::Num((to_i32(l.num()) & to_i32(r.num())) as f64),
            "|" => Val::Num((to_i32(l.num()) | to_i32(r.num())) as f64),
            "^" => Val::Num((to_i32(l.num()) ^ to_i32(r.num())) as f64),
            "<<" => Val::Num(to_i32(l.num()).wrapping_shl(to_i32(r.num()) as u32 & 31) as f64),
            ">>" => Val::Num(to_i32(l.num()).wrapping_shr(to_i32(r.num()) as u32 & 31) as f64),
            ">>>" => Val::Num((to_i32(l.num()) as u32).wrapping_shr(to_i32(r.num()) as u32 & 31) as f64),
            _ => return Err(format!("unsupported operator {op}")),
        })
    }

    fn method(&mut self, o: &Val, m: &str, a: &[Val]) -> JsResult<Val> {
        let arg = |i: usize| a.get(i).cloned().unwrap_or(Val::Undef);
        // integer argument with JS-ish defaults/clamping (UTF-16 indices)
        fn idx(v: Option<&Val>, len: usize, default: usize, neg_from_end: bool) -> usize {
            match v {
                None | Some(Val::Undef) => default,
                Some(v) => {
                    let n = v.num();
                    let n = if n.is_nan() { 0.0 } else { n.trunc() };
                    if n < 0.0 {
                        if neg_from_end {
                            (len as f64 + n).max(0.0) as usize
                        } else {
                            0
                        }
                    } else {
                        (n as usize).min(len)
                    }
                }
            }
        }
        match o {
            Val::Str(s) => {
                let u: Vec<u16> = s.encode_utf16().collect();
                let len = u.len();
                let sub = |a: usize, b: usize| Val::str(&String::from_utf16_lossy(&u[a.min(b)..b.max(a)]));
                let find = |needle: &str, from: usize, last: bool| -> f64 {
                    let n: Vec<u16> = needle.encode_utf16().collect();
                    if n.len() > len {
                        return -1.0;
                    }
                    let range: Box<dyn Iterator<Item = usize>> =
                        if last { Box::new((0..=from.min(len - n.len())).rev()) } else { Box::new(from..=len - n.len()) };
                    for i in range {
                        if u[i..i + n.len()] == n[..] {
                            return i as f64;
                        }
                    }
                    -1.0
                };
                Ok(match m {
                    "toLowerCase" | "toLocaleLowerCase" => Val::str(&s.to_lowercase()),
                    "toUpperCase" | "toLocaleUpperCase" => Val::str(&s.to_uppercase()),
                    "trim" => Val::str(s.trim()),
                    "trimStart" | "trimLeft" => Val::str(s.trim_start()),
                    "trimEnd" | "trimRight" => Val::str(s.trim_end()),
                    "toString" | "valueOf" => o.clone(),
                    "indexOf" => Val::Num(find(&arg(0).to_str(), idx(a.get(1), len, 0, false), false)),
                    "lastIndexOf" => Val::Num(find(&arg(0).to_str(), idx(a.get(1), len, len, false), true)),
                    "includes" => Val::Bool(find(&arg(0).to_str(), idx(a.get(1), len, 0, false), false) >= 0.0),
                    "startsWith" => {
                        let p = idx(a.get(1), len, 0, false);
                        Val::Bool(String::from_utf16_lossy(&u[p..]).starts_with(&arg(0).to_str()))
                    }
                    "endsWith" => {
                        let e = idx(a.get(1), len, len, false);
                        Val::Bool(String::from_utf16_lossy(&u[..e]).ends_with(&arg(0).to_str()))
                    }
                    "charAt" => {
                        let i = arg(0).num();
                        let i = if i.is_nan() { 0.0 } else { i };
                        if i >= 0.0 && (i as usize) < len {
                            sub(i as usize, i as usize + 1)
                        } else {
                            Val::str("")
                        }
                    }
                    "charCodeAt" => {
                        let i = arg(0).num();
                        let i = if i.is_nan() { 0.0 } else { i };
                        Val::Num(if i >= 0.0 && (i as usize) < len { u[i as usize] as f64 } else { f64::NAN })
                    }
                    "substring" => sub(idx(a.first(), len, 0, false), idx(a.get(1), len, len, false)),
                    "slice" => {
                        let b = idx(a.first(), len, 0, true);
                        let e = idx(a.get(1), len, len, true);
                        if b < e {
                            sub(b, e)
                        } else {
                            Val::str("")
                        }
                    }
                    "substr" => {
                        let b = idx(a.first(), len, 0, true);
                        let n = match a.get(1) {
                            None | Some(Val::Undef) => len - b,
                            Some(v) => (v.num().max(0.0) as usize).min(len - b),
                        };
                        sub(b, b + n)
                    }
                    "split" => {
                        let parts: Vec<Val> = match a.first() {
                            None | Some(Val::Undef) => vec![o.clone()],
                            Some(sep) => {
                                let sep = sep.to_str();
                                if sep.is_empty() {
                                    s.chars().map(|c| Val::str(&c.to_string())).collect()
                                } else {
                                    s.split(sep.as_str()).map(Val::str).collect()
                                }
                            }
                        };
                        if parts.len() > MAX_ARR {
                            return Err("array too long".into());
                        }
                        Val::Arr(Rc::new(RefCell::new(parts)))
                    }
                    "replace" | "replaceAll" => {
                        let pat = arg(0).to_str();
                        let rep = arg(1).to_str();
                        if matches!(arg(1), Val::Func(_) | Val::Native(_)) {
                            return Err("replace() with a callback is not supported".into());
                        }
                        Self::checked_str(if m == "replace" { s.replacen(pat.as_str(), &rep, 1) } else { s.replace(pat.as_str(), &rep) })?
                    }
                    "concat" => {
                        let mut out = s.to_string();
                        for x in a {
                            out.push_str(&x.to_str());
                        }
                        Self::checked_str(out)?
                    }
                    "repeat" => {
                        let n = arg(0).num();
                        if !(0.0..=1e6).contains(&n) || s.len().saturating_mul(n as usize) > MAX_STR {
                            return Err("repeat count out of range".into());
                        }
                        Val::str(&s.repeat(n as usize))
                    }
                    _ => return Err(format!("string method '{m}' is not supported")),
                })
            }
            Val::Arr(arr) => Ok(match m {
                "join" => {
                    let sep = match a.first() {
                        None | Some(Val::Undef) => ",".to_string(),
                        Some(v) => v.to_str(),
                    };
                    let s = arr
                        .borrow()
                        .iter()
                        .map(|v| if matches!(v, Val::Undef | Val::Null) { String::new() } else { v.to_str() })
                        .collect::<Vec<_>>()
                        .join(&sep);
                    Self::checked_str(s)?
                }
                "indexOf" => Val::Num(arr.borrow().iter().position(|v| strict_eq(v, &arg(0))).map(|i| i as f64).unwrap_or(-1.0)),
                "includes" => Val::Bool(arr.borrow().iter().any(|v| strict_eq(v, &arg(0)))),
                "push" => {
                    let mut v = arr.borrow_mut();
                    if v.len() + a.len() > MAX_ARR {
                        return Err("array too long".into());
                    }
                    v.extend(a.iter().cloned());
                    Val::Num(v.len() as f64)
                }
                "pop" => arr.borrow_mut().pop().unwrap_or(Val::Undef),
                "shift" => {
                    let mut v = arr.borrow_mut();
                    if v.is_empty() {
                        Val::Undef
                    } else {
                        v.remove(0)
                    }
                }
                "slice" => {
                    let v = arr.borrow();
                    let b = idx(a.first(), v.len(), 0, true);
                    let e = idx(a.get(1), v.len(), v.len(), true);
                    Val::Arr(Rc::new(RefCell::new(if b < e { v[b..e].to_vec() } else { vec![] })))
                }
                "toString" => Val::str(&o.to_str()),
                _ => return Err(format!("array method '{m}' is not supported")),
            }),
            Val::Num(n) if m == "toString" => Ok(Val::str(&fmt_num(*n))),
            Val::Bool(_) if m == "toString" => Ok(Val::str(&o.to_str())),
            Val::Undef | Val::Null => Err(format!("cannot call '{m}' on {}", o.to_str())),
            _ => Err(format!("method '{m}' is not supported on {}", o.type_name())),
        }
    }
}

fn to_i32(n: f64) -> i32 {
    if !n.is_finite() {
        return 0;
    }
    (n.trunc() as i64 as u64 & 0xffff_ffff) as u32 as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoNatives;
    impl Natives for NoNatives {
        fn names(&self) -> &'static [&'static str] {
            &["twice"]
        }
        fn call(&self, _name: &str, args: &[Val]) -> JsResult<Val> {
            Ok(Val::Num(args.first().map(|v| v.num()).unwrap_or(0.0) * 2.0))
        }
    }

    fn run(src: &str, f: &str, args: Vec<Val>) -> JsResult<Val> {
        let p = parse(src)?;
        let mut it = Interp::new(&p, &NoNatives)?;
        it.call_global(f, args)
    }

    fn s(src: &str) -> String {
        run(&format!("function f() {{ {src} }}"), "f", vec![]).unwrap().to_str()
    }

    #[test]
    fn expressions() {
        assert_eq!(s("return 1 + 2 * 3;"), "7");
        assert_eq!(s("return 'a' + 1 + 2;"), "a12");
        assert_eq!(s("return 1 + 2 + 'a';"), "3a");
        assert_eq!(s("return 7 % 3 + 10 / 4;"), "3.5");
        assert_eq!(s("return (1 < 2) && 'yes' || 'no';"), "yes");
        assert_eq!(s("return null == undefined && '1' == 1 && !('1' === 1);"), "true");
        assert_eq!(s("return 0 ? 'a' : '' ? 'b' : 'c';"), "c");
        assert_eq!(s("return typeof x + typeof 1 + typeof 's' + typeof twice;"), "undefinednumberstringfunction");
        assert_eq!(s("return (0xff & 0x0f) | (1 << 4);"), "31");
        assert_eq!(s("return -1 >>> 28;"), "15");
        assert_eq!(s("return 'b' > 'a' && 10 > 9 && '10' < '9';"), "true");
        assert_eq!(s("return twice(21);"), "42");
        assert_eq!(s("return parseInt('0x1F') + parseInt('12px') + parseFloat('1.5e1x');"), "58");
    }

    #[test]
    fn statements_and_strings() {
        let src = r#"
            var DOMAINS = ["a.example", ".b.example"];   // globals are allowed
            function helper(h) { return h.toLowerCase(); }
            function FindProxyForURL(url, host) {
                host = helper(host);
                var i, n = 0;
                for (i = 0; i < DOMAINS.length; i++) {
                    if (host == DOMAINS[i] || host.endsWith(DOMAINS[i])) { n++; }
                }
                while (n > 5) { n--; }
                do { n += 10; } while (false);
                /* block comment */
                if (n > 10) return "PROXY p:1; DIRECT";
                else if (url.substring(0, 5) == "https") return 'PROXY s:2';
                return "DIRECT";
            }"#;
        let call = |u: &str, h: &str| run(src, "FindProxyForURL", vec![Val::str(u), Val::str(h)]).unwrap().to_str();
        assert_eq!(call("http://x.b.example/", "X.B.example"), "PROXY p:1; DIRECT");
        assert_eq!(call("https://other/", "other"), "PROXY s:2");
        assert_eq!(call("http://other/", "other"), "DIRECT");
        assert_eq!(s("return 'Hello'.charAt(1) + 'abc'.indexOf('c') + 'a.b.c'.split('.').join('-') + 'abcdef'.slice(-2);"), "e2a-b-cef");
        assert_eq!(s("return 'abcdef'.substr(1, 2) + 'abcdef'.substring(4, 2) + 'x'.repeat(3) + ' t '.trim();"), "bccdxxxt");
        assert_eq!(s("var a = [1]; a.push(2, 3); a[5] = 6; return a.length + ':' + a + ':' + a.indexOf(3);"), "6:1,2,3,,,6:2");
        assert_eq!(s("var k = 0; for (var i = 0; i < 10; i++) { if (i % 2) continue; if (i > 6) break; k += i; } return k;"), "12");
        assert_eq!(s("return arguments.length;"), "0");
    }

    #[test]
    fn sandbox_limits_and_unsupported_syntax() {
        assert!(run("function f() { while (true) {} }", "f", vec![]).unwrap_err().contains("budget"));
        assert!(run("function f() { return f(); }", "f", vec![]).unwrap_err().contains("recursion"));
        assert!(run("function f() { var s = 'x'; while (true) { s = s + s; } }", "f", vec![]).unwrap_err().contains("too long"));
        for bad in [
            "var r = /a+/;",
            "var o = {a: 1};",
            "var x = new Date();",
            "eval('1')",
            "try { } catch (e) { }",
            "function f() { return this; }",
        ] {
            assert!(parse(bad).is_err() || run(bad, "f", vec![]).is_err(), "{bad} should be rejected");
        }
        assert!(run("function f() { return nope(); }", "f", vec![]).unwrap_err().contains("not defined"));
        assert!(run("function f() { return undefined.x; }", "f", vec![]).is_err());
        assert!(parse("function f( { }").is_err());
        assert!(parse("var s = 'unterminated").is_err());
    }
}
