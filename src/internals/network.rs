#![allow(dead_code, unused_variables)]

use crate::internals::crypto::generate_random_token;
use code_location::code_location;
use futures::executor::block_on;
use log::{info, warn};
use message_io::network::{Endpoint, NetEvent, SendStatus, Transport};
use message_io::node::{self, NodeEvent};
use primitive_types::U256;
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::io::Error;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::ops::{Deref, Range};
use std::str::FromStr;
use std::string::String;
use std::sync::mpsc::{channel, Receiver, RecvError, RecvTimeoutError, SendError, Sender, TryRecvError};
use std::sync::{Arc, LockResult, Mutex, RwLock};
use std::{io, mem, thread};
use tokio::net::UdpSocket;
use tokio::runtime::Handle;
use tokio::{select, task};
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};

const REQUEST_ADDRESS: &str = "127.0.0.1:56800";
const BROADCAST_ADDRESS: &str = "0.0.0.0:7788";

enum Signal {
    Greet,
    // Any other app event here.
}

////////////////////////////////////////////////////////////////////////////////////////////////////
#[derive(Serialize, Deserialize, Clone)]
pub struct Client {
    ip_socket: SocketAddr,
    hash_name: String,
}

enum ManageClient {
    ConnectPrev(Client),
    ConnectNext(Client),
    Disconnect(Client),
}

#[derive(Serialize, Deserialize)]
struct ClientNeighbours {
    next_client: Option<Client>,
    prev_client: Option<Client>,
}

enum ProcessNeighbours {
    NextClient(Option<Client>),
    PrevClient(Option<Client>),
}

impl Client {
    fn distance(&self, other: &Self) -> Result<U256, uint::FromStrRadixErr> {
        Ok(
            U256::from_str_radix(&self.hash_name, 16)? ^ U256::from_str_radix(&other.hash_name, 16)?,
        )
    }
}

impl PartialEq<Self> for Client {
    fn eq(&self, other: &Self) -> bool {
        U256::from_str_radix(&self.hash_name, 16).unwrap() == U256::from_str_radix(&other.hash_name, 16).unwrap()
    }
}

impl PartialOrd for Client {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(
            U256::from_str_radix(&self.hash_name, 16).unwrap().cmp(&U256::from_str_radix(&other.hash_name, 16).unwrap()),
        )
    }
}

impl Eq for Client {}

impl Ord for Client {
    fn cmp(&self, other: &Self) -> Ordering {
        U256::from_str_radix(&self.hash_name, 16).unwrap().cmp(&U256::from_str_radix(&other.hash_name, 16).unwrap())
    }
}

#[derive(Serialize, Deserialize)]
enum FromClientMessage {
    ClientPublicKey,
    ClientInfo(ClientNeighbours),
    NearestClient,
}

#[derive(Serialize, Deserialize)]
enum FromServerMessage {
    BroadcastClientInfo(Client),
}

#[non_exhaustive]
struct DiscoverySettings;

impl DiscoverySettings {
    const ANNOUNCE_FREQUENCY: u64 = 1000;
    const MDNS_ADDRESS: &'static str = "224.0.0.251";
    const MDNS_PORT: u16 = 5353;
    const NAME_SUFFIX: &'static str = "p2p_chat.local";

    const QUERY_PORT_RANGE: Range<u16> = 49500..50000;
}

////////////////////////////////////////////////////////////////////////////////////////////////////
pub struct Network {
    tokio_handle: Handle,
    bound_socket: Option<SocketAddr>,
    name_string: String,
    name_value: U256,
    neighbours: Arc<RwLock<ClientNeighbours>>,
    discovery_queue: Mutex<BTreeSet<Client>>,
}
////////////////////////////////////////////////////////////////////////////////////////////////////

impl Network {
    pub fn new(tokio_runtime_handle: Handle) -> Self {
        let token = generate_random_token();
        info!("Starting Network mod");
        Network {
            tokio_handle: tokio_runtime_handle,
            bound_socket: None,
            name_string: token.to_string(),
            name_value: U256::from(token.as_bytes()),
            neighbours: Arc::new(RwLock::new(ClientNeighbours {
                next_client: None,
                prev_client: None,
            })),
            discovery_queue: Mutex::new(Default::default()),
        }
    }

    pub async fn like_main(&mut self) {
        let (handler, listener) = node::split::<()>();
        let mut clients: HashMap<Endpoint, u64> = HashMap::new();
        for port in DiscoverySettings::QUERY_PORT_RANGE {
            let bound = handler.network().listen(
                Transport::FramedTcp,
                "127.0.0.1:".to_owned() + &port.to_string(),
            );
            if bound.is_ok() {
                let (_, socket_address) = bound.unwrap();
                self.bound_socket = Some(socket_address);
                info!("REQUEST bound to {}", socket_address);
                break;
            }
        }
        if self.bound_socket.is_none() {
            warn!(
                "Cannot bound QUERY socket in range {:#?}",
                DiscoverySettings::QUERY_PORT_RANGE
            );
            return;
        }
        let client_info = Client {
            ip_socket: self.bound_socket.unwrap(),
            hash_name: self.name_string.clone(),
        };
        let (udp_sender, udp_receiver) = channel::<Client>();
        ////////////////////////////////////////////////////////////////////////////////////////////
        // UDP Broadcast thread
        let client_info_clone = client_info.clone();
        //let handle = task::block_in_place(||{ Network::broadcast(client_info_clone, udp_sender)});
        let (broadcast_trigger_sender, broadcast_trigger_receiver) = channel::<Option<()>>();
        let handle = self.tokio_handle.spawn_blocking(move || {
            block_on(Network::broadcast(
                client_info_clone,
                udp_sender,
                broadcast_trigger_receiver,
            ));
        });
        // Processing Clients from UDP broadcast thread
        let (tcp_sender, tcp_receiver) = channel::<ProcessNeighbours>();
        let (manage_sender, manage_receiver) = channel::<ManageClient>();
        let tokio_handle_copy = self.tokio_handle.clone();
        let neighbours_copy = self.neighbours.clone();
        let sender_copy = manage_sender.clone();
        let handle = self.tokio_handle.spawn_blocking(move || {
            block_on(Network::process_broadcast_client_info(
                client_info.clone(),
                udp_receiver,
                sender_copy,
                neighbours_copy,
            ))
        });
        // TCP neighbours handler
        let neighbours_clone = self.neighbours.clone();
        let tokio_handle_clone = self.tokio_handle.clone();
        let clone_tmp = self.tokio_handle.clone();
        let handle = self.tokio_handle.spawn_blocking(move || {
            block_on(Network::manage_tcp_connections(
                neighbours_clone,
                manage_receiver,
                manage_sender,
                tokio_handle_clone,
                broadcast_trigger_sender
            ))
        });
        ////////////////////////////////////////////////////////////////////////////////////////////
        listener.for_each(move |event| match event.network() {
            NetEvent::Connected(_, _) => (),
            NetEvent::Accepted(endpoint, _listener_id) => {
                clients.insert(endpoint, 1);
                println!(
                    "Client ({}) connected (total clients: {})",
                    endpoint.addr(),
                    clients.len()
                );
            }
            NetEvent::Message(endpoint, input_data) => {
                let message: FromClientMessage = bincode::deserialize(input_data).unwrap();
                match message {
                    FromClientMessage::ClientPublicKey => {
                        info!("{} ClientPubicKey", code_location!());
                        match bincode::serialize(&self.name_string) {
                            Ok(output_data) => {
                                handler.network().send(endpoint, &output_data);
                            }
                            Err(e) => {
                                warn!(
                                    "Cannot serialize data at {} with error {}",
                                    code_location!(),
                                    e
                                );
                            }
                        }
                    }
                    _ => {
                        return;
                    } //TODO Add all branches
                }
            }
            NetEvent::Disconnected(endpoint) => {
                // Only connection oriented protocols will generate this event
                clients.remove(&endpoint).unwrap();
                println!("Client ({}) disconnected (total clients:)", endpoint.addr(), );
            }
        });
    }

    pub async fn broadcast(
        client_info: Client,
        udp_sender: Sender<Client>,
        broadcast_trigger: Receiver<Option<()>>,
    ) -> io::Result<()> {
        info!("Broadcasting");
        //WHOA! Let's construct socket with bare hands, so much fun!
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

        socket.set_reuse_address(true)?;
        socket.bind(&socket2::SockAddr::from(
            SocketAddr::from_str(BROADCAST_ADDRESS).unwrap(),
        ))?;
        socket.set_multicast_loop_v4(true)?;
        match socket.join_multicast_v4(&Ipv4Addr::new(239, 255, 42, 98), &Ipv4Addr::UNSPECIFIED) {
            Ok(_) => {}
            Err(e) => warn!("Cannot join multicast IPv4\n {}", e),
        };
        //now SO_REUSEADDR  is enabled
        let socket = UdpSocket::from_std(socket.into())?;
        send_broadcast_info(&client_info, &socket).await;
        let mut buffer = [0; mem::size_of::<Client>() * 2];
        loop {
            select! {
                result = socket.recv(&mut buffer) => {
                                    let result = match bincode::deserialize(&buffer) {
                    Ok(x) => x,
                    Err(e) => {warn!("{} Deserialize error {}",code_location!(),e);continue;}
                };
                match result {
                    FromServerMessage::BroadcastClientInfo(info) => {
                        if client_info.hash_name != info.hash_name {
                            info!("Found client with socket address {}", info.ip_socket);
                            if udp_sender.send(info).is_err() {
                                warn!("Receiver is hangs up at {}",code_location!());
                            }
                        }
                    }
                }
                }
                result = async {broadcast_trigger.recv()} =>{
                            if result.is_ok(){
                        info!("{} Broadcast Triggered",code_location!());
                            send_broadcast_info(&client_info, &socket).await;
                    }
                }
            }
        }

    async fn send_broadcast_info(client_info: &Client, socket: &UdpSocket) {
        let info = FromServerMessage::BroadcastClientInfo(client_info.clone());
        let send_buffer = bincode::serialize(&info);
        let multi_address = SocketAddrV4::new(Ipv4Addr::new(239, 255, 42, 98), 7788);
        if send_buffer.is_ok() {
            match socket.send_to(&send_buffer.unwrap(), multi_address).await {
                Ok(_) => {}
                Err(e) => {
                    warn!("Cannot send broadcast datagram\n {}", e)
                }
            }
        }
    }
}

    async fn process_broadcast_client_info(
        current: Client,
        udp_receiver: Receiver<Client>,
        tcp_sender: Sender<ManageClient>,
        neighbours: Arc<RwLock<ClientNeighbours>>,
    ) {
        info!("Processing broadcast");
        // TODO? Buffer clients?
        loop {
            let processed = match udp_receiver.recv() {
                Ok(processed) => processed,
                Err(e) => {
                    warn!(
                        "Sender is hangs up at {} with error {}",
                        code_location!(),
                        e
                    );
                    return;
                }
            };
            let neighbours_read = match neighbours.read() {
                Ok(x) => x,
                Err(e) => {
                    warn!(
                        "RwLock is poisoned at {} with error {}",
                        code_location!(),
                        e
                    );
                    return;
                }
            };
            if current > processed {
                if let Some(prev) = &neighbours_read.prev_client {
                    if current.distance(prev).unwrap() > current.distance(&processed).unwrap() {
                        tcp_sender.send(ManageClient::ConnectPrev(processed));
                    }
                } else {
                    tcp_sender.send(ManageClient::ConnectPrev(processed));
                }
            } else if let Some(next) = &neighbours_read.next_client {
                if current.distance(next).unwrap() > current.distance(&processed).unwrap() {
                    tcp_sender.send(ManageClient::ConnectNext(processed));
                }
            } else {
                tcp_sender.send(ManageClient::ConnectNext(processed));
            }
        }
    }

    async fn manage_tcp_connections(
        neighbours: Arc<RwLock<ClientNeighbours>>,
        receiver: Receiver<ManageClient>,
        manage_sender: Sender<ManageClient>,
        tokio_handle: Handle,
        broadcast_trigger: Sender<Option<()>>
    ) {
        info!("Manage TCP connections{}", code_location!());
        let mut prev_handle: Option<tokio::task::JoinHandle<()>> = None;
        let mut next_handle: Option<tokio::task::JoinHandle<()>> = None;
        loop {
            let receiver = match receiver.recv() {
                Ok(x) => x,
                Err(e) => {
                    warn!(
                        "Sender is hangs up at {} with error {}",
                        code_location!(),
                        e
                    );
                    return;
                }
            };
            match receiver {
                ManageClient::ConnectPrev(prev) => {
                    if let Some(old_handle) = prev_handle {
                        old_handle.abort();
                    }
                    let (tcp_sender, tcp_receiver) = channel::<Result<(), io::Error>>();
                    let manage_sender = manage_sender.clone();
                    let prev_clone = prev.clone();
                    prev_handle = Some(tokio_handle.spawn_blocking(move || {
                        block_on(Network::tcp_connection(
                            prev_clone,
                            tcp_sender,
                            manage_sender,
                        ))
                    }));
                    let response = tcp_receiver.recv_timeout(Duration::from_secs(5));
                    if response.is_ok() && response.unwrap().is_ok() {
                        match neighbours.write() {
                            Ok(mut neighbours_write) => {
                                neighbours_write.prev_client = Some(prev);
                            }
                            Err(e) => {
                                warn!("Poisoned RwLock at {} with error {}", code_location!(), e);
                                return;
                            }
                        }
                    }
                }
                ManageClient::ConnectNext(next) => {
                    if let Some(old_handle) = next_handle {
                        old_handle.abort();
                    }
                    let (tcp_sender, tcp_receiver) = channel::<Result<(), io::Error>>();
                    let manage_sender = manage_sender.clone();
                    let next_clone = next.clone();
                    next_handle = Some(tokio_handle.spawn_blocking(move || {
                        block_on(Network::tcp_connection(
                            next_clone,
                            tcp_sender,
                            manage_sender,
                        ))
                    }));
                    let response = tcp_receiver.recv_timeout(Duration::from_secs(5));
                    if response.is_ok() && response.unwrap().is_ok() {
                        match neighbours.write() {
                            Ok(mut neighbours_write) => neighbours_write.next_client = Some(next),
                            Err(e) => {
                                warn!("Poisoned RwLock at {} with error {}", code_location!(), e);
                                return;
                            }
                        }
                    }
                }
                ManageClient::Disconnect(disconnected) => {
                    match neighbours.write() {
                        Ok(mut neighbours_write) => {
                            info!("prev {:?}",neighbours_write.prev_client.is_some());
                            info!("next {:?}",neighbours_write.next_client.is_some());
                            if let Some(prev_original) = &neighbours_write.prev_client {
                                if prev_original == &disconnected {
                                    neighbours_write.prev_client = None;
                                    match prev_handle {
                                        Some(prev) => {
                                            prev.abort();
                                            prev_handle = None;
                                        }
                                        None => {
                                            warn!(
                                                "Trying to remove non-existed Prev client at {}",
                                                code_location!()
                                            )
                                        }
                                    }
                                }
                                } else if let Some(next_original) = &neighbours_write.next_client {
                                    if next_original == &disconnected {
                                        neighbours_write.next_client = None;
                                        match next_handle {
                                            None => {
                                                warn!("Trying to remove non-existed Next client at {}", code_location ! ())
                                            }
                                            Some(next) => {
                                                next.abort();
                                                next_handle = None;
                                            }
                                        }
                                    }
                                }
                                // No neighbours, so let's find some!
                                let broadcast_trigger_clone = broadcast_trigger.clone();
                                tokio_handle.spawn_blocking(move || block_on(Network::find_neighbours(broadcast_trigger_clone)));

                        }
                        Err(e) => {
                            warn!("Poisoned RwLock at {} with error {}", code_location!(), e);
                            return;
                        }
                    }
                }
            }
        }
    }

    async fn tcp_connection(
        client: Client,
        sender: Sender<Result<(), Error>>,
        managed_sender: Sender<ManageClient>,
    ) {
        let (handler, listener) = node::split();
        let (server, socket_addr) = match handler.network().connect(Transport::FramedTcp, client.ip_socket) {
            Ok(x) => {sender.send(Ok(()));x},
            Err(e) => {
                return;
            }
        };
        info!("TCP Connection to {:?} {:?}", server, socket_addr);
        listener.for_each(move |event| match event {
            NodeEvent::Network(net_event) => match net_event {
                NetEvent::Connected(_endpoint, _ok) => {
                    info!(" {} Connected to {}", code_location!(), _endpoint);
                    //handler.signals().send_with_timer(Signal::Greet, Duration::from_secs(10));
                }
                NetEvent::Accepted(_, _) => unreachable!(), // Only generated by listening
                NetEvent::Message(_endpoint, data) => {
                    warn!("Received: {}", String::from_utf8_lossy(data));
                }
                NetEvent::Disconnected(_endpoint) => {
                    info!("Client with IP {} disconnected", client.ip_socket);
                    managed_sender.send(ManageClient::Disconnect(client.clone()));
                }
            },
            NodeEvent::Signal(signal) => match signal {
                Signal::Greet => {
                    info!("{:?}", server);
                    let result = match handler.network().send(server, "Heartbeat!".as_bytes()) {
                        SendStatus::Sent => Some(()),
                        SendStatus::MaxPacketSizeExceeded => None,
                        SendStatus::ResourceNotFound => None,
                        SendStatus::ResourceNotAvailable => None,
                    };
                    // if result.is_some() {
                    //     handler.signals().send_with_timer(Signal::Greet, Duration::from_secs(10));
                    // }
                }
            },
        });
    }

    async fn find_neighbours(broadcast_trigger: Sender<Option<()>>) {
        //no fancy stuff here, just rebroadcast
        match broadcast_trigger.send(Some(())) {
            Ok(_) => {}
            Err(e) => { warn!("{} Broadcast receiver hangs up with {}",code_location!(),e);return;}
        }
    }

    fn distance(&self, to: &Client) -> Result<U256, uint::FromStrRadixErr> {
        Ok(self.name_value ^ U256::from_str_radix(&to.hash_name, 16)?)
    }

    fn greater(&self, than: &Client) -> Result<bool, uint::FromStrRadixErr> {
        Ok(self.name_value > U256::from_str_radix(&than.hash_name, 16)?)
    }

    pub fn get_client_info(&self) -> Option<Client> {
        Some(Client {
            ip_socket: self.bound_socket?,
            hash_name: self.name_string.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::internals::crypto::generate_random_token;
    use crate::internals::network::REQUEST_ADDRESS;
    use crate::Client;
    use primitive_types::U256;
    use socket2::Socket;
    use std::net::SocketAddr;
    use std::str::FromStr;

    #[test]
    fn test_eq() {
        let token = generate_random_token();
        let hash = U256::from(token.as_bytes());
        let client = Client {
            ip_socket: SocketAddr::from_str(REQUEST_ADDRESS).unwrap(),
            hash_name: hash.to_string(),
        };
        assert!(client == client);
    }
}
