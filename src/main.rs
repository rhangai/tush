#![allow(dead_code)]

mod base;
mod process;
mod util;

use crate::process::ProcessPool;

#[tokio::main]
async fn main() {
    let mut pool = ProcessPool::new(2048);
    pool.add("oi", ["bash", "-c", "echo 'oi'; sleep 1; echo 'tchau'"]);
    pool.add("tchau", ["bash", "-c", "echo 'oi'; sleep 1; echo 'tchau'"]);

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
