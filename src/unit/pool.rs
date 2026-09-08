use std::sync::{Arc, Weak};

pub struct UnitPool {
    ptr: Weak<UnitPool>,
}

impl UnitPool {
    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|ptr| Self { ptr: ptr.clone() })
    }
}
