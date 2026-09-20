//! A dependency graph that answers with an order, and with what is wrong.
//!
//! [`DependencyGraph`] holds nodes named by a key and the edges "this one
//! depends on that one". What it is for is the question asked once, when the
//! units are configured: in what order may these be started?
//!
//! # A bad configuration is an answer, not a failure
//!
//! A plain topological sort has two outcomes: an order, or nothing at all
//! because there is a cycle somewhere in the input. That is the wrong trade
//! here. A cycle between two units is a typo in a config file — the user is
//! going to fix it, and in the meantime the session still has to come up,
//! with the other units ordered properly and the offending ones named.
//!
//! So [`resolve`](DependencyGraph::resolve) never fails. It always returns an
//! order covering every node, and alongside it the cycles it had to break to
//! produce one. A caller that cares can refuse to start, or warn; a caller
//! that does not gets a usable order either way.
//!
//! # How both come out of one pass
//!
//! The strongly connected components of a graph are exactly its cycles: a
//! component of more than one node is a set that can all reach each other,
//! which is what a cycle is, and everything else is a component of one. The
//! condensation — the graph of components — is by construction acyclic, so
//! the components *do* have a topological order even when the nodes do not.
//!
//! That is the whole implementation. [`tarjan_scc`] hands back the components
//! already in that order, so the grouping and the ordering are the same piece
//! of work, and the cycles are not a separate search but a filter over the
//! result: the groups of more than one, plus any node that depends on itself.
//!
//! # Starting from the middle
//!
//! [`resolve_from`](DependencyGraph::resolve_from) answers the same question
//! for one node rather than all of them: what has to be up, and in what
//! order, for *this* to run. Walking the edges forwards from it reaches
//! exactly its dependencies — the rest of the graph is not ordered, and not
//! looked at — and the cycles reported are the ones actually in the way.
//!
//! # Determinism
//!
//! Nodes are numbered in the order they are first mentioned, and the result
//! is ordered by those numbers wherever the graph leaves a choice — between
//! independent nodes, and between the members of one cycle. Same config in,
//! same order out, which is what makes the output worth showing a user.

#![allow(dead_code)]

use std::{collections::HashMap, hash::Hash};

use petgraph::{
    algo::tarjan_scc,
    graph::DiGraph,
    prelude::NodeIndex,
    visit::{Dfs, NodeFiltered},
};

/// Nodes keyed by `K`, and the "depends on" edges between them.
///
/// Built once from a configuration and then asked
/// [`resolve`](DependencyGraph::resolve); there is no removal, because a
/// graph that is wrong is rebuilt from the config that was wrong, not patched
/// in place.
pub struct DependencyGraph<K> {
    /// The edges. Node weights are the keys, which is what lets the result be
    /// reported in the caller's own names rather than in indices.
    graph: DiGraph<K, ()>,
    /// Key to node, so that naming the same key twice means the same node.
    index: HashMap<K, NodeIndex>,
}

impl<K> DependencyGraph<K> {
    /// An empty graph.
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            index: HashMap::new(),
        }
    }
}

impl<K> DependencyGraph<K>
where
    K: Eq + Hash + Clone,
{
    /// Add a node with no dependencies, if it is not already there.
    ///
    /// Needed because a node that nothing mentions would otherwise never
    /// appear: a unit with no dependencies and no dependents still has to
    /// come out of the order.
    pub fn insert(&mut self, key: K) {
        self.node(key);
    }

    /// Record that `key` may only start once `depends_on` has.
    ///
    /// Both ends are inserted if they are new, so a caller reading a config
    /// need not insert first. Declaring the same pair twice changes nothing —
    /// the edge is updated, not duplicated.
    ///
    /// A key that depends on itself is allowed, and comes back as a cycle of
    /// one; it is a configuration mistake like any other, and the rule here
    /// is to report those rather than to reject them.
    pub fn add_dependency(&mut self, key: K, depends_on: K) {
        let from = self.node(key);
        let to = self.node(depends_on);
        self.graph.update_edge(from, to, ());
    }

    /// Whether the key names a node in the graph.
    pub fn contains(&self, key: &K) -> bool {
        self.index.contains_key(key)
    }

    /// What `key` directly depends on — one edge out, no walking.
    ///
    /// For the transitive answer, and in an order, use
    /// [`resolve_from`](DependencyGraph::resolve_from). A key that names no
    /// node yields nothing, like everything else here that takes a key.
    ///
    /// Comes back in the reverse of the order the dependencies were declared
    /// in, which is the order petgraph keeps its edge list in; reverse it if
    /// the declaration order is what a user will see.
    pub fn dependencies(&self, key: &K) -> impl Iterator<Item = &K> {
        self.index
            .get(key)
            .into_iter()
            .flat_map(|&node| self.graph.neighbors(node))
            .map(|node| &self.graph[node])
    }

    /// How many nodes there are.
    pub fn len(&self) -> usize {
        self.graph.node_count()
    }

    /// Whether there is nothing in the graph.
    pub fn is_empty(&self) -> bool {
        self.graph.node_count() == 0
    }

    /// Order every node so that dependencies come first, and report the
    /// cycles that stood in the way.
    ///
    /// Never fails. See the module docs for what the result means when the
    /// graph is not acyclic.
    pub fn resolve(&self) -> DependencyOrder<K> {
        self.group(tarjan_scc(&self.graph))
    }

    /// The same, for `key` alone: it, and everything it depends on.
    ///
    /// This is how a unit in the middle of the graph is started without
    /// starting the whole session — the order ends with `key` itself, after
    /// whatever it needed on the way up. Nothing that merely *depends on*
    /// `key` is in it; those are not needed for `key` to run.
    ///
    /// A key that names no node comes back as an empty order rather than as
    /// an error. Nothing is lost by that: a key that *is* in the graph always
    /// brings back at least itself, so
    /// [`is_empty`](DependencyOrder::is_empty) is exactly the question "was
    /// that a name I know?" — and the caller who does not care to ask gets
    /// the same "start nothing" that a hard error would have led to anyway.
    pub fn resolve_from(&self, key: &K) -> DependencyOrder<K> {
        self.resolve_from_many([key])
    }

    /// [`resolve_from`](DependencyGraph::resolve_from) for several keys at
    /// once — a whole group being started, say.
    ///
    /// One order covering all of them, so a dependency two of them share is
    /// in it once, in a position that satisfies both. Keys that name no node
    /// are skipped; use [`contains`](DependencyGraph::contains) first if that
    /// should be an error.
    pub fn resolve_from_many<'a, I>(&self, keys: I) -> DependencyOrder<K>
    where
        I: IntoIterator<Item = &'a K>,
        K: 'a,
    {
        // Walking the edges forwards from a node reaches exactly what it
        // depends on, since that is the direction they point. The visit map
        // carries across the roots, so a dependency several of them share is
        // walked once and the whole thing stays linear.
        let mut dfs = Dfs::empty(&self.graph);
        for key in keys {
            let Some(&node) = self.index.get(key) else {
                continue;
            };
            dfs.move_to(node);
            while dfs.next(&self.graph).is_some() {}
        }

        // What the DFS marked is the subgraph to order; the ordering itself
        // is the same work as for the whole graph, on less of it.
        let reachable = NodeFiltered(&self.graph, dfs.discovered);
        self.group(tarjan_scc(&reachable))
    }

    /// Turn components, already in dependency order, into the result.
    ///
    /// Shared by the whole graph and a part of it, which differ only in what
    /// is fed to [`tarjan_scc`].
    fn group(&self, components: Vec<Vec<NodeIndex>>) -> DependencyOrder<K> {
        let groups = components
            .into_iter()
            .map(|mut nodes| {
                // Tarjan fixes the order *of* the components but not the
                // order within one; sorting is what makes a cycle's members
                // come out in the order they were first mentioned.
                nodes.sort_unstable();
                DependencyGroup {
                    // A group of one is a cycle only if it closes on itself;
                    // anything larger is a cycle by definition.
                    cycle: nodes.len() > 1 || self.graph.contains_edge(nodes[0], nodes[0]),
                    nodes: nodes
                        .into_iter()
                        .map(|node| self.graph[node].clone())
                        .collect(),
                }
            })
            .collect();
        DependencyOrder { groups }
    }

    /// The node for `key`, created on first mention.
    fn node(&mut self, key: K) -> NodeIndex {
        if let Some(&node) = self.index.get(&key) {
            return node;
        }
        let node = self.graph.add_node(key.clone());
        self.index.insert(key, node);
        node
    }
}

impl<K> Default for DependencyGraph<K> {
    fn default() -> Self {
        Self::new()
    }
}

/// What a [`DependencyGraph`] resolved to: every node, in an order, and the
/// cycles among them.
///
/// Both views are the same storage read two ways — the nodes grouped by
/// strongly connected component. [`order`](DependencyOrder::order) walks the
/// groups flat, [`cycles`](DependencyOrder::cycles) keeps only the groups
/// that are one.
pub struct DependencyOrder<K> {
    /// One entry per component, dependencies before dependents.
    groups: Vec<DependencyGroup<K>>,
}

impl<K> DependencyOrder<K> {
    /// Every node, dependencies before dependents.
    ///
    /// Total: a node caught in a cycle is still in here, since the point is
    /// that a broken configuration still yields something startable. Where
    /// the cycle put the members in a relative order that cannot be honoured,
    /// they come out adjacent, in the order they were first mentioned.
    pub fn order(&self) -> impl Iterator<Item = &K> {
        self.groups.iter().flat_map(|group| group.nodes.iter())
    }

    /// The cycles, each as the set of nodes that close it.
    ///
    /// Empty when the graph is acyclic, which is the only case in which
    /// [`order`](DependencyOrder::order) is a true topological order.
    pub fn cycles(&self) -> impl Iterator<Item = &[K]> {
        self.groups
            .iter()
            .filter(|group| group.cycle)
            .map(|group| group.nodes.as_slice())
    }

    /// Whether there is nothing to start.
    ///
    /// Only ever true for a resolve that started from keys, and then only
    /// when none of them named a node — every node in the graph is in the
    /// order of the whole graph, and a key that is in the graph is in its
    /// own.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Whether anything depends, directly or transitively, on itself.
    pub fn has_cycles(&self) -> bool {
        self.groups.iter().any(|group| group.cycle)
    }

    /// The order in its grouped form: each step is a set of nodes that may
    /// start together, because nothing inside it depends on anything else
    /// inside it — except in a cycle, where nothing better is available.
    pub fn groups(&self) -> &[DependencyGroup<K>] {
        &self.groups
    }
}

/// One step of a [`DependencyOrder`]: a single node, or the several that are
/// tangled together in a cycle.
pub struct DependencyGroup<K> {
    /// The members, in the order they were first mentioned.
    nodes: Vec<K>,
    /// Whether these depend on each other in a circle. False for the ordinary
    /// case of one node that does not depend on itself.
    cycle: bool,
}

impl<K> DependencyGroup<K> {
    /// The nodes of this step.
    pub fn nodes(&self) -> &[K] {
        &self.nodes
    }

    /// Whether this step is a cycle, and so has no internal order to respect.
    pub fn is_cycle(&self) -> bool {
        self.cycle
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Build a graph from `(node, dependencies)` pairs, in the order given —
    /// which is the order the result is expected to fall back on.
    fn graph(spec: &[(&'static str, &[&'static str])]) -> DependencyGraph<&'static str> {
        let mut graph = DependencyGraph::new();
        for (key, dependencies) in spec {
            graph.insert(*key);
            for dependency in *dependencies {
                graph.add_dependency(*key, *dependency);
            }
        }
        graph
    }

    fn order(spec: &[(&'static str, &[&'static str])]) -> Vec<&'static str> {
        graph(spec).resolve().order().copied().collect()
    }

    fn cycles(spec: &[(&'static str, &[&'static str])]) -> Vec<Vec<&'static str>> {
        graph(spec)
            .resolve()
            .cycles()
            .map(<[&str]>::to_vec)
            .collect()
    }

    #[test]
    fn a_dependency_comes_before_what_depends_on_it() {
        assert_eq!(
            order(&[("server", &["server-setup"]), ("server-setup", &[])]),
            ["server-setup", "server"]
        );
    }

    #[test]
    fn a_chain_comes_out_end_to_end() {
        assert_eq!(
            order(&[("a", &["b"]), ("b", &["c"]), ("c", &[])]),
            ["c", "b", "a"]
        );
    }

    /// Nothing relates these, so the only thing left to order them by is the
    /// order they were written in.
    #[test]
    fn independent_nodes_keep_the_order_they_were_added_in() {
        assert_eq!(
            order(&[("a", &[]), ("b", &[]), ("c", &[])]),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn a_node_nothing_mentions_is_still_in_the_order() {
        assert_eq!(
            order(&[("a", &["b"]), ("lonely", &[])]),
            ["b", "a", "lonely"]
        );
    }

    #[test]
    fn declaring_the_same_dependency_twice_changes_nothing() {
        let mut graph = DependencyGraph::new();
        graph.add_dependency("a", "b");
        graph.add_dependency("a", "b");
        assert_eq!(graph.len(), 2);
        assert_eq!(
            graph.resolve().order().copied().collect::<Vec<_>>(),
            ["b", "a"]
        );
    }

    #[test]
    fn an_acyclic_graph_reports_no_cycles() {
        assert!(!graph(&[("a", &["b"]), ("b", &[])]).resolve().has_cycles());
    }

    /// The point of the whole thing: a cycle is reported, and the order still
    /// covers every node instead of coming back empty.
    #[test]
    fn a_cycle_is_reported_and_the_order_survives_it() {
        let resolved = graph(&[("a", &["b"]), ("b", &["a"]), ("c", &[])]).resolve();
        assert!(resolved.has_cycles());
        assert_eq!(resolved.cycles().collect::<Vec<_>>(), [["a", "b"]]);

        let mut order = resolved.order().copied().collect::<Vec<_>>();
        order.sort_unstable();
        assert_eq!(order, ["a", "b", "c"]);
    }

    #[test]
    fn what_a_cycle_depends_on_still_comes_before_it() {
        assert_eq!(
            order(&[("a", &["b", "dep"]), ("b", &["a"]), ("dep", &[])]),
            ["dep", "a", "b"]
        );
    }

    #[test]
    fn depending_on_yourself_is_a_cycle_of_one() {
        assert_eq!(cycles(&[("a", &["a"])]), [["a"]]);
    }

    #[test]
    fn a_node_on_its_own_is_not_a_cycle() {
        assert!(cycles(&[("a", &[])]).is_empty());
    }

    #[test]
    fn separate_cycles_are_reported_separately() {
        assert_eq!(
            cycles(&[
                ("a", &["b"]),
                ("b", &["a"]),
                ("x", &["y"]),
                ("y", &["x"]),
                ("free", &[]),
            ]),
            [["a", "b"], ["x", "y"]]
        );
    }

    #[test]
    fn an_empty_graph_resolves_to_nothing() {
        let graph = DependencyGraph::<&str>::new();
        assert!(graph.is_empty());
        let resolved = graph.resolve();
        assert_eq!(resolved.order().count(), 0);
        assert!(!resolved.has_cycles());
    }

    #[test]
    fn direct_dependencies_are_one_edge_out_and_no_further() {
        let spec: &[(&str, &[&str])] = &[
            ("a", &["b", "c"]),
            ("b", &["deep"]),
            ("c", &[]),
            ("deep", &[]),
        ];
        let graph = graph(spec);
        let mut direct = graph.dependencies(&"a").copied().collect::<Vec<_>>();
        direct.sort_unstable();
        assert_eq!(direct, ["b", "c"]);
        assert_eq!(graph.dependencies(&"c").count(), 0);
    }

    /// Documented as reverse declaration order, so it is worth pinning: the
    /// doc is what a caller will reverse against.
    #[test]
    fn direct_dependencies_come_back_in_reverse_declaration_order() {
        assert_eq!(
            graph(&[("a", &["x", "y", "z"])])
                .dependencies(&"a")
                .copied()
                .collect::<Vec<_>>(),
            ["z", "y", "x"]
        );
    }

    /// Dependents are not dependencies — the edge points the other way.
    #[test]
    fn what_depends_on_the_key_is_not_a_dependency_of_it() {
        assert_eq!(
            graph(&[("a", &["b"]), ("b", &[])])
                .dependencies(&"b")
                .count(),
            0
        );
    }

    #[test]
    fn the_dependencies_of_an_unknown_key_are_nothing() {
        assert_eq!(graph(&[("a", &[])]).dependencies(&"nope").count(), 0);
    }

    fn order_from(
        spec: &[(&'static str, &[&'static str])],
        key: &'static str,
    ) -> Vec<&'static str> {
        graph(spec).resolve_from(&key).order().copied().collect()
    }

    /// The point of resolving from a node: what it needs, and then it, with
    /// the rest of the session left alone.
    #[test]
    fn resolving_from_a_node_takes_its_dependencies_and_stops() {
        let spec: &[(&str, &[&str])] = &[
            ("server", &["server-setup"]),
            ("server-setup", &[]),
            ("site", &["site-setup"]),
            ("site-setup", &[]),
        ];
        assert_eq!(order_from(spec, "server"), ["server-setup", "server"]);
    }

    #[test]
    fn what_depends_on_the_node_is_not_dragged_in() {
        let spec: &[(&str, &[&str])] = &[("a", &["b"]), ("b", &["c"]), ("c", &[])];
        assert_eq!(order_from(spec, "b"), ["c", "b"]);
    }

    #[test]
    fn a_node_with_no_dependencies_resolves_to_itself() {
        assert_eq!(order_from(&[("a", &[]), ("b", &["a"])], "a"), ["a"]);
    }

    /// A name nobody declared is not an error here — it starts nothing, and
    /// the empty order says so.
    #[test]
    fn an_unknown_key_resolves_to_nothing_at_all() {
        assert!(graph(&[("a", &[])]).resolve_from(&"nope").is_empty());
    }

    /// Which is what makes the empty order worth testing against: a key that
    /// is there never comes back empty, so the two cases stay apart.
    #[test]
    fn a_key_that_is_there_always_brings_back_at_least_itself() {
        assert!(!graph(&[("a", &[])]).resolve_from(&"a").is_empty());
    }

    /// A cycle upstream is still reported — it is in the way of this node
    /// even though it is not this node.
    #[test]
    fn a_cycle_among_the_dependencies_is_reported() {
        let spec: &[(&str, &[&str])] = &[("top", &["a"]), ("a", &["b"]), ("b", &["a"])];
        let resolved = graph(spec).resolve_from(&"top");
        assert_eq!(resolved.cycles().collect::<Vec<_>>(), [["a", "b"]]);
        assert_eq!(resolved.order().count(), 3);
    }

    /// A cycle somewhere else in the graph is not this node's problem.
    #[test]
    fn a_cycle_out_of_reach_is_left_out() {
        let spec: &[(&str, &[&str])] =
            &[("a", &["dep"]), ("dep", &[]), ("x", &["y"]), ("y", &["x"])];
        let resolved = graph(spec).resolve_from(&"a");
        assert!(!resolved.has_cycles());
        assert_eq!(resolved.order().copied().collect::<Vec<_>>(), ["dep", "a"]);
    }

    /// Several roots give one order, and the dependency they share appears in
    /// it once, before both.
    #[test]
    fn several_roots_share_one_order() {
        let spec: &[(&str, &[&str])] = &[
            ("server", &["setup"]),
            ("site", &["setup"]),
            ("setup", &[]),
            ("unrelated", &[]),
        ];
        assert_eq!(
            graph(spec)
                .resolve_from_many([&"server", &"site"])
                .order()
                .copied()
                .collect::<Vec<_>>(),
            ["setup", "server", "site"]
        );
    }

    #[test]
    fn resolving_from_no_roots_gives_an_empty_order() {
        let graph = graph(&[("a", &[])]);
        assert!(graph.resolve_from_many([]).is_empty());
    }
}
