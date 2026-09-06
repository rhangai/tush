#![allow(dead_code)]

mod base;
mod unit;
mod util;

use std::num::NonZeroUsize;

use tokio::task::JoinSet;

use crate::{
    base::LogWriter,
    unit::{Unit, UnitDefiniton, UnitGroup, UnitProcess},
};

#[tokio::main]
async fn main() {
    let mut unit = UnitGroup::new();
    unit.add(UnitProcess::new());
    unit.add(UnitProcess::new());
    unit.add(UnitProcess::new());

    let mut set = JoinSet::new();
    let writer = LogWriter::new(NonZeroUsize::new(1024).unwrap());
    let log = writer.log();
    unit.exec_in_set(&mut set, writer);

    _ = set.join_all().await;
    for line in log.iter() {
        println!("{}", &line);
    }
}
