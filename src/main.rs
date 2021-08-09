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

use futures::SinkExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_stream::StreamExt;
use tokio_util::codec::{Framed, LinesCodec};
use tokio::signal::unix::{signal, SignalKind};
use tokio::io::AsyncReadExt;
use halfbrown;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // if these are locals I don't think there's much use in being constants
    let default_port = 1234u16;
    let default_host = "0.0.0.0";

    let state = Arc::new(Mutex::new(Shared::new())); // Shared state
    let port = env::args()
        .nth(1)
        .unwrap_or_else(|| default_port.to_string());
    let addr = format!("{}:{}",default_host, port);

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
    peers: halfbrown::HashMap<String, Room>, // Must be locked before writes to avoid data conflicts
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

struct Room {
    peers: HashMap<SocketAddr, Sx>
}

impl Room {
    fn new() -> Room {
        Self { peers: Default::default() }
    }

    async fn add_peer(&mut self, room_id: String, sx: Sx, peer: &Peer) -> io::Result<()> {
        let addr = peer.lines.get_ref().peer_addr()?;
        self.peers.insert(addr, sx);
        Ok(())
    }

    async fn rm_peer(&mut self, room_id: &str, addr: &SocketAddr) -> io::Result<()>{
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

impl Shared {
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

    // Send a `LineCodec` encoded message to every peer, except for the sender.
    // take things like SocketAddr by reference preferrably (though I think it might be Copy anyways)
    // same with &str; unless you need String use &str
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

/// Process an individual chat client
async fn process(
    state: Arc<Mutex<Shared>>,
    mut stream: TcpStream,
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
        Some(Ok(ln)) => {
            ln
        },
        // We didn't get a line so we return early here.
        _ => {
            tracing::error!("Failed to get username from {}. Client disconnected.", addr);
            return Ok(());
        }
    };
    // Register our peer with state which internally sets up some channels.
    let (mut peer, sx) = Peer::new(lines);
    // your way was fine but I prefer putting the Vec type in the call where it's used
    let vec = line.split_whitespace().collect::<Vec<_>>();
    let (_cmd, room_id, username) = match *vec.as_slice() {
        // if you match as a slice, you don't need to keep indexing,
        // which has bounds checking
        // this way there's only 1 bounds check and you get to name the variables
        [cmd, room_id, username] => {
            if cmd.to_uppercase() != "JOIN"
                || is_oob(&room_id)
                || is_oob(&username)
                || !username.is_ascii()
                || !room_id.is_ascii()
            {
                peer.lines.send("ERROR").await?;  // into() turns err into return type
                return Ok(());
            }
            (cmd, room_id, username)
        }
        _ => {
            peer.lines.send("ERROR").await?;  // into() turns err into return type
            return Ok(());
        }
    };
    // Add client to room
    {
        let mut state = state.lock().await;
        let mut room = state.get_room(room_id.to_string());
        room.add_peer(room_id.to_string(), sx, &peer).await?;
    } // Now let the lock go to let another processor run
    // A client has connected, let's let everyone know.
    {
        let mut state = state.lock().await;
        let mut room = state.get_room(room_id.to_string());
        let msg = format!("{} has joined", username);
        tracing::info!("{}", msg);
        room.broadcast(&addr, msg.as_str()).await;
    }


    let mut buf = [0u8; 20000];
    let ms = "".to_string();
    let mut amt: usize = 0;

    // let bytes_read = &peer.lines.get_mut().read(&mut buf).await.unwrap();
    // println!("{}", bytes_read);
        // Process incoming messages until our stream is exhausted by a disconnect.
    loop {
        tokio::select! {
            // A message was received from a peer. Send it to the current user.
            Some(msg) = peer.rx.recv() => {
                peer.lines.send(&msg).await?;
            }

            // Ok(bytes_read) = &peer.lines.get_mut().read(&mut buf) => {
            //     amt += bytes_read.unwrap();
            //     while let bytes_read = peer.lines.into_inner().read(&mut buf).await.unwrap() {
            //         amt += bytes_read;
            //         if amt > 1 {
            //             return Ok(())
            //         }
            //     }
            // }

            result = peer.lines.next() => match result {
                // A message was received from the current user, we should
                // broadcast this message to the other users.
                Some(Ok(msg)) => {
                    let mut state = state.lock().await;
                    let msg = format!("{}: {}", username, msg);
                    let mut room = state.get_room(room_id.to_string());
                    room.broadcast(&addr, msg.as_str()).await;
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
        let mut room = state.get_room(room_id.to_string());
        room.rm_peer(room_id, &addr).await;
        let msg = format!("{} has left the chat", username);
        tracing::info!("{}", msg);
        room.broadcast(&addr, msg.as_str()).await;
    }
    Ok(())
}