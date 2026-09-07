#![allow(dead_code)]

mod base;
mod process;
mod util;

use tokio::time::{Duration, sleep};

use crate::process::{Process, ProcessPool};

#[tokio::main]
async fn main() {
    let mut pool = ProcessPool::new();
    pool.add("oi".into(), Process::new(1024));
    pool.add("tchau".into(), Process::new(1024));

    println!("{:?}", pool.state("oi"));
    println!("{:?}", pool.state("tchau"));
    println!("{:?}", pool.state("kill"));

    pool.start("oi");
    pool.start("tchau");

    println!("{:?}", pool.state("oi"));
    println!("{:?}", pool.state("tchau"));
    println!("{:?}", pool.state("kill"));

    pool.wait("oi").await;
    println!("{:?}", pool.state("oi"));

    pool.wait("tchau").await;
    println!("{:?}", pool.state("tchau"));

    pool.wait("kill").await;
    println!("{:?}", pool.state("kill"));

    pool.use_lines("tchau", |lines| {
        for line in lines {
            println!("{}", line);
        }
    });
}
