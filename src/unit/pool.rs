use std::sync::{Arc, Weak};

/// The collection of every [`Unit`](crate::unit::Unit) in a session.
///
/// Not implemented yet. It is meant to own the units, resolve them by name,
/// and enforce the relationships sketched in `tmp/example.yaml`: groups and
/// `pre-condition` dependencies that must finish before a unit may start.
///
/// The pool holds a [`Weak`] pointer to itself (hence
/// [`Arc::new_cyclic`]) so that the
/// `UnitContext` handed to each unit can reach the pool back without
/// creating a reference cycle that would leak it.
pub struct UnitPool {
    ptr: Weak<UnitPool>,
}

impl UnitPool {
    /// Create an empty pool, self referencing through a weak pointer.
    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|ptr| Self { ptr: ptr.clone() })
    }
}
