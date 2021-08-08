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

// I ran cargo fmt (rustfmt)
#![warn(rust_2018_idioms)]

use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use futures::SinkExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_stream::StreamExt;
use tokio_util::codec::{Framed, LinesCodec};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // if these are locals I don't think there's much use in being constants
    let default_port = 1234u16;
    let default_host = "0.0.0.0";

    let state = Arc::new(Mutex::new(Shared::new())); // Shared state
    let addr = env::args()
        .nth(1)
        // format!'s nicer here
        .unwrap_or_else(|| format!("{}:{}", default_host, default_port));
    let listener = TcpListener::bind(&addr).await?;

    tracing::info!("server running on {}", addr);

    // Graceful shutdown if called
    // commented out for now so SIGINT works
    // cancel_tasks().await;

    loop {
        // Asynchronously wait for an inbound TcpStream.
        let (stream, addr) = listener.accept().await?;
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
    // changed the Vec of pairs to a HashMap
    // can still easily iterate through the entries as pairs
    // but you can also easily remove by SocketAddr
    peers: HashMap<String, HashMap<SocketAddr, Sx>>, // Must be locked before writes to avoid data conflicts
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
        // nicer and more idiomatic to use Self instead of repeating the type
        Self {
            peers: HashMap::new(),
        }
    }

    /// Send a `LineCodec` encoded message to every peer, except for the sender.
    // take things like SocketAddr by reference preferrably (though I think it might be Copy anyways)
    // same with &str; unless you need String use &str
    async fn broadcast(&mut self, sender: &SocketAddr, message: &str, room_id: &str) {
        self.peers
            // you probably can rework things so a broadcast, etc. is room specific,
            // so you wouldn't need this lookup (and it couldn't fail that way)
            .get_mut(room_id)
            .expect("only call broadcast with an existing room_id")
            .iter_mut()
            // to me the functional way shows intent better
            // your way is fine though
            .filter(|(socket, _sx)| *socket != sender)
            .map(|(_socket, sx)| sx)
            .for_each(|sx| {
                let _ = sx.send(message);
            });
    }

    async fn add_peer(&mut self, room_id: String, sx: Sx, peer: &Peer) -> io::Result<()> {
        let addr = peer.lines.get_ref().peer_addr()?;
        self.peers
            .entry(room_id)
            .or_default() // Default if doesn't exist
            .insert(addr, sx);
        Ok(())
    }

    async fn rm_peer(&mut self, room_id: &str, addr: &SocketAddr) {
        self.peers
            .get_mut(room_id)
            .expect("only call rm_peer with an existing room_id")
            // now that it's a HashMap you can easily remove in O(1)
            .remove(addr);
    }
}

impl Peer {
    /// Create a new instance of `Peer`.
    // No need to be async or return a Result if you're not using await or returning errors
    fn new(lines: Framed<TcpStream, LinesCodec>) -> (Self, Sx) {
        // Create a channel for this peer
        let (sx, rx) = mpsc::unbounded_channel();
        // Add an entry for this `Peer` in the shared state map.
        (Self { lines, rx }, sx)
    }
}

// no need to return a result if it's always Ok(())
async fn cancel_tasks() {
    println!("waiting for ctrl-c");
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for event");
    println!("received ctrl-c event");
}

/// Process an individual chat client
async fn process(
    state: Arc<Mutex<Shared>>,
    stream: TcpStream,
    addr: SocketAddr,
    // Box<dyn Error + Send + Sync + 'static> is better for errors
    // and most errors should be that anyways
    // you could also use the crate anyhow, which makes things more convenient
) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
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
    let (mut peer, sx) = Peer::new(lines);
    // process line
    // your way was fine but I prefer putting the Vec type in the call where it's used
    let vec = line.split_whitespace().collect::<Vec<_>>();
    let (_cmd, room_id, username) = match *vec.as_slice() {
        // if you match as a slice, you don't need to keep indexing,
        // which has bounds checking
        // this way there's only 1 bounds check and you get to name the variables
        [cmd, room_id, username] => {
            if cmd.to_uppercase() != "JOIN" {
                // TODO can I send this arm to the outermost _ arm?
                tracing::error!("Incorrect input");
                return Ok(());
            }
            (cmd, room_id, username)
        }
        _ => {
            // TODO this isn't sent in time
            // let (send, mut recv) = mpsc::channel::<String>(1);
            // {
            //     tokio::spawn(async move {
            //         let _test = sx.send("Error".to_string());
            //     });
            //
            // }
            // drop(send);
            // let _ = recv.recv();

            println!("Wrong input");
            tracing::error!("Incorrect input");
            return Ok(());
        }
    };
    // Add client to room
    {
        let mut state = state.lock().await;
        state.add_peer(room_id.to_string(), sx, &peer).await?;
    }
    // A client has connected, let's let everyone know.
    {
        let mut state = state.lock().await;
        let msg = format!("{} has joined the chat", username);
        tracing::info!("{}", msg);
        state.broadcast(&addr, msg.as_str(), room_id).await;
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
                    state.broadcast(&addr, msg.as_str(), room_id).await;
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
        state.rm_peer(room_id, &addr).await;
        let msg = format!("{} has left the chat", username);
        tracing::info!("{}", msg);
        state.broadcast(&addr, msg.as_str(), room_id).await;
    }
    Ok(())
}
