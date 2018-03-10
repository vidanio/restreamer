extern crate bytes;
#[macro_use]
extern crate futures;
extern crate pretty_env_logger;
extern crate tokio;
#[macro_use]
extern crate tokio_io;

#[macro_use]
extern crate structopt;

use structopt::StructOpt;

use tokio::executor::current_thread;
use tokio::net::{TcpListener, TcpStream};
use tokio_io::AsyncRead;
use futures::prelude::*;
use futures::task;
use futures::sync::mpsc;
use bytes::{BufMut, Bytes, BytesMut};

use std::io::{self, Write};
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;

type Tx = mpsc::UnboundedSender<Bytes>;
type Rx = mpsc::UnboundedReceiver<Bytes>;

struct Shared {
    peers: HashMap<SocketAddr, Tx>,
}

struct Peer {
    packets: TSPacket,
    state: Rc<RefCell<Shared>>,

    rx: Rx,

    addr: SocketAddr,
    producer: bool,
}

/// TS Packet chunker
struct TSPacket {
    buffer_size: usize,
    socket: TcpStream,

    rd: BytesMut,
    wr: BytesMut,
}

impl Shared {
    fn new() -> Self {
        Shared {
            peers: HashMap::new(),
        }
    }
}

impl Peer {
    fn new(state: Rc<RefCell<Shared>>, packets: TSPacket, producer: bool) -> Peer {
        let addr = packets.socket.peer_addr().unwrap();

        let (tx, rx) = mpsc::unbounded();

        if !producer {
            state.borrow_mut().peers.insert(addr, tx);
        }

        Peer {
            packets,
            state,
            rx,
            addr,
            producer,
        }
    }
}

impl Future for Peer {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        if !self.producer {
            while self.packets.wr.remaining_mut() > 0 {
                match self.rx.poll().unwrap() {
                    Async::Ready(Some(v)) => {
                        self.packets.buffer(&v);
                    }
                    _ => break,
                }
            }

            if self.packets.wr.remaining_mut() <= 0 {
                task::current().notify();
            }

            let _ = self.packets.poll_flush()?;
        } else {
            while let Async::Ready(pkt) = self.packets.poll()? {
                if let Some(packet) = pkt {
                    let packet = packet.freeze();

                    for (_addr, tx) in &self.state.borrow().peers {
                        tx.unbounded_send(packet.clone()).unwrap();
                    }
                } else {
                    return Ok(Async::Ready(()));
                }
            }
        }

        Ok(Async::NotReady)
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.state.borrow_mut().peers.remove(&self.addr);

        eprintln!("Dropping {}", self);
    }
}

use std::fmt;

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let name = if self.producer {
            "Producer"
        } else {
            "Consumer"
        };
        write!(f, "{} ({:?})", name, self.addr)
    }
}

impl TSPacket {
    fn new(socket: TcpStream, buffer_size: usize) -> Self {
        TSPacket {
            buffer_size,
            socket,
            rd: BytesMut::new(),
            wr: BytesMut::new(),
        }
    }

    /// Buffer a packet.
    fn buffer(&mut self, line: &[u8]) {
        self.wr.reserve(self.buffer_size * 4);
        self.wr.put(line);
    }

    /// Flush the write buffer to the socket
    fn poll_flush(&mut self) -> Poll<(), io::Error> {
        while !self.wr.is_empty() {
            let n = try_nb!(self.socket.write(&self.wr));

            assert!(n > 0);

            let _ = self.wr.split_to(n);
        }

        Ok(Async::Ready(()))
    }

    fn fill_read_buf(&mut self) -> Poll<(), io::Error> {
        loop {
            self.rd.reserve(self.buffer_size * 4);
            let n = try_ready!(self.socket.read_buf(&mut self.rd));
            if n == 0 {
                return Ok(Async::Ready(()));
            }
        }
    }
}

impl Stream for TSPacket {
    type Item = BytesMut;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let sock_closed = self.fill_read_buf()?.is_ready();

        if self.rd.len() > self.buffer_size {
            let pkt = self.rd.split_to(self.buffer_size);

            return Ok(Async::Ready(Some(pkt)));
        }

        if sock_closed {
            Ok(Async::Ready(None))
        } else {
            Ok(Async::NotReady)
        }
    }
}

fn setup(socket: TcpStream, state: Rc<RefCell<Shared>>, producer: bool, buffer_size: usize) {
    let packets = TSPacket::new(socket, buffer_size);

    let cons = Peer::new(state, packets, producer);

    eprintln!("Adding {}", cons);

    current_thread::spawn(cons.map_err(|e| println!("FAIL {:?}", e)));
}

use std::net::IpAddr;

#[derive(StructOpt, Debug)]
#[structopt()]
struct Config {
    #[structopt(short = "p", long = "port", help = "Set listening ports", default_value = "12345")]
    /// Set the listening ports, consumer ports is ${producer port +1}
    port: u16,
    #[structopt(short = "I", help = "Set the input host", default_value = "127.0.0.1")]
    /// Set the producer host
    input_host: IpAddr,

    #[structopt(short = "O", help = "Set the output host", default_value = "127.0.0.1")]
    /// Set the producer host
    output_host: IpAddr,

    #[structopt(short = "b", help = "Set the packet buffer size", default_value = "1316")]
    buffer: usize,
}

pub fn main() {
    pretty_env_logger::init().unwrap();

    let state = Rc::new(RefCell::new(Shared::new()));

    let prod_state = state.clone();
    let cons_state = state.clone();

    let cfg = Config::from_args();

    let l_prod = TcpListener::bind(&(cfg.input_host, cfg.port).into()).unwrap();
    let l_cons = TcpListener::bind(&(cfg.output_host, cfg.port + 1).into()).unwrap();

    let buffer_size = cfg.buffer;

    let srv_prod = l_prod
        .incoming()
        .for_each(move |socket| {
            setup(socket, prod_state.clone(), true, buffer_size.clone());
            Ok(())
        })
        .map_err(|err| {
            eprintln!("producer accept error = {:?}", err);
        });

    let srv_cons = l_cons
        .incoming()
        .for_each(move |socket| {
            setup(socket, cons_state.clone(), false, buffer_size.clone());
            Ok(())
        })
        .map_err(|err| {
            eprintln!("consumer accept error = {:?}", err);
        });

    current_thread::run(|_| {
        current_thread::spawn(srv_cons);
        current_thread::spawn(srv_prod);

        eprintln!("Server Running");
    });
}
