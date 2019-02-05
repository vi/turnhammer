#![allow(unused)]

extern crate bytecodec;
extern crate fibers;
extern crate fibers_transport;
extern crate futures;
extern crate rand;
extern crate rustun;
extern crate rusturn;
extern crate structopt;
extern crate stunclient;
extern crate tokio;

use std::net::SocketAddr;
use structopt::StructOpt;

use fibers::{Executor, Spawn};
use fibers_transport::UdpTransporter;
use futures::{Async, Future, Poll};
use rusturn::auth::AuthParams;
use rusturn::client::{wait, Client as TurnClient, UdpClient as TurnUdpClient};
use rusturn::Error;

use rustun::channel::Channel;
use rustun::client::Client as StunClient;
use rustun::message::Request;
use rustun::transport::StunUdpTransporter;
use std::net::ToSocketAddrs;
use stun_codec::rfc5389;
use stun_codec::{MessageDecoder, MessageEncoder};

use bytecodec::{DecodeExt, EncodeExt};
use stun_codec::rfc5389::attributes::{
    MappedAddress, Software, XorMappedAddress, XorMappedAddress2,
};
use stun_codec::rfc5389::{methods::BINDING, Attribute};
use stun_codec::{Message, MessageClass, TransactionId};

#[derive(Debug, StructOpt)]
struct Opt {
    /// TURN server address.
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

    let local_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let udp = std::net::UdpSocket::bind(local_addr)?;

    let extaddr = stunclient::StunClient::new(opt.server)
        .set_software(Some("TURN_hammer"))
        .query_external_address(&udp)?;
    eprintln!("External address: {}", extaddr);

    //let mut rt = tokio::runtime::current_thread::Runtime::new()?;
    /*
    let mut rt = fibers::executor::InPlaceExecutor::new()?;
    let h = rt.handle();

    let auth_params = AuthParams::new(&opt.username, &opt.password)?;

    let local_addr = "0.0.0.0:0".parse().unwrap();
    let response = UdpTransporter::<MessageEncoder<_>, MessageDecoder<_>>::bind(local_addr)
        .map_err(Error::from)
        .map(StunUdpTransporter::new)
        .map(Channel::new)
        .and_then(move |channel| {
            let h = h;
            let client = StunClient::new(&h, channel);
            let request = Request::<rfc5389::Attribute>::new(rfc5389::methods::BINDING);
            client.call(opt.server, request)
        });
    let monitor = rt.spawn_monitor(response);
    let response = rt.run_fiber(monitor)??.map_err(|e|format!("{:?}",e))?;
    let addr : &rfc5389::attributes::XorMappedAddress = response.get_attribute().ok_or_else(||format!("No XorMappedAddr?"))?;
    let addr = addr.address();
    eprintln!("STUN response: {:?}", addr);
    */

    /*
    let client = TurnUdpClient::allocate(
        opt.server,
        auth_params
    );
    let mut monitor = rt.spawn_monitor(client);
    let client = rt.run_fiber(monitor)??;

    eprintln!("{:?}", client.relay_addr());
    */

    Ok(())
}
