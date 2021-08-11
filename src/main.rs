#![warn(rust_2018_idioms)]

use std::collections::HashMap;
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
use std::io::ErrorKind;

use clap::{Arg, App, ArgMatches};

/// Run Server
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let default_port = "1234";
    let default_host = "0.0.0.0";
    let matches = setup_args();
    let port = matches.value_of("port").unwrap_or_else(|| default_port);
    // Init Logging
    if matches.is_present("is_pretty_printing") {
        let _subscriber = FmtSubscriber::new();
        let _ = tracing::subscriber::set_global_default(_subscriber)
            .map_err(|_err| eprintln!("Unable to set global default subscriber")).unwrap();
    }
    //Get Connection
    let state = Arc::new(RwLock::new(SharedMemory::new()));

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
async fn process_client( state: Arc<RwLock<SharedMemory>>, stream: TcpStream, addr: SocketAddr,)
    -> Result<(), Box<dyn Error + Send + Sync + 'static>>
{
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
            tracing::warn!("Cannot parse line from {}.", addr);
            return send_user_err(&mut line_stream).await;
        }
    };
    let (_cmd, room_id, username)  = match get_cmd(&addr, &line).await {
        Ok((cmd, room_id, username)) => { (cmd, room_id, username) },
        _ => {
            let _ = send_user_err(&mut line_stream).await;
            return Ok(());
        }
    };
    let room_id = &room_id; // Convert to &str to comply with Lifetimes TODO make succinct
    let username = &username;
    let (mut peer, sx) = Peer::new(line_stream);
    let _ = create_peer(sx, &addr, &state, room_id, username).await;
    let _ = alert_peers(&state, room_id, username).await;
    let _ = handle_messaging(&mut peer, &state, room_id, username).await;
    let _ = exit_client(&addr, &state, room_id, username).await;
    Ok(())
}

async fn get_cmd(addr: &SocketAddr, line: &String)
    -> Result<(String, String, String), Box<dyn Error + Send + Sync + 'static>>
{
    let vec = line.split_whitespace().collect::<Vec<_>>(); // Split words across vec
    match *vec.as_slice() {
        [cmd, room_id, username] => {
            if cmd.to_uppercase() != "JOIN"
                || is_oob(room_id)
                || is_oob(username)
                || !username.is_ascii()
                || !room_id.is_ascii()
            {
                tracing::warn!("{} failed cmd constraints: {} {} {}", addr, cmd, room_id, username);
            } else {
                return Ok((cmd.to_string(),
                           room_id.to_string(),
                           username.to_string()))
            }
        }
        _ => {
            tracing::warn!("{} improperly formed cmd", addr);
        }
    };
    Err(std::io::Error::new(ErrorKind::InvalidInput, format!("Invalid command from {}", addr)))?
}

async fn create_peer(sx: Sx, peer_addr: &SocketAddr, state: &Arc<RwLock<SharedMemory>>, room_id: &str, username: &str)
    -> Result<(), Box<dyn Error + Send + Sync + 'static>>
{
    let mut state = state.write().await;
    let room = state.get_room_mut(room_id);
    room.add_peer(sx, &peer_addr);
    tracing::info!("+Room {}, User: {}", room_id, username);
    Ok(())
}

async fn alert_peers(state: &Arc<RwLock<SharedMemory>>, room_id: &str, username: &str)
    -> Result<(), Box<dyn Error + Send + Sync + 'static>>
{
    let state = state.read().await;
    let room = match state.get_room(room_id) {
        Some(room) => room,
        _ => {
            tracing::warn!("{} could not alert peers in Room {}", username, room_id);
            return Ok(())
        }
    };
    let msg = format!("{} has joined", username);
    let _ = room.broadcast(msg.as_str()).await;
    Ok(())
}

/// Messaging recv + send + Peer Disconnect loop
async fn handle_messaging(peer: &mut Peer, state: &Arc<RwLock<SharedMemory>>, room_id: &str, username: &str)
    -> Result<(), Box<dyn Error + Send + Sync + 'static>>
{
    loop {
        tokio::select! {
            // Peer received message
            Some(msg) = peer.rx.recv() => {
                peer.reader.send(&msg).await?;
            }
            // User wrote message
            result = peer.reader.next() => match result {
                Some(Ok(msg)) => {
                    let msg = format!("{}: {}", username, msg);
                    tracing::info!("+Message: {}", msg);
                    let state = state.read().await;
                    let room = match state.get_room(room_id) {
                        Some(room) => room,
                        _ => {
                            tracing::warn!("{} could not message peers {} in Room", username, room_id);
                            return Ok(())
                        }
                    };
                    let _ = room.broadcast(&msg).await;
                }
                //
                Some(Err(e)) => {
                    tracing::error!( "Error while processing messages for {}; {:?}", username, e);
                    // Disconnect peer to prevent potential DoS
                    return Ok(())
                }
                // Peer disconnected
                None => {return Ok(());}
            },
        }
    }
}

/// Clean up client
async fn exit_client(peer_addr: &SocketAddr, state: &Arc<RwLock<SharedMemory>>, room_id: &str, username: &str)
-> Result<(), Box<dyn Error + Send + Sync + 'static>>
{
    let mut state = state.write().await;
    let room = state.get_room_mut(room_id);
    room.rm_peer(&peer_addr);
    let msg = format!("{} has left", username);
    tracing::info!("-Room {}, User: {}", room_id, username);
    let _ = room.broadcast(msg.as_str()).await;
    if room.is_empty() {
        state.rooms.remove(room_id);
        tracing::info!("Room {} removed", room_id);
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

    fn get_room(&self, key: &str) -> Option<&Room> {
        // expect ok here b/c key should statically be known to exist
        self.rooms.get(key)
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
    async fn broadcast(&self, message: &str)
        -> Result<(), Box<dyn Error + Send + Sync + 'static>>
    {
        self.peers.iter().map(|(_socket, sx)| sx).for_each(|sx| {
            let _ = sx.send(message.into());
        });
        Ok(())
    }
}

fn setup_args() -> ArgMatches {
    App::new("A Chat Server :)")
        .version("1.0")
        .author("ari i")
        .about("Multi-user, multi-room chat server")
        .arg(Arg::new("port")
            .index(1)
            .required(false)
        )
        .arg(Arg::new("is_pretty_printing")
            .short('v')
            .long("verbose")
            .required(false)
        )
        .get_matches()
}