//! ADR-0163 D2: the container ban's plants. Each `// PLANT` line must draw
//! `disallowed_types` naming the configured path; `xN` asks for N distinct
//! spans on that one line, the census's unit (file, line, column). Each
//! `// CONTROL` line must draw nothing. Compile only; nothing here runs.
extern crate alloc;

// The three paths, each by its plainest spelling.
pub struct HoldsMap {
    pub value: std::collections::HashMap<u64, u64>, // PLANT clippy::disallowed_types std::collections::HashMap
}

pub struct HoldsSet {
    pub value: std::collections::HashSet<u64>, // PLANT clippy::disallowed_types std::collections::HashSet
}

pub struct HoldsDeque {
    pub value: std::collections::VecDeque<u64>, // PLANT clippy::disallowed_types std::collections::VecDeque
}

// The defining module's path and the `alloc` path name the same types.
pub fn module_path() -> usize {
    let map: std::collections::hash_map::HashMap<u8, u8> = Default::default(); // PLANT clippy::disallowed_types std::collections::HashMap
    map.capacity()
}

pub fn alloc_path() -> usize {
    let deque: alloc::collections::VecDeque<u8> = Default::default(); // PLANT clippy::disallowed_types std::collections::VecDeque
    deque.capacity()
}

// A spelled constructor in an unannotated `let` is a type path.
pub fn unannotated() -> usize {
    use std::collections::VecDeque; // PLANT clippy::disallowed_types std::collections::VecDeque
    let mut deque = VecDeque::new(); // PLANT clippy::disallowed_types std::collections::VecDeque
    deque.push_back(1u8);
    deque.capacity()
}

// An alias draws at its definition and never at its uses, which is why
// the gate refuses an allow on a `type` item and the tree inlines its four.
pub type Alias = std::collections::HashMap<u8, u8>; // PLANT clippy::disallowed_types std::collections::HashMap

pub fn alias_use(map: &Alias) -> usize { // CONTROL
    map.capacity()
}

// Two containers on one line are two spans: the census key has a column.
pub fn two_on_one_line() -> usize {
    let pair: (std::collections::VecDeque<u8>, std::collections::VecDeque<u8>) = Default::default(); // PLANT clippy::disallowed_types std::collections::VecDeque x2
    pair.0.capacity().saturating_add(pair.1.capacity())
}

// The item key: two methods of one name under two impl types are two
// items, `fn KeyA::build` and `fn KeyB::build`; each draws on its own line.
pub struct KeyA;
pub struct KeyB;

impl KeyA {
    pub fn build() -> usize {
        let map: std::collections::HashMap<u8, u8> = Default::default(); // PLANT clippy::disallowed_types std::collections::HashMap
        map.capacity()
    }
}

impl KeyB {
    pub fn build() -> usize {
        let set: std::collections::HashSet<u8> = Default::default(); // PLANT clippy::disallowed_types std::collections::HashSet
        set.capacity()
    }
}

// The one-allow backing (ADR-0151 D6): the struct carries the only allow;
// its constructor builds the field by inference and its methods name no
// std type, so none of them needs a second allow.
#[allow(clippy::disallowed_types, reason = "container: capped-backing")]
pub struct Backing<T> {
    inner: std::collections::VecDeque<T>, // CONTROL
}

impl<T> Backing<T> {
    pub fn with_room(entries: usize) -> Result<Self, std::collections::TryReserveError> { // CONTROL
        let mut backing = Self { inner: Default::default() }; // CONTROL
        backing.inner.try_reserve_exact(entries)?; // CONTROL
        Ok(backing)
    }

    pub fn publish(&mut self, value: T) {
        self.inner.push_back(value); // CONTROL
    }

    pub fn take_front(&mut self) -> Option<T> {
        self.inner.pop_front() // CONTROL
    }

    pub fn entries(&self) -> impl Iterator<Item = &T> {
        self.inner.iter() // CONTROL
    }
}
