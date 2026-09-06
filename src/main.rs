#![allow(dead_code)]

mod base;
mod process;
mod util;
use tokio::time::{Duration, sleep};

use crate::process::Process;

#[tokio::main]
async fn main() {
    let mut proc = Process::new(1024);
    proc.start();
    proc.start();
    proc.start();
    proc.start();
    proc.start();
    proc.start();
    proc.start();
    proc.start();
    proc.start();
    sleep(Duration::from_millis(2000)).await;
    proc.stop();
    proc.start();
    sleep(Duration::from_millis(100)).await;
    for line in proc.lines_sync() {
        println!("{}", line);
    }
}
