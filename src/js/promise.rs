//! `Promise` and `fetch`.
//!
//! A promise is an object with a few hidden properties: `__state` ("pending", "fulfilled" or
//! "rejected"), `__value`, and `__reactions` (the callbacks waiting for it). Callbacks never run
//! at once: they are queued as jobs (on the timer queue, due now) and run after the code that is
//! running finishes, as in a browser. There is no `async`/`await` (the parser doesn't have them),
//! so this is for `.then` chains.
use super::dom_bindings::{queue_job, script_request};
use super::*;

/// Most promises one page may create (each is an object; a loop can't make millions).
const MAX_PROMISES: u32 = 50_000;

fn new_promise(i: &mut Interp) -> Result<Value, Ctrl> {
    if i.promises >= MAX_PROMISES {
        return Err(Ctrl::Abort("script made too many promises and was stopped".into()));
    }
    i.promises += 1;
    let p = i.new_object();
    Interp::set_prop(&p, "__state", str_value("pending"));
    Interp::set_prop(&p, "__value", Value::Undefined);
    let reactions = i.new_array(vec![]);
    Interp::set_prop(&p, "__reactions", reactions);
    for (name, f) in [("then", then as NativeFn), ("catch", catch), ("finally", finally)] {
        let m = i.method(f, name);
        Interp::set_prop(&p, name, m);
    }
    Ok(Value::Obj(p))
}

/// A function that calls `f` with `this` and some leading arguments (what `bind` makes).
fn bound(i: &mut Interp, f: NativeFn, this: Value, leading: Vec<Value>) -> Value {
    let func = i.native(f);
    i.objects += 1;
    Value::Obj(Rc::new(RefCell::new(Object {
        kind: Kind::Bound(Box::new((func, this, leading))),
        props: HashMap::new(),
        order: vec![],
        proto: Some(i.object_proto.clone()),
    })))
}

fn state_of(i: &mut Interp, p: &Value) -> Result<String, Ctrl> {
    match i.get_member(p, "__state")? {
        Value::Str(s) => Ok(s.to_string()),
        _ => Ok(String::new()),
    }
}

fn is_promise(i: &mut Interp, v: &Value) -> bool {
    matches!(v, Value::Obj(_)) && matches!(i.get_member(v, "__state"), Ok(Value::Str(_)))
}

/// Settle a pending promise and queue the callbacks that were waiting for it.
fn settle(i: &mut Interp, p: &Value, state: &str, value: Value) -> Result<(), Ctrl> {
    if state_of(i, p)? != "pending" {
        return Ok(());
    }
    i.set_member(p, "__state", str_value(state))?;
    i.set_member(p, "__value", value.clone())?;
    let reactions = i.get_member(p, "__reactions")?;
    let list = match &reactions {
        Value::Obj(o) => match &o.borrow().kind {
            Kind::Array(items) => items.clone(),
            _ => vec![],
        },
        _ => vec![],
    };
    i.set_member(p, "__reactions", Value::Undefined)?;
    for r in list {
        queue_reaction(i, &r, state, &value)?;
    }
    Ok(())
}

/// `reaction` is `[onFulfilled, onRejected, childPromise]`.
fn queue_reaction(i: &mut Interp, reaction: &Value, state: &str, value: &Value) -> Result<(), Ctrl> {
    let parts = match reaction {
        Value::Obj(o) => match &o.borrow().kind {
            Kind::Array(items) => items.clone(),
            _ => vec![],
        },
        _ => vec![],
    };
    if parts.len() < 3 {
        return Ok(());
    }
    let job = i.native(reaction_job);
    queue_job(i, job, vec![parts[0].clone(), parts[1].clone(), parts[2].clone(), str_value(state), value.clone()]);
    Ok(())
}

fn reaction_job(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let (on_ok, on_err, child, state, value) = (a[0].clone(), a[1].clone(), a[2].clone(), a[3].clone(), a[4].clone());
    let fulfilled = matches!(&state, Value::Str(s) if &**s == "fulfilled");
    let handler = if fulfilled { on_ok } else { on_err };
    if handler.is_callable() {
        match i.call(&handler, &Value::Undefined, std::slice::from_ref(&value)) {
            Ok(r) => resolve(i, &child, r)?,
            Err(Ctrl::Throw(e)) => settle(i, &child, "rejected", e)?,
            Err(e) => return Err(e),
        }
    } else if fulfilled {
        resolve(i, &child, value)?;
    } else {
        settle(i, &child, "rejected", value)?;
    }
    Ok(Value::Undefined)
}

/// Resolve `p` with `v`: a value fulfils it; a promise (or anything with a `then`) is followed.
fn resolve(i: &mut Interp, p: &Value, v: Value) -> Result<(), Ctrl> {
    if state_of(i, p)? != "pending" {
        return Ok(());
    }
    if let (Value::Obj(a), Value::Obj(b)) = (p, &v) {
        if Rc::ptr_eq(a, b) {
            let e = i.make_error("TypeError", "a promise can't be resolved with itself");
            return settle(i, p, "rejected", e);
        }
    }
    if matches!(v, Value::Obj(_)) {
        let then_fn = match i.get_member(&v, "then") {
            Ok(t) => t,
            Err(Ctrl::Throw(e)) => return settle(i, p, "rejected", e),
            Err(e) => return Err(e),
        };
        if then_fn.is_callable() {
            let job = i.native(follow_job);
            queue_job(i, job, vec![v, then_fn, p.clone()]);
            return Ok(());
        }
    }
    settle(i, p, "fulfilled", v)
}

fn follow_job(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let (thenable, then_fn, p) = (a[0].clone(), a[1].clone(), a[2].clone());
    let on_ok = bound(i, resolve_native, p.clone(), vec![]);
    let on_err = bound(i, reject_native, p.clone(), vec![]);
    match i.call(&then_fn, &thenable, &[on_ok, on_err]) {
        Ok(_) => {}
        Err(Ctrl::Throw(e)) => settle(i, &p, "rejected", e)?,
        Err(e) => return Err(e),
    }
    Ok(Value::Undefined)
}

fn resolve_native(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    resolve(i, this, a.first().cloned().unwrap_or(Value::Undefined))?;
    Ok(Value::Undefined)
}

fn reject_native(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    settle(i, this, "rejected", a.first().cloned().unwrap_or(Value::Undefined))?;
    Ok(Value::Undefined)
}

fn then(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    if !is_promise(i, this) {
        return i.throw("TypeError", "then called on something that isn't a promise");
    }
    let child = new_promise(i)?;
    let reaction = i.new_array(vec![
        a.first().cloned().unwrap_or(Value::Undefined),
        a.get(1).cloned().unwrap_or(Value::Undefined),
        child.clone(),
    ]);
    match state_of(i, this)?.as_str() {
        "pending" => {
            if let Value::Obj(list) = i.get_member(this, "__reactions")? {
                if let Kind::Array(items) = &mut list.borrow_mut().kind {
                    if items.len() < 10_000 {
                        items.push(reaction);
                    }
                }
            }
        }
        state => {
            let state = state.to_string();
            let value = i.get_member(this, "__value")?;
            queue_reaction(i, &reaction, &state, &value)?;
        }
    }
    Ok(child)
}

fn catch(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    then(i, this, &[Value::Undefined, a.first().cloned().unwrap_or(Value::Undefined)])
}

/// `finally(f)`: run `f` either way, then carry on with the original outcome.
fn finally(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let f = a.first().cloned().unwrap_or(Value::Undefined);
    if !f.is_callable() {
        return then(i, this, &[]);
    }
    let on_ok = bound(i, finally_ok, Value::Undefined, vec![f.clone()]);
    let on_err = bound(i, finally_err, Value::Undefined, vec![f]);
    then(i, this, &[on_ok, on_err])
}

fn finally_ok(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    i.call(&a[0], &Value::Undefined, &[])?;
    Ok(a.get(1).cloned().unwrap_or(Value::Undefined))
}

fn finally_err(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    i.call(&a[0], &Value::Undefined, &[])?;
    Err(Ctrl::Throw(a.get(1).cloned().unwrap_or(Value::Undefined)))
}

fn promise_construct(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let executor = a.first().cloned().unwrap_or(Value::Undefined);
    if !executor.is_callable() {
        return i.throw("TypeError", "Promise resolver is not a function");
    }
    let p = new_promise(i)?;
    let on_ok = bound(i, resolve_native, p.clone(), vec![]);
    let on_err = bound(i, reject_native, p.clone(), vec![]);
    match i.call(&executor, &Value::Undefined, &[on_ok, on_err]) {
        Ok(_) => {}
        Err(Ctrl::Throw(e)) => settle(i, &p, "rejected", e)?,
        Err(e) => return Err(e),
    }
    Ok(p)
}

fn promise_resolve(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let v = a.first().cloned().unwrap_or(Value::Undefined);
    if is_promise(i, &v) {
        return Ok(v);
    }
    let p = new_promise(i)?;
    resolve(i, &p, v)?;
    Ok(p)
}

fn promise_reject(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let p = new_promise(i)?;
    settle(i, &p, "rejected", a.first().cloned().unwrap_or(Value::Undefined))?;
    Ok(p)
}

/// `Promise.all(list)`: fulfilled with all the values once every one is in, rejected at the first failure.
fn promise_all(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let items = match a.first() {
        Some(Value::Obj(o)) => match &o.borrow().kind {
            Kind::Array(items) => items.clone(),
            _ => vec![],
        },
        _ => vec![],
    };
    let result = new_promise(i)?;
    let values = i.new_array(vec![Value::Undefined; items.len()]);
    let remaining = i.new_object();
    Interp::set_prop(&remaining, "n", Value::Num(items.len() as f64));
    if items.is_empty() {
        resolve(i, &result, values)?;
        return Ok(result);
    }
    for (index, item) in items.into_iter().enumerate() {
        let p = promise_resolve(i, &Value::Undefined, &[item])?;
        let on_ok = bound(i, all_item_ok, result.clone(), vec![Value::Num(index as f64), values.clone(), Value::Obj(remaining.clone())]);
        let on_err = bound(i, reject_native, result.clone(), vec![]);
        then(i, &p, &[on_ok, on_err])?;
    }
    Ok(result)
}

fn all_item_ok(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    // arguments: index, the values array, the counter, then the value that arrived
    let index = i.to_number(&a[0])? as usize;
    let value = a.get(3).cloned().unwrap_or(Value::Undefined);
    if let Value::Obj(list) = &a[1] {
        if let Kind::Array(items) = &mut list.borrow_mut().kind {
            if index < items.len() {
                items[index] = value;
            }
        }
    }
    let count = i.get_member(&a[2], "n")?;
    let left = i.to_number(&count)? - 1.0;
    i.set_member(&a[2], "n", Value::Num(left))?;
    if left <= 0.0 {
        resolve(i, this, a[1].clone())?;
    }
    Ok(Value::Undefined)
}

// ---- fetch

fn fetch(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let url = i.arg_string(a, 0)?;
    let (mut method, mut body) = ("GET".to_string(), None);
    if let Some(opts @ Value::Obj(_)) = a.get(1) {
        if let Value::Str(m) = i.get_member(opts, "method")? {
            method = m.to_ascii_uppercase();
        }
        match i.get_member(opts, "body")? {
            Value::Undefined | Value::Null => {}
            b => body = Some(i.to_string(&b)?),
        }
    }
    let p = new_promise(i)?;
    match script_request(i, &method, &url, body.as_deref()) {
        Ok((f, text)) => {
            let r = i.new_object();
            Interp::set_prop(&r, "ok", Value::Bool((200..300).contains(&f.status)));
            Interp::set_prop(&r, "status", Value::Num(f.status as f64));
            Interp::set_prop(&r, "statusText", str_value(""));
            Interp::set_prop(&r, "url", str_value(f.url.as_str()));
            Interp::set_prop(&r, "__body", str_value(&text));
            let headers = i.new_object();
            Interp::set_prop(&headers, "__content_type", str_value(&f.content_type));
            let get = i.method(headers_get, "get");
            Interp::set_prop(&headers, "get", get);
            Interp::set_prop(&r, "headers", Value::Obj(headers));
            for (name, func) in [("text", response_text as NativeFn), ("json", response_json)] {
                let m = i.method(func, name);
                Interp::set_prop(&r, name, m);
            }
            resolve(i, &p, Value::Obj(r))?;
        }
        Err(message) => {
            let e = i.make_error("TypeError", &format!("Failed to fetch: {message}"));
            settle(i, &p, "rejected", e)?;
        }
    }
    Ok(p)
}

fn headers_get(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    if i.arg_string(a, 0)?.eq_ignore_ascii_case("content-type") {
        return i.get_member(this, "__content_type");
    }
    Ok(Value::Null)
}

fn response_text(i: &mut Interp, this: &Value, _: &[Value]) -> Res {
    let body = i.get_member(this, "__body")?;
    promise_resolve(i, &Value::Undefined, &[body])
}

fn response_json(i: &mut Interp, this: &Value, _: &[Value]) -> Res {
    let body = i.get_member(this, "__body")?;
    let p = new_promise(i)?;
    match json_parse(i, &Value::Undefined, &[body]) {
        Ok(v) => resolve(i, &p, v)?,
        Err(Ctrl::Throw(e)) => settle(i, &p, "rejected", e)?,
        Err(e) => return Err(e),
    }
    Ok(p)
}

impl Interp {
    /// Add `Promise` and `fetch` to the globals.
    pub(super) fn install_promises(&mut self) {
        let g = self.global.clone();
        let ctor = self.native(promise_construct);
        if let Value::Obj(c) = &ctor {
            for (name, f) in [("resolve", promise_resolve as NativeFn), ("reject", promise_reject), ("all", promise_all)] {
                let m = self.native(f);
                Interp::set_prop(c, name, m);
            }
        }
        Interp::declare(&g, "Promise", ctor, true);
        let f = self.native(fetch);
        Interp::declare(&g, "fetch", f, true);
    }
}
