use std::sync::Weak;

use crate::unit::pool::UnitPool;

pub struct UnitContext {
    ptr: Weak<UnitPool>,
}
