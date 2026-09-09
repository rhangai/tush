#![allow(dead_code)]

mod base;
mod runner;
mod unit;
mod util;

use std::time::Duration;

use tokio::{process::Command, time::sleep};

use crate::{
    base::Process,
    runner::RunnerHandle,
    unit::{Unit, UnitDescription},
};

struct ProcessDesc {}

impl UnitDescription for ProcessDesc {
    fn spawn(
        &self,
        writer: Option<base::LogWriterRef>,
    ) -> anyhow::Result<std::sync::Arc<runner::RunnerHandle>> {
        let mut command = Command::new("bash");
        command.args(["-c", "echo 'oi'; sleep 1; echo 'tchau'; exit 2"]);
        let proc = Process::new(command, writer);
        Ok(RunnerHandle::new(proc))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let unit = Unit::new();
    let desc = ProcessDesc {};
    let h1 = unit.start(&desc)?;
    let h2 = unit.start(&desc)?;
    println!("{:?}", h1.state());
    println!("{:?}", h2.state());
    h2.wait().await;
    println!("{:?}", h1.state());
    println!("{:?}", h2.state());

    Ok(())

    // pool.use_lines("tchau", |lines| {
    //     for line in lines {
    //         println!("{}", line);
    //     }
    // });
}
