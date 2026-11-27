//! What scripts see of the page: `document`, element objects, `window`, timers.
//!
//! An element in a script is an object whose kind is `Kind::Node(id)` (one per node, so `===`
//! works); its properties are read from and written to the page's DOM, which the interpreter holds
//! (`Interp::dom`) while scripts run. Nodes are only ever added to the DOM arena or detached,
//! never moved to an earlier place in it, because styling relies on a parent coming before its
//! children: appending a node under a parent that was created after it copies the node instead.
use super::*;
use crate::css;
use crate::dom::{Dom, Kind as DKind, NodeId};

/// The most nodes a script may grow the DOM to, and the biggest `innerHTML` it may set.
const MAX_NODES: usize = 200_000;
const MAX_INNER_HTML: usize = 1024 * 1024;
/// Listeners a page may register, timers it may have pending, and how many timer callbacks run at load.
const MAX_LISTENERS: usize = 10_000;
const MAX_TIMERS: usize = 1_000;
const MAX_TIMER_RUNS: usize = 2_000;
/// An interval fires this many times at load (there is no clock to wait on), then is dropped.
const INTERVAL_RUNS: u32 = 3;

/// The document itself, in `listeners`: events on `document` and `window` use this id.
pub(super) const DOCUMENT: NodeId = usize::MAX;

pub(super) struct Listener {
    pub target: NodeId,
    pub event: String,
    pub handler: Value,
    /// Set through `el.onclick = fn` (one per event, replaced by the next assignment).
    pub property: bool,
}

pub(super) struct Timer {
    id: u64,
    due: f64,
    func: Value,
    args: Vec<Value>,
    interval: Option<f64>,
    runs: u32,
}

// ------------------------------------------------------------------ plumbing

fn this_node(i: &mut Interp, this: &Value) -> Result<NodeId, Ctrl> {
    if let Value::Obj(o) = this {
        if let Kind::Node(id) = o.borrow().kind {
            return Ok(id);
        }
    }
    i.throw("TypeError", "this is not an element")
}

fn node_arg(i: &mut Interp, a: &[Value], at: usize) -> Result<NodeId, Ctrl> {
    match a.get(at) {
        Some(Value::Obj(o)) => {
            if let Kind::Node(id) = o.borrow().kind {
                return Ok(id);
            }
            i.throw("TypeError", "argument is not an element")
        }
        _ => i.throw("TypeError", "argument is not an element"),
    }
}

fn dom_ref(i: &Interp) -> Option<&Dom> {
    i.dom.as_ref()
}

fn is_element(i: &Interp, id: NodeId) -> bool {
    dom_ref(i).is_some_and(|d| id < d.nodes.len() && matches!(d.nodes[id].kind, DKind::Element { .. }))
}

/// The script object for node `id` (the same object every time).
pub(super) fn wrap(i: &mut Interp, id: NodeId) -> Value {
    if let Some(o) = i.wrappers.get(&id) {
        return Value::Obj(o.clone());
    }
    i.objects += 1;
    i.bytes += 96;
    let o = Rc::new(RefCell::new(Object { kind: Kind::Node(id), props: HashMap::new(), order: vec![], proto: Some(i.object_proto.clone()) }));
    i.wrappers.insert(id, o.clone());
    Value::Obj(o)
}

fn wrap_opt(i: &mut Interp, id: Option<NodeId>) -> Value {
    match id {
        Some(id) => wrap(i, id),
        None => Value::Null,
    }
}

fn wrap_list(i: &mut Interp, ids: Vec<NodeId>) -> Value {
    let items: Vec<Value> = ids.into_iter().map(|id| wrap(i, id)).collect();
    i.new_array(items)
}

fn set_attr(dom: &mut Dom, id: NodeId, name: &str, value: String) {
    if let DKind::Element { attrs, .. } = &mut dom.nodes[id].kind {
        attrs.insert(name.to_ascii_lowercase(), value);
    }
}

/// Add a detached node (no parent yet) to the arena.
fn new_node(dom: &mut Dom, kind: DKind) -> NodeId {
    dom.nodes.push(crate::dom::Node { kind, parent: None, children: vec![] });
    dom.nodes.len() - 1
}

fn detach(dom: &mut Dom, id: NodeId) {
    if let Some(p) = dom.nodes[id].parent.take() {
        dom.nodes[p].children.retain(|&c| c != id);
    }
}

/// Is `maybe_ancestor` the node itself or one of its ancestors?
fn is_inside(dom: &Dom, mut node: NodeId, maybe_ancestor: NodeId) -> bool {
    let mut hops = 0;
    loop {
        if node == maybe_ancestor {
            return true;
        }
        match dom.nodes[node].parent {
            Some(p) if hops < dom.nodes.len() => {
                node = p;
                hops += 1;
            }
            _ => return false,
        }
    }
}

/// Copy a node and everything under it into new nodes (detached), returning the copy's id.
fn clone_subtree(dom: &mut Dom, id: NodeId) -> NodeId {
    let kind = match &dom.nodes[id].kind {
        DKind::Element { tag, attrs } => DKind::Element { tag: tag.clone(), attrs: attrs.clone() },
        DKind::Text(t) => DKind::Text(t.clone()),
    };
    let copy = new_node(dom, kind);
    let mut work = vec![(id, copy)];
    while let Some((src, dst)) = work.pop() {
        let kids = dom.nodes[src].children.clone();
        for k in kids {
            if dom.nodes.len() > MAX_NODES {
                return copy;
            }
            let kind = match &dom.nodes[k].kind {
                DKind::Element { tag, attrs } => DKind::Element { tag: tag.clone(), attrs: attrs.clone() },
                DKind::Text(t) => DKind::Text(t.clone()),
            };
            let kc = new_node(dom, kind);
            dom.nodes[kc].parent = Some(dst);
            dom.nodes[dst].children.push(kc);
            work.push((k, kc));
        }
    }
    copy
}

/// Make `child` the last (or `before`'s previous) child of `parent`. Returns the node that ended
/// up in the tree (a copy, when the child was older than its new parent).
fn attach(i: &mut Interp, parent: NodeId, child: NodeId, before: Option<NodeId>) -> Result<NodeId, Ctrl> {
    let Some(dom) = i.dom.as_mut() else { return i.throw("Error", "no document") };
    if parent >= dom.nodes.len() || child >= dom.nodes.len() || !matches!(dom.nodes[parent].kind, DKind::Element { .. }) {
        return i.throw("TypeError", "cannot append there");
    }
    if is_inside(dom, parent, child) {
        return i.throw("Error", "a node can't be added inside itself");
    }
    if dom.nodes.len() > MAX_NODES {
        return Err(Ctrl::Abort("the page grew too big".into()));
    }
    let mut node = child;
    if child < parent {
        // an older node can't go under a newer parent (the arena keeps parents first): move a copy
        node = clone_subtree(dom, child);
        detach(dom, child);
        // the script's object for the old node now stands for the copy
        let wrapper = i.wrappers.remove(&child);
        if let Some(w) = wrapper {
            w.borrow_mut().kind = Kind::Node(node);
            i.wrappers.insert(node, w);
        }
    }
    let dom = i.dom.as_mut().expect("checked above");
    detach(dom, node);
    dom.nodes[node].parent = Some(parent);
    match before.and_then(|b| dom.nodes[parent].children.iter().position(|&c| c == b)) {
        Some(at) => dom.nodes[parent].children.insert(at, node),
        None => dom.nodes[parent].children.push(node),
    }
    Ok(node)
}

// ------------------------------------------------------------------ serializing and parsing markup

fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

fn serialize(dom: &Dom, id: NodeId, out: &mut String, depth: usize) {
    if depth > 200 || out.len() > MAX_INNER_HTML * 4 {
        return;
    }
    match &dom.nodes[id].kind {
        DKind::Text(t) => {
            let raw = dom.nodes[id].parent.is_some_and(|p| matches!(dom.tag(p), Some("script" | "style")));
            if raw {
                out.push_str(t);
            } else {
                escape_text(t, out);
            }
        }
        DKind::Element { tag, attrs } => {
            out.push('<');
            out.push_str(tag);
            let mut names: Vec<&String> = attrs.keys().collect();
            names.sort();
            for n in names {
                out.push(' ');
                out.push_str(n);
                out.push_str("=\"");
                out.push_str(&attrs[n].replace('&', "&amp;").replace('"', "&quot;"));
                out.push('"');
            }
            out.push('>');
            if crate::html::VOID.contains(&tag.as_str()) {
                return;
            }
            for &c in &dom.nodes[id].children {
                serialize(dom, c, out, depth + 1);
            }
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
    }
}

fn inner_html(dom: &Dom, id: NodeId) -> String {
    let mut out = String::new();
    for &c in &dom.nodes[id].children {
        serialize(dom, c, &mut out, 0);
    }
    out
}

#[cfg(test)]
pub(super) fn inner_html_for_tests(dom: &Dom, id: NodeId) -> String {
    inner_html(dom, id)
}

/// Replace the children of `id` with what `markup` parses to.
fn set_inner_html(i: &mut Interp, id: NodeId, markup: &str) -> Result<(), Ctrl> {
    if markup.len() > MAX_INNER_HTML {
        return Err(Ctrl::Abort("innerHTML too big".into()));
    }
    i.charge(markup.len())?;
    let fragment = crate::html::parse(&format!("<body>{markup}</body>"));
    let Some(dom) = i.dom.as_mut() else { return Ok(()) };
    let kids = dom.nodes[id].children.clone();
    for k in kids {
        detach(dom, k);
    }
    let Some(body) = (0..fragment.nodes.len()).find(|&n| fragment.tag(n) == Some("body")) else { return Ok(()) };
    // graft: copy the fragment's nodes under `id`, in order
    let mut work: Vec<(NodeId, NodeId)> = fragment.nodes[body].children.iter().rev().map(|&c| (c, id)).collect();
    while let Some((src, parent)) = work.pop() {
        if dom.nodes.len() > MAX_NODES {
            return Err(Ctrl::Abort("the page grew too big".into()));
        }
        let kind = match &fragment.nodes[src].kind {
            DKind::Element { tag, attrs } => DKind::Element { tag: tag.clone(), attrs: attrs.clone() },
            DKind::Text(t) => DKind::Text(t.clone()),
        };
        let n = new_node(dom, kind);
        dom.nodes[n].parent = Some(parent);
        dom.nodes[parent].children.push(n);
        work.extend(fragment.nodes[src].children.iter().rev().map(|&c| (c, n)));
    }
    Ok(())
}

fn set_text_content(i: &mut Interp, id: NodeId, text: &str) -> Result<(), Ctrl> {
    let Some(dom) = i.dom.as_mut() else { return Ok(()) };
    if let DKind::Text(t) = &mut dom.nodes[id].kind {
        *t = text.to_string();
        return Ok(());
    }
    let kids = dom.nodes[id].children.clone();
    for k in kids {
        detach(dom, k);
    }
    if !text.is_empty() {
        let n = new_node(dom, DKind::Text(text.to_string()));
        dom.nodes[n].parent = Some(id);
        dom.nodes[id].children.push(n);
    }
    Ok(())
}

// ------------------------------------------------------------------ selectors

/// Elements under `root` (not `root` itself) that match `selector` (a comma-separated list), in document order.
fn query(i: &mut Interp, root: NodeId, selector: &str, first_only: bool) -> Result<Vec<NodeId>, Ctrl> {
    // split a selector list on top-level commas
    let (mut parts, mut cur, mut depth) = (vec![], String::new(), 0i32);
    for c in selector.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    parts.push(cur);
    let mut selectors = vec![];
    for p in &parts {
        match css::parse_selector(p) {
            Some(s) => selectors.push(s),
            None => return i.throw("SyntaxError", &format!("'{selector}' is not a valid selector")),
        }
    }
    let count = dom_ref(i).map_or(0, |d| d.nodes.len());
    i.charge(count)?;
    let Some(dom) = dom_ref(i) else { return Ok(vec![]) };
    let mut found = vec![];
    for id in 0..dom.nodes.len() {
        if id == root || !matches!(dom.nodes[id].kind, DKind::Element { .. }) {
            continue;
        }
        // the document's own queries only see what is attached to it; an element's, what is under it
        let in_scope = if root == DOCUMENT { is_inside(dom, id, 0) } else { is_inside(dom, id, root) };
        if !in_scope {
            continue;
        }
        if selectors.iter().any(|s| css::selector_matches(dom, id, s)) {
            found.push(id);
            if first_only {
                break;
            }
        }
    }
    Ok(found)
}

// ------------------------------------------------------------------ reading and writing node properties

fn sibling(dom: &Dom, id: NodeId, step: isize, elements_only: bool) -> Option<NodeId> {
    let parent = dom.nodes[id].parent?;
    let kids = &dom.nodes[parent].children;
    let pos = kids.iter().position(|&c| c == id)? as isize;
    let mut at = pos + step;
    while at >= 0 && (at as usize) < kids.len() {
        let c = kids[at as usize];
        if !elements_only || matches!(dom.nodes[c].kind, DKind::Element { .. }) {
            return Some(c);
        }
        at += step;
    }
    None
}

/// Properties that simply mirror an attribute.
const REFLECTED: [&str; 14] =
    ["id", "href", "src", "type", "name", "title", "alt", "placeholder", "value", "lang", "rel", "target", "action", "method"];

pub(super) fn node_get(i: &mut Interp, id: NodeId, name: &str) -> Res {
    let Some(dom) = dom_ref(i) else { return Ok(Value::Undefined) };
    if id >= dom.nodes.len() {
        return Ok(Value::Undefined);
    }
    let is_el = matches!(dom.nodes[id].kind, DKind::Element { .. });
    let value = match name {
        "nodeType" => Value::Num(if is_el { 1.0 } else { 3.0 }),
        "tagName" | "nodeName" => str_value(&match dom.tag(id) {
            Some(t) => t.to_ascii_uppercase(),
            None => "#text".into(),
        }),
        "className" => str_value(dom.attr(id, "class").unwrap_or("")),
        "textContent" | "innerText" => str_value(&dom.text_content(id)),
        "nodeValue" | "data" => match &dom.nodes[id].kind {
            DKind::Text(t) => str_value(t),
            _ => Value::Null,
        },
        "innerHTML" => str_value(&inner_html(dom, id)),
        "outerHTML" => {
            let mut s = String::new();
            serialize(dom, id, &mut s, 0);
            str_value(&s)
        }
        "children" | "childNodes" => {
            let only_elements = name == "children";
            let ids: Vec<NodeId> = dom.nodes[id].children.iter().copied().filter(|&c| !only_elements || matches!(dom.nodes[c].kind, DKind::Element { .. })).collect();
            return Ok(wrap_list(i, ids));
        }
        "childElementCount" => Value::Num(dom.nodes[id].children.iter().filter(|&&c| matches!(dom.nodes[c].kind, DKind::Element { .. })).count() as f64),
        "firstChild" | "firstElementChild" => {
            let only = name == "firstElementChild";
            let k = dom.nodes[id].children.iter().copied().find(|&c| !only || matches!(dom.nodes[c].kind, DKind::Element { .. }));
            return Ok(wrap_opt(i, k));
        }
        "lastChild" | "lastElementChild" => {
            let only = name == "lastElementChild";
            let k = dom.nodes[id].children.iter().rev().copied().find(|&c| !only || matches!(dom.nodes[c].kind, DKind::Element { .. }));
            return Ok(wrap_opt(i, k));
        }
        "parentNode" | "parentElement" => {
            let p = dom.nodes[id].parent;
            return Ok(wrap_opt(i, p));
        }
        "nextSibling" | "nextElementSibling" => {
            let s = sibling(dom, id, 1, name == "nextElementSibling");
            return Ok(wrap_opt(i, s));
        }
        "previousSibling" | "previousElementSibling" => {
            let s = sibling(dom, id, -1, name == "previousElementSibling");
            return Ok(wrap_opt(i, s));
        }
        "checked" => Value::Bool(dom.attr(id, "checked").is_some()),
        "disabled" | "hidden" | "readOnly" | "required" => Value::Bool(dom.attr(id, &name.to_ascii_lowercase()).is_some()),
        "classList" => {
            let list = i.new_object();
            Interp::set_prop(&list, "__node", Value::Num(id as f64));
            for (n, f) in [("add", class_add as NativeFn), ("remove", class_remove), ("contains", class_contains), ("toggle", class_toggle)] {
                let v = i.native(f);
                Interp::set_prop(&list, n, v);
            }
            return Ok(Value::Obj(list));
        }
        n if REFLECTED.contains(&n) => str_value(dom.attr(id, n).unwrap_or("")),
        _ => {
            // methods
            let f: Option<NativeFn> = match name {
                "getAttribute" => Some(get_attribute),
                "setAttribute" => Some(set_attribute),
                "removeAttribute" => Some(remove_attribute),
                "hasAttribute" => Some(has_attribute),
                "appendChild" => Some(append_child),
                "removeChild" => Some(remove_child),
                "insertBefore" => Some(insert_before),
                "remove" => Some(remove_self),
                "querySelector" => Some(el_query_selector),
                "querySelectorAll" => Some(el_query_selector_all),
                "getElementsByTagName" => Some(el_by_tag),
                "getElementsByClassName" => Some(el_by_class),
                "addEventListener" => Some(add_listener),
                "removeEventListener" => Some(remove_listener),
                "click" => Some(click),
                "contains" => Some(contains),
                "cloneNode" => Some(clone_node),
                "focus" | "blur" => Some(|_, _, _| Ok(Value::Undefined)),
                _ => None,
            };
            match f {
                Some(f) => i.native(f),
                None => Value::Undefined,
            }
        }
    };
    Ok(value)
}

pub(super) fn node_set(i: &mut Interp, id: NodeId, name: &str, value: Value) -> Result<(), Ctrl> {
    if i.dom.as_ref().is_none_or(|d| id >= d.nodes.len()) {
        return Ok(());
    }
    match name {
        "textContent" | "innerText" | "nodeValue" | "data" => {
            let text = i.to_string(&value)?;
            if text.len() > MAX_STRING {
                return Err(Ctrl::Abort("string too long".into()));
            }
            set_text_content(i, id, &text)
        }
        "innerHTML" => {
            let markup = i.to_string(&value)?;
            set_inner_html(i, id, &markup)
        }
        "className" => {
            let v = i.to_string(&value)?;
            if let Some(d) = i.dom.as_mut() {
                set_attr(d, id, "class", v);
            }
            Ok(())
        }
        "checked" | "disabled" | "hidden" | "readOnly" | "required" => {
            let on = value.truthy();
            let attr = name.to_ascii_lowercase();
            if let Some(DKind::Element { attrs, .. }) = i.dom.as_mut().map(|d| &mut d.nodes[id].kind) {
                if on {
                    attrs.insert(attr, String::new());
                } else {
                    attrs.remove(&attr);
                }
            }
            Ok(())
        }
        n if REFLECTED.contains(&n) => {
            let v = i.to_string(&value)?;
            if let Some(d) = i.dom.as_mut() {
                set_attr(d, id, n, v);
            }
            Ok(())
        }
        n if n.starts_with("on") && n.len() > 2 => {
            // `el.onclick = fn`: one handler per event, replacing the last
            let event = n[2..].to_string();
            i.listeners.retain(|l| !(l.target == id && l.event == event && l.property));
            if value.is_callable() && i.listeners.len() < MAX_LISTENERS {
                i.listeners.push(Listener { target: id, event, handler: value, property: true });
            }
            Ok(())
        }
        _ => {
            // any other property is just a property of the script's object
            let wrapper = wrap(i, id);
            if let Value::Obj(o) = wrapper {
                Interp::set_prop(&o, name, value);
            }
            Ok(())
        }
    }
}

// ------------------------------------------------------------------ element methods

fn get_attribute(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let id = this_node(i, this)?;
    let name = i.arg_string(a, 0)?.to_ascii_lowercase();
    Ok(match dom_ref(i).and_then(|d| d.attr(id, &name)) {
        Some(v) => str_value(v),
        None => Value::Null,
    })
}

fn set_attribute(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let id = this_node(i, this)?;
    let (name, value) = (i.arg_string(a, 0)?, i.arg_string(a, 1)?);
    if value.len() > MAX_STRING {
        return Err(Ctrl::Abort("string too long".into()));
    }
    if let Some(d) = i.dom.as_mut() {
        set_attr(d, id, &name, value);
    }
    Ok(Value::Undefined)
}

fn remove_attribute(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let id = this_node(i, this)?;
    let name = i.arg_string(a, 0)?.to_ascii_lowercase();
    if let Some(DKind::Element { attrs, .. }) = i.dom.as_mut().map(|d| &mut d.nodes[id].kind) {
        attrs.remove(&name);
    }
    Ok(Value::Undefined)
}

fn has_attribute(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let id = this_node(i, this)?;
    let name = i.arg_string(a, 0)?.to_ascii_lowercase();
    Ok(Value::Bool(dom_ref(i).is_some_and(|d| d.attr(id, &name).is_some())))
}

fn append_child(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let parent = this_node(i, this)?;
    let child = node_arg(i, a, 0)?;
    attach(i, parent, child, None)?;
    Ok(a[0].clone())
}

fn insert_before(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let parent = this_node(i, this)?;
    let child = node_arg(i, a, 0)?;
    let before = match a.get(1) {
        Some(Value::Obj(_)) => Some(node_arg(i, a, 1)?),
        _ => None,
    };
    attach(i, parent, child, before)?;
    Ok(a[0].clone())
}

fn remove_child(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let parent = this_node(i, this)?;
    let child = node_arg(i, a, 0)?;
    if let Some(d) = i.dom.as_mut() {
        if d.nodes[child].parent == Some(parent) {
            detach(d, child);
            return Ok(a[0].clone());
        }
    }
    i.throw("NotFoundError", "the node is not a child of this node")
}

fn remove_self(i: &mut Interp, this: &Value, _: &[Value]) -> Res {
    let id = this_node(i, this)?;
    if let Some(d) = i.dom.as_mut() {
        detach(d, id);
    }
    Ok(Value::Undefined)
}

fn contains(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let id = this_node(i, this)?;
    let other = node_arg(i, a, 0)?;
    Ok(Value::Bool(dom_ref(i).is_some_and(|d| other < d.nodes.len() && is_inside(d, other, id))))
}

fn clone_node(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let id = this_node(i, this)?;
    let deep = a.first().is_some_and(|v| v.truthy());
    let Some(d) = i.dom.as_mut() else { return Ok(Value::Null) };
    if d.nodes.len() > MAX_NODES {
        return Err(Ctrl::Abort("the page grew too big".into()));
    }
    let copy = if deep {
        clone_subtree(d, id)
    } else {
        let kind = match &d.nodes[id].kind {
            DKind::Element { tag, attrs } => DKind::Element { tag: tag.clone(), attrs: attrs.clone() },
            DKind::Text(t) => DKind::Text(t.clone()),
        };
        new_node(d, kind)
    };
    Ok(wrap(i, copy))
}

fn el_query_selector(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let id = this_node(i, this)?;
    let sel = i.arg_string(a, 0)?;
    let found = query(i, id, &sel, true)?;
    Ok(wrap_opt(i, found.first().copied()))
}

fn el_query_selector_all(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let id = this_node(i, this)?;
    let sel = i.arg_string(a, 0)?;
    let found = query(i, id, &sel, false)?;
    Ok(wrap_list(i, found))
}

fn el_by_tag(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let id = this_node(i, this)?;
    let tag = i.arg_string(a, 0)?.to_ascii_lowercase();
    let sel = if tag == "*" { "*".to_string() } else { tag };
    let found = query(i, id, &sel, false)?;
    Ok(wrap_list(i, found))
}

fn el_by_class(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let id = this_node(i, this)?;
    let class = i.arg_string(a, 0)?;
    let sel: String = class.split_whitespace().map(|c| format!(".{c}")).collect();
    if sel.is_empty() {
        return Ok(i.new_array(vec![]));
    }
    let found = query(i, id, &sel, false)?;
    Ok(wrap_list(i, found))
}

// ---- classList

fn class_target(i: &mut Interp, this: &Value) -> Result<NodeId, Ctrl> {
    let n = i.get_member(this, "__node")?;
    match n {
        Value::Num(n) if n >= 0.0 => Ok(n as usize),
        _ => i.throw("TypeError", "not a class list"),
    }
}

fn classes(i: &Interp, id: NodeId) -> Vec<String> {
    dom_ref(i).and_then(|d| d.attr(id, "class")).map(|c| c.split_whitespace().map(String::from).collect()).unwrap_or_default()
}

fn store_classes(i: &mut Interp, id: NodeId, list: Vec<String>) {
    if let Some(d) = i.dom.as_mut() {
        set_attr(d, id, "class", list.join(" "));
    }
}

fn class_add(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let id = class_target(i, this)?;
    let mut list = classes(i, id);
    for v in a {
        let c = i.to_string(v)?;
        if !c.is_empty() && !list.contains(&c) {
            list.push(c);
        }
    }
    store_classes(i, id, list);
    Ok(Value::Undefined)
}

fn class_remove(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let id = class_target(i, this)?;
    let mut list = classes(i, id);
    for v in a {
        let c = i.to_string(v)?;
        list.retain(|x| *x != c);
    }
    store_classes(i, id, list);
    Ok(Value::Undefined)
}

fn class_contains(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let id = class_target(i, this)?;
    let c = i.arg_string(a, 0)?;
    Ok(Value::Bool(classes(i, id).contains(&c)))
}

fn class_toggle(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let id = class_target(i, this)?;
    let c = i.arg_string(a, 0)?;
    let mut list = classes(i, id);
    let force = a.get(1).filter(|v| !matches!(v, Value::Undefined)).map(|v| v.truthy());
    let has = list.contains(&c);
    let now = force.unwrap_or(!has);
    if now && !has {
        list.push(c);
    } else if !now && has {
        list.retain(|x| *x != c);
    }
    store_classes(i, id, list);
    Ok(Value::Bool(now))
}

// ------------------------------------------------------------------ events

fn add_listener(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let target = match this {
        Value::Obj(o) => match o.borrow().kind {
            Kind::Node(id) => id,
            _ => DOCUMENT,
        },
        _ => DOCUMENT,
    };
    let event = i.arg_string(a, 0)?;
    let handler = a.get(1).cloned().unwrap_or(Value::Undefined);
    if handler.is_callable() && i.listeners.len() < MAX_LISTENERS {
        i.listeners.push(Listener { target, event, handler, property: false });
    }
    Ok(Value::Undefined)
}

fn remove_listener(i: &mut Interp, this: &Value, a: &[Value]) -> Res {
    let target = match this {
        Value::Obj(o) => match o.borrow().kind {
            Kind::Node(id) => id,
            _ => DOCUMENT,
        },
        _ => DOCUMENT,
    };
    let event = i.arg_string(a, 0)?;
    if let Some(h) = a.get(1) {
        i.listeners.retain(|l| !(l.target == target && l.event == event && strict_equals(&l.handler, h)));
    }
    Ok(Value::Undefined)
}

fn click(i: &mut Interp, this: &Value, _: &[Value]) -> Res {
    let id = this_node(i, this)?;
    i.dispatch(id, "click")?;
    Ok(Value::Undefined)
}

impl Interp {
    /// Run the listeners for `event` on node `id` and then on its ancestors (it bubbles), each with
    /// an event object. Returns whether any handler called `preventDefault()`.
    pub(super) fn dispatch(&mut self, id: NodeId, event: &str) -> Result<bool, Ctrl> {
        // the chain from the target up to the document
        let mut chain = vec![];
        if id != DOCUMENT {
            let mut cur = Some(id);
            while let Some(n) = cur {
                chain.push(n);
                cur = self.dom.as_ref().and_then(|d| d.nodes.get(n)).and_then(|n| n.parent);
                if chain.len() > 10_000 {
                    break;
                }
            }
        }
        chain.push(DOCUMENT);
        let ev = self.new_object();
        Interp::set_prop(&ev, "type", str_value(event));
        let target = if id == DOCUMENT { Value::Null } else { wrap(self, id) };
        Interp::set_prop(&ev, "target", target);
        Interp::set_prop(&ev, "defaultPrevented", Value::Bool(false));
        let prevent = self.native(|i, this, _| {
            i.set_member(this, "defaultPrevented", Value::Bool(true))?;
            Ok(Value::Undefined)
        });
        Interp::set_prop(&ev, "preventDefault", prevent);
        let stop = self.native(|i, this, _| {
            i.set_member(this, "__stopped", Value::Bool(true))?;
            Ok(Value::Undefined)
        });
        Interp::set_prop(&ev, "stopPropagation", stop);
        let event_value = Value::Obj(ev.clone());
        'levels: for node in chain {
            let handlers: Vec<Value> = self.listeners.iter().filter(|l| l.target == node && l.event == event).map(|l| l.handler.clone()).collect();
            let this = if node == DOCUMENT { self.document.clone() } else { wrap(self, node) };
            Interp::set_prop(&ev, "currentTarget", this.clone());
            for h in handlers {
                match self.call(&h, &this, std::slice::from_ref(&event_value)) {
                    Ok(_) => {}
                    Err(Ctrl::Throw(e)) => {
                        let msg = self.describe_error(&e);
                        self.log_line(format!("Uncaught {msg}"));
                    }
                    Err(e) => return Err(e),
                }
            }
            if matches!(ev.borrow().props.get("__stopped"), Some(Value::Bool(true))) {
                break 'levels;
            }
        }
        let prevented = matches!(ev.borrow().props.get("defaultPrevented"), Some(Value::Bool(true)));
        Ok(prevented)
    }

    pub(super) fn log_line(&mut self, line: String) {
        if self.console.len() < 10_000 && self.console_bytes < 4 * 1024 * 1024 {
            self.console_bytes += line.len();
            self.console.push(line);
        }
    }

    // ---- timers

    fn add_timer(&mut self, a: &[Value], repeat: bool) -> Res {
        let func = a.first().cloned().unwrap_or(Value::Undefined);
        if !func.is_callable() || self.timers.len() >= MAX_TIMERS {
            return Ok(Value::Num(0.0));
        }
        let delay = match a.get(1) {
            Some(v) => self.to_number(v)?,
            None => 0.0,
        };
        let delay = if delay.is_nan() || delay < 0.0 { 0.0 } else { delay.min(1e9) };
        self.timer_seq += 1;
        let id = self.timer_seq;
        let due = self.clock + delay;
        self.timers.push(Timer { id, due, func, args: a.get(2..).unwrap_or(&[]).to_vec(), interval: repeat.then_some(delay.max(1.0)), runs: 0 });
        Ok(Value::Num(id as f64))
    }

    /// Run the pending timers in order of their time (virtual: nothing waits), including ones they
    /// schedule, until none are left or a limit is reached. Intervals fire a few times, then stop.
    pub fn run_timers(&mut self) {
        let mut runs = 0;
        while runs < MAX_TIMER_RUNS {
            let Some(next) = (0..self.timers.len()).min_by(|&x, &y| {
                let (a, b) = (&self.timers[x], &self.timers[y]);
                a.due.total_cmp(&b.due).then(a.id.cmp(&b.id))
            }) else {
                break;
            };
            let timer = self.timers.remove(next);
            self.clock = self.clock.max(timer.due);
            runs += 1;
            let result = self.call(&timer.func, &Value::Undefined, &timer.args);
            match result {
                Ok(_) => {}
                Err(Ctrl::Throw(e)) => {
                    let msg = self.describe_error(&e);
                    self.log_line(format!("Uncaught {msg}"));
                }
                Err(Ctrl::Abort(m)) => {
                    self.log_line(m);
                    break;
                }
                Err(_) => {}
            }
            if let Some(every) = timer.interval {
                if timer.runs + 1 < INTERVAL_RUNS && self.timers.len() < MAX_TIMERS {
                    self.timers.push(Timer { due: timer.due + every, runs: timer.runs + 1, ..timer });
                }
            }
        }
        self.timers.clear();
    }

    // ---- the globals

    /// Add `document`, `window`, timers and friends.
    pub(super) fn install_dom_globals(&mut self) {
        let g = self.global.clone();
        let doc = self.new_object();
        let doc_methods = [
            ("getElementById", doc_get_by_id as NativeFn),
            ("querySelector", doc_query_selector),
            ("querySelectorAll", doc_query_selector_all),
            ("getElementsByTagName", doc_by_tag),
            ("getElementsByClassName", doc_by_class),
            ("createElement", doc_create_element),
            ("createTextNode", doc_create_text),
            ("addEventListener", add_listener),
            ("removeEventListener", remove_listener),
        ];
        for (name, f) in doc_methods {
            let v = self.native(f);
            Interp::set_prop(&doc, name, v);
        }
        let document = Value::Obj(doc.clone());
        self.document = document.clone();
        Interp::declare(&g, "document", document.clone(), true);

        let window = self.new_object();
        Interp::set_prop(&window, "document", document);
        let console = self.lookup_global("console");
        Interp::set_prop(&window, "console", console);
        for (name, f) in [("addEventListener", add_listener as NativeFn), ("removeEventListener", remove_listener)] {
            let v = self.native(f);
            Interp::set_prop(&window, name, v);
        }
        fn nf(f: NativeFn) -> NativeFn {
            f
        }
        let timer_fns = [
            ("setTimeout", nf(|i, _, a| i.add_timer(a, false))),
            ("setInterval", nf(|i, _, a| i.add_timer(a, true))),
            ("clearTimeout", nf(clear_timer)),
            ("clearInterval", nf(clear_timer)),
        ];
        for (name, f) in timer_fns {
            let v = self.native(f);
            Interp::set_prop(&window, name, v.clone());
            Interp::declare(&g, name, v, true);
        }
        let location = self.new_object();
        Interp::set_prop(&location, "href", str_value(&self.page_url));
        Interp::set_prop(&window, "location", Value::Obj(location.clone()));
        Interp::declare(&g, "location", Value::Obj(location), true);
        Interp::set_prop(&window, "window", Value::Obj(window.clone()));
        Interp::declare(&g, "window", Value::Obj(window.clone()), true);
        Interp::declare(&g, "self", Value::Obj(window.clone()), true);
        Interp::declare(&g, "globalThis", Value::Obj(window), true);
    }

    fn lookup_global(&mut self, name: &str) -> Value {
        let global = self.global.clone();
        self.lookup(&global, name).unwrap_or(Value::Undefined)
    }

    /// Read `document.body` and friends once scripts start: set them as properties.
    pub(super) fn refresh_document(&mut self) {
        let Value::Obj(doc) = self.document.clone() else { return };
        let (body, head, title) = match dom_ref(self) {
            Some(d) => {
                let find = |tag: &str| (0..d.nodes.len()).find(|&n| d.tag(n) == Some(tag));
                (find("body"), find("head"), find("title").map(|t| d.text_content(t)).unwrap_or_default())
            }
            None => (None, None, String::new()),
        };
        let b = wrap_opt(self, body);
        let h = wrap_opt(self, head);
        let root = wrap(self, 0);
        Interp::set_prop(&doc, "body", b);
        Interp::set_prop(&doc, "head", h);
        Interp::set_prop(&doc, "documentElement", root);
        Interp::set_prop(&doc, "title", str_value(&title));
        Interp::set_prop(&doc, "readyState", str_value("complete"));
    }
}

fn clear_timer(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let id = i.arg_number(a, 0)?;
    i.timers.retain(|t| t.id as f64 != id);
    Ok(Value::Undefined)
}

// ---- document methods

fn doc_get_by_id(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let wanted = i.arg_string(a, 0)?;
    let found = dom_ref(i).and_then(|d| (0..d.nodes.len()).find(|&n| is_inside(d, n, 0) && d.attr(n, "id") == Some(wanted.as_str())));
    i.charge(dom_ref(i).map_or(0, |d| d.nodes.len()))?;
    Ok(wrap_opt(i, found))
}

fn doc_query_selector(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let sel = i.arg_string(a, 0)?;
    let found = query(i, usize::MAX, &sel, true)?;
    Ok(wrap_opt(i, found.first().copied()))
}

fn doc_query_selector_all(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let sel = i.arg_string(a, 0)?;
    let found = query(i, usize::MAX, &sel, false)?;
    Ok(wrap_list(i, found))
}

fn doc_by_tag(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let tag = i.arg_string(a, 0)?.to_ascii_lowercase();
    let found = query(i, usize::MAX, &tag, false)?;
    Ok(wrap_list(i, found))
}

fn doc_by_class(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let class = i.arg_string(a, 0)?;
    let sel: String = class.split_whitespace().map(|c| format!(".{c}")).collect();
    if sel.is_empty() {
        return Ok(i.new_array(vec![]));
    }
    let found = query(i, usize::MAX, &sel, false)?;
    Ok(wrap_list(i, found))
}

fn doc_create_element(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let tag = i.arg_string(a, 0)?.to_ascii_lowercase();
    if tag.is_empty() || !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return i.throw("InvalidCharacterError", "invalid tag name");
    }
    let Some(d) = i.dom.as_mut() else { return Ok(Value::Null) };
    if d.nodes.len() > MAX_NODES {
        return Err(Ctrl::Abort("the page grew too big".into()));
    }
    let id = new_node(d, DKind::Element { tag, attrs: HashMap::new() });
    Ok(wrap(i, id))
}

fn doc_create_text(i: &mut Interp, _: &Value, a: &[Value]) -> Res {
    let text = i.arg_string(a, 0)?;
    let Some(d) = i.dom.as_mut() else { return Ok(Value::Null) };
    if d.nodes.len() > MAX_NODES {
        return Err(Ctrl::Abort("the page grew too big".into()));
    }
    let id = new_node(d, DKind::Text(text));
    Ok(wrap(i, id))
}
