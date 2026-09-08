#![allow(dead_code)]

mod base;
mod unit;
mod util;

use std::time::Duration;

use tokio::time::sleep;

use crate::unit::{Unit, UnitProcess};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let unit = Unit::new(UnitProcess::new([
        "bash",
        "-c",
        "echo 'oi'; sleep 1; echo 'tchau'; exit 1",
    ]));
    _ = unit.start();
    println!("{:?}", unit.state());
    sleep(Duration::from_secs(2)).await;
    println!("{:?}", unit.state());
    _ = unit.start();
    println!("{:?}", unit.state());
    sleep(Duration::from_secs(2)).await;
    println!("{:?}", unit.state());

    let mut h = unit.spawn(None)?;
    println!("{:?}", h.state());
    h.wait().await;
    println!("{:?}", h.state());

    Ok(())

    // pool.use_lines("tchau", |lines| {
    //     for line in lines {
    //         println!("{}", line);
    //     }
    // });
}
