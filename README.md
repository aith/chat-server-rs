## Simple Multi-user, Multi-room Asynchronous Chat Server

+ Uses Tokio for multi-threaded, concurrent async IO for massive scalability
+ Rooms hold Peers in a HalfBrown container for fast size-dependent lookup & search. It starts as a Vector & switches to rust-lang's Hashbrown after 32 users because Vectors are much faster to iterate through thanks to contiguous memory allowing spacial locality, making it better for broadcasting a message to each user. However, searching for a specific Peer is takes O(n) time. This means that as the number of Peers in a Room grows, it will take longer to add and remove peers. This is where Hashbrown's average O(1) lookup time is utilized.
+ Leak-free thanks to Rust
+ Kicks Peers who send inputs over 20,000 bytes long
+ Gracefully exits on SIGINT or SIGTERM, allowing Tokio to close ports
