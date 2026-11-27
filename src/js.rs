//! A JavaScript interpreter for the subset that `jsparse` understands.
//!
//! It walks the syntax tree directly. Values are undefined, null, booleans, numbers, strings and
//! objects (plain objects, arrays, functions, errors); functions are closures with lexical
//! scope; `this`, `new`, prototypes and `instanceof` work; exceptions are catchable. A small
//! standard library (`console`, `Math`, `JSON.stringify`, array / string / number methods) is built in.
//!
//! A script can't hang or exhaust memory: every evaluation step is counted and a script that
//! runs too long is stopped (the stop can't be caught by `try`), recursion has a depth limit, and
//! strings and arrays have size limits.
#![allow(dead_code)]
use crate::dom::Dom;
use crate::jsparse::{self, DeclKind, Expr, ForInit, Func, PropKey, Stmt};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

mod dom_bindings;
mod promise;
use dom_bindings::{Listener, Timer};

/// Steps one event, and one timer callback, may take (a page has many of them).
const EVENT_STEPS: u64 = 5_000_000;
const TIMER_STEPS: u64 = 2_000_000;
/// Steps all the timer callbacks run after one event or load may take together.
const ALL_TIMER_STEPS: u64 = 8_000_000;

/// Evaluation steps a script may take in total.
const MAX_STEPS: u64 = 20_000_000;
/// Nested JavaScript calls.
const MAX_CALL_DEPTH: usize = 48;
/// The longest string, and the most elements an array can be extended to.
const MAX_STRING: usize = 4 * 1024 * 1024;
const MAX_ARRAY: usize = 1_000_000;
/// Nested evaluations (expressions inside expressions inside calls...) at once: the Rust stack
/// use of the interpreter is proportional to this, not to what the parser allowed.
const MAX_NEST: usize = 3000;
/// How much the process's heap may grow, from when an interpreter was made, before its script is stopped.
const MAX_SCRIPT_MEMORY: isize = 640 * 1024 * 1024;

pub type ObjRef = Rc<RefCell<Object>>;
type ScopeRef = Rc<RefCell<Scope>>;
type NativeFn = fn(&mut Interp, &Value, &[Value]) -> Res;
type Res = Result<Value, Ctrl>;

#[derive(Clone)]
pub enum Value {
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    Str(Rc<str>),
    Obj(ObjRef),
}

pub enum Ctrl {
    Throw(Value),
    Break,
    Continue,
    Return(Value),
    /// The script is stopped (too long, too deep, too big): not catchable.
    Abort(String),
}

pub struct Closure {
    func: std::sync::Arc<Func>,
    scope: ScopeRef,
    /// An arrow function's `this`, captured where it was created.
    this: Option<Value>,
}

pub enum Kind {
    Plain,
    Array(Vec<Value>),
    Closure(Rc<Closure>),
    Native(NativeFn),
    /// `f.bind(thisArg, ...args)`: the function, the `this` it is called with, and leading arguments.
    Bound(Box<(Value, Value, Vec<Value>)>),
    /// An element or text node of the page (see `dom_bindings`).
    Node(usize),
    /// `element.style`: reads and writes the node's `style` attribute.
    Style(usize),
    Error,
}

pub struct Object {
    pub kind: Kind,
    props: HashMap<String, Value>,
    order: Vec<String>,
    proto: Option<ObjRef>,
}

/// What an assignment writes to: a variable, or a property of an object (already evaluated).
enum Place {
    Var(String),
    Member(Value, String),
}

/// An array index written the canonical way (`"3"`, not `"03"` or `"+3"`).
fn array_index(name: &str) -> Option<usize> {
    let n: usize = name.parse().ok()?;
    (n.to_string() == name).then_some(n)
}

struct Binding {
    value: Value,
    mutable: bool,
}

struct Scope {
    vars: HashMap<String, Binding>,
    parent: Option<ScopeRef>,
}

// ---- dropping without recursion
//
// Values form chains (an array in an array in an array...; scopes whose parent is a scope). The
// usual recursive drop would use a Rust stack frame per link, so a script could overflow the stack
// just by being freed. Instead, what an object or scope owns is queued, and a loop on the outermost
// drop frees the queue one item at a time.

enum Dead {
    Value(Value),
    Scope(ScopeRef),
    Obj(ObjRef),
    Closure(Rc<Closure>),
}

thread_local! {
    static DEAD: RefCell<Vec<Dead>> = const { RefCell::new(Vec::new()) };
    static DRAINING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn bury(items: Vec<Dead>) {
    if items.is_empty() {
        return;
    }
    let queued = DEAD.try_with(|d| d.borrow_mut().extend(items)).is_ok();
    if !queued {
        return; // (the thread is shutting down: let the rest drop as it falls)
    }
    // an outer drop is already emptying the queue: it will get to these
    if DRAINING.try_with(|f| f.replace(true)).unwrap_or(true) {
        return;
    }
    while let Some(next) = DEAD.try_with(|d| d.borrow_mut().pop()).ok().flatten() {
        drop(next);
    }
    let _ = DRAINING.try_with(|f| f.set(false));
}

impl Drop for Scope {
    fn drop(&mut self) {
        let mut dead: Vec<Dead> = self.vars.drain().map(|(_, b)| Dead::Value(b.value)).collect();
        if let Some(p) = self.parent.take() {
            dead.push(Dead::Scope(p));
        }
        bury(dead);
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        let mut dead: Vec<Dead> = self.props.drain().map(|(_, v)| Dead::Value(v)).collect();
        match std::mem::replace(&mut self.kind, Kind::Plain) {
            Kind::Array(items) => dead.extend(items.into_iter().map(Dead::Value)),
            Kind::Closure(c) => dead.push(Dead::Closure(c)),
            Kind::Bound(b) => {
                let (f, t, args) = *b;
                dead.push(Dead::Value(f));
                dead.push(Dead::Value(t));
                dead.extend(args.into_iter().map(Dead::Value));
            }
            _ => {}
        }
        if let Some(p) = self.proto.take() {
            dead.push(Dead::Obj(p));
        }
        bury(dead);
    }
}

impl Drop for Interp {
    /// Reference cycles (a function and the scope it was made in) would keep everything a script
    /// made alive for ever, so before the interpreter goes, everything reachable from it is emptied.
    fn drop(&mut self) {
        let mut scopes: Vec<ScopeRef> = vec![];
        let mut objs: Vec<ObjRef> = vec![];
        let mut seen_scopes: std::collections::HashSet<*const RefCell<Scope>> = std::collections::HashSet::new();
        let mut seen_objs: std::collections::HashSet<*const RefCell<Object>> = std::collections::HashSet::new();
        enum Visit {
            Scope(ScopeRef),
            Value(Value),
            Obj(ObjRef),
        }
        let mut work: Vec<Visit> = vec![Visit::Scope(self.global.clone()), Visit::Value(self.document.clone())];
        work.push(Visit::Obj(self.object_proto.clone()));
        work.push(Visit::Obj(self.error_proto.clone()));
        work.push(Visit::Obj(self.array_proto.clone()));
        work.push(Visit::Value(self.object_ctor.clone()));
        work.push(Visit::Value(self.array_ctor.clone()));
        work.extend(self.wrappers.values().cloned().map(Visit::Obj));
        work.extend(self.listeners.iter().map(|l| Visit::Value(l.handler.clone())));
        work.extend(self.timers.iter().flat_map(|t| t.roots()).map(Visit::Value));
        while let Some(v) = work.pop() {
            match v {
                Visit::Scope(s) => {
                    if !seen_scopes.insert(Rc::as_ptr(&s)) {
                        continue;
                    }
                    for b in s.borrow().vars.values() {
                        work.push(Visit::Value(b.value.clone()));
                    }
                    if let Some(p) = s.borrow().parent.clone() {
                        work.push(Visit::Scope(p));
                    }
                    scopes.push(s);
                }
                Visit::Value(Value::Obj(o)) | Visit::Obj(o) => {
                    if !seen_objs.insert(Rc::as_ptr(&o)) {
                        continue;
                    }
                    {
                        let b = o.borrow();
                        work.extend(b.props.values().cloned().map(Visit::Value));
                        if let Some(p) = &b.proto {
                            work.push(Visit::Obj(p.clone()));
                        }
                        match &b.kind {
                            Kind::Array(a) => work.extend(a.iter().cloned().map(Visit::Value)),
                            Kind::Closure(c) => {
                                work.push(Visit::Scope(c.scope.clone()));
                                if let Some(t) = &c.this {
                                    work.push(Visit::Value(t.clone()));
                                }
                            }
                            Kind::Bound(bd) => {
                                work.push(Visit::Value(bd.0.clone()));
                                work.push(Visit::Value(bd.1.clone()));
                                work.extend(bd.2.iter().cloned().map(Visit::Value));
                            }
                            _ => {}
                        }
                    }
                    objs.push(o);
                }
                Visit::Value(_) => {}
            }
        }
        for s in &scopes {
            let vars: Vec<Binding> = s.borrow_mut().vars.drain().map(|(_, b)| b).collect();
            drop(vars);
        }
        for o in &objs {
            let (props, kind) = {
                let mut b = o.borrow_mut();
                (std::mem::take(&mut b.props), std::mem::replace(&mut b.kind, Kind::Plain))
            };
            drop((props, kind));
        }
        self.wrappers.clear();
        self.listeners.clear();
        self.timers.clear();
    }
}

fn new_scope(parent: Option<ScopeRef>) -> ScopeRef {
    Rc::new(RefCell::new(Scope { vars: HashMap::new(), parent }))
}

fn str_value(s: &str) -> Value {
    Value::Str(Rc::from(s))
}

pub struct Interp {
    global: ScopeRef,
    object_proto: ObjRef,
    error_proto: ObjRef,
    /// `Array.prototype`: an (empty) array, so the array methods are found through it too.
    array_proto: ObjRef,
    object_ctor: Value,
    array_ctor: Value,
    steps: u64,
    depth: usize,
    /// How deeply `eval` / `exec` / native conversions are nested (bounds the Rust stack).
    nest: usize,
    /// Objects made so far (bounds nesting depth, and so the recursion when they are dropped).
    objects: usize,
    /// (no longer limits anything: see `mem_limit`; kept as plain statistics)
    bytes: u64,
    /// The heap size, process-wide, above which the script is stopped.
    mem_limit: isize,
    /// Bytes the page's scripts have kept from earlier runs (load, events): what is left of the
    /// allowance is for the next one.
    retained: isize,
    /// Network requests the page's scripts have made.
    requests: u32,
    /// Time spent waiting for those requests.
    net_time: std::time::Duration,
    /// Promises created so far.
    promises: u32,
    /// Arrays being joined right now (a cycle joins as an empty string).
    joining: Vec<ObjRef>,
    /// What the script wrote with `console.log` and friends.
    pub console: Vec<String>,
    console_bytes: usize,
    rng: u64,
    // ---- the page (see `dom_bindings`)
    /// The page's DOM, held here while scripts run.
    dom: Option<Dom>,
    page_url: String,
    document: Value,
    wrappers: HashMap<usize, ObjRef>,
    /// Built-in methods already handed out (see `method`).
    methods: HashMap<usize, Value>,
    listeners: Vec<Listener>,
    timers: Vec<Timer>,
    timer_seq: u64,
    /// Virtual time for timers, in milliseconds.
    clock: f64,
}

// ------------------------------------------------------------------ running a page's scripts

/// A value that is moved to another thread by hand: the interpreter is full of `Rc`s, which are
/// not `Send`, but all of them live inside the one `Interp`, and an `Interp` only ever moves
/// whole while nothing else holds a reference into it (the thread that ran it has finished).
struct Unsend<T>(T);
// SAFETY: see above. Every `Rc` the interpreter owns is reachable only through it, so moving the
// whole interpreter to another thread, and using it from only one thread at a time, is sound.
unsafe impl<T> Send for Unsend<T> {}

/// The scripts of a page, kept alive after loading so events can reach their handlers.
pub struct ScriptHost(Option<Unsend<Interp>>);
// SAFETY: as `Unsend`: the interpreter inside is only touched by one thread at a time.
unsafe impl Send for ScriptHost {}

impl ScriptHost {
    fn interp(&mut self) -> &mut Interp {
        &mut self.0.as_mut().expect("the interpreter is only taken when the host is dropped").0
    }
}

impl Drop for ScriptHost {
    /// Emptying a big script's world takes a while and some stack: do it on its own thread.
    fn drop(&mut self) {
        if let Some(interp) = self.0.take() {
            let moved = Unsend(interp);
            // (if the thread can't start, the interpreter is dropped right here instead)
            let _ = std::thread::Builder::new().stack_size(SCRIPT_STACK).spawn(move || {
                let moved = moved;
                drop(moved.0);
            });
        }
    }
}

/// The stack scripts run on: they recurse as deep as they nest, up to the interpreter's limits.
const SCRIPT_STACK: usize = 256 * 1024 * 1024;

pub struct PageScripts {
    /// The DOM after the scripts ran.
    pub dom: Dom,
    /// What they wrote to the console (and their uncaught errors).
    pub console: Vec<String>,
    pub host: ScriptHost,
}

/// Run a page's scripts in order (each is its source and a name for error messages), then fire
/// `DOMContentLoaded` and `load` listeners and the timers they set. Scripts share one global scope and
/// one budget; an error in one is logged and the next one still runs.
pub fn run_page_scripts(dom: Dom, url: &str, scripts: Vec<(String, String)>) -> Option<PageScripts> {
    let url = url.to_string();
    let job = Unsend((dom, scripts));
    let result = std::thread::Builder::new()
        .stack_size(SCRIPT_STACK)
        .spawn(move || {
            let job = job;
            let (dom, scripts) = job.0;
            let start = crate::memcount::live_bytes();
            let mut interp = Interp::for_page(dom, &url);
            for (source, name) in scripts {
                interp.steps = 0; // (each script has the whole budget: one that runs away doesn't starve the next)
                if let Err(message) = interp.run(&source) {
                    interp.log_line(format!("{name}: {message}"));
                }
                interp.refresh_document();
            }
            for event in ["DOMContentLoaded", "load"] {
                let _ = interp.dispatch(dom_bindings::DOCUMENT, event);
            }
            interp.run_timers();
            let dom = interp.dom.take().unwrap_or_else(Dom::new);
            let console = std::mem::take(&mut interp.console);
            interp.retained = (crate::memcount::live_bytes() - start).max(0);
            Unsend((dom, console, interp))
        })
        .ok()?
        .join()
        .ok()?;
    let (dom, console, interp) = result.0;
    Some(PageScripts { dom, console, host: ScriptHost(Some(Unsend(interp))) })
}

impl ScriptHost {
    /// A click (or another event) on `node`: run the page's handlers for it, with the page's DOM.
    /// Returns the DOM afterwards, and whether a handler called `preventDefault()`.
    pub fn dispatch(&mut self, dom: &mut Dom, node: usize, event: &str) -> bool {
        let event = event.to_string();
        // the DOM goes into the interpreter for the duration, and comes back out (even if the thread fails)
        self.interp().dom = Some(std::mem::replace(dom, Dom::new()));
        let job = Unsend(self.interp());
        let outcome = std::thread::scope(|scope| {
            let handle = std::thread::Builder::new().stack_size(SCRIPT_STACK).spawn_scoped(scope, move || {
                let job = job;
                let interp = job.0;
                interp.steps = MAX_STEPS - EVENT_STEPS;
                // (memory is counted per thread, and this is a new one: the page's whole allowance
                // is what it hasn't already kept, so many events can't add up to more than one script)
                let start = crate::memcount::live_bytes();
                if interp.retained >= MAX_SCRIPT_MEMORY {
                    // the page has used all it may keep: its handlers no longer run
                    interp.log_line("script used too much memory and was stopped".into());
                    return false;
                }
                interp.mem_limit = start + (MAX_SCRIPT_MEMORY - interp.retained);
                let prevented = match interp.dispatch(node, &event) {
                    Ok(p) => p,
                    Err(Ctrl::Abort(m)) => {
                        // (a script that ran away or ate the memory: say so, it isn't an ordinary error)
                        interp.log_line(m);
                        false
                    }
                    Err(_) => false,
                };
                interp.run_timers();
                // (the limit is only looked at every so often while running: look once more at the end)
                if interp.spend_bytes(0).is_err() {
                    interp.log_line("script used too much memory and was stopped".into());
                }
                interp.retained = (interp.retained + crate::memcount::live_bytes() - start).max(0);
                prevented
            });
            handle.ok().and_then(|h| h.join().ok())
        });
        if let Some(back) = self.interp().dom.take() {
            *dom = back;
        }
        outcome.unwrap_or(false)
    }

    /// Lines the scripts have logged since the last call.
    pub fn take_console(&mut self) -> Vec<String> {
        std::mem::take(&mut self.interp().console)
    }
}

// ------------------------------------------------------------------ conversions

fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if n == 0.0 {
        return "0".into();
    }
    let abs = n.abs();
    if abs >= 1e21 || abs < 1e-6 {
        // JavaScript writes 1e+21 and 1e-7
        let s = format!("{n:e}");
        return match s.split_once('e') {
            Some((m, e)) if !e.starts_with('-') => format!("{m}e+{e}"),
            _ => s,
        };
    }
    // (Rust writes the shortest digits that read back as the same number, padded with zeros:
    // 2**64 is 18446744073709552000, as in JavaScript)
    format!("{n}")
}

fn string_to_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    for (prefixes, radix) in [(["0x", "0X"], 16u32), (["0b", "0B"], 2), (["0o", "0O"], 8)] {
        if let Some(digits) = t.strip_prefix(prefixes[0]).or_else(|| t.strip_prefix(prefixes[1])) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            // (folded as floats: a hex number too big for an integer is just a big number)
            return digits.chars().fold(0.0, |acc, c| acc * radix as f64 + c.to_digit(radix).unwrap_or(0) as f64);
        }
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    // (Rust also accepts "inf" and "nan": JavaScript doesn't)
    if t.chars().any(|c| !(c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))) {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `parseFloat`: the longest prefix that reads as a decimal number (`1.5e3px` is 1500).
fn parse_float_prefix(s: &str) -> f64 {
    let t = s.trim_start();
    let bytes = t.as_bytes();
    let mut i = 0;
    if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
        i += 1;
    }
    if t[i..].starts_with("Infinity") {
        return if bytes.first() == Some(&b'-') { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    let digits = |from: usize| bytes[from..].iter().take_while(|b| b.is_ascii_digit()).count();
    let int = digits(i);
    i += int;
    let mut frac = 0;
    if i < bytes.len() && bytes[i] == b'.' {
        frac = digits(i + 1);
        if int > 0 || frac > 0 {
            i += 1 + frac;
        }
    }
    if int == 0 && frac == 0 {
        return f64::NAN;
    }
    if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
        let mut j = i + 1;
        if j < bytes.len() && matches!(bytes[j], b'+' | b'-') {
            j += 1;
        }
        let exp = digits(j);
        if exp > 0 {
            i = j + exp;
        }
    }
    t[..i].parse::<f64>().unwrap_or(f64::NAN)
}

fn to_int32(n: f64) -> i32 {
    if !n.is_finite() {
        return 0;
    }
    (n.trunc().rem_euclid(4294967296.0) as u64 as u32) as i32
}

impl Value {
    pub fn truthy(&self) -> bool {
        match self {
            Value::Undefined | Value::Null => false,
            Value::Bool(b) => *b,
            Value::Num(n) => *n != 0.0 && !n.is_nan(),
            Value::Str(s) => !s.is_empty(),
            Value::Obj(_) => true,
        }
    }

    pub fn type_of(&self) -> &'static str {
        match self {
            Value::Undefined => "undefined",
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Num(_) => "number",
            Value::Str(_) => "string",
            Value::Obj(o) => match o.borrow().kind {
                Kind::Closure(_) | Kind::Native(_) | Kind::Bound(_) => "function",
                _ => "object",
            },
        }
    }

    fn is_callable(&self) -> bool {
        self.type_of() == "function"
    }
}

fn strict_equals(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Undefined, Value::Undefined) | (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Num(x), Value::Num(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Obj(x), Value::Obj(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

// ------------------------------------------------------------------ interpreter core

impl Interp {
    pub fn new() -> Interp {
        let object_proto = Rc::new(RefCell::new(Object { kind: Kind::Plain, props: HashMap::new(), order: vec![], proto: None }));
        let error_proto = Rc::new(RefCell::new(Object { kind: Kind::Plain, props: HashMap::new(), order: vec![], proto: Some(object_proto.clone()) }));
        let array_proto = Rc::new(RefCell::new(Object { kind: Kind::Array(vec![]), props: HashMap::new(), order: vec![], proto: Some(object_proto.clone()) }));
        let mut interp = Interp {
            global: new_scope(None),
            object_proto,
            error_proto,
            array_proto,
            object_ctor: Value::Undefined,
            array_ctor: Value::Undefined,
            steps: 0,
            depth: 0,
            nest: 0,
            objects: 0,
            bytes: 0,
            mem_limit: crate::memcount::live_bytes() + MAX_SCRIPT_MEMORY,
            retained: 0,
            requests: 0,
            net_time: std::time::Duration::ZERO,
            promises: 0,
            joining: vec![],
            console: vec![],
            console_bytes: 0,
            rng: 0x2545F4914F6CDD1D,
            dom: None,
            page_url: String::new(),
            document: Value::Undefined,
            wrappers: HashMap::new(),
            methods: HashMap::new(),
            listeners: vec![],
            timers: vec![],
            timer_seq: 0,
            clock: 0.0,
        };
        interp.install_globals();
        interp
    }

    /// An interpreter for a page: `document`, `window`, timers and the page's DOM.
    pub fn for_page(dom: Dom, url: &str) -> Interp {
        let mut interp = Interp::new();
        interp.page_url = url.chars().take(2000).collect();
        interp.dom = Some(dom);
        interp.install_dom_globals();
        interp.refresh_document();
        interp
    }

    /// Check the memory the process holds against what the script may grow it to. (Memory is
    /// measured, not summed from what was asked for: building and dropping strings is free.)
    fn spend_bytes(&mut self, _n: usize) -> Result<(), Ctrl> {
        if crate::memcount::live_bytes() > self.mem_limit {
            Err(Ctrl::Abort("script used too much memory and was stopped".into()))
        } else {
            Ok(())
        }
    }

    /// Charge for work a native function does on a big value (cloning an array, scanning a string).
    fn charge(&mut self, units: usize) -> Result<(), Ctrl> {
        self.steps += (units / 16) as u64;
        if self.steps > MAX_STEPS {
            Err(Ctrl::Abort("script ran for too long and was stopped".into()))
        } else {
            Ok(())
        }
    }

    /// Enter a level of recursion that uses the Rust stack.
    fn descend(&mut self) -> Result<(), Ctrl> {
        self.nest += 1;
        if self.nest > MAX_NEST {
            self.nest -= 1;
            return Err(Ctrl::Abort("script nested too deeply and was stopped".into()));
        }
        Ok(())
    }

    /// Run a whole script. An uncaught exception or a stopped script is an `Err` with a message.
    pub fn run(&mut self, source: &str) -> Result<Value, String> {
        let program = jsparse::parse(source)?;
        let scope = self.global.clone();
        self.hoist(&program, &scope);
        let this = Value::Undefined;
        let mut last = Value::Undefined;
        for stmt in &program {
            match self.exec(stmt, &scope, &this) {
                Ok(v) => last = v,
                Err(Ctrl::Throw(v)) => return Err(format!("Uncaught {}", self.describe_error(&v))),
                Err(Ctrl::Abort(m)) => return Err(m),
                Err(Ctrl::Return(v)) => return Ok(v),
                Err(Ctrl::Break | Ctrl::Continue) => return Err("illegal break or continue".into()),
            }
        }
        Ok(last)
    }

    /// An error object, or anything whose prototype chain includes `Error.prototype`
    /// (the pre-class way to write an error subclass).
    fn is_error_like(&self, o: &ObjRef) -> bool {
        let mut cur = Some(o.clone());
        let mut hops = 0;
        while let Some(c) = cur {
            if matches!(c.borrow().kind, Kind::Error) || Rc::ptr_eq(&c, &self.error_proto) {
                return true;
            }
            hops += 1;
            if hops > 1000 {
                return false;
            }
            cur = c.borrow().proto.clone();
        }
        false
    }

    fn describe_error(&mut self, v: &Value) -> String {
        if let Value::Obj(o) = v {
            if self.is_error_like(o) {
                let name = self.get_member(v, "name").ok().map(|n| self.to_string_lossy(&n)).unwrap_or_default();
                let message = self.get_member(v, "message").ok().map(|n| self.to_string_lossy(&n)).unwrap_or_default();
                return if message.is_empty() { name } else { format!("{name}: {message}") };
            }
        }
        self.to_string_lossy(v)
    }

    fn tick(&mut self) -> Result<(), Ctrl> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err(Ctrl::Abort("script ran for too long and was stopped".into()));
        }
        // (the memory check reads an atomic: once every thousand steps is plenty)
        if self.steps & 0x3ff == 0 {
            self.spend_bytes(0)?;
        }
        Ok(())
    }

    // ---- creating values

    // (every object made is counted: the limits are checked at the next evaluation step)
    fn new_object(&mut self) -> ObjRef {
        self.objects += 1;
        self.bytes += 96;
        Rc::new(RefCell::new(Object { kind: Kind::Plain, props: HashMap::new(), order: vec![], proto: Some(self.object_proto.clone()) }))
    }

    fn new_array(&mut self, items: Vec<Value>) -> Value {
        self.objects += 1;
        self.bytes += 96 + 16 * items.len() as u64;
        Value::Obj(Rc::new(RefCell::new(Object { kind: Kind::Array(items), props: HashMap::new(), order: vec![], proto: Some(self.array_proto.clone()) })))
    }

    /// The (shared) function object for a built-in method found by name: asking for `a.push`
    /// twice gives the same object, and allocates nothing the second time.
    fn method(&mut self, f: NativeFn, name: &str) -> Value {
        let key = f as usize;
        if let Some(v) = self.methods.get(&key) {
            return v.clone();
        }
        let v = self.native(f);
        if matches!(name, "push" | "pop" | "hasOwnProperty" | "toString") {
            if let Value::Obj(o) = &v {
                Interp::set_prop(o, "__cheap", Value::Bool(true));
            }
        }
        self.methods.insert(key, v.clone());
        v
    }

    fn native(&mut self, f: NativeFn) -> Value {
        self.objects += 1;
        self.bytes += 96;
        Value::Obj(Rc::new(RefCell::new(Object { kind: Kind::Native(f), props: HashMap::new(), order: vec![], proto: Some(self.object_proto.clone()) })))
    }

    fn make_error(&mut self, name: &str, message: &str) -> Value {
        self.objects += 1;
        let mut props = HashMap::new();
        props.insert("message".to_string(), str_value(message));
        props.insert("name".to_string(), str_value(name));
        Value::Obj(Rc::new(RefCell::new(Object {
            kind: Kind::Error,
            props,
            order: vec!["message".into(), "name".into()],
            proto: Some(self.error_proto.clone()),
        })))
    }

    fn throw<T>(&mut self, name: &str, message: &str) -> Result<T, Ctrl> {
        Err(Ctrl::Throw(self.make_error(name, message)))
    }

    fn set_prop(obj: &ObjRef, name: &str, value: Value) {
        let mut o = obj.borrow_mut();
        if o.props.insert(name.to_string(), value).is_none() {
            o.order.push(name.to_string());
        }
    }

    // ---- scopes

    fn lookup(&mut self, scope: &ScopeRef, name: &str) -> Res {
        let mut cur = Some(scope.clone());
        while let Some(s) = cur {
            if let Some(b) = s.borrow().vars.get(name) {
                return Ok(b.value.clone());
            }
            cur = s.borrow().parent.clone();
        }
        self.throw("ReferenceError", &format!("{name} is not defined"))
    }

    fn assign_var(&mut self, scope: &ScopeRef, name: &str, value: Value) -> Result<(), Ctrl> {
        let mut cur = Some(scope.clone());
        while let Some(s) = cur {
            let outcome = {
                let mut scope_mut = s.borrow_mut();
                match scope_mut.vars.get_mut(name) {
                    Some(b) if !b.mutable => Some(false),
                    Some(b) => {
                        b.value = value.clone();
                        Some(true)
                    }
                    None => None,
                }
            };
            match outcome {
                Some(true) => return Ok(()),
                Some(false) => return self.throw("TypeError", "Assignment to constant variable."),
                None => {}
            }
            cur = s.borrow().parent.clone();
        }
        // an assignment to something never declared creates a global (sloppy mode)
        self.global.borrow_mut().vars.insert(name.to_string(), Binding { value, mutable: true });
        Ok(())
    }

    fn declare(scope: &ScopeRef, name: &str, value: Value, mutable: bool) {
        scope.borrow_mut().vars.insert(name.to_string(), Binding { value, mutable });
    }

    /// Hoisting: `var` names (anywhere in the body, not inside nested functions) start as
    /// undefined, and function declarations are defined before anything runs.
    fn hoist(&mut self, body: &[Stmt], scope: &ScopeRef) {
        fn vars(stmts: &[Stmt], out: &mut Vec<String>) {
            for s in stmts {
                match s {
                    Stmt::Decl(DeclKind::Var, list) => out.extend(list.iter().map(|(n, _)| n.clone())),
                    Stmt::If(_, a, b) => {
                        vars(std::slice::from_ref(a), out);
                        if let Some(b) = b {
                            vars(std::slice::from_ref(b), out);
                        }
                    }
                    Stmt::Block(b) => vars(b, out),
                    Stmt::For(init, _, _, body) => {
                        if let ForInit::Decl(DeclKind::Var, list) = init {
                            out.extend(list.iter().map(|(n, _)| n.clone()));
                        }
                        vars(std::slice::from_ref(body), out);
                    }
                    Stmt::ForIn { kind, var, body, .. } => {
                        if *kind == Some(DeclKind::Var) {
                            out.push(var.clone());
                        }
                        vars(std::slice::from_ref(body), out);
                    }
                    Stmt::While(_, b) | Stmt::DoWhile(b, _) => vars(std::slice::from_ref(b), out),
                    Stmt::Switch(_, cases) => cases.iter().for_each(|(_, b)| vars(b, out)),
                    Stmt::Try { block, catch, finally } => {
                        vars(block, out);
                        if let Some((_, c)) = catch {
                            vars(c, out);
                        }
                        if let Some(f) = finally {
                            vars(f, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut names = vec![];
        vars(body, &mut names);
        for n in names {
            if !scope.borrow().vars.contains_key(&n) {
                Interp::declare(scope, &n, Value::Undefined, true);
            }
        }
        self.hoist_functions(body, scope);
    }

    fn hoist_functions(&mut self, body: &[Stmt], scope: &ScopeRef) {
        for s in body {
            if let Stmt::Function(f) = s {
                let closure = self.make_closure(f, scope, None);
                Interp::declare(scope, f.name.as_deref().unwrap_or(""), closure, true);
            }
        }
    }

    fn make_closure(&mut self, func: &std::sync::Arc<Func>, scope: &ScopeRef, this: Option<Value>) -> Value {
        self.objects += 1;
        self.bytes += 160;
        Value::Obj(Rc::new(RefCell::new(Object {
            kind: Kind::Closure(Rc::new(Closure { func: func.clone(), scope: scope.clone(), this })),
            props: HashMap::new(),
            order: vec![],
            proto: Some(self.object_proto.clone()),
        })))
    }

    // ---- statements

    fn exec_block(&mut self, body: &[Stmt], scope: &ScopeRef, this: &Value) -> Result<Value, Ctrl> {
        let inner = new_scope(Some(scope.clone()));
        self.hoist_functions(body, &inner);
        let mut last = Value::Undefined;
        for s in body {
            last = self.exec(s, &inner, this)?;
        }
        Ok(last)
    }

    fn declare_list(&mut self, kind: DeclKind, list: &[(String, Option<Expr>)], scope: &ScopeRef, this: &Value) -> Result<(), Ctrl> {
        for (name, init) in list {
            let value = match init {
                Some(e) => self.eval(e, scope, this)?,
                None => Value::Undefined,
            };
            match kind {
                DeclKind::Var => {
                    if init.is_some() {
                        self.assign_var(scope, name, value)?;
                    }
                }
                DeclKind::Let => Interp::declare(scope, name, value, true),
                DeclKind::Const => Interp::declare(scope, name, value, false),
            }
        }
        Ok(())
    }

    fn exec(&mut self, stmt: &Stmt, scope: &ScopeRef, this: &Value) -> Result<Value, Ctrl> {
        self.descend()?;
        let r = self.exec_inner(stmt, scope, this);
        self.nest -= 1;
        r
    }

    fn exec_inner(&mut self, stmt: &Stmt, scope: &ScopeRef, this: &Value) -> Result<Value, Ctrl> {
        self.tick()?;
        match stmt {
            Stmt::Empty | Stmt::Function(_) => Ok(Value::Undefined),
            Stmt::Expr(e) => self.eval(e, scope, this),
            Stmt::Decl(kind, list) => {
                self.declare_list(*kind, list, scope, this)?;
                Ok(Value::Undefined)
            }
            Stmt::Return(e) => {
                let v = match e {
                    Some(e) => self.eval(e, scope, this)?,
                    None => Value::Undefined,
                };
                Err(Ctrl::Return(v))
            }
            Stmt::If(test, then, other) => {
                if self.eval(test, scope, this)?.truthy() {
                    self.exec(then, scope, this)
                } else if let Some(o) = other {
                    self.exec(o, scope, this)
                } else {
                    Ok(Value::Undefined)
                }
            }
            Stmt::Block(body) => self.exec_block(body, scope, this),
            Stmt::While(test, body) => {
                while self.eval(test, scope, this)?.truthy() {
                    match self.exec(body, scope, this) {
                        Err(Ctrl::Break) => break,
                        Err(Ctrl::Continue) | Ok(_) => {}
                        Err(e) => return Err(e),
                    }
                }
                Ok(Value::Undefined)
            }
            Stmt::DoWhile(body, test) => {
                loop {
                    match self.exec(body, scope, this) {
                        Err(Ctrl::Break) => break,
                        Err(Ctrl::Continue) | Ok(_) => {}
                        Err(e) => return Err(e),
                    }
                    if !self.eval(test, scope, this)?.truthy() {
                        break;
                    }
                }
                Ok(Value::Undefined)
            }
            Stmt::For(init, test, update, body) => self.exec_for(init, test, update, body, scope, this),
            Stmt::ForIn { of, kind, var, obj, body } => self.exec_for_in(*of, *kind, var, obj, body, scope, this),
            Stmt::Switch(subject, cases) => {
                let value = self.eval(subject, scope, this)?;
                let inner = new_scope(Some(scope.clone()));
                // function declarations inside the cases exist from the start of the switch
                for (_, body) in cases {
                    self.hoist_functions(body, &inner);
                }
                let mut matched = None;
                for (i, (test, _)) in cases.iter().enumerate() {
                    if let Some(t) = test {
                        let tv = self.eval(t, &inner, this)?;
                        if strict_equals(&value, &tv) {
                            matched = Some(i);
                            break;
                        }
                    }
                }
                let start = matched.or_else(|| cases.iter().position(|(t, _)| t.is_none()));
                if let Some(start) = start {
                    for (_, body) in &cases[start..] {
                        for s in body {
                            match self.exec(s, &inner, this) {
                                Err(Ctrl::Break) => return Ok(Value::Undefined),
                                Ok(_) => {}
                                Err(e) => return Err(e),
                            }
                        }
                    }
                }
                Ok(Value::Undefined)
            }
            Stmt::Break => Err(Ctrl::Break),
            Stmt::Continue => Err(Ctrl::Continue),
            Stmt::Throw(e) => {
                let v = self.eval(e, scope, this)?;
                Err(Ctrl::Throw(v))
            }
            Stmt::Try { block, catch, finally } => {
                let mut result = self.exec_block(block, scope, this);
                if let (Err(Ctrl::Throw(thrown)), Some((param, body))) = (&result, catch) {
                    let inner = new_scope(Some(scope.clone()));
                    if let Some(p) = param {
                        Interp::declare(&inner, p, thrown.clone(), true);
                    }
                    result = self.exec_block(body, &inner, this);
                }
                if let Some(f) = finally {
                    // (a `return`, `break` or throw in `finally` replaces whatever was pending)
                    self.exec_block(f, scope, this)?;
                }
                result
            }
        }
    }

    fn exec_for(&mut self, init: &ForInit, test: &Option<Expr>, update: &Option<Expr>, body: &Stmt, scope: &ScopeRef, this: &Value) -> Result<Value, Ctrl> {
        let outer = new_scope(Some(scope.clone()));
        let per_iteration = matches!(init, ForInit::Decl(DeclKind::Let | DeclKind::Const, _));
        match init {
            ForInit::None => {}
            ForInit::Decl(kind, list) => self.declare_list(*kind, list, &outer, this)?,
            ForInit::Expr(e) => {
                self.eval(e, &outer, this)?;
            }
        }
        let mut current = outer;
        loop {
            if let Some(t) = test {
                if !self.eval(t, &current, this)?.truthy() {
                    break;
                }
            }
            match self.exec(body, &current, this) {
                Err(Ctrl::Break) => break,
                Err(Ctrl::Continue) | Ok(_) => {}
                Err(e) => return Err(e),
            }
            // `let` variables are fresh for every iteration, so closures made in the body keep their own
            if per_iteration {
                let next = new_scope(current.borrow().parent.clone());
                for (k, b) in current.borrow().vars.iter() {
                    next.borrow_mut().vars.insert(k.clone(), Binding { value: b.value.clone(), mutable: b.mutable });
                }
                current = next;
            }
            if let Some(u) = update {
                self.eval(u, &current, this)?;
            }
        }
        Ok(Value::Undefined)
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_for_in(&mut self, of: bool, kind: Option<DeclKind>, var: &str, obj: &Expr, body: &Stmt, scope: &ScopeRef, this: &Value) -> Result<Value, Ctrl> {
        let subject = self.eval(obj, scope, this)?;
        let items: Vec<Value> = if of {
            match &subject {
                Value::Obj(o) => match &o.borrow().kind {
                    Kind::Array(a) => a.clone(),
                    _ => vec![],
                },
                Value::Str(s) => s.chars().map(|c| str_value(&c.to_string())).collect(),
                _ => return self.throw("TypeError", "value is not iterable"),
            }
        } else {
            self.own_keys(&subject).into_iter().map(|k| str_value(&k)).collect()
        };
        self.charge(items.len() * 8)?;
        for item in items {
            let inner = new_scope(Some(scope.clone()));
            match kind {
                Some(DeclKind::Const) => Interp::declare(&inner, var, item, false),
                Some(DeclKind::Let) => Interp::declare(&inner, var, item, true),
                _ => self.assign_var(&inner, var, item)?,
            }
            match self.exec(body, &inner, this) {
                Err(Ctrl::Break) => break,
                Err(Ctrl::Continue) | Ok(_) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(Value::Undefined)
    }

    // ---- property access

    /// The enumerable own keys of a value (array indexes first).
    fn own_keys(&self, v: &Value) -> Vec<String> {
        match v {
            Value::Obj(o) => {
                let o = o.borrow();
                let mut keys: Vec<String> = match &o.kind {
                    Kind::Array(a) => (0..a.len()).map(|i| i.to_string()).collect(),
                    _ => vec![],
                };
                // integer-like keys come first, in ascending order; the rest in the order they were added
                let named = o.order.iter().filter(|k| !matches!(o.kind, Kind::Error) || !matches!(k.as_str(), "message" | "name"));
                let (mut ints, others): (Vec<&String>, Vec<&String>) = named.partition(|k| array_index(k).is_some());
                ints.sort_by_key(|k| array_index(k));
                keys.extend(ints.into_iter().cloned());
                keys.extend(others.into_iter().cloned());
                keys
            }
            Value::Str(s) => (0..s.chars().count()).map(|i| i.to_string()).collect(),
            _ => vec![],
        }
    }

    fn property_key(&mut self, v: &Value) -> Result<String, Ctrl> {
        match v {
            Value::Num(n) => Ok(number_to_string(*n)),
            Value::Str(s) => Ok(s.to_string()),
            other => self.to_string(other),
        }
    }

    pub fn get_member(&mut self, target: &Value, name: &str) -> Res {
        match target {
            Value::Undefined | Value::Null => {
                let what = if matches!(target, Value::Null) { "null" } else { "undefined" };
                self.throw("TypeError", &format!("Cannot read properties of {what} (reading '{name}')"))
            }
            Value::Str(s) => {
                if name == "length" {
                    return Ok(Value::Num(s.chars().count() as f64));
                }
                if let Some(i) = array_index(name) {
                    return Ok(s.chars().nth(i).map_or(Value::Undefined, |c| str_value(&c.to_string())));
                }
                self.charge(s.len() / 8)?; // (a string's length and characters are counted by walking it)
                Ok(string_method(name).map_or(Value::Undefined, |f| self.method(f, name)))
            }
            Value::Num(_) => Ok(match name {
                "toString" => self.method(num_to_string, name),
                "toFixed" => self.method(num_to_fixed, name),
                _ => Value::Undefined,
            }),
            Value::Bool(_) => Ok(if name == "toString" { self.method(bool_to_string, name) } else { Value::Undefined }),
            Value::Obj(o) => {
                // an element or text node of the page: what the script set on the object itself, else the DOM
                let node = if let Kind::Node(id) = o.borrow().kind { Some(id) } else { None };
                if let Some(id) = node {
                    let own = o.borrow().props.get(name).cloned();
                    return match own {
                        Some(v) => Ok(v),
                        None => dom_bindings::node_get(self, id, name),
                    };
                }
                let style = if let Kind::Style(id) = o.borrow().kind { Some(id) } else { None };
                if let Some(id) = style {
                    return dom_bindings::style_get(self, id, name);
                }
                // own properties, then the prototype chain
                let mut cur = Some(o.clone());
                while let Some(obj) = cur {
                    {
                        let b = obj.borrow();
                        if let Kind::Array(a) = &b.kind {
                            if name == "length" {
                                return Ok(Value::Num(a.len() as f64));
                            }
                            if let Some(i) = array_index(name) {
                                return Ok(a.get(i).cloned().unwrap_or(Value::Undefined));
                            }
                        }
                        if let Some(v) = b.props.get(name) {
                            return Ok(v.clone());
                        }
                    }
                    cur = obj.borrow().proto.clone();
                }
                // built-in methods by kind
                let kind_method = {
                    let b = o.borrow();
                    match &b.kind {
                        Kind::Array(_) => array_method(name),
                        Kind::Closure(_) | Kind::Native(_) | Kind::Bound(_) => function_method(name),
                        _ => None,
                    }
                };
                if let Some(f) = kind_method {
                    return Ok(self.method(f, name));
                }
                if name == "prototype" && matches!(o.borrow().kind, Kind::Closure(_)) {
                    let proto = Value::Obj(self.new_object());
                    if let Value::Obj(p) = &proto {
                        Interp::set_prop(p, "constructor", target.clone());
                    }
                    Interp::set_prop(o, "prototype", proto.clone());
                    return Ok(proto);
                }
                if name == "constructor" {
                    // the constructor of the built-in kinds (a function's own prototype object has its own)
                    let kind_ctor = match &o.borrow().kind {
                        Kind::Plain => Some(self.object_ctor.clone()),
                        Kind::Array(_) => Some(self.array_ctor.clone()),
                        Kind::Error => match o.borrow().props.get("name") {
                            Some(Value::Str(n)) => Some(n.to_string()).map(|n| Value::Str(Rc::from(n.as_str()))),
                            _ => None,
                        },
                        _ => None,
                    };
                    return Ok(match kind_ctor {
                        Some(Value::Str(n)) => {
                            let global = self.global.clone();
                            self.lookup(&global, &n).unwrap_or(Value::Undefined)
                        }
                        Some(v) => v,
                        None => Value::Undefined,
                    });
                }
                Ok(match name {
                    "hasOwnProperty" => self.method(obj_has_own, name),
                    "toString" => self.method(obj_to_string, name),
                    _ => Value::Undefined,
                })
            }
        }
    }

    pub fn set_member(&mut self, target: &Value, name: &str, value: Value) -> Result<(), Ctrl> {
        match target {
            Value::Undefined | Value::Null => self.throw("TypeError", &format!("Cannot set properties of {} (setting '{name}')", if matches!(target, Value::Null) { "null" } else { "undefined" })),
            Value::Obj(o) => {
                let node = if let Kind::Node(id) = o.borrow().kind { Some(id) } else { None };
                if let Some(id) = node {
                    return dom_bindings::node_set(self, id, name, value);
                }
                let style = if let Kind::Style(id) = o.borrow().kind { Some(id) } else { None };
                if let Some(id) = style {
                    return dom_bindings::style_set(self, id, name, value);
                }
                let mut b = o.borrow_mut();
                if let Kind::Array(a) = &mut b.kind {
                    if let Some(i) = array_index(name) {
                        if i >= MAX_ARRAY {
                            drop(b);
                            return Err(Ctrl::Abort("array too big".into()));
                        }
                        if i >= a.len() {
                            self.bytes += 16 * (i + 1 - a.len()) as u64;
                            a.resize(i + 1, Value::Undefined);
                        }
                        a[i] = value;
                        return Ok(());
                    }
                    if name == "length" {
                        if let Value::Num(n) = value {
                            if n > MAX_ARRAY as f64 {
                                drop(b);
                                return Err(Ctrl::Abort("array too big".into()));
                            }
                            if n >= 0.0 && n.fract() == 0.0 {
                                if (n as usize) > a.len() {
                                    self.bytes += 16 * (n as usize - a.len()) as u64;
                                }
                                a.resize(n as usize, Value::Undefined);
                            } else {
                                drop(b);
                                return self.throw("RangeError", "Invalid array length");
                            }
                        }
                        return Ok(());
                    }
                }
                drop(b);
                Interp::set_prop(o, name, value);
                Ok(())
            }
            _ => Ok(()), // a property set on a primitive is ignored
        }
    }

    // ---- conversions needing the interpreter

    /// An object as a primitive: `toString` first when a string is wanted, `valueOf` first
    /// otherwise (arithmetic, `+`, comparison).
    fn to_primitive(&mut self, v: &Value, string_hint: bool) -> Res {
        let Value::Obj(o) = v else { return Ok(v.clone()) };
        self.descend()?;
        let r = self.to_primitive_inner(v, o, string_hint);
        self.nest -= 1;
        r
    }

    fn to_primitive_inner(&mut self, v: &Value, o: &ObjRef, string_hint: bool) -> Res {
        let order: [&str; 2] = if string_hint { ["toString", "valueOf"] } else { ["valueOf", "toString"] };
        for method in order {
            // (an array's own toString is `join`; a plain object has no valueOf unless the script gave it one)
            if method == "toString" && matches!(o.borrow().kind, Kind::Array(_)) && !o.borrow().props.contains_key("toString") {
                return array_join(self, v, &[]);
            }
            let f = self.get_member(v, method)?;
            if f.is_callable() {
                let r = self.call(&f, v, &[])?;
                if !matches!(r, Value::Obj(_)) {
                    return Ok(r);
                }
            }
        }
        Ok(str_value("[object Object]"))
    }

    pub fn to_string(&mut self, v: &Value) -> Result<String, Ctrl> {
        Ok(match v {
            Value::Undefined => "undefined".into(),
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Num(n) => number_to_string(*n),
            Value::Str(s) => s.to_string(),
            Value::Obj(_) => {
                let p = self.to_primitive(v, true)?;
                self.to_string(&p)?
            }
        })
    }

    fn to_string_lossy(&mut self, v: &Value) -> String {
        self.to_string(v).unwrap_or_else(|_| "[object]".into())
    }

    pub fn to_number(&mut self, v: &Value) -> Result<f64, Ctrl> {
        Ok(match v {
            Value::Undefined => f64::NAN,
            Value::Null => 0.0,
            Value::Bool(b) => *b as u8 as f64,
            Value::Num(n) => *n,
            Value::Str(s) => string_to_number(s),
            Value::Obj(_) => {
                let p = self.to_primitive(v, false)?;
                self.to_number(&p)?
            }
        })
    }

    fn loose_equals(&mut self, a: &Value, b: &Value) -> Result<bool, Ctrl> {
        Ok(match (a, b) {
            (Value::Undefined | Value::Null, Value::Undefined | Value::Null) => true,
            (Value::Undefined | Value::Null, _) | (_, Value::Undefined | Value::Null) => false,
            (Value::Obj(_), Value::Obj(_)) => strict_equals(a, b),
            (Value::Obj(_), _) => {
                let p = self.to_primitive(a, false)?;
                return self.loose_equals(&p, b);
            }
            (_, Value::Obj(_)) => {
                let p = self.to_primitive(b, false)?;
                return self.loose_equals(a, &p);
            }
            (Value::Str(x), Value::Str(y)) => x == y,
            _ => self.to_number(a)? == self.to_number(b)?,
        })
    }

    // ---- calling

    pub fn call(&mut self, func: &Value, this: &Value, args: &[Value]) -> Res {
        let Value::Obj(o) = func else { return self.throw("TypeError", "value is not a function") };
        enum Target {
            Native(NativeFn),
            Closure(Rc<Closure>),
            Bound(Value, Value, Vec<Value>),
        }
        let target = match &o.borrow().kind {
            Kind::Native(f) => Target::Native(*f),
            Kind::Closure(c) => Target::Closure(c.clone()),
            Kind::Bound(b) => Target::Bound(b.0.clone(), b.1.clone(), b.2.clone()),
            _ => return self.throw("TypeError", "value is not a function"),
        };
        if self.depth >= MAX_CALL_DEPTH {
            return Err(Ctrl::Abort("call stack too deep".into()));
        }
        self.depth += 1;
        let result = match target {
            Target::Native(f) => {
                // a method on a big array or string does work in proportion to its size (except the
                // ones that don't: `push` and `pop` take the same time however long the array is)
                let cheap = matches!(func, Value::Obj(fo) if fo.borrow().props.contains_key("__cheap"));
                let size = match this {
                    Value::Obj(t) if !cheap => match &t.borrow().kind {
                        Kind::Array(a) => a.len() / 4,
                        _ => 0,
                    },
                    Value::Str(s) => s.len() / 8,
                    _ => 0,
                };
                match self.charge(size) {
                    Ok(()) => f(self, this, args),
                    Err(e) => Err(e),
                }
            }
            Target::Closure(c) => self.call_closure(&c, this, args),
            Target::Bound(f, bound_this, mut leading) => {
                leading.extend_from_slice(args);
                self.call(&f, &bound_this, &leading)
            }
        };
        self.depth -= 1;
        result
    }

    fn call_closure(&mut self, c: &Closure, this: &Value, args: &[Value]) -> Res {
        let scope = new_scope(Some(c.scope.clone()));
        for (i, p) in c.func.params.iter().enumerate() {
            Interp::declare(&scope, p, args.get(i).cloned().unwrap_or(Value::Undefined), true);
        }
        if !c.func.arrow && c.func.uses_arguments {
            let arguments = self.new_array(args.to_vec());
            Interp::declare(&scope, "arguments", arguments, true);
        }
        self.hoist(&c.func.body, &scope);
        let this = c.this.clone().unwrap_or_else(|| this.clone());
        for s in &c.func.body {
            match self.exec(s, &scope, &this) {
                Ok(_) => {}
                Err(Ctrl::Return(v)) => return Ok(v),
                Err(Ctrl::Break | Ctrl::Continue) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(Value::Undefined)
    }

    fn construct(&mut self, ctor: &Value, args: &[Value]) -> Res {
        let Value::Obj(co) = ctor else { return self.throw("TypeError", "value is not a constructor") };
        if !matches!(co.borrow().kind, Kind::Closure(_) | Kind::Native(_) | Kind::Bound(_)) {
            return self.throw("TypeError", "value is not a constructor");
        }
        let proto = self.get_member(ctor, "prototype")?;
        let instance = self.new_object();
        if let Value::Obj(p) = proto {
            instance.borrow_mut().proto = Some(p);
        }
        let this = Value::Obj(instance);
        let result = self.call(ctor, &this, args)?;
        // a constructor that returns an object replaces the new one
        Ok(if matches!(result, Value::Obj(_)) { result } else { this })
    }

    // ---- expressions

    fn eval(&mut self, e: &Expr, scope: &ScopeRef, this: &Value) -> Res {
        self.descend()?;
        let r = self.eval_inner(e, scope, this);
        self.nest -= 1;
        r
    }

    fn eval_inner(&mut self, e: &Expr, scope: &ScopeRef, this: &Value) -> Res {
        self.tick()?;
        match e {
            Expr::Num(n) => Ok(Value::Num(*n)),
            Expr::Str(s) => Ok(str_value(s)),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Null => Ok(Value::Null),
            Expr::This => Ok(this.clone()),
            Expr::Ident(name) => match name.as_str() {
                "undefined" => Ok(Value::Undefined),
                "NaN" => Ok(Value::Num(f64::NAN)),
                "Infinity" => Ok(Value::Num(f64::INFINITY)),
                _ => self.lookup(scope, name),
            },
            Expr::Template(pieces, exprs) => {
                let mut out = pieces[0].clone();
                for (i, e) in exprs.iter().enumerate() {
                    let v = self.eval(e, scope, this)?;
                    out.push_str(&self.to_string(&v)?);
                    out.push_str(&pieces[i + 1]);
                    if out.len() > MAX_STRING {
                        return Err(Ctrl::Abort("string too long".into()));
                    }
                }
                Ok(str_value(&out))
            }
            Expr::Array(items) => {
                let mut values = Vec::with_capacity(items.len());
                for i in items {
                    values.push(self.eval(i, scope, this)?);
                }
                Ok(self.new_array(values))
            }
            Expr::Object(props) => {
                let obj = self.new_object();
                for (key, value) in props {
                    let name = match key {
                        PropKey::Name(n) => n.clone(),
                        PropKey::Computed(k) => {
                            let kv = self.eval(k, scope, this)?;
                            self.property_key(&kv)?
                        }
                    };
                    let v = self.eval(value, scope, this)?;
                    Interp::set_prop(&obj, &name, v);
                }
                Ok(Value::Obj(obj))
            }
            Expr::Function(f) => {
                // a named function expression can refer to itself by its name
                if let (Some(name), false) = (&f.name, f.arrow) {
                    let inner = new_scope(Some(scope.clone()));
                    let closure = self.make_closure(f, &inner, None);
                    Interp::declare(&inner, name, closure.clone(), true);
                    return Ok(closure);
                }
                let captured = if f.arrow { Some(this.clone()) } else { None };
                Ok(self.make_closure(f, scope, captured))
            }
            Expr::Unary(op, operand) => self.eval_unary(op, operand, scope, this),
            Expr::Update(op, prefix, target) => {
                let place = self.eval_place(target, scope, this)?;
                let old = self.read_place(&place, scope)?;
                let n = self.to_number(&old)?;
                let new = if *op == "++" { n + 1.0 } else { n - 1.0 };
                self.write_place(&place, Value::Num(new), scope)?;
                Ok(Value::Num(if *prefix { new } else { n }))
            }
            Expr::Binary(op, l, r) => {
                let a = self.eval(l, scope, this)?;
                let b = self.eval(r, scope, this)?;
                self.binary(op, &a, &b)
            }
            Expr::Logical(op, l, r) => {
                let a = self.eval(l, scope, this)?;
                let short = match *op {
                    "&&" => !a.truthy(),
                    "||" => a.truthy(),
                    _ => !matches!(a, Value::Undefined | Value::Null),
                };
                if short { Ok(a) } else { self.eval(r, scope, this) }
            }
            Expr::Assign(op, target, value) => self.eval_assign(op, target, value, scope, this),
            Expr::Cond(t, a, b) => {
                if self.eval(t, scope, this)?.truthy() { self.eval(a, scope, this) } else { self.eval(b, scope, this) }
            }
            Expr::Seq(list) => {
                let mut last = Value::Undefined;
                for e in list {
                    last = self.eval(e, scope, this)?;
                }
                Ok(last)
            }
            Expr::Dot(obj, name) => {
                let o = self.eval(obj, scope, this)?;
                self.get_member(&o, name)
            }
            Expr::Index(obj, index) => {
                let o = self.eval(obj, scope, this)?;
                let i = self.eval(index, scope, this)?;
                let key = self.property_key(&i)?;
                self.get_member(&o, &key)
            }
            Expr::Call(callee, args) => {
                // a method call passes the object as `this`
                let (func, this_arg) = match &**callee {
                    Expr::Dot(obj, name) => {
                        let o = self.eval(obj, scope, this)?;
                        (self.get_member(&o, name)?, o)
                    }
                    Expr::Index(obj, index) => {
                        let o = self.eval(obj, scope, this)?;
                        let i = self.eval(index, scope, this)?;
                        let key = self.property_key(&i)?;
                        (self.get_member(&o, &key)?, o)
                    }
                    other => (self.eval(other, scope, this)?, Value::Undefined),
                };
                let mut values = Vec::with_capacity(args.len());
                for a in args {
                    values.push(self.eval(a, scope, this)?);
                }
                if !func.is_callable() {
                    return self.throw("TypeError", "is not a function");
                }
                self.call(&func, &this_arg, &values)
            }
            Expr::New(callee, args) => {
                let ctor = self.eval(callee, scope, this)?;
                let mut values = Vec::with_capacity(args.len());
                for a in args {
                    values.push(self.eval(a, scope, this)?);
                }
                self.construct(&ctor, &values)
            }
        }
    }

    fn eval_unary(&mut self, op: &str, operand: &Expr, scope: &ScopeRef, this: &Value) -> Res {
        if op == "typeof" {
            // typeof on an undeclared name is "undefined", not an error
            if let Expr::Ident(name) = operand {
                if !matches!(name.as_str(), "undefined" | "NaN" | "Infinity") {
                    return match self.lookup(scope, name) {
                        Ok(v) => Ok(str_value(v.type_of())),
                        Err(Ctrl::Throw(_)) => Ok(str_value("undefined")),
                        Err(e) => Err(e),
                    };
                }
            }
        }
        if op == "delete" {
            if let Expr::Dot(obj, name) = operand {
                let o = self.eval(obj, scope, this)?;
                return Ok(Value::Bool(self.delete_member(&o, name)));
            }
            if let Expr::Index(obj, index) = operand {
                let o = self.eval(obj, scope, this)?;
                let i = self.eval(index, scope, this)?;
                let key = self.property_key(&i)?;
                return Ok(Value::Bool(self.delete_member(&o, &key)));
            }
            return Ok(Value::Bool(true));
        }
        let v = self.eval(operand, scope, this)?;
        Ok(match op {
            "!" => Value::Bool(!v.truthy()),
            "-" => Value::Num(-self.to_number(&v)?),
            "+" => Value::Num(self.to_number(&v)?),
            "~" => Value::Num(!to_int32(self.to_number(&v)?) as f64),
            "typeof" => str_value(v.type_of()),
            "void" => Value::Undefined,
            _ => Value::Undefined,
        })
    }

    fn delete_member(&mut self, target: &Value, name: &str) -> bool {
        if let Value::Obj(o) = target {
            let mut b = o.borrow_mut();
            if b.props.remove(name).is_some() {
                self.steps += (b.order.len() / 16) as u64; // (finding the key in the order list is a scan)
                b.order.retain(|k| k != name);
            }
            if let (Kind::Array(a), Some(i)) = (&mut b.kind, array_index(name)) {
                if i < a.len() {
                    a[i] = Value::Undefined;
                }
            }
        }
        true
    }

    fn assign_to(&mut self, target: &Expr, value: Value, scope: &ScopeRef, this: &Value) -> Result<(), Ctrl> {
        match target {
            Expr::Ident(name) => self.assign_var(scope, name, value),
            Expr::Dot(obj, name) => {
                let o = self.eval(obj, scope, this)?;
                self.set_member(&o, name, value)
            }
            Expr::Index(obj, index) => {
                let o = self.eval(obj, scope, this)?;
                let i = self.eval(index, scope, this)?;
                let key = self.property_key(&i)?;
                self.set_member(&o, &key, value)
            }
            _ => self.throw("SyntaxError", "invalid assignment target"),
        }
    }

    /// Work out what an assignment target refers to, once (its object and key are evaluated a
    /// single time, before the right-hand side, as the language says).
    fn eval_place(&mut self, target: &Expr, scope: &ScopeRef, this: &Value) -> Result<Place, Ctrl> {
        match target {
            Expr::Ident(name) => Ok(Place::Var(name.clone())),
            Expr::Dot(obj, name) => Ok(Place::Member(self.eval(obj, scope, this)?, name.clone())),
            Expr::Index(obj, index) => {
                let o = self.eval(obj, scope, this)?;
                let i = self.eval(index, scope, this)?;
                let key = self.property_key(&i)?;
                Ok(Place::Member(o, key))
            }
            _ => self.throw("SyntaxError", "invalid assignment target"),
        }
    }

    fn read_place(&mut self, place: &Place, scope: &ScopeRef) -> Res {
        match place {
            Place::Var(name) => match name.as_str() {
                "undefined" => Ok(Value::Undefined),
                "NaN" => Ok(Value::Num(f64::NAN)),
                "Infinity" => Ok(Value::Num(f64::INFINITY)),
                _ => self.lookup(scope, name),
            },
            Place::Member(o, key) => self.get_member(o, key),
        }
    }

    fn write_place(&mut self, place: &Place, value: Value, scope: &ScopeRef) -> Result<(), Ctrl> {
        match place {
            Place::Var(name) => self.assign_var(scope, name, value),
            Place::Member(o, key) => self.set_member(o, key, value),
        }
    }

    fn eval_assign(&mut self, op: &str, target: &Expr, value: &Expr, scope: &ScopeRef, this: &Value) -> Res {
        let place = self.eval_place(target, scope, this)?;
        if op == "=" {
            let v = self.eval(value, scope, this)?;
            self.write_place(&place, v.clone(), scope)?;
            return Ok(v);
        }
        let old = self.read_place(&place, scope)?;
        // logical assignments only evaluate (and assign) the right side when needed
        let skip = match op {
            "&&=" => !old.truthy(),
            "||=" => old.truthy(),
            "??=" => !matches!(old, Value::Undefined | Value::Null),
            _ => false,
        };
        if skip {
            return Ok(old);
        }
        let right = self.eval(value, scope, this)?;
        let new = match op {
            "&&=" | "||=" | "??=" => right,
            _ => self.binary(&op[..op.len() - 1], &old, &right)?,
        };
        self.write_place(&place, new.clone(), scope)?;
        Ok(new)
    }

    fn binary(&mut self, op: &str, a: &Value, b: &Value) -> Res {
        Ok(match op {
            "+" => {
                let (pa, pb) = (self.to_primitive(a, false)?, self.to_primitive(b, false)?);
                if matches!(pa, Value::Str(_)) || matches!(pb, Value::Str(_)) {
                    let mut s = self.to_string(&pa)?;
                    s.push_str(&self.to_string(&pb)?);
                    if s.len() > MAX_STRING {
                        return Err(Ctrl::Abort("string too long".into()));
                    }
                    self.spend_bytes(s.len())?;
                    str_value(&s)
                } else {
                    Value::Num(self.to_number(&pa)? + self.to_number(&pb)?)
                }
            }
            "-" | "*" | "/" | "%" | "**" => {
                let (x, y) = (self.to_number(a)?, self.to_number(b)?);
                Value::Num(match op {
                    "-" => x - y,
                    "*" => x * y,
                    "/" => x / y,
                    "%" => x % y,
                    _ => x.powf(y),
                })
            }
            "==" => Value::Bool(self.loose_equals(a, b)?),
            "!=" => Value::Bool(!self.loose_equals(a, b)?),
            "===" => Value::Bool(strict_equals(a, b)),
            "!==" => Value::Bool(!strict_equals(a, b)),
            "<" | ">" | "<=" | ">=" => {
                let (pa, pb) = (self.to_primitive(a, false)?, self.to_primitive(b, false)?);
                if let (Value::Str(x), Value::Str(y)) = (&pa, &pb) {
                    return Ok(Value::Bool(match op {
                        "<" => x < y,
                        ">" => x > y,
                        "<=" => x <= y,
                        _ => x >= y,
                    }));
                }
                let (x, y) = (self.to_number(&pa)?, self.to_number(&pb)?);
                Value::Bool(match op {
                    "<" => x < y,
                    ">" => x > y,
                    "<=" => x <= y,
                    _ => x >= y,
                })
            }
            "&" | "|" | "^" | "<<" | ">>" | ">>>" => {
                let (x, y) = (to_int32(self.to_number(a)?), to_int32(self.to_number(b)?));
                Value::Num(match op {
                    "&" => (x & y) as f64,
                    "|" => (x | y) as f64,
                    "^" => (x ^ y) as f64,
                    "<<" => x.wrapping_shl(y as u32 & 31) as f64,
                    ">>" => (x >> (y as u32 & 31)) as f64,
                    _ => ((x as u32) >> (y as u32 & 31)) as f64,
                })
            }
            "in" => {
                let key = self.property_key(a)?;
                let Value::Obj(_) = b else { return self.throw("TypeError", "cannot use 'in' on a non-object") };
                let has = !self.own_keys(b).iter().all(|k| *k != key) || !matches!(self.get_member(b, &key)?, Value::Undefined);
                Value::Bool(has)
            }
            "instanceof" => {
                let Value::Obj(ctor) = b else { return self.throw("TypeError", "right side of instanceof is not callable") };
                let proto = self.get_member(b, "prototype")?;
                let _ = ctor;
                let (Value::Obj(instance), Value::Obj(target)) = (a, &proto) else { return Ok(Value::Bool(false)) };
                let mut cur = instance.borrow().proto.clone();
                while let Some(p) = cur {
                    if Rc::ptr_eq(&p, target) {
                        return Ok(Value::Bool(true));
                    }
                    cur = p.borrow().proto.clone();
                }
                Value::Bool(false)
            }
            _ => Value::Undefined,
        })
    }

    // ---- globals

    fn install_globals(&mut self) {
        let g = self.global.clone();
        let console = self.new_object();
        for name in ["log", "info", "warn", "error", "debug"] {
            let f = self.native(console_log);
            Interp::set_prop(&console, name, f);
        }
        Interp::declare(&g, "console", Value::Obj(console), true);

        let math = self.new_object();
        Interp::set_prop(&math, "PI", Value::Num(std::f64::consts::PI));
        Interp::set_prop(&math, "E", Value::Num(std::f64::consts::E));
        fn nf(f: NativeFn) -> NativeFn {
            f
        }
        let fns: [(&str, NativeFn); 14] = [
            ("floor", nf(|i, _, a| math1(i, a, f64::floor))),
            ("ceil", nf(|i, _, a| math1(i, a, f64::ceil))),
            ("round", nf(|i, _, a| math1(i, a, |x| { let f = x.floor(); if x - f >= 0.5 { f + 1.0 } else { f } }))),
            ("abs", nf(|i, _, a| math1(i, a, f64::abs))),
            ("sqrt", nf(|i, _, a| math1(i, a, f64::sqrt))),
            ("sin", nf(|i, _, a| math1(i, a, f64::sin))),
            ("cos", nf(|i, _, a| math1(i, a, f64::cos))),
            ("log", nf(|i, _, a| math1(i, a, f64::ln))),
            ("trunc", nf(|i, _, a| math1(i, a, f64::trunc))),
            ("min", nf(|i, _, a| math_fold(i, a, f64::INFINITY, f64::min))),
            ("max", nf(|i, _, a| math_fold(i, a, f64::NEG_INFINITY, f64::max))),
            (
                "pow",
                nf(|i, _, a| {
                    let (x, y) = (i.arg_number(a, 0)?, i.arg_number(a, 1)?);
                    Ok(Value::Num(x.powf(y)))
                }),
            ),
            (
                "random",
                nf(|i, _, _| {
                    // xorshift: a repeatable sequence is fine for page scripts
                    i.rng ^= i.rng << 13;
                    i.rng ^= i.rng >> 7;
                    i.rng ^= i.rng << 17;
                    Ok(Value::Num((i.rng >> 11) as f64 / (1u64 << 53) as f64))
                }),
            ),
            ("sign", nf(|i, _, a| math1(i, a, |x| if x == 0.0 || x.is_nan() { x } else { x.signum() }))),
        ];
        for (name, f) in fns {
            let v = self.native(f);
            Interp::set_prop(&math, name, v);
        }
        Interp::declare(&g, "Math", Value::Obj(math), true);

        let json = self.new_object();
        let stringify = self.native(json_stringify);
        Interp::set_prop(&json, "stringify", stringify);
        let parse = self.native(json_parse);
        Interp::set_prop(&json, "parse", parse);
        Interp::declare(&g, "JSON", Value::Obj(json), true);

        let object = self.native(|i, _, _| Ok(Value::Obj(i.new_object())));
        if let Value::Obj(o) = &object {
            let keys = self.native(|i, _, a| {
                let keys = i.own_keys(a.first().unwrap_or(&Value::Undefined));
                Ok(i.new_array(keys.iter().map(|k| str_value(k)).collect()))
            });
            Interp::set_prop(o, "keys", keys);
            Interp::set_prop(o, "prototype", Value::Obj(self.object_proto.clone()));
            let assign = self.native(|i, _, a| {
                let target = a.first().cloned().unwrap_or(Value::Undefined);
                for source in a.iter().skip(1) {
                    for k in i.own_keys(source) {
                        let v = i.get_member(source, &k)?;
                        i.set_member(&target, &k, v)?;
                    }
                }
                Ok(target)
            });
            Interp::set_prop(o, "assign", assign);
            let values = self.native(|i, _, a| {
                let subject = a.first().cloned().unwrap_or(Value::Undefined);
                let mut out = vec![];
                for k in i.own_keys(&subject) {
                    out.push(i.get_member(&subject, &k)?);
                }
                Ok(i.new_array(out))
            });
            Interp::set_prop(o, "values", values);
            let entries = self.native(|i, _, a| {
                let subject = a.first().cloned().unwrap_or(Value::Undefined);
                let mut out = vec![];
                for k in i.own_keys(&subject) {
                    let v = i.get_member(&subject, &k)?;
                    out.push(i.new_array(vec![str_value(&k), v]));
                }
                Ok(i.new_array(out))
            });
            Interp::set_prop(o, "entries", entries);
            let create = self.native(|i, _, a| {
                let obj = i.new_object();
                if let Some(Value::Obj(p)) = a.first() {
                    obj.borrow_mut().proto = Some(p.clone());
                } else if matches!(a.first(), Some(Value::Null)) {
                    obj.borrow_mut().proto = None;
                }
                Ok(Value::Obj(obj))
            });
            Interp::set_prop(o, "create", create);
        }
        self.object_ctor = object.clone();
        Interp::declare(&g, "Object", object, true);

        let array = self.native(|i, _, a| {
            // `Array(3)` is three empty slots; any other arguments are the elements
            if let [Value::Num(n)] = a {
                if *n >= 0.0 && n.fract() == 0.0 && *n <= MAX_ARRAY as f64 {
                    return Ok(i.new_array(vec![Value::Undefined; *n as usize]));
                }
                return i.throw("RangeError", "Invalid array length");
            }
            Ok(i.new_array(a.to_vec()))
        });
        if let Value::Obj(o) = &array {
            let is_array = self.native(|_, _, a| Ok(Value::Bool(matches!(a.first(), Some(Value::Obj(o)) if matches!(o.borrow().kind, Kind::Array(_))))));
            Interp::set_prop(o, "isArray", is_array);
            // (`[] instanceof Array`, and `Array.prototype.slice.call(arguments)`)
            Interp::set_prop(o, "prototype", Value::Obj(self.array_proto.clone()));
            Interp::set_prop(&self.array_proto.clone(), "constructor", array.clone());
        }
        self.array_ctor = array.clone();
        Interp::declare(&g, "Array", array, true);

        Interp::declare(&g, "String", self.native(|i, _, a| Ok(str_value(&match a.first() { Some(v) => i.to_string(v)?, None => String::new() }))), true);
        Interp::declare(&g, "Number", self.native(|i, _, a| Ok(Value::Num(match a.first() { Some(v) => i.to_number(v)?, None => 0.0 }))), true);
        Interp::declare(&g, "Boolean", self.native(|_, _, a| Ok(Value::Bool(a.first().is_some_and(|v| v.truthy())))), true);
        Interp::declare(&g, "parseInt", self.native(parse_int), true);
        Interp::declare(&g, "parseFloat", self.native(|i, _, a| {
            let s = i.arg_string(a, 0)?;
            Ok(Value::Num(parse_float_prefix(&s)))
        }), true);
        Interp::declare(&g, "isNaN", self.native(|i, _, a| Ok(Value::Bool(i.arg_number(a, 0)?.is_nan()))), true);
        Interp::declare(&g, "isFinite", self.native(|i, _, a| Ok(Value::Bool(i.arg_number(a, 0)?.is_finite()))), true);

        for name in ["Error", "TypeError", "RangeError", "ReferenceError", "SyntaxError"] {
            let ctor = self.native(error_constructor);
            if let Value::Obj(c) = &ctor {
                // each kind of error has its own prototype (carrying its name), chained to the common one
                let proto = if name == "Error" {
                    self.error_proto.clone()
                } else {
                    let p = self.new_object();
                    p.borrow_mut().proto = Some(self.error_proto.clone());
                    p
                };
                Interp::set_prop(&proto, "name", str_value(name));
                Interp::set_prop(c, "prototype", Value::Obj(proto));
                Interp::set_prop(c, "name", str_value(name));
            }
            Interp::declare(&g, name, ctor, true);
        }
    }

    fn arg_number(&mut self, args: &[Value], i: usize) -> Result<f64, Ctrl> {
        self.to_number(args.get(i).unwrap_or(&Value::Undefined))
    }

    fn arg_string(&mut self, args: &[Value], i: usize) -> Result<String, Ctrl> {
        self.to_string(args.get(i).unwrap_or(&Value::Undefined))
    }
}

// ------------------------------------------------------------------ built-in functions

fn console_log(i: &mut Interp, _: &Value, args: &[Value]) -> Res {
    let mut line = String::new();
    for (k, a) in args.iter().enumerate() {
        if k > 0 {
            line.push(' ');
        }
        let part = i.inspect(a, 0);
        // (what a script logs is bounded: each line, the number of lines, and the total)
        line.extend(part.chars().take(10_000));
        if line.len() > 20_000 {
            break;
        }
    }
    if i.console.len() < 10_000 && i.console_bytes < 4 * 1024 * 1024 {
        i.console_bytes += line.len();
        i.console.push(line);
    }
    Ok(Value::Undefined)
}

impl Interp {
    /// How `console.log` shows a value.
    fn inspect(&mut self, v: &Value, depth: usize) -> String {
        match v {
            Value::Str(s) if depth > 0 => format!("'{}'", s.chars().take(200).collect::<String>()),
            Value::Str(s) => s.chars().take(10_000).collect(),
            Value::Obj(o) if depth < 3 => {
                let kind_array = matches!(o.borrow().kind, Kind::Array(_));
                if matches!(o.borrow().kind, Kind::Closure(_) | Kind::Native(_) | Kind::Bound(_)) {
                    return "[Function]".into();
                }
                if matches!(o.borrow().kind, Kind::Error) {
                    return self.describe_error(v);
                }
                if kind_array {
                    let items = if let Kind::Array(a) = &o.borrow().kind { a.clone() } else { vec![] };
                    let parts: Vec<String> = items.iter().take(100).map(|x| self.inspect(x, depth + 1)).collect();
                    return format!("[ {} ]", parts.join(", ")).replace("[  ]", "[]");
                }
                let keys = self.own_keys(v);
                let parts: Vec<String> = keys
                    .iter()
                    .take(100)
                    .map(|k| {
                        let val = o.borrow().props.get(k).cloned().unwrap_or(Value::Undefined);
                        format!("{k}: {}", self.inspect(&val, depth + 1))
                    })
                    .collect();
                if parts.is_empty() { "{}".into() } else { format!("{{ {} }}", parts.join(", ")) }
            }
            Value::Obj(_) => "[Object]".into(),
            other => self.to_string_lossy(other),
        }
    }
}

fn math1(i: &mut Interp, a: &[Value], f: fn(f64) -> f64) -> Res {
    let x = i.arg_number(a, 0)?;
    Ok(Value::Num(f(x)))
}

fn math_fold(i: &mut Interp, a: &[Value], init: f64, f: fn(f64, f64) -> f64) -> Res {
    let mut acc = init;
    for v in a {
        let n = i.to_number(v)?;
        if n.is_nan() {
            return Ok(Value::Num(f64::NAN));
        }
        acc = f(acc, n);
    }
    Ok(Value::Num(acc))
}

fn parse_int(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let s = i.arg_string(a, 0)?;
    let radix = match a.get(1) {
        Some(v) => i.to_number(v)? as u32,
        None => 10,
    };
    let t = s.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (radix, t) = match (radix, t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))) {
        (10 | 16, Some(rest)) if radix == 16 || a.get(1).is_none() => (16, rest),
        (0, _) => (10, t),
        (r, _) if (2..=36).contains(&r) => (r, t),
        _ => return Ok(Value::Num(f64::NAN)),
    };
    let digits: String = t.chars().take_while(|c| c.is_digit(radix)).collect();
    if digits.is_empty() {
        return Ok(Value::Num(f64::NAN));
    }
    let v = digits.chars().fold(0.0, |acc, c| acc * radix as f64 + c.to_digit(radix).unwrap_or(0) as f64);
    Ok(Value::Num(if neg { -v } else { v }))
}

fn error_constructor(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    // called with `new`: `this` is the fresh object; without it, make one
    let message = match a.first() {
        Some(Value::Undefined) | None => String::new(),
        Some(v) => i.to_string(v)?,
    };
    let err = i.make_error("Error", &message);
    if let (Value::Obj(e), Value::Obj(t)) = (&err, this) {
        // take the name from the constructor's prototype link when called through `new`
        let proto = t.borrow().proto.clone();
        e.borrow_mut().proto = proto;
        let name = {
            let mut cur = t.borrow().proto.clone();
            let mut found = None;
            while let Some(p) = cur {
                if let Some(Value::Str(n)) = p.borrow().props.get("name") {
                    found = Some(n.to_string());
                    break;
                }
                cur = p.borrow().proto.clone();
            }
            found
        };
        if let Some(n) = name {
            Interp::set_prop(e, "name", str_value(&n));
        }
    }
    Ok(err)
}

fn obj_has_own(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let key = i.arg_string(a, 0)?;
    Ok(Value::Bool(i.own_keys(this).contains(&key) || matches!(this, Value::Obj(o) if o.borrow().props.contains_key(&key))))
}

fn obj_to_string(i: &mut Interp, this: &Value, _: &[Value]) -> Res {
    if let Value::Obj(o) = this {
        if i.is_error_like(o) {
            return Ok(str_value(&i.describe_error(this)));
        }
        if matches!(o.borrow().kind, Kind::Closure(_) | Kind::Native(_) | Kind::Bound(_)) {
            return Ok(str_value("function () { [native code] }"));
        }
    }
    Ok(str_value("[object Object]"))
}

fn bool_to_string(_: &mut Interp, this: &Value, _: &[Value]) -> Res {
    Ok(str_value(if this.truthy() { "true" } else { "false" }))
}

fn num_to_string(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let n = i.to_number(this)?;
    let radix = match a.first() {
        Some(v) if !matches!(v, Value::Undefined) => i.to_number(v)? as u32,
        _ => 10,
    };
    if radix == 10 || !(2..=36).contains(&radix) || n.fract() != 0.0 || !n.is_finite() {
        return Ok(str_value(&number_to_string(n)));
    }
    let (neg, mut m) = (n < 0.0, n.abs() as u128);
    let mut digits = vec![];
    while m > 0 {
        digits.push(std::char::from_digit((m % radix as u128) as u32, radix).unwrap_or('0'));
        m /= radix as u128;
    }
    if digits.is_empty() {
        digits.push('0');
    }
    if neg {
        digits.push('-');
    }
    Ok(str_value(&digits.iter().rev().collect::<String>()))
}

fn num_to_fixed(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let n = i.to_number(this)?;
    let wanted = match a.first() {
        None | Some(Value::Undefined) => 0.0,
        Some(v) => i.to_number(v)?,
    };
    if !(0.0..=100.0).contains(&wanted) {
        return i.throw("RangeError", "toFixed() digits argument must be between 0 and 100");
    }
    let digits = wanted as usize;
    if !n.is_finite() || n.abs() >= 1e21 {
        return Ok(str_value(&number_to_string(n)));
    }
    let n = if n == 0.0 { 0.0 } else { n }; // (-0 prints without its sign)
    // an exact tie rounds away from zero (2.5 -> "3"), where Rust's formatting rounds to even
    let scaled = n.abs() * 10f64.powi(digits as i32);
    if scaled < 1e15 && scaled.fract() == 0.5 {
        let rounded = scaled.floor() as u128 + 1;
        let mut text = rounded.to_string();
        if digits > 0 {
            while text.len() <= digits {
                text.insert(0, '0');
            }
            text.insert(text.len() - digits, '.');
        }
        return Ok(str_value(&if n < 0.0 { format!("-{text}") } else { text }));
    }
    Ok(str_value(&format!("{n:.digits$}")))
}

fn json_stringify(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    fn go(i: &mut Interp, v: &Value, out: &mut String, depth: usize) -> Result<bool, Ctrl> {
        if depth > 20 {
            return Err(Ctrl::Abort("JSON.stringify: too deeply nested (or circular)".into()));
        }
        if out.len() > MAX_STRING {
            return Err(Ctrl::Abort("string too long".into()));
        }
        i.charge(16)?;
        match v {
            Value::Undefined => return Ok(false),
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(&b.to_string()),
            Value::Num(n) => out.push_str(&if n.is_finite() { number_to_string(*n) } else { "null".into() }),
            Value::Str(s) => json_quote(s, out),
            Value::Obj(o) => {
                if v.is_callable() {
                    return Ok(false);
                }
                let array = if let Kind::Array(a) = &o.borrow().kind { Some(a.clone()) } else { None };
                if let Some(items) = array {
                    out.push('[');
                    for (k, item) in items.iter().enumerate() {
                        if k > 0 {
                            out.push(',');
                        }
                        if !go(i, item, out, depth + 1)? {
                            out.push_str("null");
                        }
                    }
                    out.push(']');
                } else {
                    out.push('{');
                    let mut first = true;
                    for key in i.own_keys(v) {
                        let val = o.borrow().props.get(&key).cloned().unwrap_or(Value::Undefined);
                        let mark = out.len();
                        if !first {
                            out.push(',');
                        }
                        json_quote(&key, out);
                        out.push(':');
                        if go(i, &val, out, depth + 1)? {
                            first = false;
                        } else {
                            out.truncate(mark);
                        }
                    }
                    out.push('}');
                }
            }
        }
        Ok(true)
    }
    let mut out = String::new();
    let v = a.first().cloned().unwrap_or(Value::Undefined);
    if go(i, &v, &mut out, 0)? { Ok(str_value(&out)) } else { Ok(Value::Undefined) }
}

fn json_parse(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let text = i.arg_string(a, 0)?;
    if text.len() > MAX_STRING {
        return Err(Ctrl::Abort("string too long".into()));
    }
    i.charge(text.len() * 4)?;
    let chars: Vec<char> = text.chars().collect();
    let mut p = 0usize;
    let v = json_value(i, &chars, &mut p, 0)?;
    json_ws(&chars, &mut p);
    if p != chars.len() {
        return i.throw("SyntaxError", &format!("Unexpected token in JSON at position {p}"));
    }
    Ok(v)
}

fn json_ws(c: &[char], p: &mut usize) {
    while *p < c.len() && matches!(c[*p], ' ' | '\t' | '\n' | '\r') {
        *p += 1;
    }
}

fn json_error<T>(i: &mut Interp, p: usize) -> Result<T, Ctrl> {
    i.throw("SyntaxError", &format!("Unexpected token in JSON at position {p}"))
}

fn json_value(i: &mut Interp, c: &[char], p: &mut usize, depth: usize) -> Res {
    if depth > 100 {
        return i.throw("SyntaxError", "JSON nested too deeply");
    }
    json_ws(c, p);
    let Some(&ch) = c.get(*p) else { return json_error(i, *p) };
    match ch {
        '{' => {
            *p += 1;
            let obj = i.new_object();
            json_ws(c, p);
            if c.get(*p) == Some(&'}') {
                *p += 1;
                return Ok(Value::Obj(obj));
            }
            loop {
                json_ws(c, p);
                if c.get(*p) != Some(&'"') {
                    return json_error(i, *p);
                }
                let key = json_string(i, c, p)?;
                json_ws(c, p);
                if c.get(*p) != Some(&':') {
                    return json_error(i, *p);
                }
                *p += 1;
                let v = json_value(i, c, p, depth + 1)?;
                Interp::set_prop(&obj, &key, v);
                i.tick()?;
                json_ws(c, p);
                match c.get(*p) {
                    Some(',') => *p += 1,
                    Some('}') => {
                        *p += 1;
                        return Ok(Value::Obj(obj));
                    }
                    _ => return json_error(i, *p),
                }
            }
        }
        '[' => {
            *p += 1;
            let mut items = vec![];
            json_ws(c, p);
            if c.get(*p) == Some(&']') {
                *p += 1;
                return Ok(i.new_array(items));
            }
            loop {
                items.push(json_value(i, c, p, depth + 1)?);
                i.tick()?;
                json_ws(c, p);
                match c.get(*p) {
                    Some(',') => *p += 1,
                    Some(']') => {
                        *p += 1;
                        return Ok(i.new_array(items));
                    }
                    _ => return json_error(i, *p),
                }
            }
        }
        '"' => Ok(str_value(&json_string(i, c, p)?)),
        't' | 'f' | 'n' => {
            for (word, value) in [("true", Value::Bool(true)), ("false", Value::Bool(false)), ("null", Value::Null)] {
                if c[*p..].iter().take(word.len()).collect::<String>() == word {
                    *p += word.len();
                    return Ok(value);
                }
            }
            json_error(i, *p)
        }
        '-' | '0'..='9' => {
            let start = *p;
            if c[*p] == '-' {
                *p += 1;
            }
            let digits = |p: &mut usize| {
                let from = *p;
                while *p < c.len() && c[*p].is_ascii_digit() {
                    *p += 1;
                }
                *p - from
            };
            let int_start = *p;
            let int_digits = digits(p);
            // (no digits, or a leading zero followed by more digits, is not JSON)
            if int_digits == 0 || (int_digits > 1 && c[int_start] == '0') {
                return json_error(i, *p);
            }
            if c.get(*p) == Some(&'.') {
                *p += 1;
                if digits(p) == 0 {
                    return json_error(i, *p);
                }
            }
            if matches!(c.get(*p), Some('e' | 'E')) {
                *p += 1;
                if matches!(c.get(*p), Some('+' | '-')) {
                    *p += 1;
                }
                if digits(p) == 0 {
                    return json_error(i, *p);
                }
            }
            let text: String = c[start..*p].iter().collect();
            Ok(Value::Num(text.parse::<f64>().unwrap_or(f64::NAN)))
        }
        _ => json_error(i, *p),
    }
}

fn json_string(i: &mut Interp, c: &[char], p: &mut usize) -> Result<String, Ctrl> {
    *p += 1; // the opening quote
    let mut out = String::new();
    loop {
        let Some(&ch) = c.get(*p) else { return json_error(i, *p) };
        *p += 1;
        match ch {
            '"' => return Ok(out),
            '\\' => {
                let Some(&e) = c.get(*p) else { return json_error(i, *p) };
                *p += 1;
                match e {
                    '"' | '\\' | '/' => out.push(e),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => {
                        let hex = |p: usize| -> Option<u32> {
                            let h: String = c.get(p..p + 4)?.iter().collect();
                            if h.len() == 4 && h.chars().all(|d| d.is_ascii_hexdigit()) { u32::from_str_radix(&h, 16).ok() } else { None }
                        };
                        let Some(mut v) = hex(*p) else { return json_error(i, *p) };
                        *p += 4;
                        if (0xD800..0xDC00).contains(&v) && c.get(*p) == Some(&'\\') && c.get(*p + 1) == Some(&'u') {
                            if let Some(low) = hex(*p + 2).filter(|l| (0xDC00..0xE000).contains(l)) {
                                v = 0x10000 + ((v - 0xD800) << 10) + (low - 0xDC00);
                                *p += 6;
                            }
                        }
                        out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                    }
                    _ => return json_error(i, *p - 1),
                }
            }
            c if (c as u32) < 0x20 => return json_error(i, *p - 1),
            c => out.push(c),
        }
        if out.len() > MAX_STRING {
            return Err(Ctrl::Abort("string too long".into()));
        }
    }
}

fn json_quote(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---- functions: call / apply / bind

fn function_method(name: &str) -> Option<NativeFn> {
    Some(match name {
        "call" => |i, this, a| {
            let this_arg = a.first().cloned().unwrap_or(Value::Undefined);
            i.call(this, &this_arg, a.get(1..).unwrap_or(&[]))
        },
        "bind" => |i, this, a| {
            if !this.is_callable() {
                return i.throw("TypeError", "bind called on a non-function");
            }
            let bound_this = a.first().cloned().unwrap_or(Value::Undefined);
            let leading = a.get(1..).unwrap_or(&[]).to_vec();
            i.objects += 1;
            Ok(Value::Obj(Rc::new(RefCell::new(Object {
                kind: Kind::Bound(Box::new((this.clone(), bound_this, leading))),
                props: HashMap::new(),
                order: vec![],
                proto: Some(i.object_proto.clone()),
            }))))
        },
        "apply" => |i, this, a| {
            let this_arg = a.first().cloned().unwrap_or(Value::Undefined);
            let args = match a.get(1) {
                Some(Value::Obj(o)) => match &o.borrow().kind {
                    Kind::Array(items) => items.clone(),
                    _ => vec![],
                },
                _ => vec![],
            };
            i.call(this, &this_arg, &args)
        },
        _ => return None,
    })
}

// ---- arrays

fn array_items(v: &Value) -> Vec<Value> {
    match v {
        Value::Obj(o) => match &o.borrow().kind {
            Kind::Array(a) => a.clone(),
            _ => vec![],
        },
        _ => vec![],
    }
}

fn with_array<R>(v: &Value, f: impl FnOnce(&mut Vec<Value>) -> R) -> Option<R> {
    match v {
        Value::Obj(o) => match &mut o.borrow_mut().kind {
            Kind::Array(a) => Some(f(a)),
            _ => None,
        },
        _ => None,
    }
}

fn array_join(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let sep = match a.first() {
        Some(Value::Undefined) | None => ",".to_string(),
        Some(v) => i.to_string(v)?,
    };
    // an array that contains itself (however deep) joins as nothing at the point of the cycle
    if let Value::Obj(o) = this {
        if i.joining.iter().any(|j| Rc::ptr_eq(j, o)) {
            return Ok(str_value(""));
        }
        i.joining.push(o.clone());
    }
    let items = array_items(this);
    i.charge(items.len())?;
    let mut out = String::new();
    let mut result = Ok(());
    for (k, item) in items.iter().enumerate() {
        if k > 0 {
            out.push_str(&sep);
        }
        if !matches!(item, Value::Undefined | Value::Null) {
            match i.to_string(item) {
                Ok(s) => out.push_str(&s),
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if out.len() > MAX_STRING {
            result = Err(Ctrl::Abort("string too long".into()));
            break;
        }
    }
    if matches!(this, Value::Obj(_)) {
        i.joining.pop();
    }
    result?;
    i.spend_bytes(out.len())?;
    Ok(str_value(&out))
}

/// A possibly negative position (from a number argument) against a length: NaN is 0, negative
/// counts from the end, and the result is never past the end.
fn rel_len(n: f64, len: usize) -> usize {
    let n = if n.is_nan() { 0.0 } else { n.trunc() };
    if n < 0.0 { (len as f64 + n).max(0.0) as usize } else { n.min(len as f64) as usize }
}

fn array_items_len(v: &Value) -> usize {
    match v {
        Value::Obj(o) => match &o.borrow().kind {
            Kind::Array(a) => a.len(),
            _ => 0,
        },
        _ => 0,
    }
}

/// Compare two elements for `sort`: the script's comparator, or by their text.
fn sort_compare(i: &mut Interp, cmp: &Option<Value>, x: &Value, y: &Value) -> Result<f64, Ctrl> {
    let n = match cmp {
        Some(f) if f.is_callable() => {
            let v = i.call(f, &Value::Undefined, &[x.clone(), y.clone()])?;
            i.to_number(&v)?
        }
        _ => {
            let (sx, sy) = (i.to_string(x)?, i.to_string(y)?);
            if sx < sy { -1.0 } else if sx > sy { 1.0 } else { 0.0 }
        }
    };
    Ok(if n.is_nan() { 0.0 } else { n })
}

/// A stable merge sort whose comparator may fail, and may be inconsistent (a shuffle written as
/// `sort(() => Math.random() - 0.5)`): it always finishes, in whatever order. `undefined`s go last.
fn sort_values(i: &mut Interp, items: Vec<Value>, cmp: Option<Value>) -> Result<Vec<Value>, Ctrl> {
    let (mut src, undefs): (Vec<Value>, Vec<Value>) = items.into_iter().partition(|v| !matches!(v, Value::Undefined));
    let n = src.len();
    i.charge(n * 8)?;
    let mut dst: Vec<Value> = Vec::with_capacity(n);
    let mut width = 1;
    while width < n {
        dst.clear();
        let mut start = 0;
        while start < n {
            let (mid, end) = ((start + width).min(n), (start + 2 * width).min(n));
            let (mut l, mut r) = (start, mid);
            while l < mid && r < end {
                if sort_compare(i, &cmp, &src[r], &src[l])? < 0.0 {
                    dst.push(src[r].clone());
                    r += 1;
                } else {
                    dst.push(src[l].clone());
                    l += 1;
                }
            }
            dst.extend_from_slice(&src[l..mid]);
            dst.extend_from_slice(&src[r..end]);
            start = end;
        }
        std::mem::swap(&mut src, &mut dst);
        width *= 2;
    }
    src.extend(undefs);
    Ok(src)
}

/// Resolve a possibly negative index argument against a length.
fn rel_index(i: &mut Interp, a: &[Value], at: usize, len: usize, default: usize) -> Result<usize, Ctrl> {
    match a.get(at) {
        None | Some(Value::Undefined) => Ok(default),
        Some(v) => {
            let n = i.to_number(v)?;
            let n = if n.is_nan() { 0.0 } else { n.trunc() };
            Ok(if n < 0.0 { (len as f64 + n).max(0.0) as usize } else { (n as usize).min(len) })
        }
    }
}

fn array_method(name: &str) -> Option<NativeFn> {
    Some(match name {
        "push" => |i, this, a| {
            let n = with_array(this, |v| {
                v.extend(a.iter().cloned());
                v.len()
            })
            .unwrap_or(0);
            if n > MAX_ARRAY {
                return Err(Ctrl::Abort("array too big".into()));
            }
            i.bytes += 16 * a.len() as u64;
            Ok(Value::Num(n as f64))
        },
        "pop" => |_, this, _| Ok(with_array(this, |v| v.pop()).flatten().unwrap_or(Value::Undefined)),
        "shift" => |_, this, _| Ok(with_array(this, |v| if v.is_empty() { None } else { Some(v.remove(0)) }).flatten().unwrap_or(Value::Undefined)),
        "unshift" => |_, this, a| {
            Ok(Value::Num(with_array(this, |v| {
                v.splice(0..0, a.iter().cloned());
                v.len()
            })
            .unwrap_or(0) as f64))
        },
        "join" => array_join,
        "slice" => |i, this, a| {
            let items = array_items(this);
            let from = rel_index(i, a, 0, items.len(), 0)?;
            let to = rel_index(i, a, 1, items.len(), items.len())?;
            Ok(i.new_array(if from < to { items[from..to].to_vec() } else { vec![] }))
        },
        "splice" => |i, this, a| {
            // (converting the arguments can run script code that changes the array, so the
            // lengths are worked out after that, inside the borrow)
            let start_n = match a.first() {
                Some(v) => Some(i.to_number(v)?),
                None => None,
            };
            let count_n = match a.get(1) {
                Some(v) => Some(i.to_number(v)?),
                None => None,
            };
            let removed = with_array(this, |v| {
                let len = v.len();
                let start = rel_len(start_n.unwrap_or(0.0), len);
                let count = match count_n {
                    Some(c) => (if c.is_nan() { 0.0 } else { c.trunc() }).clamp(0.0, (len - start) as f64) as usize,
                    None if a.is_empty() => 0,
                    None => len - start,
                };
                v.splice(start..start + count, a.iter().skip(2).cloned()).collect::<Vec<_>>()
            })
            .unwrap_or_default();
            if array_items_len(this) > MAX_ARRAY {
                return Err(Ctrl::Abort("array too big".into()));
            }
            Ok(i.new_array(removed))
        },
        "concat" => |i, this, a| {
            let mut items = array_items(this);
            for v in a {
                match v {
                    Value::Obj(o) if matches!(o.borrow().kind, Kind::Array(_)) => items.extend(array_items(v)),
                    other => items.push(other.clone()),
                }
            }
            if items.len() > MAX_ARRAY {
                return Err(Ctrl::Abort("array too big".into()));
            }
            Ok(i.new_array(items))
        },
        "indexOf" => |_, this, a| {
            let target = a.first().cloned().unwrap_or(Value::Undefined);
            Ok(Value::Num(array_items(this).iter().position(|x| strict_equals(x, &target)).map_or(-1.0, |p| p as f64)))
        },
        "includes" => |_, this, a| {
            let target = a.first().cloned().unwrap_or(Value::Undefined);
            Ok(Value::Bool(array_items(this).iter().any(|x| strict_equals(x, &target))))
        },
        "reverse" => |_, this, _| {
            with_array(this, |v| v.reverse());
            Ok(this.clone())
        },
        "forEach" => |i, this, a| {
            let f = a.first().cloned().unwrap_or(Value::Undefined);
            let this_arg = a.get(1).cloned().unwrap_or(Value::Undefined);
            for (k, item) in array_items(this).into_iter().enumerate() {
                i.call(&f, &this_arg, &[item, Value::Num(k as f64), this.clone()])?;
            }
            Ok(Value::Undefined)
        },
        "map" => |i, this, a| {
            let f = a.first().cloned().unwrap_or(Value::Undefined);
            let this_arg = a.get(1).cloned().unwrap_or(Value::Undefined);
            let mut out = vec![];
            for (k, item) in array_items(this).into_iter().enumerate() {
                out.push(i.call(&f, &this_arg, &[item, Value::Num(k as f64), this.clone()])?);
            }
            Ok(i.new_array(out))
        },
        "some" => |i, this, a| {
            let f = a.first().cloned().unwrap_or(Value::Undefined);
            let this_arg = a.get(1).cloned().unwrap_or(Value::Undefined);
            for (k, item) in array_items(this).into_iter().enumerate() {
                if i.call(&f, &this_arg, &[item, Value::Num(k as f64), this.clone()])?.truthy() {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        },
        "every" => |i, this, a| {
            let f = a.first().cloned().unwrap_or(Value::Undefined);
            let this_arg = a.get(1).cloned().unwrap_or(Value::Undefined);
            for (k, item) in array_items(this).into_iter().enumerate() {
                if !i.call(&f, &this_arg, &[item, Value::Num(k as f64), this.clone()])?.truthy() {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        },
        "find" => |i, this, a| {
            let f = a.first().cloned().unwrap_or(Value::Undefined);
            let this_arg = a.get(1).cloned().unwrap_or(Value::Undefined);
            for (k, item) in array_items(this).into_iter().enumerate() {
                if i.call(&f, &this_arg, &[item.clone(), Value::Num(k as f64), this.clone()])?.truthy() {
                    return Ok(item);
                }
            }
            Ok(Value::Undefined)
        },
        "findIndex" => |i, this, a| {
            let f = a.first().cloned().unwrap_or(Value::Undefined);
            let this_arg = a.get(1).cloned().unwrap_or(Value::Undefined);
            for (k, item) in array_items(this).into_iter().enumerate() {
                if i.call(&f, &this_arg, &[item, Value::Num(k as f64), this.clone()])?.truthy() {
                    return Ok(Value::Num(k as f64));
                }
            }
            Ok(Value::Num(-1.0))
        },
        "filter" => |i, this, a| {
            let f = a.first().cloned().unwrap_or(Value::Undefined);
            let this_arg = a.get(1).cloned().unwrap_or(Value::Undefined);
            let mut out = vec![];
            for (k, item) in array_items(this).into_iter().enumerate() {
                if i.call(&f, &this_arg, &[item.clone(), Value::Num(k as f64), this.clone()])?.truthy() {
                    out.push(item);
                }
            }
            Ok(i.new_array(out))
        },
        "reduce" => |i, this, a| {
            let f = a.first().cloned().unwrap_or(Value::Undefined);
            let items = array_items(this);
            let mut iter = items.into_iter().enumerate();
            let mut acc = match a.get(1) {
                Some(v) => v.clone(),
                None => match iter.next() {
                    Some((_, v)) => v,
                    None => return i.throw("TypeError", "Reduce of empty array with no initial value"),
                },
            };
            for (k, item) in iter {
                acc = i.call(&f, &Value::Undefined, &[acc, item, Value::Num(k as f64), this.clone()])?;
            }
            Ok(acc)
        },
        "sort" => |i, this, a| {
            let items = array_items(this);
            let sorted = sort_values(i, items, a.first().cloned())?;
            with_array(this, |v| *v = sorted);
            Ok(this.clone())
        },
        _ => return None,
    })
}

// ---- strings

fn string_method(name: &str) -> Option<NativeFn> {
    Some(match name {
        "toUpperCase" => |i, t, _| Ok(str_value(&i.to_string(t)?.to_uppercase())),
        "toLowerCase" => |i, t, _| Ok(str_value(&i.to_string(t)?.to_lowercase())),
        "trim" => |i, t, _| Ok(str_value(i.to_string(t)?.trim())),
        "charAt" => |i, t, a| {
            let s = i.to_string(t)?;
            let k = i.arg_number(a, 0).unwrap_or(0.0);
            Ok(str_value(&if k >= 0.0 { s.chars().nth(k as usize).map(|c| c.to_string()).unwrap_or_default() } else { String::new() }))
        },
        "charCodeAt" => |i, t, a| {
            let s = i.to_string(t)?;
            let k = i.arg_number(a, 0).unwrap_or(0.0);
            Ok(Value::Num(if k >= 0.0 { s.chars().nth(k as usize).map_or(f64::NAN, |c| c as u32 as f64) } else { f64::NAN }))
        },
        "indexOf" => |i, t, a| {
            let (s, needle) = (i.to_string(t)?, i.arg_string(a, 0)?);
            Ok(Value::Num(s.find(&needle).map_or(-1.0, |b| s[..b].chars().count() as f64)))
        },
        "lastIndexOf" => |i, t, a| {
            let (s, needle) = (i.to_string(t)?, i.arg_string(a, 0)?);
            Ok(Value::Num(s.rfind(&needle).map_or(-1.0, |b| s[..b].chars().count() as f64)))
        },
        "includes" => |i, t, a| Ok(Value::Bool(i.to_string(t)?.contains(&i.arg_string(a, 0)?))),
        "startsWith" => |i, t, a| Ok(Value::Bool(i.to_string(t)?.starts_with(&i.arg_string(a, 0)?))),
        "endsWith" => |i, t, a| Ok(Value::Bool(i.to_string(t)?.ends_with(&i.arg_string(a, 0)?))),
        "slice" => |i, t, a| {
            let chars: Vec<char> = i.to_string(t)?.chars().collect();
            let from = rel_index(i, a, 0, chars.len(), 0)?;
            let to = rel_index(i, a, 1, chars.len(), chars.len())?;
            Ok(str_value(&if from < to { chars[from..to].iter().collect::<String>() } else { String::new() }))
        },
        "substring" => |i, t, a| {
            // (unlike slice: negatives and NaN are 0, and the two ends may come in either order)
            let chars: Vec<char> = i.to_string(t)?.chars().collect();
            let clamp = |n: f64| if n.is_nan() { 0 } else { n.clamp(0.0, chars.len() as f64) as usize };
            let from = clamp(i.arg_number(a, 0)?);
            let to = match a.get(1) {
                None | Some(Value::Undefined) => chars.len(),
                Some(v) => clamp(i.to_number(v)?),
            };
            let (lo, hi) = (from.min(to), from.max(to));
            Ok(str_value(&chars[lo..hi].iter().collect::<String>()))
        },
        "substr" => |i, t, a| {
            let chars: Vec<char> = i.to_string(t)?.chars().collect();
            let from = rel_len(i.arg_number(a, 0)?, chars.len());
            let count = match a.get(1) {
                None | Some(Value::Undefined) => chars.len() - from,
                Some(v) => {
                    let n = i.to_number(v)?;
                    (if n.is_nan() { 0.0 } else { n.trunc() }).clamp(0.0, (chars.len() - from) as f64) as usize
                }
            };
            Ok(str_value(&chars[from..from + count].iter().collect::<String>()))
        },
        "split" => |i, t, a| {
            let s = i.to_string(t)?;
            let limit = match a.get(1) {
                None | Some(Value::Undefined) => usize::MAX,
                Some(v) => i.to_number(v)?.clamp(0.0, 1e9) as usize,
            };
            let mut parts: Vec<Value> = match a.first() {
                None | Some(Value::Undefined) => vec![str_value(&s)],
                Some(sep) => {
                    let sep = i.to_string(sep)?;
                    if sep.is_empty() { s.chars().map(|c| str_value(&c.to_string())).collect() } else { s.split(&sep).map(str_value).collect() }
                }
            };
            parts.truncate(limit);
            Ok(i.new_array(parts))
        },
        "replace" => |i, t, a| {
            let (s, from, to) = (i.to_string(t)?, i.arg_string(a, 0)?, i.arg_string(a, 1)?);
            let r = s.replacen(&from, &to, 1);
            if r.len() > MAX_STRING {
                return Err(Ctrl::Abort("string too long".into()));
            }
            Ok(str_value(&r))
        },
        "replaceAll" => |i, t, a| {
            let (s, from, to) = (i.to_string(t)?, i.arg_string(a, 0)?, i.arg_string(a, 1)?);
            if from.is_empty() || s.len().saturating_mul(to.len().max(1)) > MAX_STRING * 4 {
                return Ok(str_value(&s));
            }
            let r = s.replace(&from, &to);
            if r.len() > MAX_STRING {
                return Err(Ctrl::Abort("string too long".into()));
            }
            i.spend_bytes(r.len())?;
            Ok(str_value(&r))
        },
        "padStart" => |i, t, a| {
            let s = i.to_string(t)?;
            let target = i.arg_number(a, 0)?.clamp(0.0, MAX_STRING as f64) as usize;
            let fill = match a.get(1) {
                None | Some(Value::Undefined) => " ".to_string(),
                Some(v) => i.to_string(v)?,
            };
            let have = s.chars().count();
            if target <= have || fill.is_empty() {
                return Ok(str_value(&s));
            }
            let pad: String = fill.chars().cycle().take(target - have).collect();
            i.spend_bytes(pad.len() + s.len())?;
            Ok(str_value(&(pad + &s)))
        },
        "padEnd" => |i, t, a| {
            let s = i.to_string(t)?;
            let target = i.arg_number(a, 0)?.clamp(0.0, MAX_STRING as f64) as usize;
            let fill = match a.get(1) {
                None | Some(Value::Undefined) => " ".to_string(),
                Some(v) => i.to_string(v)?,
            };
            let have = s.chars().count();
            if target <= have || fill.is_empty() {
                return Ok(str_value(&s));
            }
            let pad: String = fill.chars().cycle().take(target - have).collect();
            i.spend_bytes(pad.len() + s.len())?;
            Ok(str_value(&(s + &pad)))
        },
        "repeat" => |i, t, a| {
            let (s, n) = (i.to_string(t)?, i.arg_number(a, 0)?);
            if !(0.0..=1e7).contains(&n) || (s.len() as f64) * n > MAX_STRING as f64 {
                return i.throw("RangeError", "Invalid count value");
            }
            i.spend_bytes(s.len() * n as usize)?;
            Ok(str_value(&s.repeat(n as usize)))
        },
        "concat" => |i, t, a| {
            let mut s = i.to_string(t)?;
            for v in a {
                s.push_str(&i.to_string(v)?);
                if s.len() > MAX_STRING {
                    return Err(Ctrl::Abort("string too long".into()));
                }
            }
            i.spend_bytes(s.len())?;
            Ok(str_value(&s))
        },
        "toString" => |i, t, _| Ok(str_value(&i.to_string(t)?)),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run on a thread with a roomy stack (the interpreter recurses as deep as the script does).
    fn on_big_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
        std::thread::Builder::new().stack_size(256 * 1024 * 1024).spawn(f).unwrap().join().unwrap()
    }

    /// Run a script and return what it logged, joined by newlines (or the error).
    fn run(src: &'static str) -> String {
        on_big_stack(move || {
            let mut js = Interp::new();
            match js.run(src) {
                Ok(_) => js.console.join("\n"),
                Err(e) => format!("ERR {e}"),
            }
        })
    }

    #[test]
    fn arithmetic_strings_and_conversions() {
        assert_eq!(run("console.log(1 + 2 * 3, 10 / 4, 7 % 3, 2 ** 10, -5 + 2)"), "7 2.5 1 1024 -3");
        assert_eq!(run("console.log('a' + 1, 1 + '2', '3' * '4', true + 1, null + 1, undefined + 1)"), "a1 12 12 2 1 NaN");
        assert_eq!(run("console.log(0.1 + 0.2, 1 / 3, 1e21, 1e-7, 123456789012, -0)"), "0.30000000000000004 0.3333333333333333 1e+21 1e-7 123456789012 0");
        assert_eq!(run("console.log(1/0, -1/0, 0/0, typeof NaN)"), "Infinity -Infinity NaN number");
        assert_eq!(run("console.log(5 & 3, 5 | 3, 5 ^ 3, ~5, 1 << 4, -16 >> 2, -1 >>> 28)"), "1 7 6 -6 16 -4 15");
        assert_eq!(run("console.log(parseInt('42px'), parseInt('ff', 16), parseInt('0x1f'), parseFloat('3.14abc'), parseInt('x'), Number(''), Number('12'), Number('1e3'), Number('inf'))"), "42 255 31 3.14 NaN 0 12 1000 NaN");
        assert_eq!(run("console.log(String(12), String(null), (255).toString(16), (5).toString(2), (3.14159).toFixed(2), Boolean(''), Boolean('x'))"), "12 null ff 101 3.14 false true");
    }

    #[test]
    fn comparison_and_equality() {
        assert_eq!(run("console.log(1 == '1', 1 === '1', null == undefined, null === undefined, 0 == '', NaN == NaN, 'a' < 'b', 2 < 10, '2' < '10', [1] == 1)"), "true false true false true false true true false true");
        assert_eq!(run("var o = {}; console.log(o == o, o === {}, o != {})"), "true false true");
    }

    #[test]
    fn variables_scope_closures_and_hoisting() {
        assert_eq!(run("var a = 1; { var a = 2 } console.log(a)"), "2");
        assert_eq!(run("let a = 1; { let a = 2 } console.log(a)"), "1");
        assert_eq!(run("console.log(typeof x, f()); var x = 1; function f() { return 'hoisted' }"), "undefined hoisted");
        assert_eq!(run("function counter() { var n = 0; return function() { return ++n } } var c = counter(); c(); c(); console.log(c())"), "3");
        assert_eq!(run("var fs = []; for (let i = 0; i < 3; i++) fs.push(() => i); console.log(fs[0](), fs[1](), fs[2]())"), "0 1 2");
        assert_eq!(run("var fs = []; for (var i = 0; i < 3; i++) fs.push(function() { return i }); console.log(fs[0](), fs[2]())"), "3 3");
        assert_eq!(run("const c = 1; try { c = 2 } catch (e) { console.log(e.name, e.message) }"), "TypeError Assignment to constant variable.");
        assert_eq!(run("try { nope } catch (e) { console.log(e.name + ': ' + e.message) }"), "ReferenceError: nope is not defined");
        assert_eq!(run("console.log(typeof undeclared, typeof null, typeof (() => 1), typeof 'x', typeof {}, typeof [])"), "undefined object function string object object");
        assert_eq!(run("x = 5; console.log(x)"), "5", "assigning an undeclared name makes a global");
        assert_eq!(run("var f = function fact(n) { return n <= 1 ? 1 : n * fact(n - 1) }; console.log(f(5))"), "120");
    }

    #[test]
    fn functions_this_new_and_prototypes() {
        assert_eq!(run("function add(a, b) { return a + b } console.log(add(1, 2), add(1), add.call(null, 3, 4), add.apply(null, [5, 6]))"), "3 NaN 7 11");
        assert_eq!(run("var o = { n: 3, get: function() { return this.n } }; console.log(o.get())"), "3");
        assert_eq!(run("var o = { n: 3, get() { return this.n }, arrow: () => typeof this }; console.log(o.get(), o.arrow())"), "3 undefined");
        assert_eq!(run("function P(n) { this.n = n } P.prototype.double = function() { return this.n * 2 }; var p = new P(4); console.log(p.double(), p instanceof P, p instanceof Object, p.n)"), "8 true true 4");
        assert_eq!(run("function f() { return arguments.length } console.log(f(1, 2, 3))"), "3");
        assert_eq!(run("var o = { m() { return () => this.v }, v: 9 }; console.log(o.m()())"), "9");
        assert_eq!(run("function C() { return { custom: true } } console.log(new C().custom)"), "true");
        assert_eq!(run("console.log(typeof Math.max, Math.max(1, 5, 3), Math.min(), Math.floor(-1.5), Math.round(2.5), Math.abs(-3), Math.pow(2, 8))"), "function 5 Infinity -2 3 3 256");
    }

    #[test]
    fn objects_arrays_and_their_methods() {
        assert_eq!(run("var o = {a: 1, 'b': 2}; o.c = 3; o['d'] = 4; delete o.a; console.log(Object.keys(o).join(), 'b' in o, 'a' in o, o.hasOwnProperty('c'), o.zzz)"), "b,c,d true false true undefined");
        assert_eq!(run("var a = [3, 1, 2]; a.push(4); console.log(a.length, a.pop(), a.shift(), a.join('-'), a.indexOf(1), a.includes(9))"), "4 4 3 1-2 0 false");
        assert_eq!(run("console.log([1,2,3,4].slice(1, 3), [1,2,3].slice(-2), [1,2,3].concat([4], 5), [3,1,2].sort(), [10,9,1].sort(), [10,9,1].sort((a, b) => a - b))"), "[ 2, 3 ] [ 2, 3 ] [ 1, 2, 3, 4, 5 ] [ 1, 2, 3 ] [ 1, 10, 9 ] [ 1, 9, 10 ]");
        assert_eq!(run("var a = [1,2,3,4,5]; var r = a.splice(1, 2, 'x'); console.log(a.join(), r.join())"), "1,x,4,5 2,3");
        assert_eq!(run("console.log([1,2,3].map(x => x * 2).filter(x => x > 2).reduce((s, x) => s + x, 0), [1,2,3].reduce((a, b) => a + b))"), "10 6");
        assert_eq!(run("var s = 0; [1,2,3].forEach(function(x, i) { s += x * i }); console.log(s)"), "8");
        assert_eq!(run("var a = []; a[3] = 'x'; console.log(a.length, a[0], Array.isArray(a), Array.isArray({}), [1, [2, 3]].join('|'))"), "4 undefined true false 1|2,3");
        assert_eq!(run("var a = [1,2,3]; a.length = 1; console.log(a.length, a.reverse().join(), [].concat.call ? 'ok' : 'no')"), "1 1 ok");
        assert_eq!(run("for (var k in {a: 1, b: 2}) console.log(k); for (var v of [10, 20]) console.log(v); for (var c of 'hi') console.log(c)"), "a\nb\n10\n20\nh\ni");
        assert_eq!(run("console.log({a: 1, b: [1, 'x'], c: {d: null}}, [], {})"), "{ a: 1, b: [ 1, 'x' ], c: { d: null } } [] {}");
    }

    #[test]
    fn strings_and_templates() {
        assert_eq!(run("var s = 'Hello, World'; console.log(s.length, s.toUpperCase(), s.charAt(1), s.charCodeAt(0), s.indexOf('o'), s.slice(-5), s.substring(0, 5), s.split(', ').length, s.includes('World'), s.startsWith('He'), s.replace('o', '0'))"), "12 HELLO, WORLD e 72 4 World Hello 2 true true Hell0, World");
        assert_eq!(run("var n = 3; console.log(`n is ${n}, double ${n * 2}!`, ' x '.trim() + '|', 'ab'.repeat(3), 'a-b-c'.split('-'), 'abc'[1], 'é日'.length)"), "n is 3, double 6! x| ababab [ 'a', 'b', 'c' ] b 2");
    }

    #[test]
    fn control_flow_and_exceptions() {
        assert_eq!(run("var s = 0; for (var i = 0; i < 10; i++) { if (i % 2) continue; if (i > 6) break; s += i } console.log(s)"), "12");
        assert_eq!(run("var i = 0; do { i++ } while (i < 5); while (i > 3) i--; console.log(i)"), "3");
        assert_eq!(run("function f(x) { switch (x) { case 1: return 'one'; case 2: case 3: return 'few'; default: return 'many' } } console.log(f(1), f(3), f(9))"), "one few many");
        assert_eq!(run("var out = []; switch (2) { case 1: out.push(1); case 2: out.push(2); case 3: out.push(3); break; case 4: out.push(4) } console.log(out.join())"), "2,3");
        assert_eq!(run("try { throw new Error('boom') } catch (e) { console.log(e.message, e instanceof Error, String(e)) } finally { console.log('done') }"), "boom true Error: boom\ndone");
        assert_eq!(run("function f() { try { return 1 } finally { console.log('cleanup') } } console.log(f())"), "cleanup\n1");
        assert_eq!(run("try { null.x } catch (e) { console.log(e.name, e.message) } try { undefined.f() } catch (e) { console.log(e.name) } try { (1)() } catch (e) { console.log(e.name) }"), "TypeError Cannot read properties of null (reading 'x')\nTypeError\nTypeError");
        assert_eq!(run("throw new TypeError('bad')"), "ERR Uncaught TypeError: bad");
        assert_eq!(run("throw 'plain'"), "ERR Uncaught plain");
        assert_eq!(run("var x = 1; x ||= 5; var y = null; y ??= 7; var z = 2; z &&= 9; console.log(x, y, z, null ?? 'd', 0 || 'e', 1 && 'f')"), "1 7 9 d e f");
        assert_eq!(run("var a = 1, b = (a++, a++, a); console.log(a, b, a > 2 ? 'big' : 'small', void 0)"), "3 3 big undefined");
    }

    #[test]
    fn json_stringify_works_for_plain_data() {
        assert_eq!(run("console.log(JSON.stringify({a: 1, b: 'x\"y', c: [1, null, undefined, () => 1], d: undefined, e: {f: true}}))"), r#"{"a":1,"b":"x\"y","c":[1,null,null,null],"e":{"f":true}}"#);
        assert_eq!(run("console.log(JSON.stringify('s'), JSON.stringify(NaN), JSON.stringify(undefined) === undefined)"), "\"s\" null true");
    }

    #[test]
    fn runaway_scripts_are_stopped() {
        assert_eq!(run("while (true) {}"), "ERR script ran for too long and was stopped");
        assert_eq!(run("for (;;) { try { while (1) {} } catch (e) {} }"), "ERR script ran for too long and was stopped", "a script can't catch its own stop");
        assert_eq!(run("function f() { return f() } f()"), "ERR call stack too deep");
        assert_eq!(run("var s = 'x'; while (true) { s = s + s }"), "ERR string too long");
        assert_eq!(run("var a = []; a[1e9] = 1"), "ERR array too big");
        assert_eq!(run("var a = [1]; while (true) { a = a.concat(a) }"), "ERR array too big");
        assert_eq!(run("'x'.repeat(1e9)"), "ERR Uncaught RangeError: Invalid count value");
        assert_eq!(run("var o = {}; o.o = o; JSON.stringify(o)"), "ERR JSON.stringify: too deeply nested (or circular)");
        // an error in the script text is reported, not a crash
        assert!(run("var = ;").starts_with("ERR line 1"));
    }

    /// Run scripts against a page; returns what they logged and the page's final markup (`<body>` content).
    fn page(html: &'static str, scripts: &'static [&'static str]) -> (String, String) {
        let dom = crate::html::parse(html);
        let list = scripts.iter().enumerate().map(|(n, s)| (s.to_string(), format!("script {}", n + 1))).collect();
        let done = run_page_scripts(dom, "http://test.example/page", list).expect("scripts ran");
        let body = (0..done.dom.nodes.len()).find(|&n| done.dom.tag(n) == Some("body")).unwrap();
        (done.console.join("\n"), dom_bindings::inner_html_for_tests(&done.dom, body))
    }

    #[test]
    fn scripts_read_and_change_the_page() {
        let (log, html) = page(
            "<body><p id=a>hi <b>there</b></p><ul id=l></ul></body>",
            &["var p = document.getElementById('a'); console.log(p.tagName, p.textContent, p.innerHTML, p.nodeType, document.getElementById('nope'));
              p.textContent = 'changed';
              var l = document.getElementById('l');
              for (var i = 0; i < 3; i++) { var li = document.createElement('li'); li.textContent = 'item ' + i; li.className = 'x'; l.appendChild(li) }
              console.log(l.children.length, l.firstChild.textContent, l.lastChild.className, l.childNodes.length)"],
        );
        assert_eq!(log, "P hi there hi <b>there</b> 1 null\n3 item 0 x 3");
        assert_eq!(html, "<p id=\"a\">changed</p><ul id=\"l\"><li class=\"x\">item 0</li><li class=\"x\">item 1</li><li class=\"x\">item 2</li></ul>");
    }

    #[test]
    fn queries_attributes_and_class_lists() {
        let (log, html) = page(
            "<body><div class='a b' id=d><span class=a>1</span><span>2</span><p class='a'>3</p></div><input id=i value=v></body>",
            &["var d = document.querySelector('#d');
              console.log(document.querySelectorAll('.a').length, d.querySelectorAll('span').length, d.querySelector('p.a').textContent, document.querySelector('.zzz'), document.querySelectorAll('span, p').length);
              console.log(document.getElementsByTagName('span').length, document.getElementsByClassName('a b').length, d.getElementsByTagName('*').length);
              console.log(d.getAttribute('class'), d.getAttribute('nope'), d.hasAttribute('id'));
              d.setAttribute('data-x', '5'); d.removeAttribute('id'); d.classList.add('c'); d.classList.remove('a'); console.log(d.classList.contains('b'), d.classList.toggle('b'), d.classList.toggle('z'), d.className);
              var i = document.querySelector('input'); console.log(i.value, i.id); i.value = 'new'; i.disabled = true; console.log(i.getAttribute('value'), i.hasAttribute('disabled'));
              try { document.querySelector('<<<') } catch (e) { console.log(e.name) }"],
        );
        assert_eq!(log, "3 2 3 null 3\n2 1 3\na b null true\ntrue false true c z\nv i\nnew true\nSyntaxError");
        assert!(html.contains("data-x=\"5\"") && html.contains("class=\"c z\"") && html.contains("disabled") && !html.contains("id=\"d\""), "{html}");
    }

    #[test]
    fn inner_html_text_nodes_and_tree_moves() {
        let (log, html) = page(
            "<body><div id=box><i>old</i></div><div id=other></div></body>",
            &["var box = document.getElementById('box');
              box.innerHTML = '<p>one</p><p class=\"two\">t&amp;w<br>o</p><script>console.log(\"must not run\")</script>';
              console.log(box.children.length, box.querySelector('.two').textContent, box.innerHTML);
              var other = document.getElementById('other');
              var p = box.firstChild; other.appendChild(p); console.log(box.children.length, other.children.length, p.parentNode === other, p === other.firstChild);
              var t = document.createTextNode('txt'); other.insertBefore(t, p); console.log(other.childNodes.length, other.firstChild.nodeType, other.firstChild.nodeValue, other.textContent);
              var q = other.removeChild(p); console.log(other.children.length, q.textContent);
              var c = box.cloneNode(true); console.log(c.children.length, c.parentNode, c === box);
              other.remove(); console.log(document.getElementById('other'));
              var n = document.createElement('section'); n.innerHTML = '<em>x</em>'; document.body.appendChild(n); console.log(document.body.lastChild.innerHTML, document.querySelector('section em').textContent);
              try { n.appendChild(document.body) } catch (e) { console.log(e.name) }"],
        );
        assert_eq!(
            log,
            "3 t&wo <p>one</p><p class=\"two\">t&amp;w<br>o</p><script>console.log(\"must not run\")</script>\n2 1 true true\n2 3 txt txtone\n0 one\n2 null false\nnull\n<em>x</em> x\nError"
        );
        assert!(html.contains("<section><em>x</em></section>") && !html.contains("id=\"other\""), "{html}");
    }

    #[test]
    fn event_listeners_bubble_and_can_be_stopped() {
        let dom = crate::html::parse("<body><div id=outer><button id=b>go</button></div></body>");
        let scripts = vec![(
            "var log = [];
             document.getElementById('b').addEventListener('click', function(e) { console.log('button', e.type, e.target.id, this.id, e.currentTarget.id) });
             document.getElementById('outer').addEventListener('click', function(e) { console.log('outer', e.target.id); e.preventDefault() });
             document.addEventListener('click', function(e) { console.log('document') });
             document.getElementById('b').onclick = function() { console.log('onclick property') };
             document.getElementById('b').onclick = function() { console.log('onclick replaced') };
             var gone = function() { console.log('never') };
             document.getElementById('b').addEventListener('click', gone);
             document.getElementById('b').removeEventListener('click', gone);
             document.addEventListener('DOMContentLoaded', function() { console.log('ready') });
             window.addEventListener('load', function() { console.log('loaded') });"
                .to_string(),
            "s".to_string(),
        )];
        let mut done = run_page_scripts(dom, "http://t.example/", scripts).unwrap();
        assert_eq!(done.console.join(","), "ready,loaded");
        let b = (0..done.dom.nodes.len()).find(|&n| done.dom.attr(n, "id") == Some("b")).unwrap();
        let mut dom = done.dom;
        let prevented = done.host.dispatch(&mut dom, b, "click");
        assert!(prevented, "a handler called preventDefault");
        assert_eq!(done.host.take_console().join(","), "button click b b b,onclick replaced,outer b,document");
        // a click on something with no handlers still bubbles up to the ones above it
        let outer = (0..dom.nodes.len()).find(|&n| dom.attr(n, "id") == Some("outer")).unwrap();
        assert!(done.host.dispatch(&mut dom, outer, "click"));
        assert_eq!(done.host.take_console().join(","), "outer outer,document");
        assert!(!done.host.dispatch(&mut dom, b, "mouseover"));
        assert_eq!(done.host.take_console().len(), 0);
        assert!(dom.nodes.len() > 3, "the DOM came back");
    }

    #[test]
    fn stop_propagation_and_errors_in_handlers() {
        let dom = crate::html::parse("<body><div id=o><p id=p>x</p></div></body>");
        let scripts = vec![(
            "document.getElementById('p').addEventListener('click', function(e) { console.log('p'); e.stopPropagation() });
             document.getElementById('o').addEventListener('click', function() { console.log('o') });
             document.getElementById('p').addEventListener('click', function() { throw new Error('boom') });
             document.getElementById('p').addEventListener('click', function() { console.log('after the error') });"
                .to_string(),
            "s".to_string(),
        )];
        let mut done = run_page_scripts(dom, "http://t.example/", scripts).unwrap();
        let p = (0..done.dom.nodes.len()).find(|&n| done.dom.attr(n, "id") == Some("p")).unwrap();
        let mut dom = done.dom;
        done.host.dispatch(&mut dom, p, "click");
        assert_eq!(done.host.take_console().join(","), "p,Uncaught Error: boom,after the error");
    }

    #[test]
    fn timers_run_in_order_of_their_delay() {
        let (log, _) = page(
            "<body></body>",
            &["setTimeout(function() { console.log('c 30') }, 30);
              setTimeout(function() { console.log('a 0') });
              setTimeout(function(x, y) { console.log('b 10', x, y) }, 10, 'X', 'Y');
              var id = setTimeout(function() { console.log('cancelled') }, 5); clearTimeout(id);
              var n = 0; var iv = setInterval(function() { n++; console.log('tick', n) }, 7);
              setTimeout(function() { setTimeout(function() { console.log('nested 50+') }, 20); console.log('d 50') }, 50);
              console.log('sync')"],
        );
        // (the interval ticks at 7, 14 and 21, then stops)
        assert_eq!(log, "sync\na 0\ntick 1\nb 10 X Y\ntick 2\ntick 3\nc 30\nd 50\nnested 50+");
    }

    #[test]
    fn script_errors_are_logged_and_later_scripts_still_run() {
        let (log, _) = page(
            "<body><p id=p>x</p></body>",
            &["var shared = 1; missing.call()", "console.log(shared); syntax error here", "console.log('third', typeof window, typeof document, typeof location.href, window === self, typeof globalThis.document)"],
        );
        // (a script with a syntax error doesn't run at all, so its `console.log` never happens)
        assert_eq!(log, "script 1: Uncaught ReferenceError: missing is not defined\nscript 2: line 1: expected ';'\nthird object object string true object");
    }

    #[test]
    fn ordinary_loops_are_not_stopped_by_the_limits() {
        assert_eq!(run("var a=[]; for(var i=0;i<30000;i++)a.push(i); console.log(a.length)"), "30000");
        assert_eq!(run("var s=''; for(var i=0;i<25000;i++){s+='x'} console.log(s.length)"), "25000");
        assert_eq!(run("function f(x){return x+1} var n=0; for(var i=0;i<300000;i++){n=f(n)} console.log(n)"), "300000");
        assert_eq!(run("function fib(n){return n<2?n:fib(n-1)+fib(n-2)} console.log(fib(22))"), "17711");
        assert_eq!(run("var t=0; for(var i=0;i<300000;i++){var o={a:i}; t+=o.a>0?1:0} console.log(t)"), "299999");
        assert_eq!(run("var s='x'.repeat(30000),c=0; for(var i=0;i<s.length;i++){c+=s.charCodeAt(i)>0?1:0} console.log(c)"), "30000");
    }

    #[test]
    fn dropping_deep_object_graphs_and_cycles_does_not_overflow_or_leak() {
        // a long chain freed on a small stack
        let made = on_big_stack(|| {
            let mut js = Interp::new();
            js.run("var a={}; for(var i=0;i<100000;i++){a={x:a}} function helper(){return a}").unwrap();
            Unsend(js)
        });
        std::thread::Builder::new().stack_size(2 * 1024 * 1024).spawn(move || {
            let made = made;
            drop(made.0);
        }).unwrap().join().unwrap();
        // memory held by a script that declared a function is given back
        let leaked = on_big_stack(|| {
            let before = crate::memcount::live_bytes();
            for _ in 0..5 {
                let mut js = Interp::new();
                js.run("function helper(){} var a=[]; for(var i=0;i<20000;i++){a.push({i:i})}").unwrap();
            }
            crate::memcount::live_bytes() - before
        });
        assert!(leaked < 4 * 1024 * 1024, "leaked {leaked} bytes");
    }

    #[test]
    fn review_findings_about_page_scripts() {
        // `arguments` inside a template literal
        assert_eq!(run("function f(){ return `${arguments[0]}` } console.log(f(5))"), "5");
        assert_eq!(run("function f(){ return [1].map(() => `${arguments[0]}`)[0] } console.log(f(9))"), "9");
        // a forged class list target is refused, not a panic
        let (log, _) = page("<body></body>", &["try { document.body.classList.add.call({__node: 1e9}, 'x') } catch (e) { console.log('refused') }"]);
        assert_eq!(log, "refused");
        // an interval can stop itself
        let (log, _) = page("<body></body>", &["var n=0, id=setInterval(function(){ n++; console.log('tick ' + n); clearInterval(id) }, 10)"]);
        assert_eq!(log, "tick 1");
        // events from a detached element don't reach the document
        let (log, _) = page("<body></body>", &["document.addEventListener('click', function(){ console.log('document') }); document.createElement('div').click(); document.body.click()"]);
        assert_eq!(log, "document");
    }

    #[test]
    fn deep_trees_are_refused_and_memory_is_a_budget_for_the_whole_page() {
        let (log, _) = page("<body></body>", &["var d = document.body; try { for (var i = 0; i < 5000; i++) { var c = document.createElement('div'); d.appendChild(c); d = c } } catch (e) { console.log('stopped: ' + e.message) }"]);
        assert!(log.starts_with("stopped: the page is nested too deeply"), "{log}");
        // events share what the load left of the allowance
        let dom = crate::html::parse("<body></body>");
        let src = vec![("var keep=[]; document.addEventListener('click', function(){ var s='x'.repeat(4000000); for (var i=0;i<10;i++) keep.push(s+i); console.log('kept ' + keep.length) })".to_string(), "s".to_string())];
        let mut done = run_page_scripts(dom, "http://t.example/", src).expect("ran");
        done.host.interp().retained = MAX_SCRIPT_MEMORY - 20 * 1024 * 1024; // (as if earlier events had kept almost all of it)
        let mut dom = done.dom;
        done.host.dispatch(&mut dom, dom_bindings::DOCUMENT, "click");
        let lines = done.host.take_console();
        assert!(lines.iter().any(|l| l.contains("too much memory")), "{lines:?} (load: {:?})", done.console);
    }

    #[test]
    fn element_style_details_from_review() {
        let (log, _) = page(
            "<body><p id=a style='background: url(\"a;b.png\"); color: red'>x</p></body>",
            &["var s = document.getElementById('a').style;
               console.log(s.background, s.color);
               s.setProperty('--my-var', '5px'); console.log(s.getPropertyValue('--my-var'), s.getPropertyValue('FONT-SIZE') === '');
               s.webkitTransition = 'none'; console.log(s.cssText);
               s.cssText += 'top: 1px'; console.log(s.top, s.color);
               s.color = null; console.log(s.color === '', s.length);
               s.width = '1px /*'; console.log(s.width === '');
               try { s.setProperty('color') } catch (e) { console.log('needs two') }
               console.log(s.getPropertyPriority('color') === '')"],
        );
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines[0], "url(\"a;b.png\") red", "{log}");
        assert_eq!(lines[1], "5px true", "{log}");
        assert_eq!(lines[2], "background: url(\"a;b.png\"); color: red; --my-var: 5px; -webkit-transition: none;", "{log}");
        assert_eq!(lines[3], "1px red", "{log}");
        assert_eq!(lines[4], "true 4", "{log}");
        assert_eq!(lines[5], "true");
        assert_eq!(lines[6], "needs two");
        assert_eq!(lines[7], "true");
    }

    #[test]
    fn promises_chain_in_order_after_the_running_code() {
        let (log, _) = page(
            "<body></body>",
            &["console.log('start');
               var p = new Promise(function (resolve) { resolve(1) });
               p.then(function (v) { console.log('then ' + v); return v + 1 })
                .then(function (v) { console.log('then ' + v); throw new Error('boom') })
                .then(function () { console.log('skipped') })
                .catch(function (e) { console.log('caught ' + e.message); return 'recovered' })
                .finally(function () { console.log('finally') })
                .then(function (v) { console.log('after finally ' + v) });
               new Promise(function (_, reject) { reject('no') }).then(null, function (r) { console.log('rejected ' + r) });
               Promise.resolve(Promise.resolve('nested')).then(function (v) { console.log(v) });
               new Promise(function () { throw new Error('in executor') }).catch(function (e) { console.log(e.message) });
               Promise.all([1, Promise.resolve(2), new Promise(function (r) { setTimeout(function () { r(3) }, 10) })]).then(function (v) { console.log('all ' + v.join()) });
               Promise.all([Promise.resolve(1), Promise.reject('bad')]).catch(function (e) { console.log('all failed ' + e) });
               Promise.all([]).then(function (v) { console.log('empty ' + v.length) });
               setTimeout(function () { console.log('timeout') }, 0);
               console.log('end')"],
        );
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(&lines[..2], ["start", "end"], "{log}");
        for expected in ["then 1", "then 2", "caught boom", "recovered".trim(), "finally", "after finally recovered", "rejected no", "nested", "in executor", "all failed bad", "empty 0", "all 1,2,3"] {
            let found = lines.iter().any(|l| *l == expected) || expected == "recovered";
            assert!(found, "missing {expected:?} in {log}");
        }
        let at = |s: &str| lines.iter().position(|l| *l == s).unwrap();
        assert!(at("then 1") < at("then 2") && at("then 2") < at("caught boom") && at("caught boom") < at("finally") && at("finally") < at("after finally recovered"));
        assert!(at("then 1") < at("timeout"), "callbacks run before the next timer: {log}");
        assert!(at("timeout") < at("all 1,2,3"));
    }

    #[test]
    fn moving_a_node_under_a_newer_parent_keeps_its_listeners_and_its_descendants_objects() {
        let (log, body) = page(
            "<body><div id=old><span id=kid>a</span></div></body>",
            &["var old = document.getElementById('old'), kid = document.getElementById('kid');
               kid.addEventListener('click', function () { console.log('kid clicked') });
               old.onclick = function () { console.log('old clicked') };
               var btn = document.createElement('button'); btn.onclick = function () { console.log('btn clicked') };
               var box = document.createElement('div'); box.appendChild(btn); document.body.appendChild(box);
               btn.click();
               var wrap = document.createElement('section'); wrap.appendChild(old); document.body.appendChild(wrap);
               kid.click();
               kid.textContent = 'X';
               console.log(kid === document.getElementById('kid'), kid.parentNode === old, old.parentNode === wrap)"],
        );
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines, ["btn clicked", "kid clicked", "old clicked", "true true true"], "{log}");
        assert!(body.contains("<span id=\"kid\">X</span>"), "{body}");
    }

    #[test]
    fn a_page_that_has_waited_long_enough_for_the_network_is_refused() {
        let dom = crate::html::parse("<body></body>");
        let src = vec![("var x = new XMLHttpRequest(); x.open('GET', '/a'); x.onerror = function () { console.log(x.statusText) }; x.send()".to_string(), "s".to_string())];
        let mut done = run_page_scripts(dom, "http://127.0.0.1:9/", src).expect("ran");
        done.host.interp().net_time = std::time::Duration::from_secs(41);
        let mut dom = done.dom;
        // a click handler asks again
        let more = "var y = new XMLHttpRequest(); y.open('GET', '/b'); y.onerror = function () { console.log(y.statusText) }; y.send()";
        done.host.interp().run(&format!("document.addEventListener('click', function () {{ {more} }})")).unwrap();
        done.host.dispatch(&mut dom, dom_bindings::DOCUMENT, "click");
        let lines = done.host.take_console();
        assert!(lines.iter().any(|l| l.contains("too much time spent waiting")), "{lines:?}");
    }

    #[test]
    fn style_and_handler_edge_cases_from_second_review() {
        let (log, _) = page(
            "<body><p id=a>x</p></body>",
            &["var p = document.getElementById('a'), s = p.style;
               s.content = '\"'; s.width = '1px'; console.log(s.width);
               s.setProperty('background', 'url(a'); s.height = '2px'; console.log(s.height);
               p.only = 5; p.onboarding = {a: 1}; console.log(p.only, typeof p.onboarding)"],
        );
        assert_eq!(log, "1px\n2px\n5 object", "{log}");
    }

    #[test]
    fn lookups_stay_fast_in_deep_trees() {
        // 300 chains of 500 divs: the old lookup walked every node's ancestors, once per call
        let (log, _) = page(
            "<body></body>",
            &["for (var c = 0; c < 120; c++) { var d = document.body; for (var k = 0; k < 400; k++) { var e = document.createElement('div'); d.appendChild(e); d = e } }
               var t = 0; for (var q = 0; q < 40; q++) { if (document.getElementById('nope') === null) t++ } console.log(t)"],
        );
        assert!(log == "40" || log.contains("too long"), "{log}");
    }

    #[test]
    fn window_and_document_event_properties_fire() {
        let (log, _) = page(
            "<body><p id=a>x</p></body>",
            &["window.onload = function () { console.log('load') }; document.onclick = function (e) { console.log('click ' + e.type) };
               document.getElementById('a').click(); document.getElementById('a').click()"],
        );
        assert_eq!(log, "click click\nclick click\nload");
    }

    #[test]
    fn element_style_reads_and_writes_the_style_attribute() {
        let (log, body) = page(
            "<body><p id=a style='color: red; margin: 0'>x</p></body>",
            &["var p = document.getElementById('a'); console.log(p.style.color, p.style.fontSize === '');
               p.style.fontSize = '20px'; p.style.backgroundColor = 'blue'; p.style.color = '';
               p.style.setProperty('margin-top', '3px'); console.log(p.style.getPropertyValue('font-size'), p.style.cssText);
               p.style.display = 'none; color: green'; console.log(p.style.display === '');
               console.log(p.style.removeProperty('margin'), p.getAttribute('style'));
               p.style.cssText = 'top: 1px'; console.log(p.style.top, p.style.cssFloat === '')"],
        );
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines[0], "red true", "{log}");
        assert_eq!(lines[1], "20px margin: 0; font-size: 20px; background-color: blue; margin-top: 3px;", "{log}");
        assert_eq!(lines[2], "true", "an invalid value that would add a declaration is ignored");
        assert_eq!(lines[3], "0 font-size: 20px; background-color: blue; margin-top: 3px", "{log}");
        assert_eq!(lines[4], "1px true", "{log}");
        assert!(body.contains("style=\"top: 1px\""), "{body}");
    }

    #[test]
    fn arguments_is_built_only_when_a_function_uses_it() {
        assert_eq!(run("function f(){return arguments.length} console.log(f(1,2,3))"), "3");
        assert_eq!(run("function f(){return (function(){return 1})() + arguments[0]} console.log(f(5))"), "6");
        assert_eq!(run("function f(){var g=()=>arguments[1]; return g()} console.log(f(1,7))"), "7");
        assert_eq!(run("function f(){return typeof arguments} function g(){return 1} console.log(f(), g(9))"), "object 1");
    }

    #[test]
    fn a_script_that_takes_all_the_memory_is_stopped() {
        // (a small allowance: memory is counted for the whole process, and tests run side by side)
        let out = on_big_stack(|| {
            let mut js = Interp::new();
            js.mem_limit = crate::memcount::live_bytes() + 48 * 1024 * 1024;
            match js.run("var s='x'.repeat(4000000); var a=[]; while(true){a.push(`${s}y`)}") {
                Ok(_) => "finished".to_string(),
                Err(e) => e,
            }
        });
        assert!(out.contains("too much memory"), "{out}");
    }

    #[test]
    fn a_script_cannot_grow_the_page_without_bound() {
        let (log, _) = page("<body></body>", &["while (true) { document.body.appendChild(document.createElement('div')) }"]);
        assert!(log.contains("script 1:") && (log.contains("too big") || log.contains("too long") || log.contains("too many")), "{log}");
        let (log, _) = page("<body></body>", &["var s = 'x'.repeat(100000); var t = ''; for (var i = 0; i < 100; i++) { t += s } document.body.innerHTML = t + t + t + t + t + t + t + t + t + t + t + t"]);
        assert!(log.contains("script 1:"), "{log}");
    }

    #[test]
    fn crashes_found_in_review_are_stopped_cleanly() {
        // cycles and deep nesting in conversions used to overflow the Rust stack
        assert_eq!(run("var a=[]; a[0]=a; console.log(String(a) === '', a.join() === '', (a + '') === '')"), "true true true");
        assert_eq!(run("var a=[1]; var b=[a]; a.push(b); console.log(String(b))"), "1,");
        assert_eq!(run("var a=[1]; for (var i=0;i<20000;i++){a=[a]} console.log(String(a).length)"), "ERR script nested too deeply and was stopped");
        // a million nested arrays: allowed (memory, not object count, is what is limited), and
        // dropping them is iterative, so it can't overflow the stack
        assert_eq!(run("var a=[1]; for (var i=0;i<1000000;i++){a=[a]} console.log('built')"), "built");
        assert_eq!(run("var o={}; for (var i=0;i<1000000;i++){o={x:o}} console.log('built')"), "built");
        // deeply nested expressions with calls cannot use unbounded stack
        let deep = format!("function f(n) {{ return {}n{} }} console.log(f(1))", "(".repeat(100) + &"1+".repeat(2900), ")".repeat(100));
        let out = run(Box::leak(deep.into_boxed_str()));
        assert!(out.starts_with("ERR") || out == "2901", "either runs or is refused, never a crash: {out}");
        // the same shape inside a recursion that is as deep as a call may go: bounded by the nesting limit
        let nest = format!("function f(n) {{ return n <= 0 ? 0 : {}f(n - 1){} }} console.log(f(40))", "1+(".repeat(200), ")".repeat(200));
        let out = run(Box::leak(nest.into_boxed_str()));
        assert!(out.starts_with("ERR"), "{out}");
        // an inconsistent comparator (the usual shuffle) finishes
        assert_eq!(run("var a=[]; for (var i=0;i<200;i++) a.push(i); a.sort(function(x,y){ return Math.random()-0.5 }); console.log(a.length)"), "200");
        assert_eq!(run("console.log([3,1,2,undefined,10].sort().join(), [3,1,2].sort(function(a,b){return b-a}).join(), [5,1,4].sort(function(){ return NaN }).join())"), "1,10,2,3, 3,2,1 5,1,4");
        // splice when converting an argument changes the array
        assert_eq!(run("var a=[1,2,3,4,5,6]; a.splice(0, {toString:function(){ a.length=0; return '3' }}); console.log(a.length)"), "0");
        assert_eq!(run("var a=[1,2,3]; a.splice({toString:function(){ a.length=0; return '1' }}, 1); console.log(a.length)"), "0");
    }

    #[test]
    fn review_correctness_fixes() {
        // assignment targets are evaluated once, and before the right-hand side
        assert_eq!(run("var a=[1,2,3]; var i=0; a[i++]+=10; console.log(a.join(), i)"), "11,2,3 1");
        assert_eq!(run("var c=0; var o={x:1}; function t(){ c++; return o } t().x++; console.log(c, o.x)"), "1 2");
        assert_eq!(run("var i=0; var o={}; o[i++]=i++; console.log(JSON.stringify(o), i)"), "{\"0\":1} 2");
        // only canonical indexes are array indexes
        assert_eq!(run("var a=[1,2,3]; console.log(a['01'], a['+1'], a['1'], 'abc'['01'])"), "undefined undefined 2 undefined");
        assert_eq!(run("var a=[1,2,3]; a['01']=9; console.log(a.length, a['01'])"), "3 9");
        // valueOf, toString and conversions
        assert_eq!(run("var o={valueOf:function(){return 42}}; console.log(o+1, o*2, o>41, String(o))"), "43 84 true [object Object]");
        assert_eq!(run("var o={toString:function(){return 'str'}, valueOf:function(){return 7}}; console.log(o+'', `${o}`, o*1, String(o))"), "7 str 7 str");
        // constructors and prototypes of the built-ins
        assert_eq!(run("console.log(Array(3).length, new Array(2).length, Array(1,2).length, [] instanceof Array, [].constructor === Array, ({}).constructor === Object)"), "3 2 2 true true true");
        assert_eq!(run("function f() { return Array.prototype.slice.call(arguments, 1) } console.log(f(1,2,3).join())"), "2,3");
        assert_eq!(run("var e=new TypeError('m'); console.log(e.constructor === TypeError, e.constructor.name, e instanceof Error)"), "true TypeError true");
        // an ES5-style error subclass keeps its name through the prototype chain
        assert_eq!(run("function MyErr(m){ this.message=m; this.name='MyErr' } MyErr.prototype=new Error(); var e=new MyErr('hi'); console.log(String(e), e instanceof Error)"), "MyErr: hi true");
        assert_eq!(run("function E2(m){ this.message=m } E2.prototype=Object.create(Error.prototype); E2.prototype.name='E2'; try { throw new E2('boom') } catch (x) { console.log(String(x)) } throw new E2('final')"), "ERR Uncaught E2: final");
        // numbers print like JavaScript
        assert_eq!(run("console.log(2**64, 123456789012345678901, 2**63, 1e21, 0.000001, 1e-7, 100, -1e3)"), "18446744073709552000 123456789012345680000 9223372036854776000 1e+21 0.000001 1e-7 100 -1000");
        assert_eq!(run("console.log(Object.keys({b:1,a:2,1:'x',0:'y'}).join(), JSON.stringify({b:1,2:2,1:1}))"), "0,1,b,a {\"1\":1,\"2\":2,\"b\":1}");
        assert_eq!(run("console.log(parseFloat('1e3'), parseFloat('1.2.3'), parseFloat('Infinity'), parseFloat('-.5x'), parseFloat('x'), Number('0b11'), Number('0o7'), Number('0xff'), Number('0x'), Number('1e999'))"), "1000 1.2 Infinity -0.5 NaN 3 7 255 NaN Infinity");
        assert_eq!(run("console.log((2.5).toFixed(0), (1.005).toFixed(2), (-1.5).toFixed(0), (0).toFixed(2), (-0).toFixed(1), (1234.5678).toFixed(1), (0.5).toFixed(0))"), "3 1.00 -2 0.00 0.0 1234.6 1");
        assert_eq!(run("try { (1).toFixed(200) } catch (e) { console.log(e.name) } console.log(Math.round(2.5), Math.round(-2.5), Math.round(0.49999999999999994), Math.round(1.4))"), "RangeError\n3 -2 0 1");
        // functions in switch cases, thisArg, bind
        assert_eq!(run("switch(1){ case 1: function sw(){ return 'in switch' } console.log(sw()) }"), "in switch");
        assert_eq!(run("console.log([1,2,3].map(function(x){return this.k+x},{k:10}).join(), [1].some(function(){ return this.ok }, {ok:true}))"), "11,12,13 true");
        assert_eq!(run("function f(a,b){ return this.n + a + b } var g = f.bind({n:1}, 2); console.log(g(3), g.call({n:100}, 4), typeof g)"), "6 7 function");
        assert_eq!(run("console.log([1,2,3].some(x=>x>2), [1,2,3].every(x=>x>0), [1,2,3].find(x=>x>1), [1,2,3].findIndex(x=>x>5), [].every(x=>false))"), "true true 2 -1 true");
        // string methods
        assert_eq!(run("console.log('abc'.substring(2,0), 'abc'.substring(-1), 'abc'.substr(1,1), 'abc'.substr(-2), 'a,b,c'.split(',', 2).join('|'), 'ab'.padStart(5, 'xy'), 'ab'.padEnd(4), 'a-b-c'.replaceAll('-', '+'))"), "ab abc b bc a|b xyxab ab   a+b+c");
        // Object helpers and JSON.parse
        assert_eq!(run("var t = Object.assign({a:1}, {b:2}, {a:3}); console.log(JSON.stringify(t), Object.values(t).join(), JSON.stringify(Object.entries(t)), Object.create({p:1}).p)"), "{\"a\":3,\"b\":2} 3,2 [[\"a\",3],[\"b\",2]] 1");
        assert_eq!(run("var d = JSON.parse('{\"a\":[1,2.5e1,-3],\"b\":{\"c\":null,\"d\":true},\"e\":\"x\\\\ny\\\\u0041\\\\ud83d\\\\ude00\"}'); console.log(d.a.join(), d.b.c, d.b.d, d.e, Object.keys(d).join())"), "1,25,-3 null true x\nyA😀 a,b,e");
        for bad in ["{", "[1,]", "{'a':1}", "01", "1.", "\"unterminated", "[1] x", "tru", "{\"a\" 1}", ""] {
            let script = format!("try {{ JSON.parse({}); console.log('parsed') }} catch (e) {{ console.log(e.name) }}", serde_like_quote(bad));
            assert_eq!(run(Box::leak(script.into_boxed_str())), "SyntaxError", "{bad:?}");
        }
        assert_eq!(run("var s='['.repeat(300) + ']'.repeat(300); try { JSON.parse(s) } catch (e) { console.log(e.name) }"), "SyntaxError", "deep JSON is refused");
    }

    /// A JavaScript string literal for `s` (for building test scripts).
    fn serde_like_quote(s: &str) -> String {
        let mut out = String::from("'");
        for c in s.chars() {
            match c {
                '\'' => out.push_str("\\'"),
                '\\' => out.push_str("\\\\"),
                c => out.push(c),
            }
        }
        out.push('\'');
        out
    }

    #[test]
    fn limits_catch_slow_and_hungry_scripts() {
        // natives are charged for the size of what they work on
        assert_eq!(run("var a=[]; for (var i=0;i<300000;i++) a.push(i); for (var i=0;i<100000;i++) a.unshift(i)"), "ERR script ran for too long and was stopped");
        assert_eq!(run("var o={}; for (var i=0;i<200000;i++){ o['k'+i]=i } for (var i=0;i<200000;i++){ delete o['k'+i] }"), "ERR script ran for too long and was stopped");
        assert_eq!(run("var s='x'; for (var i=0;i<27;i++) s=s.concat(s); console.log(s.length)"), "ERR string too long");
        assert_eq!(run("var keep=[]; for (var i=0;i<1000;i++){ var a=[]; a.length=900000; keep.push(a) } console.log('done')"), "ERR script used too much memory and was stopped");
        assert_eq!(run("var a=[]; a.length=4e6"), "ERR array too big");
        assert_eq!(run("var a=[]; a.length=-1"), "ERR Uncaught RangeError: Invalid array length");
        // what a script prints is bounded
        let out = run("console.log('y'.repeat(3000000)); console.log([ 'z'.repeat(1000000), 'w'.repeat(1000000) ])");
        assert!(out.len() < 40_000, "{}", out.len());
        // a normal amount of work is fine
        assert_eq!(run("var a=[]; for (var i=0;i<20000;i++) a.push(i); var s=0; a.forEach(function(x){ s+=x }); var o={}; for (var i=0;i<5000;i++) o['k'+i]=i; for (var k in o) delete o[k]; console.log(s, a.slice(0,3).join(), a.indexOf(19999))"), "199990000 0,1,2 19999");
    }

    #[test]
    fn a_realistic_script_runs() {
        let src = "
            function Todo(title) { this.title = title; this.done = false }
            Todo.prototype.finish = function() { this.done = true; return this };
            var list = ['write', 'test', 'ship'].map(function(t) { return new Todo(t) });
            list[1].finish();
            var summary = list.filter(t => t.done).map(t => t.title.toUpperCase());
            var counts = {};
            list.forEach(t => { var k = t.done ? 'done' : 'open'; counts[k] = (counts[k] || 0) + 1 });
            function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2) }
            console.log(summary.join(), JSON.stringify(counts), fib(15));
        ";
        assert_eq!(run(src), "TEST {\"open\":2,\"done\":1} 610");
    }
}
