## Simple Multi-user, Multi-room Asynchronous Chat Server
by ari i

+ Uses Tokio for multi-threaded, concurrent async IO for massive scalability
+ Rooms hold Peers in a HalfBrown container for fast size-dependent lookup & 
  search. It starts as a Vector & switches to rust-lang's Hashbrown after 32 
  users because Vectors are much faster to iterate through thanks to 
  contiguous memory allowing spacial locality, making it better for 
  broadcasting a message to each user. However, searching for a specific Peer 
  is takes O(n) time. This means that as the number of Peers in a Room grows, 
  it will take longer to add and remove peers. This is where Hashbrown's average 
  O(1) lookup time is utilized.
+ Uses reader-writer lock for more concurrent accesses to the state*
+ Leak-free thanks to 0 cycles in Arc<Rwlock<T>>
+ Kicks Peers who send inputs over 20,000 bytes long
+ Gracefully exits on SIGINT or SIGTERM, allowing Tokio to close ports
+ Users shouldn't be able to crash the program

### Further Optimizations + Scaling
The scalability of this architecture could be improved by moving away the 
reader-writer lock from the entire Shared Memory, and instead using a 
reader-writer lock on each Room. This change would require a statically-allocated
container (like an array) whose indices would represent the name of the Room hashed 
with the max room count. However, the specification does not mention
mention a maximum number of users or rooms, so the architecture goes instead for
flexibility of user count and room count at the cost of performance by using 
dynamically-allocated rooms & users structures.

## Building
```sh
cargo build --release
```

## Running
Without logging to stdout:
```sh
cargo run <port_id>
```
With logging to stdout:
```sh
cargo run <port_id> -v
```

## Joining
If joining on your machine, you can use localhost as your address:
```sh
telnet <address> <port_id>
```

Tested on Ubuntu 20.04