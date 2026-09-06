#![allow(dead_code)]

mod base;
mod unit;
mod util;

use std::num::NonZeroUsize;

use tokio::task::JoinSet;

use crate::{
    base::{Log, LogWriterRef},
    unit::{Unit, UnitDefiniton, UnitGroup, UnitProcess},
};

#[tokio::main]
async fn main() {
    let mut unit = UnitGroup::new();
    unit.add(UnitProcess::new());
    unit.add(UnitProcess::new());
    unit.add(UnitProcess::new());

    let mut set = JoinSet::new();
    let log = Log::new(1024);
    let writer = log.writer();
    unit.exec_in_set(&mut set, writer);

    _ = set.join_all().await;

    let buffer = log.new_buffer();
    for line in buffer.lines() {
        println!("{}", &line);
    }
}
