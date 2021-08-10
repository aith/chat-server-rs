// I ran cargo fmt (rustfmt)
#![warn(rust_2018_idioms)]

use std::collections::HashMap;
use std::env;
use tokio::sync::mpsc::{channel, Sender};
use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use futures::executor::block_on;
use bytes::BytesMut;

use futures::SinkExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_stream::StreamExt;
use tokio_util::codec::{Framed, LinesCodec};
use tokio::signal::unix::{signal, SignalKind};
use tokio::io::{AsyncReadExt, BufReader, AsyncWriteExt, AsyncBufReadExt};
use halfbrown;
use tokio::net::unix::ReadHalf;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // if these are locals I don't think there's much use in being constants
    let default_port = 1234u16;
    let default_host = "0.0.0.0";
    let state = Arc::new(Mutex::new(SharedMemory::new())); 
    let port = env::args()
        .nth(1)
        .unwrap_or_else(|| default_port.to_string());
    let addr = format!("{}:{}", default_host, port);
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("server running on {}", addr);
    tokio::spawn(async move {
        loop {
            // Asynchronously wait for an inbound TcpStream.
            let (stream, addr) = listener.accept().await.unwrap();
            // Clone a handle to the `Shared` state for the new connection.
            let state = state.clone(); // Get & increase ref counts
            // Spawn our handler to be run asynchronously.
            tokio::spawn(async move {
                tracing::debug!("accepted connection");
                if let Err(e) = process(state, stream, addr).await {
                    tracing::info!("an error occurred; error = {:?}", e);
                }
            });
        }
    });

    // Graceful shutdown
    cancel_tasks().await
}

type Sx = mpsc::UnboundedSender<String>;
type Rx = mpsc::UnboundedReceiver<String>;

/// Data that is shared between all peers in the chat server.
struct SharedMemory {
    peers: halfbrown::HashMap<String, Room>, // Must be locked before writes to avoid data conflicts
}

/// The state for each connected client.
struct Peer {
    /// The TCP socket wrapped with the `Lines` codec, defined below.
    ///
    /// This handles sending and receiving data on the socket. When using
    /// `Lines`, we can work at the line level instead of having to manage the
    /// raw byte operations.
    reader: BufReader<OwnedReadHalf>,

    /// Receive half of the message channel.
    ///
    /// This is used to receive messages from peers. When a message is received
    /// off of this `Rx`, it will be written to the socket.
    rx: Rx,
}

struct Room {
    peers: HashMap<SocketAddr, Sx>,
}

impl Room {
    fn new() -> Room {
        Self { peers: Default::default() }
    }

    async fn add_peer(&mut self, room_id: String, sx: Sx, peer: &SocketAddr) -> io::Result<()> {
        // let addr = peer.lines.into_inner().as_ref().peer_addr()?;
        let addr = peer;
        self.peers.insert(*addr, sx);
        Ok(())
    }

    async fn rm_peer(&mut self, room_id: &str, addr: &SocketAddr) -> io::Result<()> {
        self.peers.remove(addr);
        Ok(())
    }

    async fn broadcast(&mut self, sender: &SocketAddr, message: &str) {
        self.peers
            .iter_mut()
            // .filter(|(socket, _sx)| *socket != sender)
            .map(|(_socket, sx)| sx)
            .for_each(|sx| {
                let _ = sx.send(message.into());
            });
    }
}

impl SharedMemory {
    /// Create a new, empty, instance of `Shared`.
    fn new() -> Self {
        Self {
            // Halfbrown: uses Vec at low count (>32) for faster iterations
            //            but switches to HashMap after that for faster lookup
            peers: halfbrown::HashMap::new(),
        }
    }

    fn get_room(&mut self, key: String) -> &mut Room {
        return self.peers.entry(key).or_insert(Room::new());
    }
}

impl Peer {
    /// Create a new instance of `Peer`.
    // No need to be async or return a Result if you're not using await or returning errors
    fn new(lines: BufReader<OwnedReadHalf>) -> (Self, Sx) {
        // Create a channel for this peer
        let (sx, rx) = mpsc::unbounded_channel();
        // Add an entry for this `Peer` in the shared state map.
        (Self { reader: lines, rx }, sx)
    }
}

async fn cancel_tasks() -> Result<(), Box<dyn std::error::Error>>
{
    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        _option = tokio::signal::ctrl_c() => {
            println!("SIGINT");
            Ok(())
        },
        _option = term.recv() => {
            println!("SIGTERM");
            Ok(())
        }
    }
}

fn is_oob(st: &str) -> bool {
    let length = st.len();
    return length < 1 || length > 20;
}

fn bytes_to_str(buf: &BytesMut) -> String {
    std::str::from_utf8(&buf.to_vec()).expect("Could not convert bytes to str").to_string()
}

/// Process an individual chat client
async fn process(
    state: Arc<Mutex<SharedMemory>>,
    mut stream: TcpStream,
    addr: SocketAddr,
    // Box<dyn Error + Send + Sync + 'static> is better for errors
    // and most errors should be that anyways
    // you could also use the crate anyhow, which makes things more convenient
) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    // let mut lines = Framed::new(stream, LinesCodec::new());
    // Send a prompt to the client to enter their username.
    stream.write_all(b"Please enter your username:\n").await?;

    let msg_max = 20000usize;
    let mut buffer = BytesMut::with_capacity(1024);
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Keep reading until newline is found
    match limited_read(&mut reader, &mut buffer, 0, msg_max).await {
        Ok(_result) => {}
        _ => {
            return send_user_err(&mut writer).await;
        }
    }
    let line = bytes_to_str(&buffer); // The JOIN command line
    let vec = line.split_whitespace().collect::<Vec<_>>(); // Split words across vec
    let (_cmd, room_id, username) = match *vec.as_slice() {
        [cmd, room_id, username] => {
            if cmd.to_uppercase() != "JOIN"
                || is_oob(&room_id)
                || is_oob(&username)
                || !username.is_ascii()
                || !room_id.is_ascii()
            {
                return send_user_err(&mut writer).await;
            }
            (cmd, room_id, username)
        }
        _ => {
            return send_user_err(&mut writer).await;
        }
    };
    // Register our peer with state which internally sets up some channels.
    let (mut peer, sx) = Peer::new(reader);
    {
        // Add client to room, let go of lock after
        let mut state = state.lock().await;
        let mut room = state.get_room(room_id.to_string());
        room.add_peer(room_id.to_string(), sx, &addr).await?;
    }
    {
        // A client has connected, let's let everyone know.
        let mut state = state.lock().await;
        let mut room = state.get_room(room_id.to_string());
        let msg = format!("{} has joined\n", username);
        tracing::info!("{}", msg);
        room.broadcast(&addr, msg.as_str()).await;
    }
    buffer.clear();
    // Process incoming messages until our stream is exhausted by a disconnect.
    loop {
        tokio::select! {
            result = peer.reader.read_buf(&mut buffer) => {
                match result {
                    Ok(bytes_read) => {
                        if bytes_read == 0 { break; } // Peer left
                        println!("{}", bytes_read);
                        match limited_read(&mut peer.reader, &mut buffer, bytes_read, msg_max).await {
                            Ok(()) => {
                                let msg = std::str::from_utf8(&buffer).unwrap();
                                let mut state = state.lock().await;
                                let msg = format!("{}: {}", username, msg);
                                let mut room = state.get_room(room_id.to_string());
                                room.broadcast(&addr, msg.as_str()).await;
                            },
                            _ => { tracing::info!("Large message ignored."); } // Ignore message
                        }
                        buffer.clear();
                    },
                    _ => { // IO Error
                        tracing::info!("IO Error while receiving message.");
                        break;
                    },
                }
            }
            // A message was received from a peer. Send it to the current user.
            Some(msg) = peer.rx.recv() => {
                writer.write_all(&msg.into_bytes()).await?;
            }
        }
    }
    // If this section is reached it means that the client was disconnected!
    // Let's let everyone still connected know about it.
    {
        let mut state = state.lock().await;
        let mut room = state.get_room(room_id.to_string());
        room.rm_peer(room_id, &addr).await;
        let msg = format!("{} has left the chat\n", username);
        tracing::info!("{}", msg);
        room.broadcast(&addr, msg.as_str()).await;
    }
    Ok(())
}

async fn limited_read(
    buf_reader: &mut BufReader<OwnedReadHalf>,
    buf: &mut BytesMut,
    mut num_bytes: usize,
    lim_bytes: usize) -> Result<(), &'static str>
{
    if num_bytes > lim_bytes { return Err("Too large of a buffer."); }
    while !buf.ends_with(&[b'\n']) {
        let bytes_read = buf_reader.read_buf(buf).await.unwrap();
        num_bytes += bytes_read;
        if num_bytes > lim_bytes {
            tracing::info!("User inputted too large of a message");
            return Err("Too large of a buffer.");
        }
    }
    Ok(())
}

async fn send_user_err(writer: &mut OwnedWriteHalf)
-> Result<(), Box<(dyn std::error::Error + Sync + std::marker::Send + 'static)>>
{
    writer.write_all(b"ERROR\n").await;
    Ok(())
}