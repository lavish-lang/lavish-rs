use super::{Atom, Error, Message, PendingRequests};

use std::any::Any;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use log::*;

mod codec;
use codec::{Decoder, Encoder};

pub trait Conn: Read + Write + Send + Sized + 'static {
    fn try_clone(&self) -> io::Result<Self>;
    fn shutdown(&self, how: Shutdown) -> io::Result<()>;
}

impl Conn for std::net::TcpStream {
    fn try_clone(&self) -> io::Result<Self> {
        self.try_clone()
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.shutdown(how)
    }
}

pub trait Handler<P, NP, R>: Sync + Send
where
    P: Atom,
    NP: Atom,
    R: Atom,
{
    fn handle(&self, client: Client<P, NP, R>, params: P) -> Result<R, Error>;
}

impl<P, NP, R, F> Handler<P, NP, R> for F
where
    P: Atom,
    R: Atom,
    NP: Atom,
    F: (Fn(Client<P, NP, R>, P) -> Result<R, Error>) + Send + Sync,
{
    fn handle(&self, client: Client<P, NP, R>, params: P) -> Result<R, Error> {
        self(client, params)
    }
}

pub enum SinkValue<P, NP, R>
where
    P: Atom,
    NP: Atom,
    R: Atom,
{
    Shutdown,
    Message(Message<P, NP, R>),
}

pub struct Client<P, NP, R>
where
    P: Atom,
    NP: Atom,
    R: Atom,
{
    queue: Arc<Mutex<Queue<P, NP, R>>>,
    sink: mpsc::Sender<SinkValue<P, NP, R>>,
}

impl<P, NP, R> Client<P, NP, R>
where
    P: Atom,
    NP: Atom,
    R: Atom,
{
    fn clone(&self) -> Self {
        Client {
            queue: self.queue.clone(),
            sink: self.sink.clone(),
        }
    }

    pub fn call_raw(&self, params: P) -> Result<Message<P, NP, R>, Error> {
        let id = {
            let mut queue = self.queue.lock()?;
            queue.next_id()
        };

        let method = params.method();
        let m = Message::Request { id, params };

        let (tx, rx) = mpsc::channel::<Message<P, NP, R>>();
        let in_flight = InFlightRequest { method, tx };
        {
            let mut queue = self.queue.lock()?;
            queue.in_flight_requests.insert(id, in_flight);
        }

        {
            let sink = self.sink.clone();
            sink.send(SinkValue::Message(m))?;
        }
        Ok(rx.recv()?)
    }

    #[allow(clippy::needless_lifetimes)]
    pub fn call<D, RR>(&self, params: P, downgrade: D) -> Result<RR, Error>
    where
        D: Fn(R) -> Option<RR>,
    {
        match self.call_raw(params) {
            Ok(m) => match m {
                Message::Response { results, error, .. } => {
                    if let Some(error) = error {
                        Err(Error::RemoteError(error))
                    } else if let Some(results) = results {
                        downgrade(results).ok_or_else(|| Error::WrongResults)
                    } else {
                        Err(Error::MissingResults)
                    }
                }
                _ => Err(Error::WrongMessageType),
            },
            Err(msg) => Err(Error::TransportError(format!("{:#?}", msg))),
        }
    }
}

pub fn default_timeout() -> Duration {
    Duration::from_secs(2)
}

// Connect to a TCP address, then spawn a new RPC
// system with the given handler
pub fn connect_tcp<AH, H, P, NP, R>(
    handler: AH,
    addr: &SocketAddr,
) -> Result<Runtime<TcpStream, P, NP, R>, Error>
where
    AH: Into<Arc<H>>,
    H: Handler<P, NP, R> + 'static,
    P: Atom,
    NP: Atom,
    R: Atom,
{
    let conn = TcpStream::connect_timeout(addr, default_timeout())?;
    return spawn(handler, conn);
}

pub struct Runtime<C, P, NP, R>
where
    C: Conn,
    P: Atom,
    NP: Atom,
    R: Atom,
{
    proto_client: Client<P, NP, R>,
    err_rx: mpsc::Receiver<Result<(), Box<dyn Any + Send>>>,
    shutdown_handle: C,
}

impl<C, P, NP, R> Runtime<C, P, NP, R>
where
    C: Conn,
    P: Atom,
    NP: Atom,
    R: Atom,
{
    pub fn join(&self) -> Result<(), Box<dyn Any + Send>> {
        let res1 = self.err_rx.recv().unwrap();
        if let Err(e) = res1.as_ref() {
            warn!("While joining: {:#?}", e);
        }
        let res2 = self.err_rx.recv().unwrap();
        if let Err(e) = res2.as_ref() {
            warn!("While joining: {:#?}", e);
        }

        match res1 {
            Err(e) => return Err(e),
            _ => match res2 {
                Err(e) => return Err(e),
                _ => Ok(()),
            },
        }
    }

    pub fn client(&self) -> Client<P, NP, R> {
        self.proto_client.clone()
    }

    pub fn shutdown(&self) -> Result<(), Error> {
        debug!("Runtime: shutting down ");
        self.shutdown_handle.shutdown(Shutdown::Both)?;
        Ok(())
    }
}

pub fn spawn<C, AH, H, P, NP, R>(handler: AH, conn: C) -> Result<Runtime<C, P, NP, R>, Error>
where
    C: Conn,
    AH: Into<Arc<H>>,
    H: Handler<P, NP, R> + 'static,
    P: Atom,
    NP: Atom,
    R: Atom,
{
    let handler = handler.into();
    let queue = Arc::new(Mutex::new(Queue::new()));

    let shutdown_handle = conn.try_clone()?;
    let write = conn.try_clone()?;
    let read = conn;
    let mut decoder = Decoder::new(read, queue.clone());
    let mut encoder = Encoder::new(write);
    let (tx, rx) = mpsc::channel();

    let client = Client::<P, NP, R> {
        queue: queue.clone(),
        sink: tx,
    };

    let proto_client = client.clone();
    let (err_tx, err_rx) = mpsc::channel();
    let err_tx2 = err_tx.clone();

    let encode_handle = std::thread::spawn(move || loop {
        match rx.recv().unwrap() {
            SinkValue::Message(m) => {
                encoder.encode(m).unwrap();
            }
            SinkValue::Shutdown => {
                debug!("Encoder loop: dropping receiver");
                return;
            }
        }
    });
    std::thread::spawn(move || {
        err_tx.send(encode_handle.join()).unwrap();
    });

    let decode_handle = std::thread::spawn(move || loop {
        let m = decoder.decode().unwrap();
        let handler = handler.clone();
        let client = client.clone();

        std::thread::spawn(move || {
            let res = handle_message(m, handler, client);
            if let Err(e) = res {
                eprintln!("message stream error: {:#?}", e);
            }
        });
    });
    std::thread::spawn(move || {
        err_tx2.send(decode_handle.join()).unwrap();
    });

    Ok(Runtime {
        proto_client,
        err_rx,
        shutdown_handle,
    })
}

fn handle_message<P, NP, R, H>(
    inbound: Message<P, NP, R>,
    handler: Arc<H>,
    client: Client<P, NP, R>,
) -> Result<(), Error>
where
    P: Atom,
    NP: Atom,
    R: Atom,
    H: Handler<P, NP, R>,
{
    match inbound {
        Message::Request { id, params } => {
            let m = match handler.handle(client.clone(), params) {
                Ok(results) => Message::Response::<P, NP, R> {
                    id,
                    results: Some(results),
                    error: None,
                },
                Err(error) => Message::Response::<P, NP, R> {
                    id,
                    results: None,
                    error: Some(format!("internal error: {:#?}", error)),
                },
            };
            client.sink.send(SinkValue::Message(m)).unwrap();
        }
        Message::Response { id, error, results } => {
            if let Some(in_flight) = {
                let mut queue = client.queue.lock()?;
                queue.in_flight_requests.remove(&id)
            } {
                in_flight
                    .tx
                    .send(Message::Response { id, error, results })
                    .unwrap();
            }
        }
        Message::Notification { .. } => unimplemented!(),
    };
    Ok(())
}

struct InFlightRequest<P, NP, R>
where
    P: Atom,
    NP: Atom,
    R: Atom,
{
    method: &'static str,
    tx: mpsc::Sender<Message<P, NP, R>>,
}

pub struct Queue<P, NP, R>
where
    P: Atom,
    NP: Atom,
    R: Atom,
{
    id: u32,
    in_flight_requests: HashMap<u32, InFlightRequest<P, NP, R>>,
}

impl<P, NP, R> Queue<P, NP, R>
where
    P: Atom,
    NP: Atom,
    R: Atom,
{
    fn new() -> Self {
        Queue {
            id: 0,
            in_flight_requests: HashMap::new(),
        }
    }

    fn next_id(&mut self) -> u32 {
        let res = self.id;
        self.id += 1;
        res
    }
}

impl<P, NP, R> PendingRequests for Queue<P, NP, R>
where
    P: Atom,
    NP: Atom,
    R: Atom,
{
    fn get_pending(&self, id: u32) -> Option<&'static str> {
        self.in_flight_requests.get(&id).map(|req| req.method)
    }
}
