#![recursion_limit = "1024"]
#![allow(dead_code,unused_variables,unused_imports)]
#[macro_use]
extern crate error_chain;

mod errors {
    error_chain! {}
}




use std::{env, thread};
use log::{info, warn};

use std::sync::mpsc::channel;
use tokio::runtime::Handle;
use tokio::task;
use crate::internals::network::{Client, Network};

pub mod internals;

#[tokio::main]
async fn main() {
    env::set_var("RUST_LOG", "info");
    env_logger::init();
    info!("Hello, world!");
    let handle = Handle::current();
    let mut net = Network::new(handle.clone());
    let (sender, receiver) = channel::<Client>();
    use::tokio::task;
    let task_handle = task::block_in_place(||{async move { net.like_main().await    }});
    task_handle.await;
    //let broadcast_handler = thread::spawn(move|| {
    //});
    //net.broadcast().await;
}
