#![allow(dead_code)]

mod base;
mod process;
mod util;

use crate::process::ProcessPool;

#[tokio::main]
async fn main() {
    let mut pool = ProcessPool::new(2048);
    pool.add(
        "oi",
        ["bash", "-c", "echo 'oi'; sleep 1; echo 'tchau'; exit 1;"],
    );
    pool.add("tchau", ["bash", "-c", "echo 'oi'; sleep 2; echo 'tchau'"]);

    println!("{:?}", pool.state("oi"));
    println!("{:?}", pool.state("tchau"));

    pool.start("oi");
    pool.start("tchau");

    println!("{:?}", pool.state("oi"));
    println!("{:?}", pool.state("tchau"));

    pool.wait("oi").await;
    println!("{:?}", pool.state("oi"));

    pool.stop("tchau");
    pool.wait("tchau").await;
    println!("{:?}", pool.state("tchau"));

    // pool.use_lines("tchau", |lines| {
    //     for line in lines {
    //         println!("{}", line);
    //     }
    // });
}
