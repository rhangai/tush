mod base;
mod util;

#[tokio::main]
async fn main() {
    let log = base::log::Log {};
    println!("Hello, world!");
}
