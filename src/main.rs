//! A chat server that broadcasts a message to all connections.
//!
//! This example is explicitly more verbose than it has to be. This is to
//! illustrate more concepts.
//!
//! A chat server for telnet clients. After a telnet client connects, the first
//! line should contain the client's name. After that, all lines sent by a
//! client are broadcasted to all other connected clients.
//!
//! Because the client is telnet, lines are delimited by "\r\n".
//!
//! You can test this out by running:
//!
//!     cargo run --example chat
//!
//! And then in another terminal run:
//!
//!     telnet localhost 6142
//!
//! You can run the `telnet` command in any number of additional windows.
//!
//! You can run the second command in multiple windows and then chat between the
//! two, seeing the messages from the other client as they're received. For all
//! connected clients they'll all join the same room and see everyone else's
//! messages.

#![warn(rust_2018_idioms)]

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex, oneshot};
use tokio::signal::unix::{signal, SignalKind};
use tokio_stream::StreamExt;
use tokio_util::codec::{Framed, LinesCodec};

use futures::SinkExt;
use std::collections::HashMap;
use std::env;
use tokio::sync::mpsc::{channel, Sender};
use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use futures::executor::block_on;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    const DEFAULT_PORT: u16 = 1234;
    const DEFAULT_HOST: &str = "0.0.0.0";

    let state = Arc::new(Mutex::new(Shared::new()));  // Shared state
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_HOST.to_owned() + ":" + &DEFAULT_PORT.to_string());
    let listener = TcpListener::bind(&addr).await?;

    tracing::info!("server running on {}", addr);

    // Graceful shutdown if called
    tokio::spawn(async move {
        return cancel_tasks().await;
    });

    loop {
        // Asynchronously wait for an inbound TcpStream.
        let (stream, addr) = listener.accept().await?;
        // Clone a handle to the `Shared` state for the new connection.
        let state = Arc::clone(&state);  // Get & increase ref counts
        // Spawn our handler to be run asynchronously.
        tokio::spawn(async move {
            tracing::debug!("accepted connection");
            if let Err(e) = process(state, stream, addr).await {
                tracing::info!("an error occurred; error = {:?}", e);
            }
        });
    }
}

/// Shorthand for the transmit half of the message channel.
type Sx = mpsc::UnboundedSender<String>;

/// Shorthand for the receive half of the message channel.
type Rx = mpsc::UnboundedReceiver<String>;

/// Data that is shared between all peers in the chat server.
///
/// This is the set of `Sx` handles for all connected clients. Whenever a
/// message is received from a client, it is broadcasted to all peers by
/// iterating over the `peers` entries and sending a copy of the message on each
/// `Sx`.
struct Shared {
    peers: HashMap<String, Vec<(SocketAddr, Sx)>>,  // Must be locked before writes to avoid data conflicts
}

/// The state for each connected client.
struct Peer {
    /// The TCP socket wrapped with the `Lines` codec, defined below.
    ///
    /// This handles sending and receiving data on the socket. When using
    /// `Lines`, we can work at the line level instead of having to manage the
    /// raw byte operations.
    lines: Framed<TcpStream, LinesCodec>,

    /// Receive half of the message channel.
    ///
    /// This is used to receive messages from peers. When a message is received
    /// off of this `Rx`, it will be written to the socket.
    rx: Rx,
}

impl Shared {
    /// Create a new, empty, instance of `Shared`.
    fn new() -> Self {
        Shared {
            peers: HashMap::new(),
        }
    }

    /// Send a `LineCodec` encoded message to every peer, except for the sender.
    async fn broadcast(&mut self, sender: SocketAddr, message: &str, room_id: String) {
        for peer in self.peers.get_mut(&room_id.to_string()).unwrap().iter_mut() {
            if peer.0 != sender {
                let _ = peer.1.send(message.into());
            }
        }
    }

    async fn add_peer(
        &mut self,
        room_id: String,
        sx: Sx,
        peer: &Peer,
    ) -> io::Result<()> {
        let addr = peer.lines.get_ref().peer_addr()?;
        let _test = self.peers
            .entry(room_id)
            .or_default() // Default if doesn't exist
            .push((addr, sx));
        Ok(())
    }

    async fn rm_peer(
        &mut self,
        roomid: String,
        addr: SocketAddr,
    ) -> io::Result<()>
    {
        let room_peers = self.peers.get_mut(&roomid.to_string()).unwrap();
        for (idx, peer) in room_peers.iter().enumerate() {
            if peer.0 == addr {
                room_peers.remove(idx);
                break;
            }
        }
        Ok(())
    }
}

impl Peer {
    /// Create a new instance of `Peer`.
    async fn new(
        lines: Framed<TcpStream, LinesCodec>,
    ) -> io::Result<(Peer, Sx)> {
        // Create a channel for this peer
        let (sx, rx) = mpsc::unbounded_channel();
        // Add an entry for this `Peer` in the shared state map.
        Ok((Peer { lines, rx }, sx))
    }
}

async fn cancel_tasks() -> Result<(), Box<dyn std::error::Error>>
{
    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        option = tokio::signal::ctrl_c() => {
            Ok(())
        },
        option = term.recv() => {
            Ok(())
        }
    }
}

/// Process an individual chat client
async fn process(
    state: Arc<Mutex<Shared>>,
    stream: TcpStream,
    addr: SocketAddr,
) -> Result<(), Box<dyn Error>> {
    let mut lines = Framed::new(stream, LinesCodec::new());
    // Send a prompt to the client to enter their username.
    lines.send("Please enter your username:").await?;
    // Read the first line from the `LineCodec` stream to get the username.
    let line = match lines.next().await {
        Some(Ok(ln)) => ln,
        // We didn't get a line so we return early here.
        _ => {
            tracing::error!("Failed to get username from {}. Client disconnected.", addr);
            return Ok(());
        }
    };
    // Register our peer with state which internally sets up some channels.
    let (mut peer, sx) = Peer::new(lines).await?;
    // process line
    let vec: Vec<&str> = line.split_whitespace().collect();
    let (_cmd, roomid, username): (&str, &str, &str) = match vec.len() {
        3 => {
            if vec[0].to_uppercase() != "JOIN" {
                // TODO can I send this arm to the outermost _ arm?
                tracing::error!("Incorrect input");
                return Ok(());
            }
            (vec[0], vec[1], vec[2])
        }
        _ => {
            // TODO this isn't sent in time
            peer.lines.send("Error").await?;  // into() turns err into return type
            return Ok(());
        }
    };
    // Add client to room
    {
        let mut state = state.lock().await;
        state.add_peer(roomid.to_string(), sx, &peer).await?;
    }
    // A client has connected, let's let everyone know.
    {
        let mut state = state.lock().await;
        let msg = format!("{} has joined the chat", username);
        tracing::info!("{}", msg);
        state.broadcast(addr, &msg, roomid.into()).await;
    }
    // Process incoming messages until our stream is exhausted by a disconnect.
    loop {
        tokio::select! {
            // A message was received from a peer. Send it to the current user.
            Some(msg) = peer.rx.recv() => {
                peer.lines.send(&msg).await?;
            }
            result = peer.lines.next() => match result {
                // A message was received from the current user, we should
                // broadcast this message to the other users.
                Some(Ok(msg)) => {
                    let mut state = state.lock().await;
                    let msg = format!("{}: {}", username, msg);
                    state.broadcast(addr, &msg, roomid.to_string()).await;
                }
                // An error occurred.
                Some(Err(e)) => {
                    tracing::error!(
                        "an error occurred while processing messages for {}; error = {:?}",
                        username,
                        e
                    );
                }
                // The stream has been exhausted.
                None => break,
            },
        }
    }
    // If this section is reached it means that the client was disconnected!
    // Let's let everyone still connected know about it.
    {
        let mut state = state.lock().await;
        state.rm_peer(roomid.to_string(), addr).await?;
        let msg = format!("{} has left the chat", username);
        tracing::info!("{}", msg);
        state.broadcast(addr, &msg, roomid.to_string()).await;
    }
    Ok(())
}