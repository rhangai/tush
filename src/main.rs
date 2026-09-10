#![allow(dead_code)]

mod base;
mod runner;
mod unit;
mod util;

use crate::unit::{Unit, UnitDescription};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let unit = Unit::new(UnitDescription::program());
    let h1 = unit.start()?;
    let h2 = unit.start_with_description(UnitDescription::noop())?;
    println!("{:?}", h1.state());
    println!("{:?}", h2.state());
    h2.wait().await;
    let h3 = unit.start_with_description(UnitDescription::noop())?;
    println!("{:?}", h1.state());
    println!("{:?}", h2.state());
    println!("{:?}", h3.state());

    Ok(())

    // pool.use_lines("tchau", |lines| {
    //     for line in lines {
    //         println!("{}", line);
    //     }
    // });
}
