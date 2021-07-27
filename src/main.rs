extern crate bytecodec;
extern crate futures;
extern crate rand;
extern crate argh;
extern crate stunclient;
extern crate turnclient;
extern crate tokio;
extern crate spin_sleep;
extern crate byteorder;

use std::time::{Duration};
use tokio::time::Instant;
use std::net::SocketAddr;
use std::sync::Arc;

use futures::{FutureExt};
use futures::StreamExt as FuturesStreamExt;

use tokio::sync::oneshot;
use tokio_stream::StreamExt as TokioStreamExt;

type Error = anyhow::Error;

unzip_n::unzip_n!(3);


#[derive(Debug, argh::FromArgs)]
/// A tool to test TURN server for performance and measure it's packet loss and RTT.
struct Opt {
    /// TURN server address (hostname is not resolved)
    #[argh(positional)]
    server: SocketAddr,

    /// username for TURN authorization
    #[argh(positional)]
    username: String,

    /// credential for TURN authorizaion
    #[argh(positional)]
    password: String,

    /// number of simultaneous connections
    #[argh(option, short='j', long="parallel-connections", default="1")]
    num_connections: usize,

    /// packet size
    #[argh(option, short='s', long="pkt-size", default="100")]
    packet_size: usize,

    /// packets per second
    #[argh(option, long="pps", default="5")]
    packets_per_second: u32,

    /// experiment duration, seconds
    #[argh(option, short='d', long="duration", default="5")]
    duration: u64,

    /// seconds to wait and receive after stopping sender
    #[argh(option, long="delay-after-stopping-sender", default="3")]
    delay_after_stopping_sender: u64,

    /// microseconds to wait between TURN allocations
    #[argh(option, long="delay-between-allocations", default="2000")]
    delay_between_allocations: u64,

    /// don't actually run, only calculate bandwidth and traffic
    #[argh(switch, long="calc")]
    only_calc: bool,

    /// override bandwidth or traffic limitation
    #[argh(switch, long="force", short='f')]
    force: bool,

    /// set pps to 90 and pktsize to 960
    #[argh(switch, long="video")]
    video_override: bool,

    /// set pps to 16 and pktsize to 192
    #[argh(switch, long="audio")]
    audio_override: bool,

    /// output as JSON instead of plain text
    #[argh(switch, long="json", short='J')]
    json_mode: bool,
}

#[derive(Debug)]
enum ServeTurnEventOrShutdown {
    TurnEvent(Result<turnclient::MessageFromTurnServer,turnclient::Error>),
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
    total_packets: u64,
    time_base: Instant,
    json_mode: bool,
) {
    use std::collections::BinaryHeap;
    use byteorder::{BE,ByteOrder};

    #[derive(PartialEq,Eq)]
    struct Packet {
        no: u32,
        rtt4: Duration,
    }
    /// Compare by `no` field, reversed
    impl Ord for Packet {
        fn cmp(&self, other: &Packet) -> std::cmp::Ordering {
            other.no.cmp(&self.no)
        }
    }
    impl PartialOrd for Packet {
        fn partial_cmp(&self, other: &Packet) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    let mut jitter_buffer = BinaryHeap::with_capacity(1024);

    let deadline = Instant::now() + Duration::from_secs(duration_seconds);

    let mut min_n = std::u32::MAX;
    let mut max_n = std::u32::MIN;
    
    let mut ctr = 0;
    let mut prevn = std::u32::MAX;
    let mut badloss = 0;
    let mut dup = 0;
    let mut rtt4stats = [0; 6];

    let mut received_something = false;

    let mut analyse_packet = |p:&Packet| {
        //eprintln!("no={}", p.no);
        if min_n > p.no { min_n = p.no }
        if max_n < p.no { max_n = p.no }
        
        if p.no == prevn {
            // duplicate packet, ignore
            dup+=1;
            return;
        }

        
        if p.no > prevn && p.no - prevn > 50 {
             badloss += p.no - prevn;
        }

        if p.rtt4 < Duration::from_millis(50) {
            rtt4stats[0]+=1;
        } else if p.rtt4 < Duration::from_millis(180) {
            rtt4stats[1]+=1;
        } else if p.rtt4 < Duration::from_millis(400) {
            rtt4stats[2]+=1;
        } else if p.rtt4 < Duration::from_millis(1000) {
            rtt4stats[3]+=1;
        } else if p.rtt4 < Duration::from_millis(2000) {
            rtt4stats[4]+=1;
        } else {
            rtt4stats[5]+=1;
        }

        ctr+=1;
        prevn = p.no;
    };

    let mut buf = vec![0; packet_size];
    loop {
        let ret = udp.recv_from(&mut buf[..]);
        let now = Instant::now();
        if now > deadline {
            break;
        }
        if let Ok((_len, _addr)) = ret {
            //println!("Received a packet from {}", _addr);

            let s = BE::read_u64(&mut buf[0..8]);
            let ns= BE::read_u32(&mut buf[8..12]);
            let no= BE::read_u32(&mut buf[12..16]);

            let since_base = Duration::new(s,ns);
            let send_time = time_base + since_base;
            let rtt4 = if now >= send_time { now - send_time } else { Duration::from_secs(999)};
            jitter_buffer.push(Packet {
                no,
                rtt4,
            });
            if jitter_buffer.len() > 1022 {
                analyse_packet(&jitter_buffer.pop().unwrap());
            };

            if !received_something {
                received_something = true;
                eprintln!("Received the first datagram");
            }

        } else {
            // don't care
        }
    }
    while let Some(p) = jitter_buffer.pop() {
        analyse_packet(&p);
    }
    drop(analyse_packet);

    if ctr == 0 {
        if !json_mode {
            println!("Received no packets");
        } else {
            println!("{}", r#"{"status":"no_packets_received"}"#);
        }
        return;
    }
    let nn = max_n - min_n + 1;

    if json_mode { println!("{}", r#"{"status":"ok""#); }

    if !json_mode {
        print!(
            "Received {} packets from {} window of total {} || ",
            ctr,
            nn,
            total_packets,
        );
    } else {
        println!(
            r#","received_packets":{} ,"min_max_window":{} ,"sent_packets":{}"#,
            ctr,
            nn,
            total_packets,
        )
    }
    let nn = nn as f64;
    let loss = 100.0 - (ctr as f64) * 100.0 / nn;
    let badl = (badloss as f64) * 100.0 / nn;
    let rtts0 =(rtt4stats[0] as f64) * 100.0 / nn;
    let rtts1 =(rtt4stats[1] as f64) * 100.0 / nn;
    let rtts2 =(rtt4stats[2] as f64) * 100.0 / nn;
    let rtts3 =(rtt4stats[3] as f64) * 100.0 / nn;
    let rtts4 =(rtt4stats[4] as f64) * 100.0 / nn;
    let rtts5 =(rtt4stats[5] as f64) * 100.0 / nn;
    if !json_mode {
        println!(
            "Loss: {:07.04}%   bad loss: {:07.04}%",
            loss,
            badl,
        );
        println!(
            "RTT4 brackets: 0-49ms: {:07.04}%   180-399ms: {:07.04}%  1000-1999ms: {:07.04}%",
            rtts0,
            rtts2,
            rtts4,
        );
        println!(
            "             50-179ms: {:07.04}%   400-999ms: {:07.04}%      2000+ms: {:07.04}%",
            rtts1,
            rtts3,
            rtts5,
        );
    } else {
        println!(
            r#","loss":{} ,"bad_loss":{}"#,
           loss,
           badl,
        );

        println!(
            r#","rtt4":{{"0_49":{} ,"50_179":{} ,"180_399":{} ,"400_999":{} ,"1000_1999":{} ,"2000+":{}}}"#,
            rtts0,
            rtts1,
            rtts2,
            rtts3,
            rtts4,
            rtts5,
        );
    }
    let overall = 10.0 - loss/3.0 - badl/1.0 - rtts2/200.0 - rtts3/80.0 - rtts4/40.0 - rtts5/20.0;

    if !json_mode {
        println!(" <<<  Overall score:  {:.01} / 10.0  >>>", overall);
    } else {
        println!(
            r#","score":{:.01}"#,
            overall,
        );
    }

    if json_mode { println!("{}", r#"}"#); }
}

#[tokio::main(flavor="current_thread")]
async fn main() -> Result<(), Error> {
    let mut opt : Opt = argh::from_env();

    if opt.audio_override && opt.video_override {
        anyhow::bail!("Both --audio and --video is meaningless");
    }
    if opt.audio_override {
        opt.packets_per_second = 16;
        opt.packet_size = 192;
    }
    if opt.video_override {
        opt.packets_per_second = 90;
        opt.packet_size = 960;
    }

    let mbps = ((opt.packets_per_second as u64) * (opt.num_connections as u64) * (opt.packet_size as u64 + 40) * 2 * 8) as f64 / 1000.0 / 1000.0;
    let traffic = mbps * (opt.duration as f64) / 9.0;

    if !(opt.json_mode && opt.only_calc) {
        eprintln!(
            "The test would do approx {:.3} Mbit/s and consume {:.3} megabytes of traffic",
            mbps,
            traffic,
        );
    }

    if opt.only_calc {
        if opt.json_mode {
            println!(
                r#"{{"status":"calc", "kpbs":{:.0}, "bytes":{:.0}}}"#,
                mbps * 1000.0,
                traffic * 1000_000.0,
            );
        }
        return Ok(());
    }

    if mbps > 50.0 || traffic > 100.0 {
        if !opt.force {
            if opt.json_mode {
                println!("{}", r#"{"status":"refused"}"#);
            }
            anyhow::bail!("Refusing to run test of that scale. Use --force to override.");
        }
    }

    let local_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let probing_udp = std::net::UdpSocket::bind(local_addr)?;
    probing_udp.set_read_timeout(Some(Duration::from_millis(100)))?;
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

    let clientstream = futures::stream::repeat(
        (opt.server, opt.username, opt.password),
    );
    let clientstream = TokioStreamExt::take(clientstream,k);
    let clientstream = TokioStreamExt::throttle(clientstream,
        Duration::from_micros(opt.delay_between_allocations),
    );

    let clienthandles = FuturesStreamExt::then(clientstream, move |(serv, user, passwd)| { async move {
        let (snd, rcv) = oneshot::channel::<SocketAddr>();
        let (shutdown_handle, rcv2) = oneshot::channel::<()>(); // for shutdown
        let mut snd = Some(snd);

        use turnclient::{MessageFromTurnServer,MessageToTurnServer,ChannelUsage};

        let udp = tokio::net::UdpSocket::bind(&local_addr).await.expect("Can't bind UDP anymore");
        let mut c = turnclient::TurnClientBuilder::new(serv, user, passwd);
        c.max_retries = 30;
        c.software = Some("TURN_Hammer");
        let (turnsink, turnstream) = c.build_and_send_request(udp).split();

        let srcevents = futures::stream::select(TokioStreamExt::map(turnstream,|x|ServeTurnEventOrShutdown::TurnEvent(x))
        ,TokioStreamExt::map(rcv2.into_stream(),|_|ServeTurnEventOrShutdown::Shutdown));

        use MessageFromTurnServer::*;
        use ServeTurnEventOrShutdown::*;

        let mut relay_address_buf = None;

        let f = TokioStreamExt::map(srcevents,move |x| {
            //eprintln!("{:?}", x);
            Ok(match x {
                TurnEvent(Ok(AllocationGranted{relay_address, ..})) => {
                    if relay_address_buf.is_some() {
                        eprintln!("AllocationGranted happened one more time?");
                    } else {
                        relay_address_buf = Some(relay_address);
                    }
                    MessageToTurnServer::AddPermission(extaddr, ChannelUsage::WithChannel)
                },
                TurnEvent(Ok(MessageFromTurnServer::RecvFrom(sa,data))) => {
                    //eprintln!("Incoming {} bytes from {}", data.len(), sa);
                    MessageToTurnServer::SendTo(sa, data)
                },
                TurnEvent(Ok(MessageFromTurnServer::PermissionCreated(sa))) => {
                    if relay_address_buf.is_none() {
                        anyhow::bail!("Strangely, a permission was granted even before the allocation");
                    }
                    if sa != extaddr {
                        eprintln!("Strangely, granted permission is not the same as we requested it");
                    } else {
                        if let Some(s) = snd.take() {
                            // signal that we can start + tell our related address
                            let _ = s.send(relay_address_buf.unwrap());
                        }
                    }
                    MessageToTurnServer::Noop
                },
                Shutdown => {
                    MessageToTurnServer::Disconnect
                },
                TurnEvent(Err(e)) => {
                    eprintln!("{}", e);
                    MessageToTurnServer::Noop
                }
                _ => MessageToTurnServer::Noop,
            })
        }).forward(turnsink);

        let joinh = tokio::spawn(async move {
            if let Err(e) = f.await {
                let s = format!("{}", e);
                if s != "TURN client received a shutdown request" {
                    eprintln!("{}", e);
                }
            }
        });
        (rcv, shutdown_handle, joinh)
    }});
    let clienthandles : Vec<(tokio::sync::oneshot::Receiver<std::net::SocketAddr>, tokio::sync::oneshot::Sender<()>, _)> 
        = FuturesStreamExt::collect(clienthandles).await;
    let (init_handles, shutdown_handles, join_handles) : (Vec<_>, Vec<_>, Vec<_>) = clienthandles.into_iter().unzip_n_vec();
    let destinations : Vec<Result<std::net::SocketAddr,_>> = futures::future::join_all(init_handles).await;
    let destinations : Vec<std::net::SocketAddr> = destinations.into_iter().collect::<Result<_,_>>()?;
  
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
    let json_mode = opt.json_mode;
    let recvthread = std::thread::spawn(move || {
        receiving_thread(
            probing_udp,
            duration + delay_after_stopping_sender,
            packet_size,
            duration * (pps as u64) * (k as u64),
            time_base,
            json_mode,
        );
    });

    tokio::time::sleep_until(
        Instant::now() + 
        Duration::from_secs(
            duration + delay_after_stopping_sender + 1
        )
    ).await;

    // Phase 4: Stopping

    eprintln!("Stopping TURN clients");
    for sh in shutdown_handles {
        let _ = sh.send(());
    }

    let _ = recvthread.join();

    let shutdown_deadline = tokio::time::Instant::now() + Duration::from_millis(100);

    for jh in join_handles {
        match tokio::time::timeout_at(shutdown_deadline, jh).await {
            Ok(Ok(_)) => (),
            Err(_) => (),
            Ok(Err(e)) => eprintln!("{}", e),
        }
    }

    Ok(())
}
