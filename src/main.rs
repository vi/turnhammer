#![allow(unused)]

extern crate bytecodec;
extern crate futures;
extern crate rand;
extern crate structopt;
extern crate stunclient;
extern crate turnclient;
extern crate tokio;
extern crate spin_sleep;
extern crate tokio_timer;
extern crate byteorder;

use std::time::{Duration,Instant};
use std::net::SocketAddr;
use structopt::StructOpt;
use std::sync::Arc;

use futures::{Async, Future, Poll};
use futures::{Stream, Sink};

use std::net::ToSocketAddrs;
use stun_codec::rfc5389;
use stun_codec::{MessageDecoder, MessageEncoder};

use bytecodec::{DecodeExt, EncodeExt};
use stun_codec::rfc5389::attributes::{
    MappedAddress, Software, XorMappedAddress, XorMappedAddress2,
};
use stun_codec::rfc5389::{methods::BINDING, Attribute};
use stun_codec::{Message, MessageClass, TransactionId};

use tokio::sync::oneshot;

type Error = Box<dyn std::error::Error>;


#[derive(Debug, StructOpt)]
struct Opt {
    /// TURN server address (hostname is not resolved)
    server: SocketAddr,

    /// Username for TURN authorization
    username: String,

    /// Credential for TURN authorizaion
    password: String,

    /// Number of simultaneous connections
    #[structopt(short="-j", long="parallel-connections", default_value="1")]
    num_connections: u64,

    /// Packet size
    #[structopt(short="-s", long="pkt-size", default_value="100")]
    packet_size: usize,

    /// Packets per second
    #[structopt(long="pps", default_value="5")]
    packets_per_second: u32,

    /// Experiment duration, seconds
    #[structopt(short="d", long="duration", default_value="5")]
    duration: u64,

    /// Seconds to wait and receive after stopping sender
    #[structopt(long="delay-after-stopping-sender", default_value="3")]
    delay_after_stopping_sender: u64,

    /// Microseconds to wait between TURN allocations
    #[structopt(long="delay-between-allocations", default_value="2000")]
    delay_between_allocations: u64,
}

enum ServeTurnEventOrShutdown {
    TurnEvent(turnclient::MessageFromTurnServer),
    Shutdown,
}

fn sending_thread(
    udp: Arc<std::net::UdpSocket>,
    packet_size: usize,
    packets_per_second: u32,
    duration_seconds: u64,
    destinations: Vec<SocketAddr>,
    time_base: Instant,
) {
    let sleeper = spin_sleep::SpinSleeper::default();
    sleeper.sleep_ns(500_000_000); // to allow receiver to warm up
    let start = Instant::now();
    let step = Duration::from_secs(1) / packets_per_second;
    let n = packets_per_second * (duration_seconds as u32);

    use byteorder::{BE,ByteOrder};

    let mut buf = vec![0; packet_size];

    let mut totalctr : u32 = 0;

    for i in 0..n {
        let deadline = start + step * i;
        let now = Instant::now();
        let delta = now - time_base;

        BE::write_u64(&mut buf[0..8], delta.as_secs());
        BE::write_u32(&mut buf[8..12], delta.subsec_nanos());

        if now < deadline {
            sleeper.sleep(deadline - now);
        }

        let udp = &*udp;
        for addr in &destinations {
            BE::write_u32(&mut buf[12..16], totalctr);
            udp.send_to(&buf[..], addr).expect("UDP send_to failed");
            totalctr+=1;
        }
    }
}

fn receiving_thread(
    udp: Arc<std::net::UdpSocket>,
    duration_seconds: u64,
    packet_size: usize,
    time_base: Instant,
) {
    let mut buf = vec![0; packet_size];
    loop {
        let (_len, _addr) = udp.recv_from(&mut buf[..]).expect("Failed to receive packet");
        println!("Received a packet from {}", _addr);
    }
}

fn main() -> Result<(), Error> {
    let opt = Opt::from_args();

    let local_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let probing_udp = std::net::UdpSocket::bind(local_addr)?;
    let probing_udp = Arc::new(probing_udp);
    let time_base = Instant::now();

    // Phase 1: Query my own external address
    let extaddr = stunclient::StunClient::new(opt.server)
        .set_software(Some("TURN_hammer"))
        .query_external_address(&probing_udp)?;
    eprintln!("My external address: {}", extaddr);

    // Phase 2: Allocate K instances of a TURN client

    let k = opt.num_connections;
    let duration = opt.duration;
    let delay_after_stopping_sender = opt.delay_after_stopping_sender;
    let packet_size = opt.packet_size;
    let pps = opt.packets_per_second;

    let clientstream = futures::stream::repeat::<_,Error>(
        (opt.server, opt.username, opt.password),
    );
    let clientstream = clientstream.take(k);
    let clientstream = tokio_timer::throttle::Throttle::new(
        clientstream,
        Duration::from_micros(opt.delay_between_allocations),
    );
    let clientstream = clientstream.map_err(|e|Error::from("Error throttling"));

    let clienthandles = clientstream.map(move |(serv, user, passwd)| {
        let (snd, rcv) = oneshot::channel::<SocketAddr>();
        let (snd2, rcv2) = oneshot::channel::<()>(); // for shutdown
        let mut snd = Some(snd);
        let rcv2 = rcv2.map_err(|_|Error::from("Oneshot error 2"));

        use turnclient::{MessageFromTurnServer,MessageToTurnServer,ChannelUsage};

        let udp = tokio::net::UdpSocket::bind(&local_addr).expect("Can't bind UDP anymore");
        let mut c = turnclient::TurnClientBuilder::new(serv, user, passwd);
        c.max_retries = 30;
        let (turnsink, turnstream) = c.build_and_send_request(udp).split();

        let srcevents = turnstream.map(|x|ServeTurnEventOrShutdown::TurnEvent(x))
        .select(rcv2.into_stream().map(|()|ServeTurnEventOrShutdown::Shutdown));

        use MessageFromTurnServer::*;
        use ServeTurnEventOrShutdown::*;

        let f = srcevents.map(move |x| {
            match x {
                TurnEvent(AllocationGranted{relay_address, ..}) => {
                    snd.take().expect("More than one AllocationGranted?").send(relay_address);
                    MessageToTurnServer::AddPermission(extaddr, ChannelUsage::WithChannel)
                },
                TurnEvent(MessageFromTurnServer::RecvFrom(sa,data)) => {
                    //eprintln!("Incoming {} bytes from {}", data.len(), sa);
                    MessageToTurnServer::SendTo(sa, data)
                },
                Shutdown => {
                    MessageToTurnServer::Disconnect
                },
                _ => MessageToTurnServer::Noop,
            }
        }).forward(turnsink)
        .and_then(|(_turnstream,_turnsink)|{
            futures::future::ok(())
        })
        .map_err(|e|eprintln!("{}", e));

        tokio::runtime::current_thread::spawn(f);
        (rcv, snd2)
    });
    let clienthandles = clienthandles.collect();
    let clienthandles = clienthandles.and_then(move |x|{
        let (init_handles, shutdown_handles) : (Vec<_>, Vec<_>) = x.into_iter().unzip();
        futures::future::join_all(init_handles)
        .map_err(|_e|Error::from("Oneshot error"))
        .and_then(move |destinations| {
            eprintln!("Allocated {} TURN clients", destinations.len());
            // Phase 3: Starting sender and receiver

            let probing_udp2 = probing_udp.clone();
            std::thread::spawn(move || {
                sending_thread(
                    probing_udp2,
                    packet_size,
                    pps,
                    duration,
                    destinations,
                    time_base,
                );
            });
            std::thread::spawn(move || {
                receiving_thread(
                    probing_udp,
                    duration,
                    packet_size, 
                    time_base,
                );
            });

            tokio_timer::Delay::new(
                Instant::now() + 
                Duration::from_secs(
                    duration + delay_after_stopping_sender
                )
            ).and_then(|()| {
                // Phase 4: Stopping

                eprintln!("Stopping TURN clients");
                for sh in shutdown_handles {
                    sh.send(());
                }
                futures::future::ok(())
            }).map_err(|_e|Error::from("Timer error"))
        })
    });

    let f = clienthandles.map_err(|e|eprintln!("{}",e));

    tokio::runtime::current_thread::run(f);

    Ok(())
}
