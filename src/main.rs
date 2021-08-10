#![warn(rust_2018_idioms)]

use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};
use tokio::signal::unix::{signal, SignalKind};
// Tokio's RwLock has a solid fairness policy
use tokio::sync::{mpsc, RwLock};
use tokio_stream::StreamExt;

// For printing logs to stdout
use tracing_subscriber::FmtSubscriber;

use tokio_util::codec::{Framed, LinesCodec};
use futures::SinkExt;

/// Run Server
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Init Logging
    let _subscriber = FmtSubscriber::new();
    // To print log to stdout, enable the following lines
    let _ = tracing::subscriber::set_global_default(_subscriber)
        .map_err(|_err| eprintln!("Unable to set global default subscriber")).unwrap();

    //Get Connection
    let default_port = 1234u16;
    let default_host = "0.0.0.0";
    let state = Arc::new(RwLock::new(SharedMemory::new()));
    let port = get_port(default_port);
    let addr = format!("{}:{}", default_host, port);
    let listener = TcpListener::bind(&addr).await.expect("Couldn't acquire port.");
    tracing::info!("Server started on: {}", addr);
    tokio::spawn(async move {
        loop {
            let (stream, addr) = match listener.accept().await {
                Ok(it) => it,
                Err(e) => {
                    tracing::error!("Could not accept connection. Error: {}", e);
                    continue;
                }
            };
            tracing::info!(
                "Server accepted: {}",
                stream
                    .peer_addr()
                    .map(|it| it.to_string())
                    .unwrap_or_else(|_| "Could not get address of new connection".into())
            );
            // clone a handle to the `Shared` state for the new connection.
            let state = state.clone(); // Get & increase ref counts
            // run async spawner
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
    let mut line_stream = Framed::new(
        stream,
        LinesCodec::new_with_max_length(msg_max) // To prevent DoS
    );

    // Send a prompt to the client to enter their username.
    line_stream .send("Welcome! Use the command:\nJOIN <ROOMNAME> <USERNAME>") .await?;

    // Parse line
    let line = match line_stream.next().await {
        Some(Ok(msg)) => msg,
        _ => {
            tracing::warn!("Cannot parse line from {} into utf8.", addr);
            return send_user_err(&mut line_stream).await;
        }
    };
    // Get JOIN, Room name, and Peer name
    let vec = line.split_whitespace().collect::<Vec<_>>(); // Split words across vec
    let (_cmd, room_id, username) = match *vec.as_slice() {
        [cmd, room_id, username] => {
            if cmd.to_uppercase() != "JOIN"
                || is_oob(room_id)
                || is_oob(username)
                || !username.is_ascii()
                || !room_id.is_ascii()
            {
                tracing::warn!("{} failed cmd constraints: {} {} {}", addr, cmd, room_id, username);
                return send_user_err(&mut line_stream).await;
            } else {
                (cmd, room_id, username)
            }
        }
        _ => {
            tracing::warn!("{} improperly formed cmd", addr);
            return send_user_err(&mut line_stream).await;
        }
    };
    // Register our Peer with state which internally sets up some channels
    let (mut peer, sx) = Peer::new(line_stream);
    // that way you just do 1 state.get_room_mut(room_id) lookup at the beginning
    // and then lock the room instead of the whole state each time you access it
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
        let msg = format!("{} has joined", username);
        room.broadcast(msg.as_str()).await;
    }
    loop {
        tokio::select! {
            Some(msg) = peer.rx.recv() => {
                peer.reader.send(&msg).await?;
            }
            result = peer.reader.next() => match result {
                Some(Ok(msg)) => {
                    let msg = format!("{}: {}", username, msg);
                    tracing::info!("+Message: {}", msg);
                    let state = state.read().await;
                    let room = state.get_room(room_id);
                    room.broadcast(&msg).await;
                }
                //
                Some(Err(e)) => {
                    tracing::error!( "Error while processing messages for {}; {:?}", username, e);
                    // Disconnect peer to prevent potential DoS
                    break;
                }
                // Peer disconnected
                None => break,
            },
        }
    }
    {
        // Exit Client
        let mut state = state.write().await;
        let room = state.get_room_mut(room_id);
        room.rm_peer(&addr);
        let msg = format!("{} has left", username);
        tracing::info!("-Room {}, User: {}", room_id, username);
        room.broadcast(msg.as_str()).await;
        if room.is_empty() {
            state.rooms.remove(room_id);
        }
    }
    Ok(())
}

/// Write generic ERROR message to peer connection
async fn send_user_err(
    writer: &mut Framed<TcpStream, LinesCodec>,
) -> Result<(), Box<dyn Error + Sync + Send + 'static>> {
    writer.send("ERROR").await?;
    Ok(())
}

/// Signal Handler
async fn handle_signals() -> Result<(), Box<dyn Error>> {
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
    !(1..=20).contains(&st.len())
}

/// Return inputted Port or default Port
fn get_port(default: u16) -> String {
    env::args().nth(1).unwrap_or_else(|| {
        println!("Using default port: {}", default);
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
        // expect ok here b/c key should statically be known to exist
        self.rooms.get(key).expect("Room does not exist")
    }

    fn get_room_mut(&mut self, key: &str) -> &mut Room {
        self.rooms.entry(key.into()).or_insert(Room::new())
    }
}

/// Connected Client
struct Peer {
    reader: Framed<TcpStream, LinesCodec>,
    rx: Rx,
}

impl Peer {
    fn new(lines: Framed<TcpStream, LinesCodec>) -> (Self, Sx) {
        // Create a channel for this peer
        let (sx, rx) = mpsc::unbounded_channel();
        // Add an entry for this `Peer` in the shared state map.
        (Self { reader: lines, rx }, sx)
    }
}

/// Contains Peers
#[derive(Default)]
struct Room {
    peers: HashMap<SocketAddr, Sx>,
}

impl Room {
    fn new() -> Room {
        Self::default()
    }

    fn is_empty(&self) -> bool {
        self.peers.is_empty()
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
        self.peers.iter().map(|(_socket, sx)| sx).for_each(|sx| {
            let _ = sx.send(message.into());
        });
    }
}
