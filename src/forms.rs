//! Forms: which elements are controls, what their current values are (typed
//! text, checked state, chosen option), and how a submitted form becomes a
//! request. This module is pure state and logic (no drawing, no windowing).
use crate::dom::{Dom, NodeId};
use std::collections::HashMap;
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

pub fn is_disabled(dom: &Dom, id: NodeId) -> bool {
    dom.attr(id, "disabled").is_some()
}

/// Can the user focus this control (click into it, Tab to it)?
pub fn is_focusable(dom: &Dom, id: NodeId) -> bool {
    kind_of(dom, id).is_some_and(|k| k != ControlKind::Hidden) && !is_disabled(dom, id)
}

/// The `<form>` element enclosing `id`.
pub fn form_of(dom: &Dom, id: NodeId) -> Option<NodeId> {
    let mut cur = dom.nodes[id].parent;
    while let Some(p) = cur {
        if dom.tag(p) == Some("form") {
            return Some(p);
        }
        cur = dom.nodes[p].parent;
    }
    None
}

/// Every control inside `form`, in document order.
pub fn controls_in(dom: &Dom, form: NodeId) -> Vec<NodeId> {
    let mut out = vec![];
    let mut stack: Vec<NodeId> = dom.nodes[form].children.iter().rev().copied().collect();
    while let Some(n) = stack.pop() {
        if kind_of(dom, n).is_some() {
            out.push(n);
        }
        stack.extend(dom.nodes[n].children.iter().rev());
    }
    out
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

    /// Type (or paste) text into a text control. Control characters are dropped,
    /// except that a textarea keeps newlines.
    pub fn type_text(&mut self, dom: &Dom, id: NodeId, text: &str) {
        let Some(kind) = kind_of(dom, id).filter(|k| is_text_like(*k)) else { return };
        if is_disabled(dom, id) {
            return;
        }
        let mut value = self.value(dom, id);
        let mut count = value.chars().count();
        for c in text.chars() {
            let ok = !c.is_control() || (c == '\n' && kind == ControlKind::Textarea);
            if ok && count < MAX_VALUE_CHARS {
                value.push(c);
                count += 1;
            }
        }
        self.values.insert(id, value);
    }

    pub fn backspace(&mut self, dom: &Dom, id: NodeId) {
        if !kind_of(dom, id).is_some_and(is_text_like) || is_disabled(dom, id) {
            return;
        }
        let mut value = self.value(dom, id);
        value.pop();
        self.values.insert(id, value);
    }

    /// Click a checkbox or radio button. Checking a radio unchecks the others
    /// in its group (same form, same name).
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
                if let (Some(name), Some(form)) = (dom.attr(id, "name"), form_of(dom, id)) {
                    for other in controls_in(dom, form) {
                        if other != id && kind_of(dom, other) == Some(ControlKind::Radio) && dom.attr(other, "name") == Some(name) {
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
    /// clicked: only its name and value are sent. Returns None if the action can't
    /// be resolved against `base`.
    pub fn submit(&self, dom: &Dom, form: NodeId, submitter: Option<NodeId>, base: &Url) -> Option<Submission> {
        let mut pairs: Vec<(String, String)> = vec![];
        for id in controls_in(dom, form) {
            if is_disabled(dom, id) {
                continue;
            }
            let Some(name) = dom.attr(id, "name").filter(|n| !n.is_empty()) else { continue };
            match kind_of(dom, id)? {
                ControlKind::Hidden | ControlKind::Text | ControlKind::Password | ControlKind::Textarea => {
                    pairs.push((name.to_string(), self.value(dom, id)));
                }
                ControlKind::Checkbox | ControlKind::Radio => {
                    if self.is_checked(dom, id) {
                        pairs.push((name.to_string(), dom.attr(id, "value").unwrap_or("on").to_string()));
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
        let method = match dom.attr(form, "method").map(|m| m.trim().to_ascii_lowercase()).as_deref() {
            Some("post") => Method::Post,
            _ => Method::Get,
        };
        let action = dom.attr(form, "action").map(str::trim).unwrap_or("");
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
                let _ = st.value(&dom, i);
            }
        }
    }
}
