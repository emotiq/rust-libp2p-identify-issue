// Copyright 2017 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

extern crate bytes;
extern crate env_logger;
extern crate futures;
extern crate libp2p;
extern crate tokio_codec;
extern crate tokio;

use futures::future::{loop_fn, Future, IntoFuture, Loop};
use futures::{Sink, Stream};
use std::env;
use libp2p::SimpleProtocol;
use libp2p::core::Transport;
use libp2p::core::{upgrade, either::EitherOutput, PublicKey};
use libp2p::tcp::TcpConfig;
use libp2p::peerstore::{PeerAccess, PeerId, Peerstore};
use libp2p::multiaddr::Multiaddr;
use tokio_codec::{BytesCodec, Framed};
use libp2p::websocket::WsConfig;
use tokio::runtime::current_thread::block_on_all;
use std::time::Duration;
use std::sync::Arc;

fn main() {
    env_logger::init();

    // Determine which address to listen to.
    let listen_addr = env::args()
        .nth(1)
        .unwrap_or("/ip4/0.0.0.0/tcp/10333".to_owned());

    // We start by creating a `TcpConfig` that indicates that we want TCP/IP.
    let transport = TcpConfig::new()
        // In addition to TCP/IP, we also want to support the Websockets protocol on top of TCP/IP.
        // The parameter passed to `WsConfig::new()` must be an implementation of `Transport` to be
        // used for the underlying multiaddress.
        .or_transport(WsConfig::new(TcpConfig::new()))

        // On top of TCP/IP, we will use either the plaintext protocol or the secio protocol,
        // depending on which one the remote supports.
        .with_upgrade({
            let plain_text = upgrade::PlainTextConfig;

            let secio = {
                let private_key = include_bytes!("test-rsa-private-key.pk8");
                let public_key = include_bytes!("test-rsa-public-key.der").to_vec();
                libp2p::secio::SecioConfig {
                    key: libp2p::secio::SecioKeyPair::rsa_from_pkcs8(private_key, public_key).unwrap(),
                }
            };

            upgrade::or(
                upgrade::map(plain_text, |pt| EitherOutput::First(pt)),
                upgrade::map(secio, |out: libp2p::secio::SecioOutput<_>| EitherOutput::Second(out.stream))
            )
        })

        // On top of plaintext or secio, we will use the multiplex protocol.
        .with_upgrade(libp2p::mplex::MplexConfig::new())
        // The object returned by the call to `with_upgrade(MplexConfig::new())` can't be used as a
        // `Transport` because the output of the upgrade is not a stream but a controller for
        // muxing. We have to explicitly call `into_connection_reuse()` in order to turn this into
        // a `Transport`.
        .map(|val, _| ((), val))
        .into_connection_reuse()
        .map(|((), val), _| val);

    // We now have a `transport` variable that can be used either to dial nodes or listen to
    // incoming connections, and that will automatically apply secio and multiplex on top
    // of any opened stream.

    let peer_store = Arc::new(libp2p::peerstore::memory_peerstore::MemoryPeerstore::empty());
    ipfs_bootstrap(&*peer_store);

    let addr_resolver = {
        let peer_store = peer_store.clone();
        move |peer_id| {
            peer_store
                .peer(&peer_id)
                .into_iter()
                .flat_map(|peer| peer.addrs())
                .collect::<Vec<_>>()
                .into_iter()
        }
    };

    let transport = libp2p::identify::PeerIdTransport::new(transport, addr_resolver)
        .and_then({
            let peer_store = peer_store.clone();
            move |id_out, _, remote_addr| {
                let socket = id_out.socket;
                let original_addr = id_out.original_addr;
                println!("Trying to extract peer_id from id_out");
                id_out.info.map(move |info| {
                    println!("received id_out.info: {:?}", info);
                    let peer_id = info.info.public_key.into_peer_id();
                    println!("Trying to store peer_id: {:?}", peer_id);
                    peer_store.peer_or_create(&peer_id).add_addr(original_addr, Duration::from_secs(3600));
                    println!("connection with peer: {:?}", peer_id);
                    (socket, remote_addr)
                })
            }
        });

    let my_peer_id = PeerId::from_public_key(PublicKey::Rsa(include_bytes!("test-rsa-public-key.der").to_vec()));
    println!("Local peer id is: {:?}", my_peer_id);

    // We now prepare the protocol that we are going to negotiate with nodes that open a connection
    // or substream to our server.
    let proto = SimpleProtocol::new("/echo/1.0.0", |socket| {
        // This closure is called whenever a stream using the "echo" protocol has been
        // successfully negotiated. The parameter is the raw socket (implements the AsyncRead
        // and AsyncWrite traits), and the closure must return an implementation of
        // `IntoFuture` that can yield any type of object.
        Ok(Framed::new(socket, BytesCodec::new()))
    });

    // Let's put this `transport` into a *swarm*. The swarm will handle all the incoming and
    // outgoing connections for us.
    let (swarm_controller, swarm_future) = libp2p::core::swarm(
        transport.clone().with_upgrade(proto),
        |socket, _client_addr| {
            println!("Successfully negotiated protocol");

            // The type of `socket` is exactly what the closure of `SimpleProtocol` returns.

            // We loop forever in order to handle all the messages sent by the client.
            loop_fn(socket, move |socket| {
                socket
                    .into_future()
                    .map_err(|(e, _)| e)
                    .and_then(move |(msg, rest)| {
                        if let Some(msg) = msg {
                            // One message has been received. We send it back to the client.
                            println!(
                                "Received a message: {:?}\n => Sending back \
                                 identical message to remote", msg
                            );
                            Box::new(rest.send(msg.freeze()).map(|m| Loop::Continue(m)))
                                as Box<Future<Item = _, Error = _>>
                        } else {
                            // End of stream. Connection closed. Breaking the loop.
                            println!("Received EOF\n => Dropping connection");
                            Box::new(Ok(Loop::Break(())).into_future())
                                as Box<Future<Item = _, Error = _>>
                        }
                    })
            })
        },
    );

    // We now use the controller to listen on the address.
    let address = swarm_controller
        .listen_on(listen_addr.parse().expect("invalid multiaddr"))
        // If the multiaddr protocol exists but is not supported, then we get an error containing
        // the original multiaddress.
        .expect("unsupported multiaddr");
    // The address we actually listen on can be different from the address that was passed to
    // the `listen_on` function. For example if you pass `/ip4/0.0.0.0/tcp/0`, then the port `0`
    // will be replaced with the actual port.
    println!("Now listening on {:?}", address);

    // `swarm_future` is a future that contains all the behaviour that we want, but nothing has
    // actually started yet. Because we created the `TcpConfig` with tokio, we need to run the
    // future through the tokio core.
    block_on_all(swarm_future.for_each(|_| Ok(()))).unwrap();
}

/// Stores initial addresses on the given peer store. Uses a very large timeout.
pub fn ipfs_bootstrap<P>(peer_store: P)
where
    P: Peerstore + Clone,
{
    const ADDRESSES: &[&str] = &[
        "/ip4/127.0.0.1/tcp/10333/p2p/QmbEM3WYRJHEjEknK67akUBjWHicVXDQ91EHkjDNjxXCKv",
        // TODO: add some bootstrap nodes here
    ];

    let ttl = Duration::from_secs(100 * 365 * 24 * 3600);

    for address in ADDRESSES.iter() {
        let mut multiaddr = address
            .parse::<Multiaddr>()
            .expect("failed to parse hard-coded multiaddr");

        let p2p_component = multiaddr.pop().expect("hard-coded multiaddr is empty");
        let peer = match p2p_component {
            libp2p::multiaddr::AddrComponent::P2P(key) => {
                PeerId::from_multihash(key).expect("invalid peer id")
            }
            _ => panic!("hard-coded multiaddr didn't end with /p2p/"),
        };

        peer_store
            .clone()
            .peer_or_create(&peer)
            .add_addr(multiaddr, ttl.clone());
    }
}
