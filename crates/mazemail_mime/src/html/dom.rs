//! A small arena DOM filled by `html5ever`.
//!
//! The sanitizer (`ammonia`) keeps its DOM private, so the passes that need a tree
//! after sanitizing (tracker removal, resource rewriting, the reader model) parse the
//! sanitized output again into this arena. Nodes are addressed by index, children are
//! plain vectors, and every traversal is iterative, so hostile nesting depth can't
//! overflow the stack.

use std::borrow::Cow;
use std::cell::{Ref, RefCell};
use std::io;

use super::nesting::limit_nesting;
use html5ever::interface::{Attribute, ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::serialize::{Serialize, SerializeOpts, Serializer, TraversalScope, serialize};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{LocalName, ParseOpts, QualName, local_name, ns, parse_fragment};

/// Index of a node in [`Dom::nodes`].
pub(crate) type NodeId = usize;

/// The document node is always the first node in the arena.
const DOCUMENT: NodeId = 0;

#[derive(Debug)]
enum NodeData {
    Document,
    Element {
        name: QualName,
        attrs: Vec<Attribute>,
    },
    Text(StrTendril),
    /// Comments, doctypes and processing instructions. Kept in the arena so handles
    /// stay valid, but never serialized or read.
    Other,
}

#[derive(Debug)]
struct Node {
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    data: NodeData,
}

/// A parsed HTML fragment.
#[derive(Debug)]
pub(crate) struct Dom {
    nodes: Vec<Node>,
}

impl Dom {
    /// Parse an HTML fragment the way a browser parses `div.innerHTML = html`, with
    /// nesting capped (see [`limit_nesting`]).
    pub(crate) fn parse_fragment(html: &str) -> Self {
        let html = limit_nesting(html);
        let sink = Sink {
            arena: RefCell::new(Arena {
                nodes: vec![Node {
                    parent: None,
                    children: Vec::new(),
                    data: NodeData::Document,
                }],
                fallback_name: QualName::new(None, ns!(html), local_name!("div")),
            }),
        };
        parse_fragment(
            sink,
            ParseOpts::default(),
            QualName::new(None, ns!(html), local_name!("div")),
            Vec::new(),
            false,
        )
        .one(&*html)
    }

    /// The element whose children are the fragment content.
    pub(crate) fn root(&self) -> NodeId {
        self.nodes[DOCUMENT]
            .children
            .iter()
            .copied()
            .find(|&id| matches!(self.nodes[id].data, NodeData::Element { .. }))
            .unwrap_or(DOCUMENT)
    }

    pub(crate) fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id].children
    }

    /// The local name of an HTML element, `None` for every other node.
    pub(crate) fn tag(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Element { name, .. } => Some(&name.local),
            _ => None,
        }
    }

    pub(crate) fn text(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Text(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn set_text(&mut self, id: NodeId, value: &str) {
        if let NodeData::Text(text) = &mut self.nodes[id].data {
            *text = StrTendril::from_slice(value);
        }
    }

    pub(crate) fn attr(&self, id: NodeId, attr: &str) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Element { attrs, .. } => attrs
                .iter()
                .find(|a| a.name.ns == ns!() && &*a.name.local == attr)
                .map(|a| &*a.value),
            _ => None,
        }
    }

    pub(crate) fn set_attr(&mut self, id: NodeId, attr: &str, value: &str) {
        if let NodeData::Element { attrs, .. } = &mut self.nodes[id].data {
            if let Some(existing) = attrs
                .iter_mut()
                .find(|a| a.name.ns == ns!() && &*a.name.local == attr)
            {
                existing.value = StrTendril::from_slice(value);
            } else {
                attrs.push(Attribute {
                    name: QualName::new(None, ns!(), LocalName::from(attr)),
                    value: StrTendril::from_slice(value),
                });
            }
        }
    }

    pub(crate) fn remove_attr(&mut self, id: NodeId, attr: &str) {
        if let NodeData::Element { attrs, .. } = &mut self.nodes[id].data {
            attrs.retain(|a| !(a.name.ns == ns!() && &*a.name.local == attr));
        }
    }

    /// Unlink a node (and its subtree) from its parent.
    pub(crate) fn detach(&mut self, id: NodeId) {
        if let Some(parent) = self.nodes[id].parent.take() {
            self.nodes[parent].children.retain(|&c| c != id);
        }
    }

    /// All nodes below `id` in document order, `id` excluded.
    pub(crate) fn descendants(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = self.nodes[id].children.iter().rev().copied().collect();
        while let Some(next) = stack.pop() {
            out.push(next);
            stack.extend(self.nodes[next].children.iter().rev().copied());
        }
        out
    }

    /// The concatenated text below `id`, in document order.
    pub(crate) fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        for node in self.descendants(id) {
            if let Some(text) = self.text(node) {
                out.push_str(text);
            }
        }
        out
    }

    /// Serialize the children of `id` as HTML.
    pub(crate) fn serialize_children(&self, id: NodeId) -> String {
        let mut out = Vec::new();
        let opts = SerializeOpts {
            traversal_scope: TraversalScope::ChildrenOnly(None),
            ..SerializeOpts::default()
        };
        // Writing into a Vec can't fail, and the serializer only emits UTF-8.
        if serialize(&mut out, &Serializable { dom: self, id }, opts).is_err() {
            return String::new();
        }
        String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into())
    }
}

struct Serializable<'a> {
    dom: &'a Dom,
    id: NodeId,
}

enum Op {
    Open(NodeId),
    Close(QualName),
}

impl Serialize for Serializable<'_> {
    fn serialize<S: Serializer>(
        &self,
        serializer: &mut S,
        scope: TraversalScope,
    ) -> io::Result<()> {
        let mut ops: Vec<Op> = match scope {
            TraversalScope::IncludeNode => vec![Op::Open(self.id)],
            TraversalScope::ChildrenOnly(_) => self
                .dom
                .children(self.id)
                .iter()
                .rev()
                .map(|&c| Op::Open(c))
                .collect(),
        };
        while let Some(op) = ops.pop() {
            match op {
                Op::Open(id) => match &self.dom.nodes[id].data {
                    NodeData::Element { name, attrs } => {
                        serializer
                            .start_elem(name.clone(), attrs.iter().map(|a| (&a.name, &*a.value)))?;
                        ops.push(Op::Close(name.clone()));
                        ops.extend(self.dom.children(id).iter().rev().map(|&c| Op::Open(c)));
                    }
                    NodeData::Text(text) => serializer.write_text(text)?,
                    NodeData::Document | NodeData::Other => {}
                },
                Op::Close(name) => serializer.end_elem(name)?,
            }
        }
        Ok(())
    }
}

struct Arena {
    nodes: Vec<Node>,
    /// Returned by `elem_name` for non-elements, which html5ever never asks for.
    fallback_name: QualName,
}

impl Arena {
    fn push(&mut self, data: NodeData) -> NodeId {
        self.nodes.push(Node {
            parent: None,
            children: Vec::new(),
            data,
        });
        self.nodes.len() - 1
    }

    fn detach(&mut self, id: NodeId) {
        if let Some(parent) = self.nodes[id].parent.take() {
            self.nodes[parent].children.retain(|&c| c != id);
        }
    }

    fn append(&mut self, parent: NodeId, child: NodeOrText<NodeId>) {
        match child {
            NodeOrText::AppendNode(id) => {
                self.detach(id);
                self.nodes[id].parent = Some(parent);
                self.nodes[parent].children.push(id);
            }
            NodeOrText::AppendText(text) => {
                if let Some(&last) = self.nodes[parent].children.last()
                    && let NodeData::Text(existing) = &mut self.nodes[last].data
                {
                    existing.push_tendril(&text);
                    return;
                }
                let id = self.push(NodeData::Text(text));
                self.nodes[id].parent = Some(parent);
                self.nodes[parent].children.push(id);
            }
        }
    }

    fn insert_before(&mut self, sibling: NodeId, child: NodeOrText<NodeId>) {
        let Some(parent) = self.nodes[sibling].parent else {
            return;
        };
        match child {
            NodeOrText::AppendNode(id) => {
                self.detach(id);
                let Some(pos) = self.position(parent, sibling) else {
                    return;
                };
                self.nodes[id].parent = Some(parent);
                self.nodes[parent].children.insert(pos, id);
            }
            NodeOrText::AppendText(text) => {
                let Some(pos) = self.position(parent, sibling) else {
                    return;
                };
                if pos > 0 {
                    let prev = self.nodes[parent].children[pos - 1];
                    if let NodeData::Text(existing) = &mut self.nodes[prev].data {
                        existing.push_tendril(&text);
                        return;
                    }
                }
                let id = self.push(NodeData::Text(text));
                self.nodes[id].parent = Some(parent);
                self.nodes[parent].children.insert(pos, id);
            }
        }
    }

    fn position(&self, parent: NodeId, child: NodeId) -> Option<usize> {
        self.nodes[parent].children.iter().position(|&c| c == child)
    }
}

struct Sink {
    arena: RefCell<Arena>,
}

impl TreeSink for Sink {
    type Handle = NodeId;
    type Output = Dom;
    type ElemName<'a> = Ref<'a, QualName>;

    fn finish(self) -> Dom {
        Dom {
            nodes: self.arena.into_inner().nodes,
        }
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> NodeId {
        DOCUMENT
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> Ref<'a, QualName> {
        Ref::map(self.arena.borrow(), |arena| {
            match arena.nodes.get(*target).map(|n| &n.data) {
                Some(NodeData::Element { name, .. }) => name,
                _ => &arena.fallback_name,
            }
        })
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, _: ElementFlags) -> NodeId {
        self.arena
            .borrow_mut()
            .push(NodeData::Element { name, attrs })
    }

    fn create_comment(&self, _text: StrTendril) -> NodeId {
        self.arena.borrow_mut().push(NodeData::Other)
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> NodeId {
        self.arena.borrow_mut().push(NodeData::Other)
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        self.arena.borrow_mut().append(*parent, child);
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        let mut arena = self.arena.borrow_mut();
        if arena.nodes[*element].parent.is_some() {
            arena.insert_before(*element, child);
        } else {
            arena.append(*prev_element, child);
        }
    }

    fn append_doctype_to_document(&self, _: StrTendril, _: StrTendril, _: StrTendril) {}

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        // Template content is attached to the template element itself. The sanitizer
        // drops `<template>` with its content before we ever parse.
        *target
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, _mode: QuirksMode) {}

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        self.arena.borrow_mut().insert_before(*sibling, new_node);
    }

    fn add_attrs_if_missing(&self, target: &NodeId, new_attrs: Vec<Attribute>) {
        let mut arena = self.arena.borrow_mut();
        if let Some(NodeData::Element { attrs, .. }) =
            arena.nodes.get_mut(*target).map(|n| &mut n.data)
        {
            for attr in new_attrs {
                if !attrs.iter().any(|a| a.name == attr.name) {
                    attrs.push(attr);
                }
            }
        }
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.arena.borrow_mut().detach(*target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let mut arena = self.arena.borrow_mut();
        let children = std::mem::take(&mut arena.nodes[*node].children);
        for &child in &children {
            arena.nodes[child].parent = Some(*new_parent);
        }
        arena.nodes[*new_parent].children.extend(children);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_fragment() {
        let dom = Dom::parse_fragment("<p class=x>Hello <b>world</b> &amp; you</p><br>");
        assert_eq!(
            dom.serialize_children(dom.root()),
            "<p class=\"x\">Hello <b>world</b> &amp; you</p><br>"
        );
    }

    #[test]
    fn edits_attributes_and_detaches() {
        let mut dom = Dom::parse_fragment("<img src=a alt=b><span>gone</span>");
        let root = dom.root();
        let img = dom.children(root)[0];
        let span = dom.children(root)[1];
        dom.set_attr(img, "src", "c");
        dom.remove_attr(img, "alt");
        dom.set_attr(img, "width", "3");
        dom.detach(span);
        assert_eq!(dom.serialize_children(root), "<img src=\"c\" width=\"3\">");
    }

    #[test]
    fn deep_nesting_does_not_overflow() {
        let html = "<div>".repeat(50_000) + "x";
        let dom = Dom::parse_fragment(&html);
        assert!(dom.text_content(dom.root()).contains('x'));
        assert!(!dom.serialize_children(dom.root()).is_empty());
    }

    #[test]
    fn misnested_markup_is_repaired() {
        let dom = Dom::parse_fragment("<b>bold <i>both</b> italic</i><table><td>cell</table>");
        assert_eq!(
            dom.serialize_children(dom.root()),
            "<b>bold <i>both</i></b><i> italic</i><table><tbody><tr><td>cell</td></tr></tbody></table>"
        );
    }
}
