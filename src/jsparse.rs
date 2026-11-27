//! A JavaScript lexer and parser: source text to a syntax tree.
//!
//! The subset covers what ordinary page scripts use: `var`/`let`/`const`, functions and arrow
//! functions, objects and arrays, the usual statements (`if`, `for`, `for..in`, `for..of`,
//! `while`, `do..while`, `switch`, `try`/`catch`/`finally`, `throw`, `break`/`continue`/`return`),
//! all the operators, `new`, template literals without substitutions and with `${}`, and automatic
//! semicolon insertion. Not covered: classes, generators, `async`/`await`, destructuring,
//! spread, getters and setters, regular expression literals, labels, `with`.
//! Hostile input can't recurse without bound (nesting is limited) or make the parser loop.
#![allow(dead_code)]
// (the tree is shared between threads as `Arc`s: scripts are parsed on a thread with a big stack)
use std::sync::Arc as Rc;

/// How deeply expressions and statements may nest before parsing gives up.
const MAX_DEPTH: usize = 300;
/// The most links in operator and call chains (`a+b+c...`, `a.b.c()()`) that may be open at once:
/// such chains build a lopsided tree that is dropped recursively, which must stay shallow.
const MAX_CHAIN: usize = 3000;
/// Template literals nested inside `${}` of other templates.
const MAX_TEMPLATE_DEPTH: usize = 20;
/// The stack of the thread a script is parsed on (it is only touched as deep as the script nests).
const PARSE_STACK: usize = 256 * 1024 * 1024;
/// The longest script we are willing to tokenize.
const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;

// ------------------------------------------------------------------ tokens

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Num(f64),
    Str(String),
    /// A template literal: the text pieces and the source of each `${...}` between them.
    Template(Vec<String>, Vec<(String, u32)>),
    Ident(String),
    Punct(&'static str),
    Eof,
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    /// A line break came before this token (automatic semicolon insertion looks at it).
    newline_before: bool,
    line: u32,
}

const PUNCTUATORS: [&str; 52] = [
    ">>>=", "...", "===", "!==", "**=", "<<=", ">>=", ">>>", "&&=", "||=", "??=", "=>", "==", "!=", "<=", ">=", "&&", "||", "??", "++",
    "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<", ">>", "**", "{", "}", "(", ")", "[", "]", ";", ",", "<", ">", "+", "-", "*", "/",
    "%", "&", "|", "^", "!", "~",
];
const SINGLES: [&str; 5] = ["?", ":", ".", "=", "@"];

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}

fn is_ident_part(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || c == '\u{200c}' || c == '\u{200d}'
}

pub struct Lexer<'a> {
    chars: Vec<char>,
    pos: usize,
    line: u32,
    template_depth: usize,
    _src: std::marker::PhantomData<&'a str>,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Lexer<'a> {
        Lexer { chars: src.chars().collect(), pos: 0, line: 1, template_depth: 0, _src: std::marker::PhantomData }
    }

    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).copied()
    }

    /// Exactly `n` hex digits (no sign, no spaces), consumed; None if they aren't there.
    fn hex_digits(&mut self, n: usize) -> Option<u32> {
        let digits: Vec<char> = (0..n).filter_map(|i| self.peek(i)).collect();
        if digits.len() != n || !digits.iter().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        self.pos += n;
        Some(digits.iter().fold(0, |acc, c| acc * 16 + c.to_digit(16).unwrap_or(0)))
    }

    fn looking_at(&self, text: &str) -> bool {
        text.chars().enumerate().all(|(i, c)| self.peek(i) == Some(c))
    }

    /// Skip whitespace and comments; returns whether a line break was crossed.
    fn skip_trivia(&mut self) -> Result<bool, String> {
        let mut newline = false;
        loop {
            match self.peek(0) {
                Some('\n' | '\r' | '\u{2028}' | '\u{2029}') => {
                    newline = true;
                    if self.peek(0) == Some('\r') && self.peek(1) == Some('\n') {
                        self.pos += 1; // (a CR LF pair is one line break)
                    }
                    self.line += 1;
                    self.pos += 1;
                }
                Some(c) if c.is_whitespace() || c == '\u{feff}' => self.pos += 1,
                Some('/') if self.peek(1) == Some('/') => {
                    while !matches!(self.peek(0), None | Some('\n' | '\r' | '\u{2028}' | '\u{2029}')) {
                        self.pos += 1;
                    }
                }
                Some('/') if self.peek(1) == Some('*') => {
                    self.pos += 2;
                    loop {
                        match self.peek(0) {
                            None => return Err(format!("line {}: unterminated comment", self.line)),
                            Some('*') if self.peek(1) == Some('/') => {
                                self.pos += 2;
                                break;
                            }
                            Some('\n' | '\r' | '\u{2028}' | '\u{2029}') => {
                                newline = true;
                                self.line += 1;
                                self.pos += 1;
                            }
                            Some(_) => self.pos += 1,
                        }
                    }
                }
                // old-style `<!--` and (at the start of a line) `-->` comments, which scripts once
                // wrapped themselves in to hide from browsers that didn't know <script>
                Some('<') if self.looking_at("<!--") => {
                    while !matches!(self.peek(0), None | Some('\n' | '\r' | '\u{2028}' | '\u{2029}')) {
                        self.pos += 1;
                    }
                }
                Some('-') if (newline || self.pos == 0) && self.looking_at("-->") => {
                    while !matches!(self.peek(0), None | Some('\n' | '\r' | '\u{2028}' | '\u{2029}')) {
                        self.pos += 1;
                    }
                }
                // a `#!` line at the very start
                Some('#') if self.pos == 0 && self.peek(1) == Some('!') => {
                    while !matches!(self.peek(0), None | Some('\n' | '\r' | '\u{2028}' | '\u{2029}')) {
                        self.pos += 1;
                    }
                }
                _ => return Ok(newline),
            }
        }
    }

    fn number(&mut self) -> Result<f64, String> {
        let start = self.pos;
        if self.peek(0) == Some('0') && matches!(self.peek(1), Some('x' | 'X' | 'o' | 'O' | 'b' | 'B')) {
            let radix = match self.peek(1) {
                Some('x' | 'X') => 16,
                Some('o' | 'O') => 8,
                _ => 2,
            };
            self.pos += 2;
            let digits_start = self.pos;
            while self.peek(0).is_some_and(|c| c.is_digit(radix) || c == '_') {
                self.pos += 1;
            }
            let text: String = self.chars[digits_start..self.pos].iter().filter(|c| **c != '_').collect();
            if text.is_empty() {
                return Err(format!("line {}: malformed number", self.line));
            }
            // (parsed digit by digit as f64: big hex literals lose precision instead of failing)
            return Ok(text.chars().fold(0.0, |acc, c| acc * radix as f64 + c.to_digit(radix).unwrap_or(0) as f64));
        }
        while self.peek(0).is_some_and(|c| c.is_ascii_digit() || c == '_') {
            self.pos += 1;
        }
        if self.peek(0) == Some('.') {
            self.pos += 1;
            while self.peek(0).is_some_and(|c| c.is_ascii_digit() || c == '_') {
                self.pos += 1;
            }
        }
        if matches!(self.peek(0), Some('e' | 'E')) {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(0), Some('+' | '-')) {
                self.pos += 1;
            }
            if self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
                while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
                    self.pos += 1;
                }
            } else {
                self.pos = save; // not an exponent after all
            }
        }
        let text: String = self.chars[start..self.pos].iter().filter(|c| **c != '_').collect();
        if self.peek(0).is_some_and(is_ident_start) {
            return Err(format!("line {}: a number can't be followed by a letter", self.line));
        }
        // a leading zero followed only by octal digits is a legacy octal number: 010 is 8
        if text.len() > 1 && text.starts_with('0') && text.chars().all(|c| c.is_ascii_digit()) && text.chars().all(|c| c < '8') {
            return Ok(text.chars().fold(0.0, |acc, c| acc * 8.0 + c.to_digit(8).unwrap_or(0) as f64));
        }
        text.parse::<f64>().map_err(|_| format!("line {}: malformed number {text}", self.line))
    }

    /// One escape sequence after the backslash, appended to `out`.
    fn escape(&mut self, out: &mut String) -> Result<(), String> {
        let Some(c) = self.peek(0) else { return Err(format!("line {}: unterminated string", self.line)) };
        self.pos += 1;
        match c {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0' if !self.peek(0).is_some_and(|d| d.is_ascii_digit()) => out.push('\0'),
            'x' => {
                let v = self.hex_digits(2).ok_or_else(|| format!("line {}: bad \\x escape", self.line))?;
                out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
            }
            'u' => {
                let v = if self.peek(0) == Some('{') {
                    let end = (self.pos..self.chars.len().min(self.pos + 10)).find(|&i| self.chars[i] == '}');
                    let Some(end) = end else { return Err(format!("line {}: bad \\u escape", self.line)) };
                    let h: String = self.chars[self.pos + 1..end].iter().collect();
                    if h.is_empty() || !h.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Err(format!("line {}: bad \\u escape", self.line));
                    }
                    self.pos = end + 1;
                    u32::from_str_radix(&h, 16).map_err(|_| format!("line {}: bad \\u escape", self.line))?
                } else {
                    let high = self.hex_digits(4).ok_or_else(|| format!("line {}: bad \\u escape", self.line))?;
                    // a surrogate pair written as two escapes is one character
                    if (0xD800..0xDC00).contains(&high) && self.peek(0) == Some('\\') && self.peek(1) == Some('u') {
                        let save = self.pos;
                        self.pos += 2;
                        match self.hex_digits(4) {
                            Some(low) if (0xDC00..0xE000).contains(&low) => 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00),
                            _ => {
                                self.pos = save;
                                high
                            }
                        }
                    } else {
                        high
                    }
                };
                out.push(char::from_u32(v).unwrap_or('\u{fffd}')); // (a lone surrogate becomes U+FFFD)
            }
            '\n' => self.line += 1, // a line continuation
            '\r' => {
                if self.peek(0) == Some('\n') {
                    self.pos += 1;
                }
                self.line += 1;
            }
            other => out.push(other),
        }
        Ok(())
    }

    fn string(&mut self, quote: char) -> Result<String, String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek(0) {
                None | Some('\n') => return Err(format!("line {}: unterminated string", self.line)),
                Some(c) if c == quote => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some('\\') => {
                    self.pos += 1;
                    self.escape(&mut out)?;
                }
                Some(c) => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// A backtick template: the literal pieces, and the raw source of each `${ ... }`.
    fn template(&mut self) -> Result<(Vec<String>, Vec<(String, u32)>), String> {
        // (templates nested inside `${}` recurse, and each level lexes its inner source again)
        self.template_depth += 1;
        if self.template_depth > MAX_TEMPLATE_DEPTH {
            return Err(format!("line {}: templates nested too deeply", self.line));
        }
        let r = self.template_inner();
        self.template_depth -= 1;
        r
    }

    /// The source of each `${}` comes with the line it starts on, for error messages.
    fn template_inner(&mut self) -> Result<(Vec<String>, Vec<(String, u32)>), String> {
        let start_line = self.line;
        self.pos += 1;
        let (mut pieces, mut exprs, mut current) = (vec![], vec![], String::new());
        loop {
            match self.peek(0) {
                None => return Err(format!("line {start_line}: unterminated template")),
                Some('`') => {
                    self.pos += 1;
                    pieces.push(current);
                    return Ok((pieces, exprs));
                }
                Some('\\') => {
                    self.pos += 1;
                    self.escape(&mut current)?;
                }
                Some('$') if self.peek(1) == Some('{') => {
                    self.pos += 2;
                    pieces.push(std::mem::take(&mut current));
                    // find the matching brace, skipping nested braces, strings and comments
                    let (start, mut depth, expr_line) = (self.pos, 1usize, self.line);
                    while depth > 0 {
                        match self.peek(0) {
                            None => return Err(format!("line {start_line}: unterminated template")),
                            Some('/') if self.peek(1) == Some('/') => {
                                while !matches!(self.peek(0), None | Some('\n' | '\r' | '\u{2028}' | '\u{2029}')) {
                                    self.pos += 1;
                                }
                                continue;
                            }
                            Some('/') if self.peek(1) == Some('*') => {
                                self.pos += 2;
                                while !(self.peek(0) == Some('*') && self.peek(1) == Some('/')) {
                                    match self.peek(0) {
                                        None => return Err(format!("line {start_line}: unterminated template")),
                                        Some('\n') => self.line += 1,
                                        _ => {}
                                    }
                                    self.pos += 1;
                                }
                                self.pos += 2;
                                continue;
                            }
                            Some('{') => depth += 1,
                            Some('}') => depth -= 1,
                            Some(q @ ('"' | '\'')) => {
                                self.string(q)?;
                                continue;
                            }
                            Some('`') => {
                                self.template()?;
                                continue;
                            }
                            Some('\n') => self.line += 1,
                            _ => {}
                        }
                        self.pos += 1;
                    }
                    exprs.push((self.chars[start..self.pos - 1].iter().collect(), expr_line));
                }
                Some(c) => {
                    if c == '\n' {
                        self.line += 1;
                    }
                    current.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    fn next_token(&mut self) -> Result<Token, String> {
        let newline_before = self.skip_trivia()?;
        let line = self.line;
        let tok = match self.peek(0) {
            None => Tok::Eof,
            Some(c) if c.is_ascii_digit() || (c == '.' && self.peek(1).is_some_and(|d| d.is_ascii_digit())) => Tok::Num(self.number()?),
            Some(c @ ('"' | '\'')) => Tok::Str(self.string(c)?),
            Some('`') => {
                let (pieces, exprs) = self.template()?;
                Tok::Template(pieces, exprs)
            }
            Some(c) if is_ident_start(c) => {
                let start = self.pos;
                while self.peek(0).is_some_and(is_ident_part) {
                    self.pos += 1;
                }
                Tok::Ident(self.chars[start..self.pos].iter().collect())
            }
            Some(c) => {
                let rest: String = (0..4).filter_map(|i| self.peek(i)).collect();
                let found = PUNCTUATORS.iter().chain(SINGLES.iter()).find(|p| rest.starts_with(**p));
                match found {
                    Some(p) => {
                        self.pos += p.len();
                        Tok::Punct(p)
                    }
                    None => return Err(format!("line {line}: unexpected character '{c}'")),
                }
            }
        };
        Ok(Token { tok, newline_before, line })
    }
}

fn tokenize(src: &str) -> Result<Vec<Token>, String> {
    tokenize_at(src, 1)
}

/// Tokenize `src`, whose first line is line `first_line` of the script.
fn tokenize_at(src: &str, first_line: u32) -> Result<Vec<Token>, String> {
    if src.len() > MAX_SOURCE_BYTES {
        return Err("script too big".into());
    }
    let mut lexer = Lexer::new(src);
    lexer.line = first_line;
    let mut out = vec![];
    loop {
        let t = lexer.next_token()?;
        let end = t.tok == Tok::Eof;
        out.push(t);
        if end {
            return Ok(out);
        }
    }
}

// ------------------------------------------------------------------ syntax tree

#[derive(Debug, PartialEq)]
pub struct Func {
    pub name: Option<String>,
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
    /// An arrow function: `this` is the enclosing one's.
    pub arrow: bool,
    /// The body mentions `arguments` (so a call has to build the arguments array).
    pub uses_arguments: bool,
    /// The last parameter is `...rest`: it receives the arguments from there on, as an array.
    pub rest: bool,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum DeclKind {
    Var,
    Let,
    Const,
}

#[derive(Debug, PartialEq, Clone)]
pub enum PropKey {
    Name(String),
    Computed(Expr),
}

#[derive(Debug, PartialEq, Clone)]
pub enum Expr {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    Ident(String),
    This,
    Template(Vec<String>, Vec<Expr>),
    Array(Vec<Expr>),
    Object(Vec<(PropKey, Expr)>),
    Function(Rc<Func>),
    Unary(&'static str, Box<Expr>),
    /// `++x`, `x--` and so on: the operator (`++` or `--`), whether it comes first, the target.
    Update(&'static str, bool, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Logical(&'static str, Box<Expr>, Box<Expr>),
    /// `=`, `+=` ...: the operator as written, the target and the value.
    Assign(&'static str, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    New(Box<Expr>, Vec<Expr>),
    /// `obj.name`
    Dot(Box<Expr>, String),
    /// `obj[expr]`
    Index(Box<Expr>, Box<Expr>),
    Seq(Vec<Expr>),
    /// `...list` inside a call's arguments or an array literal.
    Spread(Box<Expr>),
}

/// A destructuring pattern (see `Parser::pattern`); only used while parsing.
struct Pat {
    target: PatTarget,
    default: Option<Expr>,
}

enum PatTarget {
    Name(String),
    Obj(Vec<(String, Pat)>),
    /// Items (a `None` is a skipped slot) and the `...rest` name.
    Arr(Vec<Option<Pat>>, Option<String>),
}

#[derive(Debug, PartialEq)]
pub enum ForInit {
    None,
    Decl(DeclKind, Vec<(String, Option<Expr>)>),
    Expr(Expr),
}

#[derive(Debug, PartialEq)]
pub enum Stmt {
    Empty,
    Expr(Expr),
    Decl(DeclKind, Vec<(String, Option<Expr>)>),
    Function(Rc<Func>),
    Return(Option<Expr>),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    Block(Vec<Stmt>),
    For(ForInit, Option<Expr>, Option<Expr>, Box<Stmt>),
    /// `for (x in obj)` / `for (x of obj)`: whether it is `of`, the declaration kind (if any), the variable, the object.
    ForIn { of: bool, kind: Option<DeclKind>, var: String, obj: Expr, body: Box<Stmt> },
    While(Expr, Box<Stmt>),
    DoWhile(Box<Stmt>, Expr),
    Switch(Expr, Vec<(Option<Expr>, Vec<Stmt>)>),
    Break,
    Continue,
    Throw(Expr),
    Try { block: Vec<Stmt>, catch: Option<(Option<String>, Vec<Stmt>)>, finally: Option<Vec<Stmt>> },
}

// ------------------------------------------------------------------ parser

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    depth: usize,
    /// Links of operator / call chains currently being built (see `MAX_CHAIN`).
    chain: usize,
    /// `arguments` was mentioned in the function being parsed.
    arguments_seen: bool,
    /// Hidden variables made for destructuring so far (their names must not repeat).
    temps: usize,
}

type PResult<T> = Result<T, String>;

const RESERVED: [&str; 36] = [
    "break", "case", "catch", "continue", "default", "delete", "do", "else", "finally", "for", "function", "if", "in", "instanceof", "new",
    "return", "switch", "this", "throw", "try", "typeof", "var", "void", "while", "const", "let", "true", "false", "null", "class", "enum",
    "export", "import", "super", "with", "debugger",
];

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }

    fn line(&self) -> u32 {
        self.toks[self.pos].line
    }

    fn err<T>(&self, msg: &str) -> PResult<T> {
        Err(format!("line {}: {msg}", self.line()))
    }

    fn advance(&mut self) -> Tok {
        let t = self.toks[self.pos].tok.clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn is_punct(&self, p: &str) -> bool {
        matches!(self.peek(), Tok::Punct(q) if *q == p)
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Tok::Ident(i) if i == w)
    }

    fn eat_punct(&mut self, p: &str) -> bool {
        if self.is_punct(p) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if self.is_word(w) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_punct(&mut self, p: &str) -> PResult<()> {
        if self.eat_punct(p) { Ok(()) } else { self.err(&format!("expected '{p}'")) }
    }

    fn enter(&mut self) -> PResult<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH { self.err("nested too deeply") } else { Ok(()) }
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// The end of a statement: `;`, or (automatic semicolon insertion) a `}` or end of input,
    /// or a line break before the next token.
    fn semicolon(&mut self) -> PResult<()> {
        if self.eat_punct(";") || self.is_punct("}") || *self.peek() == Tok::Eof || self.toks[self.pos].newline_before {
            Ok(())
        } else {
            self.err("expected ';'")
        }
    }

    fn ident(&mut self) -> PResult<String> {
        match self.peek().clone() {
            Tok::Ident(name) if !RESERVED.contains(&name.as_str()) => {
                self.advance();
                Ok(name)
            }
            _ => self.err("expected a name"),
        }
    }

    // ---- statements

    fn program(&mut self) -> PResult<Vec<Stmt>> {
        let mut out = vec![];
        while *self.peek() != Tok::Eof {
            out.push(self.statement()?);
        }
        Ok(out)
    }

    fn block(&mut self) -> PResult<Vec<Stmt>> {
        self.expect_punct("{")?;
        let mut out = vec![];
        while !self.is_punct("}") {
            if *self.peek() == Tok::Eof {
                return self.err("missing '}'");
            }
            out.push(self.statement()?);
        }
        self.advance();
        Ok(out)
    }

    // ---- destructuring
    //
    // `{a, b: c = 1, ...}` and `[x, , y, ...rest]` on the left of a declaration, in a parameter
    // list, or in `for (const [k, v] of ...)` are written out as plain declarations: the value
    // goes into a hidden variable, and each name is read from it (`a = tmp.a`, `x = tmp[0]`).

    fn pattern_ahead(&self) -> bool {
        self.is_punct("{") || self.is_punct("[")
    }

    /// One target of a pattern, with its optional `= default`.
    fn pattern(&mut self) -> PResult<Pat> {
        self.enter()?;
        let r = self.pattern_inner();
        self.leave();
        r
    }

    fn pattern_inner(&mut self) -> PResult<Pat> {
        let target = if self.eat_punct("{") {
            let mut props = vec![];
            while !self.is_punct("}") {
                let key = match self.advance() {
                    Tok::Ident(n) => n,
                    Tok::Str(s) => s,
                    Tok::Num(n) => n.to_string(),
                    other => {
                        self.back(&other);
                        return self.err("expected a property name in the pattern");
                    }
                };
                let value = if self.eat_punct(":") {
                    self.element()?
                } else {
                    // `{a}` and `{a = 1}`
                    let default = if self.eat_punct("=") { Some(self.assignment()?) } else { None };
                    Pat { target: PatTarget::Name(key.clone()), default }
                };
                props.push((key, value));
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.expect_punct("}")?;
            PatTarget::Obj(props)
        } else if self.eat_punct("[") {
            let mut items = vec![];
            let mut rest = None;
            while !self.is_punct("]") {
                if self.eat_punct(",") {
                    items.push(None);
                    continue;
                }
                if self.eat_punct("...") {
                    rest = Some(self.ident()?);
                    break;
                }
                items.push(Some(self.element()?));
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.expect_punct("]")?;
            PatTarget::Arr(items, rest)
        } else {
            PatTarget::Name(self.ident()?)
        };
        Ok(Pat { target, default: None })
    }

    /// A pattern that may be followed by `= default` (inside another pattern, or as a parameter).
    fn element(&mut self) -> PResult<Pat> {
        let mut pat = self.pattern()?;
        if self.eat_punct("=") {
            pat.default = Some(self.assignment()?);
        }
        Ok(pat)
    }

    /// The declarations that bind `pat` to the value of `source`, in order.
    fn flatten_pattern(&mut self, pat: Pat, source: Expr, out: &mut Vec<(String, Option<Expr>)>) {
        let value = match pat.default {
            Some(d) => Expr::Cond(
                Box::new(Expr::Binary("===", Box::new(source.clone()), Box::new(Expr::Ident("undefined".into())))),
                Box::new(d),
                Box::new(source),
            ),
            None => source,
        };
        match pat.target {
            PatTarget::Name(n) => out.push((n, Some(value))),
            PatTarget::Obj(props) => {
                let tmp = self.temp_name();
                out.push((tmp.clone(), Some(value)));
                for (key, p) in props {
                    let src = Expr::Index(Box::new(Expr::Ident(tmp.clone())), Box::new(Expr::Str(key)));
                    self.flatten_pattern(p, src, out);
                }
            }
            PatTarget::Arr(items, rest) => {
                let tmp = self.temp_name();
                out.push((tmp.clone(), Some(value)));
                let count = items.len();
                for (i, p) in items.into_iter().enumerate() {
                    if let Some(p) = p {
                        let src = Expr::Index(Box::new(Expr::Ident(tmp.clone())), Box::new(Expr::Num(i as f64)));
                        self.flatten_pattern(p, src, out);
                    }
                }
                if let Some(r) = rest {
                    let slice = Expr::Call(Box::new(Expr::Dot(Box::new(Expr::Ident(tmp)), "slice".into())), vec![Expr::Num(count as f64)]);
                    out.push((r, Some(slice)));
                }
            }
        }
    }

    fn temp_name(&mut self) -> String {
        self.temps += 1;
        format!("__d{}_{}", self.toks.get(self.pos).map_or(0, |t| t.line), self.temps)
    }

    fn declarators(&mut self, kind: DeclKind) -> PResult<Vec<(String, Option<Expr>)>> {
        let mut list = vec![];
        loop {
            if self.pattern_ahead() {
                let pat = self.pattern()?;
                if !self.eat_punct("=") {
                    return self.err("a pattern needs a value");
                }
                let value = self.assignment()?;
                self.flatten_pattern(pat, value, &mut list);
                if !self.eat_punct(",") {
                    return Ok(list);
                }
                continue;
            }
            let name = self.ident()?;
            let init = if self.eat_punct("=") { Some(self.assignment()?) } else { None };
            if kind == DeclKind::Const && init.is_none() {
                return self.err("a const needs a value");
            }
            list.push((name, init));
            if !self.eat_punct(",") {
                return Ok(list);
            }
        }
    }

    fn decl_kind(&self) -> Option<DeclKind> {
        match self.peek() {
            Tok::Ident(w) if w == "var" => Some(DeclKind::Var),
            Tok::Ident(w) if w == "let" && matches!(self.peek_at(1), Tok::Ident(_) | Tok::Punct("{") | Tok::Punct("[")) => Some(DeclKind::Let),
            Tok::Ident(w) if w == "const" => Some(DeclKind::Const),
            _ => None,
        }
    }

    fn statement(&mut self) -> PResult<Stmt> {
        self.enter()?;
        let r = self.statement_inner();
        self.leave();
        r
    }

    fn statement_inner(&mut self) -> PResult<Stmt> {
        if self.eat_punct(";") {
            return Ok(Stmt::Empty);
        }
        if self.is_punct("{") {
            return Ok(Stmt::Block(self.block()?));
        }
        if let Some(kind) = self.decl_kind() {
            self.advance();
            let list = self.declarators(kind)?;
            self.semicolon()?;
            return Ok(Stmt::Decl(kind, list));
        }
        let word = match self.peek() {
            Tok::Ident(w) => w.clone(),
            _ => String::new(),
        };
        match word.as_str() {
            "function" => {
                self.advance();
                let name = self.ident()?;
                return Ok(Stmt::Function(Rc::new(self.function_rest(Some(name))?)));
            }
            "class" => {
                self.advance();
                let name = self.ident()?;
                let class = self.class_rest(Some(name.clone()))?;
                return Ok(Stmt::Decl(DeclKind::Let, vec![(name, Some(class))]));
            }
            "return" => {
                self.advance();
                // `return` followed by a line break returns nothing
                let value = if self.is_punct(";") || self.is_punct("}") || *self.peek() == Tok::Eof || self.toks[self.pos].newline_before {
                    None
                } else {
                    Some(self.expression()?)
                };
                self.semicolon()?;
                return Ok(Stmt::Return(value));
            }
            "if" => {
                self.advance();
                self.expect_punct("(")?;
                let test = self.expression()?;
                self.expect_punct(")")?;
                let then = Box::new(self.statement()?);
                let other = if self.eat_word("else") { Some(Box::new(self.statement()?)) } else { None };
                return Ok(Stmt::If(test, then, other));
            }
            "while" => {
                self.advance();
                self.expect_punct("(")?;
                let test = self.expression()?;
                self.expect_punct(")")?;
                return Ok(Stmt::While(test, Box::new(self.statement()?)));
            }
            "do" => {
                self.advance();
                let body = Box::new(self.statement()?);
                if !self.eat_word("while") {
                    return self.err("expected 'while'");
                }
                self.expect_punct("(")?;
                let test = self.expression()?;
                self.expect_punct(")")?;
                self.eat_punct(";");
                return Ok(Stmt::DoWhile(body, test));
            }
            "for" => return self.for_statement(),
            "switch" => return self.switch_statement(),
            "break" => {
                self.advance();
                self.semicolon()?;
                return Ok(Stmt::Break);
            }
            "continue" => {
                self.advance();
                self.semicolon()?;
                return Ok(Stmt::Continue);
            }
            "debugger" => {
                self.advance();
                self.semicolon()?;
                return Ok(Stmt::Empty);
            }
            "throw" => {
                self.advance();
                if self.toks[self.pos].newline_before {
                    return self.err("a line break is not allowed after 'throw'");
                }
                let value = self.expression()?;
                self.semicolon()?;
                return Ok(Stmt::Throw(value));
            }
            "try" => return self.try_statement(),
            _ => {}
        }
        let e = self.expression()?;
        self.semicolon()?;
        Ok(Stmt::Expr(e))
    }

    fn for_statement(&mut self) -> PResult<Stmt> {
        self.advance();
        self.expect_punct("(")?;
        // for (var x in/of obj) / for (x in/of obj)
        let save = self.pos;
        let kind = self.decl_kind();
        if kind.is_some() {
            self.advance();
        }
        // `for (const [k, v] of pairs)`: the pattern is bound from a hidden variable at the top of each pass
        if let (Some(k), true) = (kind, self.pattern_ahead()) {
            let pat = self.pattern()?;
            let of = match self.advance() {
                Tok::Ident(w) if w == "of" => true,
                Tok::Ident(w) if w == "in" => false,
                other => {
                    self.back(&other);
                    return self.err("expected 'of' after the pattern");
                }
            };
            let obj = if of { self.assignment()? } else { self.expression()? };
            self.expect_punct(")")?;
            let tmp = self.temp_name();
            let mut decls = vec![];
            self.flatten_pattern(pat, Expr::Ident(tmp.clone()), &mut decls);
            let inner = self.statement()?;
            let body = Box::new(Stmt::Block(vec![Stmt::Decl(DeclKind::Let, decls), inner]));
            return Ok(Stmt::ForIn { of, kind: Some(k), var: tmp, obj, body });
        }
        if let Tok::Ident(name) = self.peek().clone() {
            if !RESERVED.contains(&name.as_str()) && matches!(self.peek_at(1), Tok::Ident(w) if w == "in" || w == "of") {
                self.advance();
                let of = matches!(self.advance(), Tok::Ident(w) if w == "of");
                let obj = if of { self.assignment()? } else { self.expression()? };
                self.expect_punct(")")?;
                let body = Box::new(self.statement()?);
                return Ok(Stmt::ForIn { of, kind, var: name, obj, body });
            }
        }
        self.pos = save;
        let init = if self.eat_punct(";") {
            ForInit::None
        } else if let Some(kind) = self.decl_kind() {
            self.advance();
            let list = self.declarators(kind)?;
            self.expect_punct(";")?;
            ForInit::Decl(kind, list)
        } else {
            let e = self.expression()?;
            self.expect_punct(";")?;
            ForInit::Expr(e)
        };
        let test = if self.is_punct(";") { None } else { Some(self.expression()?) };
        self.expect_punct(";")?;
        let update = if self.is_punct(")") { None } else { Some(self.expression()?) };
        self.expect_punct(")")?;
        Ok(Stmt::For(init, test, update, Box::new(self.statement()?)))
    }

    fn switch_statement(&mut self) -> PResult<Stmt> {
        self.advance();
        self.expect_punct("(")?;
        let subject = self.expression()?;
        self.expect_punct(")")?;
        self.expect_punct("{")?;
        let mut cases = vec![];
        while !self.is_punct("}") {
            let test = if self.eat_word("case") {
                Some(self.expression()?)
            } else if self.eat_word("default") {
                None
            } else {
                return self.err("expected 'case' or 'default'");
            };
            self.expect_punct(":")?;
            let mut body = vec![];
            while !self.is_punct("}") && !self.is_word("case") && !self.is_word("default") {
                if *self.peek() == Tok::Eof {
                    return self.err("missing '}'");
                }
                body.push(self.statement()?);
            }
            cases.push((test, body));
        }
        self.advance();
        Ok(Stmt::Switch(subject, cases))
    }

    fn try_statement(&mut self) -> PResult<Stmt> {
        self.advance();
        let block = self.block()?;
        let mut catch = None;
        if self.eat_word("catch") {
            let param = if self.eat_punct("(") {
                let name = self.ident()?;
                self.expect_punct(")")?;
                Some(name)
            } else {
                None
            };
            catch = Some((param, self.block()?));
        }
        let finally = if self.eat_word("finally") { Some(self.block()?) } else { None };
        if catch.is_none() && finally.is_none() {
            return self.err("'try' needs 'catch' or 'finally'");
        }
        Ok(Stmt::Try { block, catch, finally })
    }

    /// The parameters and body of a function, after its name.
    fn function_rest(&mut self, name: Option<String>) -> PResult<Func> {
        self.expect_punct("(")?;
        let (params, rest, prologue) = self.parameter_list()?;
        // (`arguments` seen in the body belongs to this function, not the one around it)
        let outer = std::mem::take(&mut self.arguments_seen);
        let body = self.block();
        let uses_arguments = std::mem::replace(&mut self.arguments_seen, outer);
        Ok(Func { name, params, body: Parser::with_prologue(prologue, body?), arrow: false, uses_arguments, rest })
    }

    /// Parameter names up to and including `)`, the last of which may be `...rest`.
    /// The third part is declarations to put first in the body: what a destructured parameter
    /// (`function f({a, b}, [c])`) or a default (`function f(x = 1)`) needs.
    fn parameter_list(&mut self) -> PResult<(Vec<String>, bool, Vec<(String, Option<Expr>)>)> {
        let mut params = vec![];
        let mut rest = false;
        let mut prologue = vec![];
        while !self.is_punct(")") {
            if self.eat_punct("...") {
                rest = true;
            }
            if self.pattern_ahead() {
                let pat = self.element()?;
                let tmp = self.temp_name();
                self.flatten_pattern(pat, Expr::Ident(tmp.clone()), &mut prologue);
                params.push(tmp);
            } else {
                let name = self.ident()?;
                if !rest && self.eat_punct("=") {
                    // `x = 1`: the default is used when the argument is undefined
                    let default = self.assignment()?;
                    let pat = Pat { target: PatTarget::Name(name.clone()), default: Some(default) };
                    let tmp = self.temp_name();
                    self.flatten_pattern(pat, Expr::Ident(tmp.clone()), &mut prologue);
                    params.push(tmp);
                } else {
                    params.push(name);
                }
            }
            if rest || !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct(")")?;
        Ok((params, rest, prologue))
    }

    /// Put a parameter list's declarations at the start of a function body.
    fn with_prologue(prologue: Vec<(String, Option<Expr>)>, mut body: Vec<Stmt>) -> Vec<Stmt> {
        if !prologue.is_empty() {
            body.insert(0, Stmt::Decl(DeclKind::Let, prologue));
        }
        body
    }

    // ---- expressions

    pub fn expression(&mut self) -> PResult<Expr> {
        let first = self.assignment()?;
        if !self.is_punct(",") {
            return Ok(first);
        }
        let mut list = vec![first];
        while self.eat_punct(",") {
            list.push(self.assignment()?);
        }
        Ok(Expr::Seq(list))
    }

    /// Does an arrow function start here: `x =>`, `(` params `) =>`?
    fn arrow_ahead(&self) -> bool {
        if matches!(self.peek(), Tok::Ident(n) if !RESERVED.contains(&n.as_str())) && matches!(self.peek_at(1), Tok::Punct("=>")) {
            return true;
        }
        if !self.is_punct("(") {
            return false;
        }
        let mut depth = 0usize;
        let mut i = self.pos;
        while i < self.toks.len() {
            match &self.toks[i].tok {
                Tok::Punct("(") => depth += 1,
                Tok::Punct(")") => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(self.toks.get(i + 1).map(|t| &t.tok), Some(Tok::Punct("=>")));
                    }
                }
                Tok::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    fn arrow(&mut self) -> PResult<Expr> {
        let (params, rest, prologue) = if self.eat_punct("(") { self.parameter_list()? } else { (vec![self.ident()?], false, vec![]) };
        self.expect_punct("=>")?;
        // an arrow function has no `arguments` of its own: a mention is the enclosing function's
        // (it stays flagged on the parser, so the enclosing function sees it)
        let body = if self.is_punct("{") {
            self.block()?
        } else {
            vec![Stmt::Return(Some(self.assignment()?))]
        };
        Ok(Expr::Function(Rc::new(Func { name: None, params, body: Parser::with_prologue(prologue, body), arrow: true, uses_arguments: false, rest })))
    }

    pub fn assignment(&mut self) -> PResult<Expr> {
        self.enter()?;
        let r = self.assignment_inner();
        self.leave();
        r
    }

    fn assignment_inner(&mut self) -> PResult<Expr> {
        if self.arrow_ahead() {
            return self.arrow();
        }
        let left = self.conditional()?;
        if let Tok::Punct(p) = self.peek().clone() {
            if matches!(p, "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "**=" | "<<=" | ">>=" | ">>>=" | "&=" | "|=" | "^=" | "&&=" | "||=" | "??=") {
                if !matches!(left, Expr::Ident(_) | Expr::Dot(..) | Expr::Index(..)) {
                    return self.err("invalid assignment target");
                }
                self.advance();
                let value = self.assignment()?;
                return Ok(Expr::Assign(p, Box::new(left), Box::new(value)));
            }
        }
        Ok(left)
    }

    fn conditional(&mut self) -> PResult<Expr> {
        let test = self.binary(0)?;
        if self.eat_punct("?") {
            let then = self.assignment()?;
            self.expect_punct(":")?;
            let other = self.assignment()?;
            return Ok(Expr::Cond(Box::new(test), Box::new(then), Box::new(other)));
        }
        Ok(test)
    }

    /// Binary operator precedence (higher binds tighter), or None if the token isn't one.
    fn binary_prec(&self) -> Option<(&'static str, u8)> {
        let op: &'static str = match self.peek() {
            Tok::Punct(p) => p,
            Tok::Ident(w) if w == "instanceof" => "instanceof",
            Tok::Ident(w) if w == "in" => "in",
            _ => return None,
        };
        let prec = match op {
            "??" => 1,
            "||" => 2,
            "&&" => 3,
            "|" => 4,
            "^" => 5,
            "&" => 6,
            "==" | "!=" | "===" | "!==" => 7,
            "<" | ">" | "<=" | ">=" | "instanceof" | "in" => 8,
            "<<" | ">>" | ">>>" => 9,
            "+" | "-" => 10,
            "*" | "/" | "%" => 11,
            "**" => 12,
            _ => return None,
        };
        Some((op, prec))
    }

    /// One more link in an operator or call chain: the chains open at once are bounded, because
    /// the tree they build is dropped recursively.
    fn chain_step(&mut self, added: &mut usize) -> PResult<()> {
        *added += 1;
        self.chain += 1;
        if self.chain > MAX_CHAIN { self.err("expression too long") } else { Ok(()) }
    }

    /// Parse the sources of a template's `${}` parts (each knows the line it starts on).
    fn template_exprs(&mut self, sources: Vec<(String, u32)>) -> PResult<Vec<Expr>> {
        let mut exprs = vec![];
        for (src, line) in sources {
            let mut inner = Parser { toks: tokenize_at(&src, line)?, pos: 0, depth: self.depth, chain: self.chain, arguments_seen: false, temps: 1000 + self.temps };
            let e = inner.expression()?;
            if *inner.peek() != Tok::Eof {
                return inner.err("bad expression in template");
            }
            self.arguments_seen |= inner.arguments_seen;
            exprs.push(e);
        }
        Ok(exprs)
    }

    fn binary(&mut self, min: u8) -> PResult<Expr> {
        let mut left = self.unary()?;
        let mut added = 0usize;
        while let Some((op, prec)) = self.binary_prec() {
            if prec < min {
                break;
            }
            self.advance();
            self.chain_step(&mut added)?;
            // `**` is right-associative (and recurses, so it counts as nesting); everything else is left-associative
            let right = if op == "**" {
                self.enter()?;
                let r = self.binary(prec);
                self.leave();
                r?
            } else {
                self.binary(prec + 1)?
            };
            left = if matches!(op, "&&" | "||" | "??") {
                Expr::Logical(op, Box::new(left), Box::new(right))
            } else {
                Expr::Binary(op, Box::new(left), Box::new(right))
            };
        }
        self.chain -= added;
        Ok(left)
    }

    fn unary(&mut self) -> PResult<Expr> {
        self.enter()?;
        let r = self.unary_inner();
        self.leave();
        r
    }

    fn unary_inner(&mut self) -> PResult<Expr> {
        let op: Option<&'static str> = match self.peek() {
            Tok::Punct(p @ ("!" | "-" | "+" | "~")) => Some(p),
            Tok::Ident(w) if w == "typeof" => Some("typeof"),
            Tok::Ident(w) if w == "void" => Some("void"),
            Tok::Ident(w) if w == "delete" => Some("delete"),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            return Ok(Expr::Unary(op, Box::new(self.unary()?)));
        }
        if let Tok::Punct(p @ ("++" | "--")) = self.peek().clone() {
            self.advance();
            let target = self.unary()?;
            if !matches!(target, Expr::Ident(_) | Expr::Dot(..) | Expr::Index(..)) {
                return self.err("invalid increment target");
            }
            return Ok(Expr::Update(p, true, Box::new(target)));
        }
        let e = self.postfix()?;
        Ok(e)
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let e = self.call_or_member()?;
        if let Tok::Punct(p @ ("++" | "--")) = self.peek().clone() {
            // (a line break before `++` ends the statement instead)
            if !self.toks[self.pos].newline_before {
                if !matches!(e, Expr::Ident(_) | Expr::Dot(..) | Expr::Index(..)) {
                    return self.err("invalid increment target");
                }
                self.advance();
                return Ok(Expr::Update(p, false, Box::new(e)));
            }
        }
        Ok(e)
    }

    fn arguments(&mut self) -> PResult<Vec<Expr>> {
        self.expect_punct("(")?;
        let mut args = vec![];
        while !self.is_punct(")") {
            args.push(self.argument()?);
            if !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct(")")?;
        Ok(args)
    }

    /// One argument or array element: an expression, or `...list`.
    fn argument(&mut self) -> PResult<Expr> {
        if self.eat_punct("...") {
            return Ok(Expr::Spread(Box::new(self.assignment()?)));
        }
        self.assignment()
    }

    /// `new Foo(args)`, `new a.b.C()`, `new Foo` (no arguments), `new new X()()`.
    fn new_expression(&mut self) -> PResult<Expr> {
        self.enter()?;
        self.advance(); // `new`
        let mut callee = if self.is_word("new") { self.new_expression()? } else { self.primary()? };
        let mut added = 0usize;
        loop {
            if self.eat_punct(".") {
                callee = Expr::Dot(Box::new(callee), self.property_name()?);
            } else if self.eat_punct("[") {
                let index = self.expression()?;
                self.expect_punct("]")?;
                callee = Expr::Index(Box::new(callee), Box::new(index));
            } else {
                break;
            }
            self.chain_step(&mut added)?;
        }
        self.chain -= added;
        let args = if self.is_punct("(") { self.arguments()? } else { vec![] };
        self.leave();
        Ok(Expr::New(Box::new(callee), args))
    }

    fn call_or_member(&mut self) -> PResult<Expr> {
        let mut e = if self.is_word("new") { self.new_expression()? } else { self.primary()? };
        let mut added = 0usize;
        loop {
            if self.eat_punct(".") {
                e = Expr::Dot(Box::new(e), self.property_name()?);
            } else if self.eat_punct("[") {
                let index = self.expression()?;
                self.expect_punct("]")?;
                e = Expr::Index(Box::new(e), Box::new(index));
            } else if self.is_punct("(") {
                let args = self.arguments()?;
                e = Expr::Call(Box::new(e), args);
            } else if matches!(self.peek(), Tok::Template(..)) && !self.toks[self.pos].newline_before {
                // a tagged template: tag`a${x}b` calls tag(["a", "b"], x)
                let Tok::Template(pieces, sources) = self.advance() else { unreachable!() };
                let exprs = self.template_exprs(sources)?;
                let strings = Expr::Array(pieces.into_iter().map(Expr::Str).collect());
                let mut args = vec![strings];
                args.extend(exprs);
                e = Expr::Call(Box::new(e), args);
            } else {
                self.chain -= added;
                return Ok(e);
            }
            self.chain_step(&mut added)?;
        }
    }

    /// Step back over the token just consumed, so an error is reported where it was (but not
    /// past the end of the script, where there is nothing to step back to).
    fn back(&mut self, consumed: &Tok) {
        if *consumed != Tok::Eof {
            self.pos = self.pos.saturating_sub(1);
        }
    }

    /// After `class` and its name: `extends X { constructor() {...} method() {...} static s() {...} }`.
    /// A class is written out as what it stands for in older JavaScript: a function that
    /// is the constructor, with the methods assigned to its `prototype` (and the statics to the
    /// function), inside a function that is given the parent class as `__super`. `super(...)` and
    /// `super.m(...)` become calls through `__super`. Accessors (`get`/`set`), class fields and
    /// computed method names aren't supported.
    fn class_rest(&mut self, name: Option<String>) -> PResult<Expr> {
        let cname = name.unwrap_or_else(|| "__class".to_string());
        let parent = if self.eat_word("extends") { Some(self.assignment()?) } else { None };
        self.expect_punct("{")?;
        let id = |s: &str| Expr::Ident(s.to_string());
        let dot = |e: Expr, n: &str| Expr::Dot(Box::new(e), n.to_string());
        let mut constructor: Option<Func> = None;
        let mut body: Vec<Stmt> = vec![];
        let mut members: Vec<Stmt> = vec![];
        while !self.is_punct("}") {
            if self.eat_punct(";") {
                continue;
            }
            let is_static = self.is_word("static") && !matches!(self.peek_at(1), Tok::Punct("("));
            if is_static {
                self.advance();
            }
            let key = match self.advance() {
                Tok::Ident(n) => n,
                Tok::Str(s) => s,
                Tok::Num(n) => n.to_string(),
                other => {
                    self.back(&other);
                    return self.err("expected a method name");
                }
            };
            if !self.is_punct("(") {
                return self.err(&format!("'{key}': accessors and class fields are not supported"));
            }
            let func = self.function_rest(None)?;
            if key == "constructor" && !is_static {
                constructor = Some(Func { name: Some(cname.clone()), ..func });
                continue;
            }
            let holder = if is_static { id(&cname) } else { dot(id(&cname), "prototype") };
            let target = if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') && !key.starts_with(|c: char| c.is_ascii_digit()) {
                dot(holder, &key)
            } else {
                Expr::Index(Box::new(holder), Box::new(Expr::Str(key)))
            };
            members.push(Stmt::Expr(Expr::Assign("=", Box::new(target), Box::new(Expr::Function(Rc::new(func))))));
        }
        self.expect_punct("}")?;
        let ctor = match constructor {
            Some(c) => c,
            None if parent.is_some() => Func {
                name: Some(cname.clone()),
                params: vec![],
                // (the default constructor of a derived class passes its arguments on)
                body: vec![Stmt::Expr(Expr::Call(Box::new(dot(id("__super"), "apply")), vec![Expr::This, id("arguments")]))],
                arrow: false,
                uses_arguments: true,
                rest: false,
            },
            None => Func { name: Some(cname.clone()), params: vec![], body: vec![], arrow: false, uses_arguments: false, rest: false },
        };
        body.push(Stmt::Function(Rc::new(ctor)));
        if parent.is_some() {
            let proto = dot(id(&cname), "prototype");
            let made = Expr::Call(Box::new(dot(id("Object"), "create")), vec![dot(id("__super"), "prototype")]);
            body.push(Stmt::Expr(Expr::Assign("=", Box::new(proto), Box::new(made))));
            let back = Expr::Assign("=", Box::new(dot(dot(id(&cname), "prototype"), "constructor")), Box::new(id(&cname)));
            body.push(Stmt::Expr(back));
        }
        body.extend(members);
        body.push(Stmt::Return(Some(id(&cname))));
        let wrapper = Func { name: None, params: vec!["__super".to_string()], body, arrow: false, uses_arguments: false, rest: false };
        Ok(Expr::Call(Box::new(Expr::Function(Rc::new(wrapper))), vec![parent.unwrap_or(Expr::Null)]))
    }

    /// `super(...)` and `super.name(...)` / `super.name`, as calls through the `__super` of the
    /// class being written out (see `class_rest`).
    fn super_expression(&mut self) -> PResult<Expr> {
        let id = |s: &str| Expr::Ident(s.to_string());
        let dot = |e: Expr, n: &str| Expr::Dot(Box::new(e), n.to_string());
        if self.is_punct("(") {
            let mut args = vec![Expr::This];
            args.extend(self.arguments()?);
            return Ok(Expr::Call(Box::new(dot(id("__super"), "call")), args));
        }
        if self.eat_punct(".") {
            let name = self.property_name()?;
            let member = dot(dot(id("__super"), "prototype"), &name);
            if self.is_punct("(") {
                let mut args = vec![Expr::This];
                args.extend(self.arguments()?);
                return Ok(Expr::Call(Box::new(dot(member, "call")), args));
            }
            return Ok(member);
        }
        self.err("'super' must be followed by '(' or '.'")
    }

    /// A property name after `.`: any word, reserved ones included.
    fn property_name(&mut self) -> PResult<String> {
        match self.advance() {
            Tok::Ident(n) => Ok(n),
            other => {
                self.back(&other);
                self.err("expected a property name")
            }
        }
    }

    fn primary(&mut self) -> PResult<Expr> {
        match self.advance() {
            Tok::Num(n) => Ok(Expr::Num(n)),
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::Template(pieces, sources) => {
                let exprs = self.template_exprs(sources)?;
                Ok(Expr::Template(pieces, exprs))
            }
            Tok::Ident(w) => match w.as_str() {
                "true" => Ok(Expr::Bool(true)),
                "false" => Ok(Expr::Bool(false)),
                "null" => Ok(Expr::Null),
                "this" => Ok(Expr::This),
                "super" => self.super_expression(),
                "class" => {
                    let name = match self.peek().clone() {
                        Tok::Ident(n) if n != "extends" && !RESERVED.contains(&n.as_str()) => {
                            self.advance();
                            Some(n)
                        }
                        _ => None,
                    };
                    self.class_rest(name)
                }
                "function" => {
                    let name = match self.peek().clone() {
                        Tok::Ident(n) if !RESERVED.contains(&n.as_str()) => {
                            self.advance();
                            Some(n)
                        }
                        _ => None,
                    };
                    Ok(Expr::Function(Rc::new(self.function_rest(name)?)))
                }
                _ if RESERVED.contains(&w.as_str()) => {
                    self.pos = self.pos.saturating_sub(1);
                    self.err(&format!("unexpected '{w}'"))
                }
                _ => {
                    if w == "arguments" {
                        self.arguments_seen = true;
                    }
                    Ok(Expr::Ident(w))
                }
            },
            Tok::Punct("(") => {
                let e = self.expression()?;
                self.expect_punct(")")?;
                Ok(e)
            }
            Tok::Punct("[") => {
                let mut items = vec![];
                while !self.is_punct("]") {
                    // `[1,,2]`: an empty slot is undefined
                    if self.eat_punct(",") {
                        items.push(Expr::Ident("undefined".into()));
                        continue;
                    }
                    items.push(self.argument()?);
                    if !self.eat_punct(",") {
                        break;
                    }
                }
                self.expect_punct("]")?;
                Ok(Expr::Array(items))
            }
            Tok::Punct("{") => {
                let mut props = vec![];
                while !self.is_punct("}") {
                    let key_is_word = matches!(self.peek(), Tok::Ident(_));
                    let key_text = match self.peek() {
                        Tok::Ident(n) => n.clone(),
                        _ => String::new(),
                    };
                    let key = match self.advance() {
                        Tok::Ident(n) => PropKey::Name(n),
                        Tok::Str(s) => PropKey::Name(s),
                        Tok::Num(n) => PropKey::Name(if n.fract() == 0.0 && n.abs() < 1e15 { format!("{}", n as i64) } else { n.to_string() }),
                        Tok::Punct("[") => {
                            let k = self.assignment()?;
                            self.expect_punct("]")?;
                            PropKey::Computed(k)
                        }
                        other => {
                            self.back(&other);
                            return self.err("expected a property name");
                        }
                    };
                    let value = if self.eat_punct(":") {
                        self.assignment()?
                    } else if self.is_punct("(") {
                        // a method: `name(args) { ... }`
                        let name = match &key {
                            PropKey::Name(n) => Some(n.clone()),
                            _ => None,
                        };
                        Expr::Function(Rc::new(self.function_rest(name)?))
                    } else if let (PropKey::Name(n), true) = (&key, key_is_word && !RESERVED.contains(&key_text.as_str())) {
                        // shorthand `{ x }` (only for a plain name, not `{ 1 }`, `{ "a" }` or `{ if }`)
                        Expr::Ident(n.clone())
                    } else {
                        return self.err("expected ':'");
                    };
                    props.push((key, value));
                    if !self.eat_punct(",") {
                        break;
                    }
                }
                self.expect_punct("}")?;
                Ok(Expr::Object(props))
            }
            other => {
                self.back(&other);
                self.err("unexpected token")
            }
        }
    }
}

/// Parse a whole script.
pub fn parse(src: &str) -> Result<Vec<Stmt>, String> {
    // The parser recurses as deep as the script nests, so it runs on a thread with a big stack
    // (memory for it is only used as far as it is reached). The tree itself is `Send`.
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(PARSE_STACK)
        .spawn(move || parse_here(&src))
        .map_err(|e| format!("could not start the parser: {e}"))?
        .join()
        .map_err(|_| "the parser crashed".to_string())?
}

fn parse_here(src: &str) -> Result<Vec<Stmt>, String> {
    let mut p = Parser { toks: tokenize(src)?, pos: 0, depth: 0, chain: 0, arguments_seen: false, temps: 0 };
    p.program()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(src: &str) -> Expr {
        let mut p = Parser { toks: tokenize(src).unwrap(), pos: 0, depth: 0, chain: 0, arguments_seen: false, temps: 0 };
        let e = p.expression().unwrap_or_else(|e| panic!("{src}: {e}"));
        assert_eq!(*p.peek(), Tok::Eof, "{src}: leftover tokens");
        e
    }

    fn ok(src: &str) -> Vec<Stmt> {
        parse(src).unwrap_or_else(|e| panic!("{src}: {e}"))
    }

    fn bad(src: &str) -> String {
        parse(src).err().unwrap_or_else(|| panic!("{src} should not parse"))
    }

    fn n(v: f64) -> Box<Expr> {
        Box::new(Expr::Num(v))
    }

    #[test]
    fn numbers_and_strings_lex() {
        assert_eq!(expr("42"), Expr::Num(42.0));
        assert_eq!(expr("3.5e2"), Expr::Num(350.0));
        assert_eq!(expr(".5"), Expr::Num(0.5));
        assert_eq!(expr("0xff"), Expr::Num(255.0));
        assert_eq!(expr("0b101"), Expr::Num(5.0));
        assert_eq!(expr("0o17"), Expr::Num(15.0));
        assert_eq!(expr("1_000"), Expr::Num(1000.0));
        assert!(tokenize("5e").is_err(), "a number can't run into a letter");
        assert_eq!(expr(r#""a\nb\t\u0041\x42\u{1F600}""#), Expr::Str("a\nb\tAB😀".into()));
        assert_eq!(expr("'it\\'s'"), Expr::Str("it's".into()));
        assert_eq!(expr("'line\\\ncontinued'"), Expr::Str("linecontinued".into()));
        assert!(tokenize("'open").is_err());
        assert!(tokenize("1abc").is_err());
        assert!(tokenize("0x").is_err());
        assert!(tokenize("\"bad \\u12\"").is_err());
        assert!(tokenize("a # b").is_err());
        assert!(tokenize("/* never closed").is_err());
    }

    #[test]
    fn operators_follow_precedence_and_associativity() {
        assert_eq!(expr("1 + 2 * 3"), Expr::Binary("+", n(1.0), Box::new(Expr::Binary("*", n(2.0), n(3.0)))));
        assert_eq!(expr("1 - 2 - 3"), Expr::Binary("-", Box::new(Expr::Binary("-", n(1.0), n(2.0))), n(3.0)));
        assert_eq!(expr("2 ** 3 ** 2"), Expr::Binary("**", n(2.0), Box::new(Expr::Binary("**", n(3.0), n(2.0)))));
        assert_eq!(expr("(1 + 2) * 3"), Expr::Binary("*", Box::new(Expr::Binary("+", n(1.0), n(2.0))), n(3.0)));
        assert_eq!(
            expr("a || b && c"),
            Expr::Logical("||", Box::new(Expr::Ident("a".into())), Box::new(Expr::Logical("&&", Box::new(Expr::Ident("b".into())), Box::new(Expr::Ident("c".into())))))
        );
        assert_eq!(expr("a ?? b"), Expr::Logical("??", Box::new(Expr::Ident("a".into())), Box::new(Expr::Ident("b".into()))));
        assert_eq!(expr("1 < 2 == true"), Expr::Binary("==", Box::new(Expr::Binary("<", n(1.0), n(2.0))), Box::new(Expr::Bool(true))));
        assert!(matches!(expr("x instanceof Y"), Expr::Binary("instanceof", ..)));
        assert!(matches!(expr("'a' in o"), Expr::Binary("in", ..)));
        assert!(matches!(expr("a ? b : c ? d : e"), Expr::Cond(_, _, e) if matches!(*e, Expr::Cond(..))));
        assert!(matches!(expr("a = b = 3"), Expr::Assign("=", _, v) if matches!(*v, Expr::Assign("=", ..))));
        assert!(matches!(expr("a += 1"), Expr::Assign("+=", ..)));
        assert!(matches!(expr("a, b, c"), Expr::Seq(v) if v.len() == 3));
    }

    #[test]
    fn unary_update_member_and_call_expressions() {
        assert_eq!(expr("!x"), Expr::Unary("!", Box::new(Expr::Ident("x".into()))));
        assert!(matches!(expr("typeof x"), Expr::Unary("typeof", _)));
        assert!(matches!(expr("-x ** 2"), Expr::Unary("-", _)) || matches!(expr("-x ** 2"), Expr::Binary("**", ..)));
        assert!(matches!(expr("i++"), Expr::Update("++", false, _)));
        assert!(matches!(expr("--i"), Expr::Update("--", true, _)));
        assert!(matches!(expr("a.b.c"), Expr::Dot(inner, name) if name == "c" && matches!(*inner, Expr::Dot(..))));
        assert!(matches!(expr("a[0][1]"), Expr::Index(..)));
        assert!(matches!(expr("f(1, 2)(3)"), Expr::Call(callee, args) if args.len() == 1 && matches!(*callee, Expr::Call(..))));
        assert!(matches!(expr("o.m(x).n"), Expr::Dot(..)));
        assert!(matches!(expr("new Foo(1)"), Expr::New(_, a) if a.len() == 1));
        assert!(matches!(expr("new a.B"), Expr::New(c, a) if a.is_empty() && matches!(*c, Expr::Dot(..))));
        assert!(matches!(expr("new Foo().bar()"), Expr::Call(..)));
        assert!(matches!(expr("a.class.default"), Expr::Dot(..)), "reserved words are fine as property names");
        assert!(matches!(expr("this"), Expr::This));
    }

    #[test]
    fn literals_functions_and_templates() {
        assert!(matches!(expr("[1, 2, 3,]"), Expr::Array(v) if v.len() == 3));
        assert!(matches!(expr("[]"), Expr::Array(v) if v.is_empty()));
        match expr("({a: 1, 'b c': 2, 3: 4, [k]: 5, m() { return 1 }, short})") {
            Expr::Object(props) => {
                assert_eq!(props.len(), 6);
                assert!(matches!(&props[0].0, PropKey::Name(n) if n == "a"));
                assert!(matches!(&props[1].0, PropKey::Name(n) if n == "b c"));
                assert!(matches!(&props[2].0, PropKey::Name(n) if n == "3"));
                assert!(matches!(&props[3].0, PropKey::Computed(_)));
                assert!(matches!(&props[4].1, Expr::Function(f) if f.name.as_deref() == Some("m")));
                assert_eq!(props[5].1, Expr::Ident("short".into()));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(expr("function (a, b) { return a + b }"), Expr::Function(f) if f.params == ["a", "b"] && !f.arrow));
        assert!(matches!(expr("function fact(n) { return n }"), Expr::Function(f) if f.name.as_deref() == Some("fact")));
        assert!(matches!(expr("x => x * 2"), Expr::Function(f) if f.arrow && f.params == ["x"] && matches!(f.body[0], Stmt::Return(Some(_)))));
        assert!(matches!(expr("(a, b) => { return a }"), Expr::Function(f) if f.arrow && f.params.len() == 2));
        assert!(matches!(expr("() => 1"), Expr::Function(f) if f.params.is_empty()));
        assert!(matches!(expr("(a) + (b)"), Expr::Binary("+", ..)), "parentheses that are not an arrow's");
        match expr("`a${x + 1}b${y}c`") {
            Expr::Template(pieces, exprs) => {
                assert_eq!(pieces, ["a", "b", "c"]);
                assert_eq!(exprs.len(), 2);
                assert!(matches!(exprs[0], Expr::Binary("+", ..)));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(expr("`plain \\n text`"), Expr::Template(p, e) if p == ["plain \n text"] && e.is_empty()));
        assert!(matches!(expr("`nested ${ `inner ${1}` }`"), Expr::Template(..)));
        assert!(matches!(expr("`obj ${ {a: '}'}.a }`"), Expr::Template(..)), "braces and strings inside a substitution");
    }

    #[test]
    fn statements_parse() {
        let prog = ok("var a = 1, b; let c = 2; const d = 3;");
        assert!(matches!(&prog[0], Stmt::Decl(DeclKind::Var, l) if l.len() == 2 && l[1].1.is_none()));
        assert!(matches!(&prog[1], Stmt::Decl(DeclKind::Let, _)));
        assert!(matches!(&prog[2], Stmt::Decl(DeclKind::Const, _)));
        assert!(matches!(&ok("function f(a) { return a; }")[0], Stmt::Function(f) if f.name.as_deref() == Some("f")));
        assert!(matches!(&ok("if (a) b; else if (c) d; else e;")[0], Stmt::If(_, _, Some(_))));
        assert!(matches!(&ok("while (x) { x-- }")[0], Stmt::While(..)));
        assert!(matches!(&ok("do x++; while (x < 3)")[0], Stmt::DoWhile(..)));
        assert!(matches!(&ok("for (var i = 0; i < 3; i++) {}")[0], Stmt::For(ForInit::Decl(DeclKind::Var, _), Some(_), Some(_), _)));
        assert!(matches!(&ok("for (;;) { break }")[0], Stmt::For(ForInit::None, None, None, _)));
        assert!(matches!(&ok("for (i = 0; i < 3; i++) ;")[0], Stmt::For(ForInit::Expr(_), ..)));
        assert!(matches!(&ok("for (var k in obj) {}")[0], Stmt::ForIn { of: false, kind: Some(DeclKind::Var), .. }));
        assert!(matches!(&ok("for (const v of list) {}")[0], Stmt::ForIn { of: true, kind: Some(DeclKind::Const), .. }));
        assert!(matches!(&ok("for (k in obj) {}")[0], Stmt::ForIn { kind: None, .. }));
        assert!(matches!(&ok("switch (x) { case 1: a(); break; case 2: default: b() }")[0], Stmt::Switch(_, cases) if cases.len() == 3));
        assert!(matches!(&ok("try { a() } catch (e) { b() } finally { c() }")[0], Stmt::Try { catch: Some((Some(_), _)), finally: Some(_), .. }));
        assert!(matches!(&ok("try { a() } catch { b() }")[0], Stmt::Try { catch: Some((None, _)), .. }));
        assert!(matches!(&ok("throw new Error('x')")[0], Stmt::Throw(_)));
        assert!(matches!(&ok(";")[0], Stmt::Empty));
        assert!(matches!(&ok("{ a; b }")[0], Stmt::Block(b) if b.len() == 2));
        assert!(ok("").is_empty());
        assert!(ok("// only a comment\n/* and another */").is_empty());
        assert!(ok("#!/usr/bin/env node\nx = 1").len() == 1);
    }

    #[test]
    fn automatic_semicolon_insertion() {
        assert_eq!(ok("a = 1\nb = 2\nc").len(), 3);
        assert_eq!(ok("var x = 1\nvar y = 2").len(), 2);
        assert_eq!(ok("function f() { return 1 }").len(), 1);
        assert_eq!(ok("if (a) b\nelse c").len(), 1);
        // `return` + line break returns nothing, and the next line is its own statement
        match &ok("function f() {\n return\n 5\n}")[0] {
            Stmt::Function(f) => {
                assert_eq!(f.body.len(), 2);
                assert!(matches!(f.body[0], Stmt::Return(None)));
            }
            other => panic!("{other:?}"),
        }
        // a line break before ++ ends the statement
        assert_eq!(ok("a\n++b").len(), 2);
        // no line break and no semicolon: an error
        bad("a = 1 b = 2");
        bad("throw\nnew Error()");
    }

    #[test]
    fn syntax_errors_are_reported_with_a_line_number() {
        for src in [
            "var", "var 1", "if (", "if (a", "function", "function (", "a +", "(1", "[1, 2", "{a: 1", "x = ;", "for (;;", "try {}", "switch (a) { foo }",
            "1 = 2", "a++ ++", "new", "var if = 1", "a.", "a.1", "}", ")", "let let = 1", "x => ", "({a b})", "`${`",
        ] {
            let e = parse(src).err().unwrap_or_else(|| panic!("{src:?} should not parse"));
            assert!(!e.is_empty(), "{src}");
        }
        assert!(bad("a = 1;\nb = ;\n").starts_with("line 2"), "{}", bad("a = 1;\nb = ;\n"));
    }

    #[test]
    fn long_chains_and_templates_cannot_overflow_the_stack() {
        // each of these used to abort the process when the tree was dropped, or while it was built
        for (name, src) in [
            ("sum", vec!["1"; 100_000].join("+")),
            ("or", vec!["a"; 100_000].join("||")),
            ("member", format!("a{}", ".b".repeat(100_000))),
            ("index", format!("a{}", "[0]".repeat(100_000))),
            ("pow", vec!["2"; 100_000].join("**")),
            ("calls", format!("a{}", "()".repeat(100_000))),
            ("templates", format!("{}1{}", "`${".repeat(5_000), "}`".repeat(5_000))),
            ("news", format!("{}x", "new ".repeat(5_000))),
        ] {
            let result = parse(&src);
            assert!(result.is_err(), "{name}: refused, not parsed");
            drop(result);
        }
        // a sensible chain is fine
        assert!(parse(&vec!["1"; 2000].join("+")).is_ok());
        assert!(parse(&vec!["2"; 150].join("**")).is_ok());
        assert!(parse(&format!("{}1{}", "`${".repeat(15), "}`".repeat(15))).is_ok());
    }

    #[test]
    fn realistic_nesting_is_accepted() {
        // callbacks inside callbacks, nested calls, else-if ladders: well past what hand-written code does
        let callbacks = format!("{}1{}", "f(function(){ ".repeat(40), " })".repeat(40));
        assert!(parse(&callbacks).is_ok());
        assert!(parse(&format!("{}1{}", "f(".repeat(60), ")".repeat(60))).is_ok());
        let ladder: String = (0..150).map(|i| format!("if (x == {i}) y = {i}; else ")).collect::<String>() + "y = -1;";
        assert!(parse(&ladder).is_ok());
        assert!(parse(&format!("{}1{}", "(".repeat(100), ")".repeat(100))).is_ok());
    }

    #[test]
    fn more_of_the_language_parses_like_ecmascript() {
        ok("new new X()()");
        assert!(matches!(expr("[1,,2]"), Expr::Array(v) if v.len() == 3));
        assert_eq!(expr("010"), Expr::Num(8.0));
        assert_eq!(expr("08"), Expr::Num(8.0));
        assert_eq!(expr("0"), Expr::Num(0.0));
        assert_eq!(expr(r#""😀""#), Expr::Str("😀".into()), "a surrogate pair is one character");
        assert_eq!(expr(r#""\uD83D x""#), Expr::Str("\u{fffd} x".into()));
        assert!(tokenize(r#""\x+f""#).is_err() && tokenize(r#""\u+123""#).is_err() && tokenize(r#""\u{}""#).is_err());
        // comments inside a template substitution
        assert!(matches!(expr("`a${ /* } */ 1 }d`"), Expr::Template(p, e) if p == ["a", "d"] && e.len() == 1));
        assert!(matches!(expr("`a${ 1 // }\n }d`"), Expr::Template(p, _) if p == ["a", "d"]));
        // tagged templates call the tag with the strings and the values
        assert!(matches!(expr("tag`x${1}y`"), Expr::Call(_, args) if args.len() == 2 && matches!(&args[0], Expr::Array(p) if p.len() == 2)));
        assert_eq!(ok("a\n`t`").len(), 2, "a template on a new line is not a tag");
        // every kind of line break ends a // comment and a statement
        assert_eq!(ok("a = 1 // c\rb = 2").len(), 2);
        assert_eq!(ok("a = 1 // c\u{2028}b = 2").len(), 2);
        assert!(matches!(&ok("function f() { return\r1 }")[0], Stmt::Function(f) if f.body.len() == 2));
        assert_eq!(ok("/* a\rb */ x = 1").len(), 1);
        // HTML-style comments
        assert_eq!(ok("<!-- hide\nvar x = 1\n--> done\nvar y = 2").len(), 2);
        assert_eq!(ok("a = b --> 0").len(), 1, "`-->` after code is `b-- > 0`");
        assert!(matches!(&ok("debugger;")[0], Stmt::Empty));
        // things that must be rejected
        for src in ["var null = 1", "var class = 1", "var import = 1", "const x;", "({if})", "({1})", "({\"a\"})", "super", "with (a) b"] {
            assert!(parse(src).is_err(), "{src}");
        }
        assert!(ok("({if: 1, null: 2, class: 3})").len() == 1, "reserved words are fine as keys");
        assert!(ok("var o = {a, b}; const z = 1;").len() == 2);
    }

    #[test]
    fn error_lines_are_where_the_problem_is() {
        assert!(bad("a = 1;\nb = 2;\nx = `${ 1 # }`;").starts_with("line 3"), "{}", bad("a = 1;\nb = 2;\nx = `${ 1 # }`;"));
        assert!(bad("a\nb\nx = `${ 1 + + }`").starts_with("line 3"), "{}", bad("a\nb\nx = `${ 1 + + }`"));
        assert!(bad("a.\n\n\n\n").starts_with("line 5"), "{}", bad("a.\n\n\n\n"));
        assert!(bad("x = 1;\ny = `never\nends\nhere").starts_with("line 2"), "{}", bad("x = 1;\ny = `never\nends\nhere"));
        assert!(bad("function f(\n\n").starts_with("line 3"), "{}", bad("function f(\n\n"));
    }

    #[test]
    fn hostile_input_is_bounded() {
        // deep nesting stops with an error instead of overflowing the stack
        let deep = format!("{}1{}", "(".repeat(5000), ")".repeat(5000));
        assert!(parse(&deep).is_err());
        let blocks = format!("{}{}", "{".repeat(5000), "}".repeat(5000));
        assert!(parse(&blocks).is_err());
        let arrays = format!("{}{}", "[".repeat(5000), "]".repeat(5000));
        assert!(parse(&arrays).is_err());
        let unary = format!("{}x", "!".repeat(5000));
        assert!(parse(&unary).is_err());
        let calls = format!("x{}", "(1)".repeat(2_000));
        assert!(parse(&calls).is_ok(), "a long chain of calls is flat, not nested");
        assert!(parse(&format!("x{}", "(1)".repeat(10_000))).is_err(), "but an enormous one is refused (its tree is dropped recursively)");
        let arrows = "x=>".repeat(5000) + "1";
        assert!(parse(&arrows).is_err());
        // a big but flat script parses in reasonable time
        let long: String = (0..20_000).map(|i| format!("var v{i} = {i} + 1;\n")).collect();
        let t = std::time::Instant::now();
        assert_eq!(parse(&long).unwrap().len(), 20_000);
        assert!(t.elapsed().as_secs() < 30, "{:?}", t.elapsed());
        // garbage bytes never panic
        for junk in ["\u{0}", "\u{feff}", "'\u{0}'", "𝒳 = 1", "é = 1; ñ = é", "a\u{2028}b", "\\", "@", "`${"] {
            let _ = parse(junk);
        }
        assert!(parse(&"a".repeat(MAX_SOURCE_BYTES + 1)).is_err(), "over the size limit");
    }
}
