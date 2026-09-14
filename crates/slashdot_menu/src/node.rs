//! Menu node descriptor (design §2) — static-level subset for Phase 3.
//!
//! A node is one level's definition. Static children are a fixed list;
//! value-typed children (enum/number) accept a set of values in the
//! selection position with one shared handler. Dynamic data levels arrive
//! in Phase 4 via a `DataProvider` field.

/// A value-typed child (design §2): matches any of a defined set of values
/// in the selection position.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueChild {
    /// Enum value set (completion aid only; any token is permissively valid).
    Enum(Vec<String>),
    /// Any permissively-parsed number.
    Number,
}

/// A menu node: one level of the descriptor tree.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MenuNode {
    /// The token that selects this node at its parent level. Space-free.
    pub key: String,
    /// Optional single-line description; fuzzy-searched with the key.
    pub description: Option<String>,
    /// Optional single-line header shown when this node's child menu is
    /// active (R8-4 static half).
    pub child_header: Option<String>,
    /// Static children (fixed node list).
    pub children: Vec<MenuNode>,
    /// Optional value-typed child occupying the selection position.
    pub value: Option<ValueChild>,
    /// Leaf flag: node has an accept handler (Enter commits; Phase 5 wires
    /// the actual invocation). Absent ⇒ Enter behaves like Tab + drill.
    pub is_leaf: bool,
    /// Dynamic level marker: rows come from a registered DataProvider.
    pub dynamic: bool,
    /// Interstitial text (D4): shown on Enter before any leaf call or
    /// drilling. With no leaf handler: second Enter dismisses (/stat).
    /// With a leaf: first Enter shows, next Enter invokes (/replay).
    pub interstitial: Option<String>,
    /// Confirm property (D5): the leaf runs in two steps through a single
    /// handler instance (prep then accept/abort), state surviving between.
    pub confirm: bool,
}

impl MenuNode {
    /// A plain static child.
    pub fn new(key: impl Into<String>) -> Self {
        MenuNode {
            key: key.into(),
            ..Default::default()
        }
    }

    pub fn desc(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }

    pub fn child_header(mut self, h: impl Into<String>) -> Self {
        self.child_header = Some(h.into());
        self
    }

    pub fn children(mut self, c: Vec<MenuNode>) -> Self {
        self.children = c;
        self
    }

    /// Leaf node (commits on Enter).
    pub fn leaf(mut self) -> Self {
        self.is_leaf = true;
        self
    }

    /// Dynamic level: rows come from a registered DataProvider (§8).
    pub fn dynamic(mut self) -> Self {
        self.dynamic = true;
        self
    }

    /// Interstitial text (D4).
    pub fn interstitial(mut self, text: impl Into<String>) -> Self {
        self.interstitial = Some(text.into());
        self
    }

    /// Confirm flow (D5): two-step leaf through one handler instance.
    pub fn confirm(mut self) -> Self {
        self.confirm = true;
        self
    }

    /// Node carrying a value-typed child (§2).
    pub fn value(mut self, v: ValueChild) -> Self {
        self.value = Some(v);
        self
    }

    /// Find a static child by exact key.
    pub fn child(&self, key: &str) -> Option<&MenuNode> {
        self.children.iter().find(|c| c.key == key)
    }
}
