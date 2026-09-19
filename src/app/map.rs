use std::collections::HashSet;

use string_interner::{DefaultStringInterner, DefaultSymbol};

use crate::util::graph::DependencyGraph;

pub struct AppUnitMap {
    dependency_graph: DependencyGraph<DefaultSymbol>,
    pending: HashSet<DefaultSymbol>,
    interner: DefaultStringInterner,
}

impl AppUnitMap {
    fn new() -> Self {
        let interner = DefaultStringInterner::new();
        Self {
            dependency_graph: DependencyGraph::new(),
            pending: HashSet::new(),
            interner,
        }
    }
}
