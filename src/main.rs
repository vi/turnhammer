#![allow(unused)]


extern crate rusturn;
extern crate rustun;
extern crate structopt;
extern crate futures;
extern crate tokio;


use std::net::SocketAddr;
use structopt::StructOpt;


use futures::{Async, Future, Poll};
use rusturn::auth::AuthParams;
use rusturn::client::{wait, Client, UdpClient};
use rusturn::Error;

#[derive(Debug, StructOpt)]
struct Opt {
    /// STUN server address.
    #[structopt(long = "server", default_value = "127.0.0.1:3478")]
    server: SocketAddr,

    /// Username.
    #[structopt(long = "username", default_value = "foo")]
    username: String,

    /// Password.
    #[structopt(long = "password", default_value = "bar")]
    password: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opt = Opt::from_args();

    let mut rt = tokio::runtime::current_thread::Runtime::new()?;

    let auth_params = AuthParams::new(&opt.username, &opt.password)?;

    let client = UdpClient::allocate(
        opt.server,
        auth_params
    );

    let client = rt.block_on(client)?;

    eprintln!("{:?}", client.relay_addr());

    Ok(())
}
