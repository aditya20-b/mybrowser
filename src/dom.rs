//! The DOM: an arena of nodes. Parents are always created before their
//! children, so `NodeId`s are in document order and `parent < child`.
use std::collections::HashMap;

pub type NodeId = usize;

pub enum Kind {
    Element { tag: String, attrs: HashMap<String, String> },
    Text(String),
}

pub struct Node {
    pub kind: Kind,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
}

pub struct Dom {
    pub nodes: Vec<Node>,
}

impl Dom {
    /// Creates a DOM containing only the implicit root `<html>` element (id 0).
    pub fn new() -> Dom {
        let root = Node {
            kind: Kind::Element { tag: "html".into(), attrs: HashMap::new() },
            parent: None,
            children: vec![],
        };
        Dom { nodes: vec![root] }
    }

    pub fn add(&mut self, parent: NodeId, kind: Kind) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node { kind, parent: Some(parent), children: vec![] });
        self.nodes[parent].children.push(id);
        id
    }

    pub fn tag(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].kind {
            Kind::Element { tag, .. } => Some(tag),
            Kind::Text(_) => None,
        }
    }

    pub fn attr(&self, id: NodeId, name: &str) -> Option<&str> {
        match &self.nodes[id].kind {
            Kind::Element { attrs, .. } => attrs.get(name).map(|s| s.as_str()),
            Kind::Text(_) => None,
        }
    }

    /// Concatenated text of all descendant text nodes.
    pub fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.collect_text(id, &mut out);
        out
    }

    fn collect_text(&self, id: NodeId, out: &mut String) {
        match &self.nodes[id].kind {
            Kind::Text(t) => out.push_str(t),
            Kind::Element { .. } => {
                for &c in &self.nodes[id].children {
                    self.collect_text(c, out);
                }
            }
        }
    }
}
