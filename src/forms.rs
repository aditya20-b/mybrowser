//! Forms: which elements are controls, what their current values are (typed
//! text, checked state, chosen option), and how a submitted form becomes a
//! request. This module is pure state and logic (no drawing, no windowing).
use crate::dom::{Dom, NodeId};
use std::collections::{HashMap, HashSet};
use url::Url;

/// Longest value a text control will hold, so holding a key down can't grow
/// memory without bound.
pub const MAX_VALUE_CHARS: usize = 10_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ControlKind {
    Text,
    Password,
    Checkbox,
    Radio,
    /// `<input type=submit>`, `<button>` (the default type) and image buttons.
    Submit,
    /// `<input type=button|reset>` and `<button type=button|reset>`: draws, does nothing.
    Button,
    Textarea,
    Select,
    Hidden,
}

/// What kind of form control `id` is, if it is one.
pub fn kind_of(dom: &Dom, id: NodeId) -> Option<ControlKind> {
    let ty = || dom.attr(id, "type").unwrap_or("").trim().to_ascii_lowercase();
    match dom.tag(id)? {
        "input" => Some(match dom.attr(id, "type").map(|t| t.trim().to_ascii_lowercase()).as_deref().unwrap_or("text") {
            "hidden" => ControlKind::Hidden,
            "password" => ControlKind::Password,
            "checkbox" => ControlKind::Checkbox,
            "radio" => ControlKind::Radio,
            "submit" | "image" => ControlKind::Submit,
            "button" | "reset" => ControlKind::Button,
            _ => ControlKind::Text, // text, search, email, url, tel, number, ...
        }),
        "button" => Some(if matches!(ty().as_str(), "button" | "reset") { ControlKind::Button } else { ControlKind::Submit }),
        "textarea" => Some(ControlKind::Textarea),
        "select" => Some(ControlKind::Select),
        _ => None,
    }
}

pub fn is_text_like(kind: ControlKind) -> bool {
    matches!(kind, ControlKind::Text | ControlKind::Password | ControlKind::Textarea)
}

/// Is the control disabled, by its own attribute or by an enclosing
/// `<fieldset disabled>`? Controls inside that fieldset's FIRST `<legend>` child
/// are exempt; a second legend, or one nested deeper, is not special.
pub fn is_disabled(dom: &Dom, id: NodeId) -> bool {
    if dom.attr(id, "disabled").is_some() {
        return true;
    }
    // the closest legend passed on the way up since the last fieldset
    let mut legend: Option<NodeId> = None;
    let mut cur = dom.nodes[id].parent;
    while let Some(p) = cur {
        match dom.tag(p) {
            Some("legend") => legend = Some(p),
            Some("fieldset") => {
                if dom.attr(p, "disabled").is_some() {
                    // (only look for the fieldset's first legend when we came through one:
                    // scanning every child for every control made a wide fieldset quadratic)
                    let exempt = legend.is_some_and(|l| {
                        dom.nodes[l].parent == Some(p)
                            && dom.nodes[p].children.iter().copied().find(|&c| dom.tag(c) == Some("legend")) == Some(l)
                    });
                    if !exempt {
                        return true;
                    }
                }
                legend = None;
            }
            _ => {}
        }
        cur = dom.nodes[p].parent;
    }
    false
}

/// Does `id` or any ancestor carry the `hidden` attribute (so it isn't rendered)?
fn is_hidden_by_attribute(dom: &Dom, id: NodeId) -> bool {
    let mut cur = Some(id);
    while let Some(n) = cur {
        if dom.attr(n, "hidden").is_some() {
            return true;
        }
        cur = dom.nodes[n].parent;
    }
    false
}

/// Can the user focus this control (click into it, Tab to it)?
pub fn is_focusable(dom: &Dom, id: NodeId) -> bool {
    kind_of(dom, id).is_some_and(|k| k != ControlKind::Hidden) && !is_disabled(dom, id) && !is_hidden_by_attribute(dom, id)
}

/// The `<form>` that owns control `id`: the one named by its `form="id"`
/// attribute if there is one, else the enclosing `<form>`.
pub fn form_of(dom: &Dom, id: NodeId) -> Option<NodeId> {
    if let Some(wanted) = dom.attr(id, "form").map(str::trim).filter(|w| !w.is_empty()) {
        return (0..dom.nodes.len()).find(|&n| dom.tag(n) == Some("form") && dom.attr(n, "id") == Some(wanted));
    }
    let mut cur = dom.nodes[id].parent;
    while let Some(p) = cur {
        if dom.tag(p) == Some("form") {
            return Some(p);
        }
        cur = dom.nodes[p].parent;
    }
    None
}

/// Every control owned by `form`, in document order (including ones outside
/// the element that point at it with `form="id"`).
pub fn controls_in(dom: &Dom, form: NodeId) -> Vec<NodeId> {
    (0..dom.nodes.len()).filter(|&n| kind_of(dom, n).is_some() && form_of(dom, n) == Some(form)).collect()
}

/// Every element's `id` -> the first element that has it.
pub fn id_map(dom: &Dom) -> HashMap<String, NodeId> {
    let mut m = HashMap::new();
    for n in 0..dom.nodes.len() {
        if let Some(id) = dom.attr(n, "id") {
            m.entry(id.to_string()).or_insert(n);
        }
    }
    m
}

/// The control a `<label>` is for: the element its `for` names (if that is a visible
/// control), otherwise the first visible control inside the label.
/// `ids` maps each `id` to its first element (see `id_map`); `shown` says whether a control is
/// actually displayed (the page's styles decide that, so the caller supplies it).
pub fn label_target(dom: &Dom, label: NodeId, ids: &HashMap<String, NodeId>, shown: &dyn Fn(NodeId) -> bool) -> Option<NodeId> {
    let usable = |n: NodeId| is_focusable(dom, n) && shown(n);
    if let Some(target) = dom.attr(label, "for") {
        // an empty `for` names nothing
        return ids.get(target).copied().filter(|_| !target.is_empty()).filter(|&n| usable(n));
    }
    let mut stack: Vec<NodeId> = dom.nodes[label].children.iter().rev().copied().collect();
    while let Some(n) = stack.pop() {
        if usable(n) {
            return Some(n);
        }
        stack.extend(dom.nodes[n].children.iter().rev().copied());
    }
    None
}

/// The `(value, label)` of each `<option>` in a `<select>`.
pub fn options(dom: &Dom, select: NodeId) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut stack: Vec<NodeId> = dom.nodes[select].children.iter().rev().copied().collect();
    while let Some(n) = stack.pop() {
        if dom.tag(n) == Some("option") {
            let label = dom.text_content(n).split_whitespace().collect::<Vec<_>>().join(" ");
            let value = dom.attr(n, "value").map(String::from).unwrap_or_else(|| label.clone());
            out.push((value, label));
        } else {
            stack.extend(dom.nodes[n].children.iter().rev());
        }
    }
    out
}

/// The most characters a text control accepts: its `maxlength`, capped.
fn max_chars(dom: &Dom, id: NodeId) -> usize {
    dom.attr(id, "maxlength")
        .and_then(|m| m.trim().parse::<usize>().ok())
        .map_or(MAX_VALUE_CHARS, |m| m.min(MAX_VALUE_CHARS))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Method {
    Get,
    Post,
}

/// A form ready to send.
#[derive(Debug, PartialEq)]
pub struct Submission {
    pub url: Url,
    pub method: Method,
    /// The `application/x-www-form-urlencoded` body (POST only; empty for GET,
    /// whose data is in the URL's query).
    pub body: String,
}

/// What the user has done to the page's controls. Anything not recorded here
/// has the value the markup gave it.
#[derive(Clone, Default, Debug)]
pub struct FormState {
    values: HashMap<NodeId, String>,
    checked: HashMap<NodeId, bool>,
    selected: HashMap<NodeId, usize>,
    /// The control with keyboard focus.
    pub focus: Option<NodeId>,
}

impl FormState {
    pub fn value(&self, dom: &Dom, id: NodeId) -> String {
        if let Some(v) = self.values.get(&id) {
            return v.clone();
        }
        match kind_of(dom, id) {
            Some(ControlKind::Textarea) => {
                let t = dom.text_content(id);
                t.strip_prefix('\n').unwrap_or(&t).to_string()
            }
            Some(ControlKind::Select) => {
                let opts = options(dom, id);
                opts.get(self.selected_index(dom, id)).map(|(v, _)| v.clone()).unwrap_or_default()
            }
            _ => dom.attr(id, "value").unwrap_or("").to_string(),
        }
    }

    /// The label to show for a select: its chosen option's text.
    pub fn selected_label(&self, dom: &Dom, id: NodeId) -> String {
        options(dom, id).get(self.selected_index(dom, id)).map(|(_, l)| l.clone()).unwrap_or_default()
    }

    pub fn selected_index(&self, dom: &Dom, id: NodeId) -> usize {
        if let Some(&i) = self.selected.get(&id) {
            return i;
        }
        // an <option selected> in the markup, else the first
        let mut stack: Vec<NodeId> = dom.nodes[id].children.iter().rev().copied().collect();
        let mut index = 0;
        while let Some(n) = stack.pop() {
            if dom.tag(n) == Some("option") {
                if dom.attr(n, "selected").is_some() {
                    return index;
                }
                index += 1;
            } else {
                stack.extend(dom.nodes[n].children.iter().rev());
            }
        }
        0
    }

    pub fn is_checked(&self, dom: &Dom, id: NodeId) -> bool {
        self.checked.get(&id).copied().unwrap_or_else(|| dom.attr(id, "checked").is_some())
    }

    /// Can the user change this text control's value? Not if it is disabled or `readonly`.
    fn is_editable(dom: &Dom, id: NodeId) -> bool {
        kind_of(dom, id).is_some_and(is_text_like) && !is_disabled(dom, id) && dom.attr(id, "readonly").is_none()
    }

    /// Type (or paste) text into a text control. Control characters are dropped,
    /// except that a textarea keeps newlines. Honours `readonly` and `maxlength`.
    pub fn type_text(&mut self, dom: &Dom, id: NodeId, text: &str) {
        let Some(kind) = kind_of(dom, id).filter(|k| is_text_like(*k)) else { return };
        if !Self::is_editable(dom, id) {
            return;
        }
        let limit = max_chars(dom, id);
        let mut value = self.value(dom, id);
        let mut count = value.chars().count();
        for c in text.chars() {
            let ok = !c.is_control() || (c == '\n' && kind == ControlKind::Textarea);
            if ok && count < limit {
                value.push(c);
                count += 1;
            }
        }
        self.values.insert(id, value);
    }

    pub fn backspace(&mut self, dom: &Dom, id: NodeId) {
        if !Self::is_editable(dom, id) {
            return;
        }
        let mut value = self.value(dom, id);
        value.pop();
        self.values.insert(id, value);
    }

    /// Click a checkbox or radio button. Checking a radio unchecks the others
    /// in its group (same owning form, or none, and same name).
    pub fn toggle(&mut self, dom: &Dom, id: NodeId) {
        if is_disabled(dom, id) {
            return;
        }
        match kind_of(dom, id) {
            Some(ControlKind::Checkbox) => {
                let now = self.is_checked(dom, id);
                self.checked.insert(id, !now);
            }
            Some(ControlKind::Radio) => {
                if let Some(name) = dom.attr(id, "name").filter(|n| !n.is_empty()) {
                    let owner = form_of(dom, id);
                    for other in 0..dom.nodes.len() {
                        if other != id
                            && kind_of(dom, other) == Some(ControlKind::Radio)
                            && dom.attr(other, "name") == Some(name)
                            && form_of(dom, other) == owner
                        {
                            self.checked.insert(other, false);
                        }
                    }
                }
                self.checked.insert(id, true);
            }
            _ => {}
        }
    }

    /// Choose the next option of a select (wrapping), a stand-in for a dropdown menu.
    pub fn cycle_select(&mut self, dom: &Dom, id: NodeId) {
        let n = options(dom, id).len();
        if n == 0 || is_disabled(dom, id) {
            return;
        }
        let next = (self.selected_index(dom, id) + 1) % n;
        self.selected.insert(id, next);
    }

    /// Move focus to the next (or previous) focusable control, wrapping around.
    /// With nothing focused, Tab goes to the first and Shift+Tab to the last.
    pub fn focus_next(&mut self, dom: &Dom, forward: bool) {
        let all: Vec<NodeId> = (0..dom.nodes.len()).filter(|&i| is_focusable(dom, i)).collect();
        if all.is_empty() {
            self.focus = None;
            return;
        }
        let pos = self.focus.and_then(|f| all.iter().position(|&i| i == f));
        let next = match (pos, forward) {
            (Some(p), true) => (p + 1) % all.len(),
            (Some(p), false) => (p + all.len() - 1) % all.len(),
            (None, true) => 0,
            (None, false) => all.len() - 1,
        };
        self.focus = Some(all[next]);
    }

    /// Build the request for submitting `form`. `submitter` is the button that was
    /// clicked: only its name and value are sent, and its `formaction` and
    /// `formmethod` override the form's. Returns None if the action can't be
    /// resolved against `base`, or the form is `method=dialog` (which only closes a dialog).
    pub fn submit(&self, dom: &Dom, form: NodeId, submitter: Option<NodeId>, base: &Url) -> Option<Submission> {
        let mut pairs: Vec<(String, String)> = vec![];
        // a radio group submits one value: if markup left several checked, the last one wins,
        // and if that one is disabled the group submits nothing
        let mut radio_slot: HashMap<String, usize> = HashMap::new();
        let mut dropped: HashSet<usize> = HashSet::new();
        for id in controls_in(dom, form) {
            let kind = kind_of(dom, id)?;
            let disabled = is_disabled(dom, id);
            if disabled && kind != ControlKind::Radio {
                continue;
            }
            let Some(name) = dom.attr(id, "name").filter(|n| !n.is_empty()) else { continue };
            match kind {
                ControlKind::Hidden | ControlKind::Text | ControlKind::Password => {
                    pairs.push((name.to_string(), self.value(dom, id)));
                }
                ControlKind::Textarea => {
                    // browsers submit line breaks in a textarea as CRLF
                    let value = self.value(dom, id).replace("\r\n", "\n").replace('\n', "\r\n");
                    pairs.push((name.to_string(), value));
                }
                ControlKind::Checkbox => {
                    if self.is_checked(dom, id) {
                        pairs.push((name.to_string(), dom.attr(id, "value").unwrap_or("on").to_string()));
                    }
                }
                ControlKind::Radio => {
                    if self.is_checked(dom, id) {
                        let pair = (name.to_string(), dom.attr(id, "value").unwrap_or("on").to_string());
                        let slot = match radio_slot.get(name) {
                            Some(&slot) => {
                                pairs[slot] = pair;
                                slot
                            }
                            None => {
                                radio_slot.insert(name.to_string(), pairs.len());
                                pairs.push(pair);
                                pairs.len() - 1
                            }
                        };
                        if disabled {
                            dropped.insert(slot);
                        } else {
                            dropped.remove(&slot);
                        }
                    }
                }
                ControlKind::Select => pairs.push((name.to_string(), self.value(dom, id))),
                ControlKind::Submit => {
                    if Some(id) == submitter {
                        pairs.push((name.to_string(), dom.attr(id, "value").unwrap_or("").to_string()));
                    }
                }
                ControlKind::Button => {}
            }
        }
        let pairs: Vec<(String, String)> =
            pairs.into_iter().enumerate().filter(|(i, _)| !dropped.contains(i)).map(|(_, p)| p).collect();
        // the submitting button may override the form's action and method
        let from_button = |attr: &str| submitter.and_then(|s| dom.attr(s, attr)).map(str::trim).filter(|v| !v.is_empty());
        let method_attr = from_button("formmethod").or_else(|| dom.attr(form, "method")).map(|m| m.trim().to_ascii_lowercase());
        let method = match method_attr.as_deref() {
            Some("post") => Method::Post,
            Some("dialog") => return None,
            _ => Method::Get,
        };
        let action = from_button("formaction").or_else(|| dom.attr(form, "action").map(str::trim)).unwrap_or("");
        let mut url = if action.is_empty() { base.clone() } else { base.join(action).ok()? };
        url.set_fragment(None);
        Some(match method {
            Method::Get => {
                // the form's data replaces whatever query the action had
                if pairs.is_empty() {
                    url.set_query(None);
                } else {
                    url.set_query(None);
                    url.query_pairs_mut().extend_pairs(&pairs);
                }
                Submission { url, method, body: String::new() }
            }
            Method::Post => {
                let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(&pairs).finish();
                Submission { url, method, body }
            }
        })
    }

    /// The first submit button of `form`: what pressing Enter in a text field "clicks".
    pub fn default_button(dom: &Dom, form: NodeId) -> Option<NodeId> {
        controls_in(dom, form)
            .into_iter()
            .find(|&c| kind_of(dom, c) == Some(ControlKind::Submit) && !is_disabled(dom, c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    fn find(dom: &Dom, tag: &str, nth: usize) -> NodeId {
        (0..dom.nodes.len()).filter(|&i| dom.tag(i) == Some(tag)).nth(nth).unwrap_or_else(|| panic!("no {tag} #{nth}"))
    }

    #[test]
    fn a_label_points_at_its_for_target_or_its_first_visible_control() {
        let dom = html::parse("<label id=l1 for=a>A</label><input id=a name=a><label id=l2>B <input type=hidden name=h><input name=b></label><label id=l3 for=missing>C</label><label id=l4 for=hid>D</label><input id=hid type=hidden><label id=l5>none</label><label id=l6 for=a><input name=inner></label>");
        let ids = id_map(&dom);
        let l = |id: &str| ids[id];
        let target = |id: &str| label_target(&dom, l(id), &ids, &|_| true);
        assert_eq!(target("l1"), Some(l("a")));
        assert_eq!(target("l2"), Some(by_name(&dom, "b")), "skips the hidden input");
        assert_eq!(target("l3"), None);
        assert_eq!(target("l4"), None, "a hidden control can't be focused");
        assert_eq!(target("l5"), None);
        assert_eq!(target("l6"), Some(l("a")), "`for` wins over a nested control");
        // controls the page doesn't display, the `hidden` attribute, and an empty `for`
        assert_eq!(label_target(&dom, l("l1"), &ids, &|n| n != l("a")), None);
        let dom = html::parse("<label id=x for=a>X</label><input id=a hidden><label id=y for=''>Y</label><input id='' type=checkbox><label id=z for=b>Z</label><div hidden><input id=b></div>");
        let ids = id_map(&dom);
        for label in ["x", "y", "z"] {
            assert_eq!(label_target(&dom, ids[label], &ids, &|_| true), None, "{label}");
        }
    }

    fn by_name(dom: &Dom, name: &str) -> NodeId {
        (0..dom.nodes.len()).find(|&i| dom.attr(i, "name") == Some(name)).unwrap_or_else(|| panic!("no control named {name}"))
    }

    fn base() -> Url {
        Url::parse("https://example.com/dir/page.html?old=1#frag").unwrap()
    }

    #[test]
    fn control_kinds() {
        let dom = html::parse(
            "<form><input name=a><input type=PASSWORD><input type=checkbox><input type=radio><input type=submit>\
             <input type=hidden><input type=button><input type=email><input type=image><button>b</button>\
             <button type=button>c</button><textarea></textarea><select></select><div></div></form>",
        );
        let kinds: Vec<_> = (0..dom.nodes.len()).filter_map(|i| kind_of(&dom, i)).collect();
        use ControlKind::*;
        assert_eq!(kinds, [Text, Password, Checkbox, Radio, Submit, Hidden, Button, Text, Submit, Submit, Button, Textarea, Select]);
        assert!(!is_focusable(&dom, find(&dom, "div", 0)));
        let hidden = (0..dom.nodes.len()).find(|&i| kind_of(&dom, i) == Some(Hidden)).unwrap();
        assert!(!is_focusable(&dom, hidden), "hidden inputs can't be focused");
    }

    #[test]
    fn typing_editing_and_limits() {
        let dom = html::parse("<form><input name=q value='ab'><textarea name=t>\nline1</textarea><input type=checkbox name=c></form>");
        let (q, t, c) = (by_name(&dom, "q"), by_name(&dom, "t"), by_name(&dom, "c"));
        let mut st = FormState::default();
        assert_eq!((st.value(&dom, q), st.value(&dom, t)), ("ab".into(), "line1".into()), "textarea drops its first newline");
        st.type_text(&dom, q, "c\nd\te\u{1b}");
        assert_eq!(st.value(&dom, q), "abcde", "control characters are dropped from a text input");
        st.type_text(&dom, t, "\nline2");
        assert_eq!(st.value(&dom, t), "line1\nline2", "but a textarea keeps newlines");
        st.backspace(&dom, q);
        st.backspace(&dom, q);
        assert_eq!(st.value(&dom, q), "abc");
        for _ in 0..10 {
            st.backspace(&dom, q);
        }
        assert_eq!(st.value(&dom, q), "", "backspacing an empty field is fine");
        // typing into a non-text control does nothing; length is capped
        st.type_text(&dom, c, "xyz");
        assert_eq!(st.value(&dom, c), "");
        st.type_text(&dom, q, &"x".repeat(MAX_VALUE_CHARS * 2));
        assert_eq!(st.value(&dom, q).chars().count(), MAX_VALUE_CHARS);
    }

    #[test]
    fn readonly_and_maxlength_are_honoured() {
        // a readonly field used to accept typing, and maxlength was ignored
        let dom = html::parse("<form><input name=ro readonly value=fixed><input name=ml maxlength=3 value=ab><input name=bad maxlength=junk><textarea name=ta maxlength=4>xy</textarea></form>");
        let (ro, ml, bad, ta) = (by_name(&dom, "ro"), by_name(&dom, "ml"), by_name(&dom, "bad"), by_name(&dom, "ta"));
        let mut st = FormState::default();
        st.type_text(&dom, ro, "XYZ");
        st.backspace(&dom, ro);
        assert_eq!(st.value(&dom, ro), "fixed");
        assert!(is_focusable(&dom, ro), "readonly fields can still be focused and selected");
        st.type_text(&dom, ml, "cdef");
        assert_eq!(st.value(&dom, ml), "abc");
        st.type_text(&dom, ta, "123456");
        assert_eq!(st.value(&dom, ta), "xy12");
        st.type_text(&dom, bad, "still works");
        assert_eq!(st.value(&dom, bad), "still works", "an invalid maxlength is ignored");
    }

    #[test]
    fn disabled_controls_ignore_input_and_are_not_submitted() {
        let dom = html::parse("<form><input name=a value=1 disabled><input name=b value=2></form>");
        let (a, form) = (by_name(&dom, "a"), find(&dom, "form", 0));
        let mut st = FormState::default();
        st.type_text(&dom, a, "zzz");
        assert_eq!(st.value(&dom, a), "1");
        assert!(!is_focusable(&dom, a));
        let sub = st.submit(&dom, form, None, &base()).unwrap();
        assert_eq!(sub.url.query(), Some("b=2"));
    }

    #[test]
    fn a_disabled_fieldset_disables_everything_inside_except_its_legend() {
        let dom = html::parse(
            "<form><fieldset disabled><legend><input name=in_legend value=L></legend><input name=a value=1>\
             <div><button name=btn>go</button><textarea name=t>x</textarea></div></fieldset><input name=out value=2></form>",
        );
        let st = FormState::default();
        for n in ["a", "btn", "t"] {
            assert!(is_disabled(&dom, by_name(&dom, n)), "{n}");
            assert!(!is_focusable(&dom, by_name(&dom, n)), "{n}");
        }
        assert!(!is_disabled(&dom, by_name(&dom, "in_legend")), "controls in the legend are exempt");
        assert!(!is_disabled(&dom, by_name(&dom, "out")));
        let sub = st.submit(&dom, find(&dom, "form", 0), None, &base()).unwrap();
        assert_eq!(sub.url.query(), Some("in_legend=L&out=2"));
        let mut st = FormState::default();
        st.type_text(&dom, by_name(&dom, "a"), "zzz");
        assert_eq!(st.value(&dom, by_name(&dom, "a")), "1");
    }

    #[test]
    fn only_the_first_legend_of_a_disabled_fieldset_is_exempt() {
        let dom = html::parse(
            "<fieldset disabled><legend><input name=first></legend><legend><input name=second></legend>\
             <div><legend><input name=nested></legend></div></fieldset>\
             <fieldset disabled><legend>t</legend><fieldset><input name=inner></fieldset></fieldset>",
        );
        assert!(!is_disabled(&dom, by_name(&dom, "first")));
        assert!(is_disabled(&dom, by_name(&dom, "second")), "a second legend gives no exemption");
        assert!(is_disabled(&dom, by_name(&dom, "nested")), "neither does one that isn't a direct child");
        assert!(is_disabled(&dom, by_name(&dom, "inner")), "a fieldset inside a disabled one stays disabled");
        // a legend of an INNER disabled fieldset doesn't free a control from the OUTER one
        let dom = html::parse("<fieldset disabled><div><fieldset disabled><legend><input name=x></legend></fieldset></div></fieldset>");
        assert!(is_disabled(&dom, by_name(&dom, "x")));
    }

    #[test]
    fn a_wide_disabled_fieldset_is_not_quadratic() {
        // every control used to scan all the fieldset's children for its first legend
        let inputs: String = (0..20_000).map(|i| format!("<input name=a{i}>")).collect();
        let dom = html::parse(&format!("<form><fieldset disabled>{inputs}</fieldset></form>"));
        let t = std::time::Instant::now();
        let n = (0..dom.nodes.len()).filter(|&i| kind_of(&dom, i).is_some() && is_disabled(&dom, i)).count();
        assert_eq!(n, 20_000);
        assert!(t.elapsed().as_secs() < 5, "{:?}", t.elapsed());
    }

    #[test]
    fn several_checked_radios_in_markup_submit_only_the_last() {
        let dom = html::parse(
            "<form action=/s><input type=radio name=r value=1 checked><input type=radio name=r value=2 checked>\
             <input name=mid value=m><input type=radio name=r value=3 checked><input type=radio name=other value=a checked></form>",
        );
        let sub = FormState::default().submit(&dom, find(&dom, "form", 0), None, &base()).unwrap();
        assert_eq!(sub.url.query(), Some("r=3&mid=m&other=a"), "one value per group, in the place of the first");
        // clicking a radio decides it, as before
        let radios: Vec<_> = (0..dom.nodes.len()).filter(|&i| kind_of(&dom, i) == Some(ControlKind::Radio)).collect();
        let mut st = FormState::default();
        st.toggle(&dom, radios[0]);
        assert_eq!(st.submit(&dom, find(&dom, "form", 0), None, &base()).unwrap().url.query(), Some("r=1&mid=m&other=a"));
    }

    #[test]
    fn a_disabled_checked_radio_still_decides_its_group() {
        // the last checked radio is the group's choice; if it is disabled nothing is sent
        let dom = html::parse("<form action=/s><input type=radio name=r value=1 checked><input type=radio name=r value=2 checked disabled><input name=z value=1></form>");
        let sub = FormState::default().submit(&dom, find(&dom, "form", 0), None, &base()).unwrap();
        assert_eq!(sub.url.query(), Some("z=1"));
        // through a disabled fieldset too
        let dom = html::parse("<form action=/s><input type=radio name=r value=1 checked><fieldset disabled><input type=radio name=r value=2 checked></fieldset></form>");
        let sub = FormState::default().submit(&dom, find(&dom, "form", 0), None, &base()).unwrap();
        assert_eq!(sub.url.query(), None);
        // a lone disabled radio is simply not sent, and an enabled last one wins over a disabled earlier one
        let dom = html::parse("<form action=/s><input type=radio name=r value=1 checked disabled><input type=radio name=r value=2 checked></form>");
        let sub = FormState::default().submit(&dom, find(&dom, "form", 0), None, &base()).unwrap();
        assert_eq!(sub.url.query(), Some("r=2"));
        let dom = html::parse("<form action=/s><input type=radio name=r value=1 checked disabled></form>");
        assert_eq!(FormState::default().submit(&dom, find(&dom, "form", 0), None, &base()).unwrap().url.query(), None);
    }

    #[test]
    fn the_hidden_attribute_removes_controls_from_the_tab_order() {
        let dom = html::parse("<form><input name=a hidden><div hidden><input name=b></div><input name=c></form>");
        let mut st = FormState::default();
        st.focus_next(&dom, true);
        assert_eq!(st.focus, Some(by_name(&dom, "c")));
        st.focus_next(&dom, true);
        assert_eq!(st.focus, Some(by_name(&dom, "c")), "only one focusable control, so it wraps to itself");
    }

    #[test]
    fn checkboxes_toggle_and_radios_are_exclusive_within_a_form() {
        let dom = html::parse(
            "<form><input type=checkbox name=c value=yes checked><input type=radio name=r value=1 checked><input type=radio name=r value=2><input type=radio name=r value=3></form>\
             <form><input type=radio name=r value=other checked></form>",
        );
        let boxes: Vec<_> = (0..dom.nodes.len()).filter(|&i| matches!(kind_of(&dom, i), Some(ControlKind::Radio))).collect();
        let c = by_name(&dom, "c");
        let mut st = FormState::default();
        assert!(st.is_checked(&dom, c) && st.is_checked(&dom, boxes[0]) && !st.is_checked(&dom, boxes[1]));
        st.toggle(&dom, c);
        assert!(!st.is_checked(&dom, c));
        st.toggle(&dom, c);
        assert!(st.is_checked(&dom, c));
        st.toggle(&dom, boxes[1]);
        assert!(!st.is_checked(&dom, boxes[0]) && st.is_checked(&dom, boxes[1]), "radio 2 replaces radio 1");
        assert!(st.is_checked(&dom, boxes[3]), "a radio of the same name in ANOTHER form is untouched");
        st.toggle(&dom, boxes[1]);
        assert!(st.is_checked(&dom, boxes[1]), "clicking a checked radio keeps it checked");
    }

    #[test]
    fn radios_outside_any_form_still_form_a_group() {
        // they used to stay checked together, since grouping required an enclosing <form>
        let dom = html::parse("<input type=radio name=r value=1 checked><input type=radio name=r value=2><form><input type=radio name=r value=3 checked></form>");
        let radios: Vec<_> = (0..dom.nodes.len()).filter(|&i| kind_of(&dom, i) == Some(ControlKind::Radio)).collect();
        let mut st = FormState::default();
        st.toggle(&dom, radios[1]);
        assert!(!st.is_checked(&dom, radios[0]) && st.is_checked(&dom, radios[1]));
        assert!(st.is_checked(&dom, radios[2]), "the radio inside a form is a different group");
    }

    #[test]
    fn selects_choose_and_cycle_options() {
        let dom = html::parse("<form><select name=s><option value=a>Apple<option value=b selected>Banana<option>Cherry  Pie</select></form>");
        let s = by_name(&dom, "s");
        let mut st = FormState::default();
        assert_eq!(options(&dom, s), [("a".into(), "Apple".into()), ("b".into(), "Banana".into()), ("Cherry Pie".into(), "Cherry Pie".into())]);
        assert_eq!((st.selected_index(&dom, s), st.value(&dom, s)), (1, "b".into()), "the selected attribute picks the start");
        st.cycle_select(&dom, s);
        assert_eq!((st.value(&dom, s), st.selected_label(&dom, s)), ("Cherry Pie".into(), "Cherry Pie".into()));
        st.cycle_select(&dom, s);
        assert_eq!(st.value(&dom, s), "a", "wraps around");
        // an empty select doesn't panic
        let dom = html::parse("<select name=e></select>");
        let mut st = FormState::default();
        st.cycle_select(&dom, by_name(&dom, "e"));
        assert_eq!(st.value(&dom, by_name(&dom, "e")), "");
    }

    #[test]
    fn get_submission_encodes_values_and_replaces_the_query() {
        let dom = html::parse(
            "<form action='search?keep=no'><input name=q><input type=hidden name=h value='a&b'>\
             <input type=checkbox name=on checked><input type=checkbox name=off><input type=radio name=r value=x checked>\
             <select name=s><option value=1>One<option value=2 selected>Two</select><textarea name=t>hi there</textarea>\
             <input type=submit name=go value=Go><input type=submit name=other value=Other><input name=''><input value=nameless></form>",
        );
        let (form, q, go) = (find(&dom, "form", 0), by_name(&dom, "q"), by_name(&dom, "go"));
        let mut st = FormState::default();
        st.type_text(&dom, q, "rust & más?");
        let sub = st.submit(&dom, form, Some(go), &base()).unwrap();
        assert_eq!(sub.method, Method::Get);
        assert_eq!(sub.url.path(), "/dir/search", "the action resolves against the page");
        assert_eq!(sub.url.fragment(), None);
        assert_eq!(
            sub.url.query(),
            Some("q=rust+%26+m%C3%A1s%3F&h=a%26b&on=on&r=x&s=2&t=hi+there&go=Go"),
            "only the clicked submit button is sent; unchecked and nameless controls are skipped"
        );
        // no submitter: no button is sent
        let sub = st.submit(&dom, form, None, &base()).unwrap();
        assert!(!sub.url.query().unwrap().contains("go="));
    }

    #[test]
    fn textarea_line_breaks_are_submitted_as_crlf() {
        let dom = html::parse("<form method=post><textarea name=t>a</textarea></form>");
        let t = by_name(&dom, "t");
        let mut st = FormState::default();
        st.type_text(&dom, t, "\nb\n\nc");
        let sub = st.submit(&dom, find(&dom, "form", 0), None, &base()).unwrap();
        assert_eq!(sub.body, "t=a%0D%0Ab%0D%0A%0D%0Ac");
    }

    #[test]
    fn the_form_attribute_associates_controls_with_a_form_anywhere() {
        let dom = html::parse(
            "<form id=f action=/one><input name=in value=1><input name=moved form=g value=X></form>\
             <input name=out form=f value=2><input name=orphan value=3>\
             <form id=g action=/two></form>",
        );
        let (f, g) = (find(&dom, "form", 0), find(&dom, "form", 1));
        let st = FormState::default();
        let one = st.submit(&dom, f, None, &base()).unwrap();
        assert_eq!((one.url.path(), one.url.query()), ("/one", Some("in=1&out=2")));
        let two = st.submit(&dom, g, None, &base()).unwrap();
        assert_eq!((two.url.path(), two.url.query()), ("/two", Some("moved=X")));
        assert_eq!(form_of(&dom, by_name(&dom, "orphan")), None);
        // a form= naming no form owns nothing, even inside a <form>
        let dom = html::parse("<form id=f><input name=a form=nope></form>");
        assert_eq!(form_of(&dom, by_name(&dom, "a")), None);
        assert!(controls_in(&dom, find(&dom, "form", 0)).is_empty());
    }

    #[test]
    fn a_submit_button_can_override_action_and_method() {
        let dom = html::parse(
            "<form action=/default method=get><input name=a value=1>\
             <button name=plain>x</button><button name=alt formaction=/other formmethod=POST>y</button></form>",
        );
        let (form, plain, alt) = (find(&dom, "form", 0), by_name(&dom, "plain"), by_name(&dom, "alt"));
        let st = FormState::default();
        let a = st.submit(&dom, form, Some(plain), &base()).unwrap();
        assert_eq!((a.method, a.url.path()), (Method::Get, "/default"));
        let b = st.submit(&dom, form, Some(alt), &base()).unwrap();
        assert_eq!((b.method, b.url.as_str(), b.body.as_str()), (Method::Post, "https://example.com/other", "a=1&alt="));
    }

    #[test]
    fn method_dialog_submits_nothing() {
        let dom = html::parse("<form method=dialog action=/x><input name=a></form>");
        assert_eq!(FormState::default().submit(&dom, find(&dom, "form", 0), None, &base()), None);
    }

    #[test]
    fn empty_action_targets_the_current_page_and_empty_forms_drop_the_query() {
        let dom = html::parse("<form><input name=q></form><form method=get action=''></form>");
        let st = FormState::default();
        let sub = st.submit(&dom, find(&dom, "form", 0), None, &base()).unwrap();
        assert_eq!(sub.url.as_str(), "https://example.com/dir/page.html?q=");
        // no controls at all: the old query is dropped, not kept
        let sub = st.submit(&dom, find(&dom, "form", 1), None, &base()).unwrap();
        assert_eq!(sub.url.as_str(), "https://example.com/dir/page.html");
        // absolute and protocol-relative actions
        let dom = html::parse("<form action='//other.test/x'><input name=a value=1></form>");
        assert_eq!(st.submit(&dom, find(&dom, "form", 0), None, &base()).unwrap().url.as_str(), "https://other.test/x?a=1");
    }

    #[test]
    fn post_submission_builds_an_urlencoded_body() {
        let dom = html::parse("<form method=POST action=/login><input name=user value=a><input name=pw type=password value='p w'></form>");
        let sub = FormState::default().submit(&dom, find(&dom, "form", 0), None, &base()).unwrap();
        assert_eq!((sub.method, sub.url.as_str(), sub.body.as_str()), (Method::Post, "https://example.com/login", "user=a&pw=p+w"));
    }

    #[test]
    fn tab_order_wraps_and_skips_hidden_and_disabled() {
        let dom = html::parse("<form><input name=a><input type=hidden name=h><input name=b disabled><input type=checkbox name=c><button>go</button></form>");
        let (a, c) = (by_name(&dom, "a"), by_name(&dom, "c"));
        let button = find(&dom, "button", 0);
        let mut st = FormState::default();
        st.focus_next(&dom, true);
        assert_eq!(st.focus, Some(a));
        st.focus_next(&dom, true);
        assert_eq!(st.focus, Some(c), "hidden and disabled controls are skipped");
        st.focus_next(&dom, true);
        assert_eq!(st.focus, Some(button));
        st.focus_next(&dom, true);
        assert_eq!(st.focus, Some(a), "wraps forward");
        st.focus_next(&dom, false);
        assert_eq!(st.focus, Some(button), "and backward");
        // a page with no controls
        let none = html::parse("<p>text</p>");
        let mut st = FormState { focus: Some(3), ..Default::default() };
        st.focus_next(&none, true);
        assert_eq!(st.focus, None);
    }

    #[test]
    fn enter_in_a_field_clicks_the_first_submit_button() {
        let dom = html::parse("<form><input name=q><input type=button name=no><input type=submit name=first value=1><input type=submit name=second value=2></form>");
        let form = find(&dom, "form", 0);
        assert_eq!(FormState::default_button(&dom, form), Some(by_name(&dom, "first")));
        let dom = html::parse("<form><input name=q></form>");
        assert_eq!(FormState::default_button(&dom, find(&dom, "form", 0)), None);
        assert_eq!(form_of(&dom, by_name(&dom, "q")), Some(find(&dom, "form", 0)));
        assert_eq!(form_of(&dom, find(&dom, "form", 0)), None);
    }

    #[test]
    fn hostile_markup_does_not_panic() {
        for src in [
            "<input>", "<form><form><input name=a></form></form>", "<select><option></select>", "<textarea>",
            "<form action='http://[::1'><input name=a></form>", "<form action='javascript:alert(1)'><input name=a></form>",
            "<option><input name=x>", "<form><input name='é日🎉' value='é日🎉'></form>",
            "<form id=a><input form=a form=a name=x></form>", "<fieldset disabled><fieldset><legend><legend><input>",
            "<input maxlength=99999999999999999999 readonly hidden>",
        ] {
            let dom = html::parse(src);
            let st = FormState::default();
            for f in (0..dom.nodes.len()).filter(|&i| dom.tag(i) == Some("form")) {
                let _ = st.submit(&dom, f, None, &base());
            }
            for i in 0..dom.nodes.len() {
                let mut st = FormState::default();
                st.type_text(&dom, i, "x");
                st.backspace(&dom, i);
                st.toggle(&dom, i);
                st.cycle_select(&dom, i);
                st.focus_next(&dom, true);
                let _ = st.value(&dom, i);
            }
        }
    }
}
