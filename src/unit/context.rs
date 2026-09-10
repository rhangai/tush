use std::sync::Weak;

use crate::unit::pool::UnitPool;

/// The handle a unit uses to talk back to the pool it belongs to.
///
/// Not implemented yet. It will be how a unit resolves its `pre-condition`
/// dependencies and reacts to sibling state changes without owning the pool —
/// hence the [`Weak`] pointer, which keeps the units from keeping the pool
/// alive.
pub struct UnitContext {
    ptr: Weak<UnitPool>,
}
