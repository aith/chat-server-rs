#![warn(rust_2018_idioms)]

use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::BytesMut;
use halfbrown; // For efficient Room size-based lookup & search

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
// Tokio's RwLock has a solid fairness policy
use tokio::signal::unix::{signal, SignalKind};
use tokio::io::{AsyncReadExt, BufReader, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

// For printing logs to stdout
use tracing_subscriber::{FmtSubscriber};

/// Run Server
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Init Logging
    let _subscriber = FmtSubscriber::new();
    // To print log to stdout, enable the following lines
    // let _ = tracing::subscriber::set_global_default(_subscriber)
    //     .map_err(|_err| eprintln!("Unable to set global default subscriber")).unwrap();

    //Get Connection
    let default_port = 1234u16;
    let default_host = "0.0.0.0";
    let state = Arc::new(RwLock::new(SharedMemory::new()));
    let port = get_port(default_port);
    let addr = format!("{}:{}", default_host, port);
    let listener = TcpListener::bind(&addr).await.expect("Port could not be acquired");
    tracing::info!("Server started on: {}", addr);
    tokio::spawn(async move {
        loop {
            // async wait for an inbound TcpStream
            let (stream, addr) = listener.accept().await.unwrap();
            tracing::info!("Server accepted: {}", stream.peer_addr()
                .expect("Could not get address of new connection").to_string());
            // clone a handle to the `Shared` state for the new connection.
            let state = state.clone(); // Get & increase ref counts
            // spawn our handler to be run asynchronously.
            tokio::spawn(async move {
                if let Err(e) = process_client(state, stream, addr).await {
                    tracing::error!("Server error: {:?}", e);
                }
            });
        }
    });
    // Graceful shutdown
    handle_signals().await
}

/// Process an individual chat client
async fn process_client(
    state: Arc<RwLock<SharedMemory>>,
    stream: TcpStream,
    addr: SocketAddr,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    let msg_max = 20000usize;
    let mut buffer = BytesMut::with_capacity(1000);
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send a prompt to the client to enter their username.
    writer.write_all(b"Welcome! Use the command:\nJOIN <ROOMNAME> <USERNAME>\n").await?;

    // Keep reading into buffer until newline is found
    match limited_read(&mut reader, &mut buffer, 0, msg_max).await {
        Ok(_result) => {}
        _ => {
            return send_user_err(&mut writer).await;
        }
    }
    // Parse line
    let line = match std::str::from_utf8(&buffer.to_vec()) {
        Ok(msg) => { msg.to_string() },
        _ => {
            tracing::warn!("Cannot parse line from {} into utf8.", addr);
            return send_user_err(&mut writer).await
        }
    };

    // Get JOIN, Room name, and Peer name
    let vec = line.split_whitespace().collect::<Vec<_>>(); // Split words across vec
    let (_cmd, room_id, username) = match *vec.as_slice() {
        [cmd, room_id, username] => {
            if cmd.to_uppercase() != "JOIN"
                || is_oob(&room_id)
                || is_oob(&username)
                || !username.is_ascii()
                || !room_id.is_ascii()
            { return send_user_err(&mut writer).await; } else { (cmd, room_id, username) }
        }
        _ => { return send_user_err(&mut writer).await; }
    };
    // Register our Peer with state which internally sets up some channels
    let (mut peer, sx) = Peer::new(reader);
    {
        let mut state = state.write().await;
        let room = state.get_room_mut(room_id);
        room.add_peer(sx, &addr);
        tracing::info!("+Room {}, User: {}", room_id, username);
    }
    // Alert all other Peers in the Room of new Peer
    {
        let state = state.read().await;
        let room = state.get_room(room_id);
        let msg = format!("{} has joined\n", username);
        room.broadcast(msg.as_str()).await;
    }
    buffer.clear();
    // Process incoming messages until our stream is exhausted by a disconnect
    loop {
        tokio::select! {
            result = peer.reader.read_buf(&mut buffer) => {
                match result {
                    Ok(bytes_read) => {
                        if bytes_read == 0 { break; } // Peer left
                        match limited_read(&mut peer.reader, &mut buffer, bytes_read, msg_max).await {
                            Ok(()) => {
                                match std::str::from_utf8(&buffer) {
                                    Ok(msg) => {
                                        let state = state.read().await;
                                        let msg = format!("{}: {}", username, msg);
                                        tracing::info!("+Message: {}", msg);
                                        let room = state.get_room(room_id);
                                        room.broadcast(msg.as_str()).await;
                                    },
                                    _ => {
                                        // User entered non-utf8 message, so ignore it
                                        tracing::warn!("Cannot parse {}'s msg into utf8.", username);
                                    }
                                }
                            },
                            _ => {  // Large message -> kick user
                                send_user_err(&mut writer).await.expect("Could not send user err.");
                                tracing::warn!("-User {} tried to sent massive message.", username);
                                break;
                            }
                        }
                    },
                    _ => {
                        tracing::warn!("-User {} IO Error.", username);
                        break;
                    },
                }
                buffer.clear();  // Reuse buffer
            }
            // A message was received from a peer. Send it to the current user.
            Some(msg) = peer.rx.recv() => {
                writer.write_all(&msg.into_bytes()).await?;
            }
        }
    }
    {
        // Exit Client
        let mut state = state.write().await;
        let room = state.get_room_mut(room_id);
        room.rm_peer(&addr);
        let msg = format!("{} has left\n", username);
        tracing::info!("-Room {}, User: {}", room_id, username);
        room.broadcast(msg.as_str()).await;
        if room.is_empty() { state.rooms.remove(room_id); }
    }
    Ok(())
}

/// Repeatedly read into buffer until
///  1) given byte limit is reached
///  2) reaches newline
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
            return Err("Too large of a buffer.");
        }
    }
    Ok(())
}

/// Write generic ERROR message to peer connection
async fn send_user_err(writer: &mut OwnedWriteHalf)
                       -> Result<(), Box<(dyn std::error::Error + Sync + std::marker::Send + 'static)>>
{
    writer.write_all(b"ERROR\n").await.unwrap();
    Ok(())
}

/// Signal Handler
async fn handle_signals() -> Result<(), Box<dyn std::error::Error>>
{
    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        _option = tokio::signal::ctrl_c() => {
            tracing::info!("Server shutting down on SIGINT");
            Ok(())
        },
        _option = term.recv() => {
            tracing::info!("Server shutting down on SIGINT");
            Ok(())
        }
    }
}

/// Check if str is out of the 1-20 characters length bounds
fn is_oob(st: &str) -> bool {
    let length = st.len();
    return length < 1 || length > 20;
}

/// Return inputted Port or default Port
fn get_port(default: u16) -> String {
    env::args()
        .nth(1)
        .unwrap_or_else(|| {
            println!("Using default port: 1234");
            default.to_string()
        })
}

/// Send half of channel
type Sx = mpsc::UnboundedSender<String>;
/// Receive half of channel
type Rx = mpsc::UnboundedReceiver<String>;

/// Memory that each thread & task shares
struct SharedMemory {
    // Halfbrown: uses Vec at low count (>32) for faster iterations
    //            but switches to HashMap after that for faster lookup
    rooms: halfbrown::HashMap<String, Room>,
}

impl SharedMemory {
    /// Create a new, empty, instance of `Shared`.
    fn new() -> Self {
        Self {
            rooms: halfbrown::HashMap::new(),
        }
    }

    fn get_room(&self, key: &str) -> &Room {
        return self.rooms.get(key).expect("Room does not exist");
    }

    fn get_room_mut(&mut self, key: &str) -> &mut Room {
        return self.rooms.entry(key.to_string()).or_insert(Room::new());
    }
}

/// Connected Client
struct Peer {
    reader: BufReader<OwnedReadHalf>,
    rx: Rx,
}

impl Peer {
    fn new(lines: BufReader<OwnedReadHalf>) -> (Self, Sx) {
        // Create a channel for this peer
        let (sx, rx) = mpsc::unbounded_channel();
        // Add an entry for this `Peer` in the shared state map.
        (Self { reader: lines, rx }, sx)
    }
}

/// Contains Peers
struct Room {
    peers: HashMap<SocketAddr, Sx>,
}

impl Room {
    fn new() -> Room {
        Self { peers: Default::default() }
    }

    fn is_empty(&self) -> bool {
        return self.peers.len() <= 0;
    }

    fn add_peer(&mut self, sx: Sx, peer_addr: &SocketAddr) {
        // TODO change to try_insert when stable
        self.peers.insert(*peer_addr, sx);
    }

    fn rm_peer(&mut self, peer_addr: &SocketAddr) {
        self.peers.remove(peer_addr);
    }

    /// Send a message ot each Peer in the Room
    async fn broadcast(&self, message: &str) {
        self.peers
            .iter()
            // .filter(|(socket, _sx)| *socket != sender)
            .map(|(_socket, sx)| sx)
            .for_each(|sx| {
                let _ = sx.send(message.into());
            });
    }
}
