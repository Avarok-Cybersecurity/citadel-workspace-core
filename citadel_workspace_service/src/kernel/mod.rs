use bytes::Bytes;
use citadel_logging::{error, info};
use citadel_sdk::prefabs::ClientServerRemote;
use citadel_sdk::prelude::*;
use citadel_workspace_types::{InternalServicePayload, InternalServiceResponse};
use futures::stream::{SplitSink, StreamExt};
use futures::SinkExt;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use uuid::Uuid;

pub struct CitadelWorkspaceService {
    pub remote: Option<NodeRemote>,
    // 127.0.0.1:55555
    pub bind_address: SocketAddr,
}

struct Connection {
    sink_to_server: PeerChannelSendHalf,
    client_server_remote: ClientServerRemote,
    peers: HashMap<u64, PeerConnection>,
}

struct PeerConnection {
    sink: PeerChannelSendHalf,
    remote: SymmetricIdentifierHandle,
}

impl Connection {
    fn new(sink: PeerChannelSendHalf, client_server_remote: ClientServerRemote) -> Self {
        Connection {
            peers: HashMap::new(),
            sink_to_server: sink,
            client_server_remote,
        }
    }

    fn add_peer_connection<T: ToOwned<Owned = SymmetricIdentifierHandle>>(
        &mut self,
        peer_cid: u64,
        sink: PeerChannelSendHalf,
        remote: T,
    ) {
        self.peers.insert(
            peer_cid,
            PeerConnection {
                sink,
                remote: remote.to_owned(),
            },
        );
    }

    fn clear_peer_connection(&mut self, peer_cid: u64) {
        self.peers.remove(&peer_cid);
    }
}

#[async_trait]
impl NetKernel for CitadelWorkspaceService {
    fn load_remote(&mut self, node_remote: NodeRemote) -> Result<(), NetworkError> {
        self.remote = Some(node_remote);
        Ok(())
    }

    async fn on_start(&self) -> Result<(), NetworkError> {
        let mut remote = self.remote.clone().unwrap();
        let listener = tokio::net::TcpListener::bind(self.bind_address).await?;
        //from TCP to command handler
        //read task
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<InternalServicePayload>();

        let tcp_connection_map: &Arc<
            tokio::sync::Mutex<HashMap<Uuid, UnboundedSender<InternalServiceResponse>>>,
        > = &Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let listener_task = async move {
            while let Ok((conn, _addr)) = listener.accept().await {
                //from command handler to the TCP write tak in handle_connection
                let (tx1, rx1) = tokio::sync::mpsc::unbounded_channel::<InternalServiceResponse>();
                let id = Uuid::new_v4();
                tcp_connection_map.lock().await.insert(id, tx1);
                handle_connection(conn, tx.clone(), rx1, id);
            }
            Ok(())
        };

        let mut server_connection_map: HashMap<u64, Connection> = HashMap::new();

        let inbound_command_task = async move {
            while let Some(command) = rx.recv().await {
                payload_handler(
                    command,
                    &mut server_connection_map,
                    &mut remote,
                    tcp_connection_map,
                )
                .await;
            }
            Ok(())
        };

        tokio::select! {
            res0 = listener_task => res0,
            res1 = inbound_command_task => res1,
        }
    }

    async fn on_node_event_received(&self, _message: NodeResult) -> Result<(), NetworkError> {
        // TODO: handle disconnect properly by removing entries from the hashmap
        Ok(())
    }

    async fn on_stop(&mut self) -> Result<(), NetworkError> {
        Ok(())
    }
}

async fn send_response_to_tcp_client(
    hash_map: &Arc<tokio::sync::Mutex<HashMap<Uuid, UnboundedSender<InternalServiceResponse>>>>,
    response: InternalServiceResponse,
    uuid: Uuid,
) {
    hash_map
        .lock()
        .await
        .get(&uuid)
        .unwrap()
        .send(response)
        .unwrap()
}

async fn payload_handler(
    command: InternalServicePayload,
    server_connection_map: &mut HashMap<u64, Connection>,
    remote: &mut NodeRemote,
    tcp_connection_map: &Arc<
        tokio::sync::Mutex<HashMap<Uuid, UnboundedSender<InternalServiceResponse>>>,
    >,
) {
    match command {
        InternalServicePayload::Connect {
            uuid,
            username,
            password,
            connect_mode,
            udp_mode,
            keep_alive_timeout,
            session_security_settings,
        } => {
            match remote
                .connect(
                    AuthenticationRequest::credentialed(username, password),
                    connect_mode,
                    udp_mode,
                    keep_alive_timeout,
                    session_security_settings,
                )
                .await
            {
                Ok(conn_success) => {
                    let cid = conn_success.cid;

                    let (sink, mut stream) = conn_success.channel.split();
                    let client_server_remote =
                        create_client_server_remote(stream.vconn_type, remote.clone());
                    let connection_struct = Connection::new(sink, client_server_remote);
                    server_connection_map.insert(cid, connection_struct);

                    let hm_for_conn = tcp_connection_map.clone();

                    let response = InternalServiceResponse::ConnectSuccess { cid };

                    send_response_to_tcp_client(tcp_connection_map, response, uuid).await;

                    let connection_read_stream = async move {
                        while let Some(message) = stream.next().await {
                            let message = InternalServiceResponse::MessageReceived {
                                message: message.into_buffer(),
                                cid,
                                peer_cid: 0,
                            };
                            match hm_for_conn.lock().await.get(&uuid) {
                                Some(entry) => match entry.send(message) {
                                    Ok(res) => res,
                                    Err(_) => info!(target: "citadel", "tx not sent"),
                                },
                                None => {
                                    info!(target:"citadel","Hash map connection not found")
                                }
                            }
                        }
                    };
                    tokio::spawn(connection_read_stream);
                }

                Err(err) => {
                    let response = InternalServiceResponse::ConnectionFailure {
                        message: err.into_string(),
                    };
                    send_response_to_tcp_client(tcp_connection_map, response, uuid).await;
                }
            };
        }
        InternalServicePayload::Register {
            uuid,
            server_addr,
            full_name,
            username,
            proposed_password,
            default_security_settings,
        } => {
            info!(target: "citadel", "About to connect to server {server_addr:?} for user {username}");
            match remote
                .register(
                    server_addr,
                    full_name,
                    username,
                    proposed_password,
                    default_security_settings,
                )
                .await
            {
                Ok(_res) => {
                    // TODO: add trace ID to ensure uniqueness of request
                    let response = InternalServiceResponse::RegisterSuccess { id: uuid };
                    send_response_to_tcp_client(tcp_connection_map, response, uuid).await
                }
                Err(err) => {
                    let response = InternalServiceResponse::RegisterFailure {
                        message: err.into_string(),
                    };
                    send_response_to_tcp_client(tcp_connection_map, response, uuid).await
                }
            };
        }
        InternalServicePayload::Message {
            uuid,
            message,
            cid,
            user_cid: _,
            security_level,
        } => {
            match server_connection_map.get_mut(&cid) {
                Some(conn) => {
                    conn.sink_to_server.set_security_level(security_level);

                    let response = InternalServiceResponse::MessageSent { cid };
                    conn.sink_to_server
                        .send_message(message.into())
                        .await
                        .unwrap();
                    send_response_to_tcp_client(tcp_connection_map, response, uuid).await;
                    info!(target: "citadel", "Into the message handler command send")
                }
                None => {
                    info!(target: "citadel","connection not found");
                    send_response_to_tcp_client(
                        tcp_connection_map,
                        InternalServiceResponse::MessageSendError {
                            cid,
                            message: format!("Connection for {cid} not found"),
                        },
                        uuid,
                    )
                    .await;
                }
            };
        }

        InternalServicePayload::Disconnect { cid, uuid } => {
            let request = NodeRequest::DisconnectFromHypernode(DisconnectFromHypernode {
                implicated_cid: cid,
                v_conn_type: VirtualTargetType::LocalGroupServer {
                    implicated_cid: cid,
                },
            });
            server_connection_map.remove(&cid);
            match remote.send(request).await {
                Ok(res) => {
                    let disconnect_success = InternalServiceResponse::DisconnectSuccess { cid };
                    send_response_to_tcp_client(tcp_connection_map, disconnect_success, uuid).await;
                    info!(target: "citadel", "Disconnected {res:?}")
                }
                Err(err) => {
                    let error_message = format!("Failed to disconnect {err:?}");
                    info!(target: "citadel", "{error_message}");
                    let disconnect_failure = InternalServiceResponse::DisconnectFailure {
                        cid,
                        message: error_message,
                    };
                    send_response_to_tcp_client(tcp_connection_map, disconnect_failure, uuid).await;
                }
            };
        }

        InternalServicePayload::SendFile {
            uuid,
            source,
            cid,
            chunk_size,
            transfer_type,
        } => {
            let mut client_to_server_remote = ClientServerRemote::new(
                VirtualTargetType::LocalGroupServer {
                    implicated_cid: cid,
                },
                remote.clone(),
            );
            match client_to_server_remote
                .send_file_with_custom_opts(source, chunk_size, transfer_type)
                .await
            {
                Ok(_) => {
                    send_response_to_tcp_client(
                        tcp_connection_map,
                        InternalServiceResponse::SendFileSuccess { cid },
                        uuid,
                    )
                    .await;
                }

                Err(err) => {
                    send_response_to_tcp_client(
                        tcp_connection_map,
                        InternalServiceResponse::SendFileFailure {
                            cid,
                            message: err.into_string(),
                        },
                        uuid,
                    )
                    .await;
                }
            }
        }

        InternalServicePayload::DownloadFile {
            virtual_path,
            transfer_security_level,
            delete_on_pull,
            cid,
            uuid,
        } => {
            // let mut client_to_server_remote = ClientServerRemote::new(VirtualTargetType::LocalGroupServer { implicated_cid: cid }, remote.clone());
            // match client_to_server_remote.(virtual_path, transfer_security_level, delete_on_pull).await {
            //     Ok(_) => {

            //     },
            //     Err(err) => {

            //     }
            // }
        }

        InternalServicePayload::StartGroup {
            initial_users_to_invite,
            cid,
            uuid: _uuid,
        } => {
            let mut client_to_server_remote = ClientServerRemote::new(
                VirtualTargetType::LocalGroupServer {
                    implicated_cid: cid,
                },
                remote.clone(),
            );
            match client_to_server_remote
                .create_group(initial_users_to_invite)
                .await
            {
                Ok(_group_channel) => {}

                Err(_err) => {}
            }
        }

        InternalServicePayload::PeerRegister {
            uuid,
            cid,
            username,
            peer_username,
        } => {
            let mut client_to_server_remote = ClientServerRemote::new(
                VirtualTargetType::LocalGroupServer {
                    implicated_cid: cid,
                },
                remote.clone(),
            );
            match client_to_server_remote
                .propose_target(username.clone(), peer_username)
                .await
            {
                // username or cid?
                Ok(mut symmetric_identifier_handle_ref) => {
                    match symmetric_identifier_handle_ref.register_to_peer().await {
                        Ok(peer_register_success) => {
                            match symmetric_identifier_handle_ref.account_manager().find_target_information(cid, peer_username.clone()).await {
                                Ok(target_information) => {
                                    let (peer_cid, mutual_peer) = target_information.unwrap();
                                    // TODO: pass peer_cid and peer_username to the TCP client
                                    send_response_to_tcp_client(
                                        tcp_connection_map,
                                        InternalServiceResponse::PeerRegisterSuccess {
                                            cid,
                                            peer_cid,
                                            username,
                                        },
                                        uuid,
                                    )
                                        .await;
                                }
                                Err(_) => {}
                            }
                        }

                        Err(err) => {
                            send_response_to_tcp_client(
                                tcp_connection_map,
                                InternalServiceResponse::PeerRegisterFailure {
                                    cid,
                                    message: err.into_string(),
                                },
                                uuid,
                            )
                            .await;
                        }
                    }
                }

                Err(err) => {
                    send_response_to_tcp_client(
                        tcp_connection_map,
                        InternalServiceResponse::PeerRegisterFailure {
                            cid,
                            message: err.into_string(),
                        },
                        uuid,
                    )
                    .await;
                }
            }
        }

        InternalServicePayload::PeerConnect {
            uuid,
            cid,
            username,
            peer_cid,
            peer_username,
            udp_mode,
            session_security_settings,
        } => {
            // TODO: check to see if peer is already in the hashmap
            let mut client_to_server_remote = ClientServerRemote::new(
                VirtualTargetType::LocalGroupPeer {
                    implicated_cid: cid,
                    peer_cid,
                },
                remote.clone(),
            );
            match client_to_server_remote
                .find_target(username, peer_username)
                .await
            {
                // username or cid?
                Ok(mut symmetric_identifier_handle_ref) => {
                    match symmetric_identifier_handle_ref
                        .connect_to_peer_custom(session_security_settings, udp_mode)
                        .await
                    {
                        Ok(peer_connect_success) => {
                            let connection_cid = peer_connect_success.channel.get_peer_cid();
                            let (sink, mut stream) = peer_connect_success.channel.split();
                            server_connection_map
                                .get_mut(&cid)
                                .unwrap()
                                .add_peer_connection(peer_cid, sink, symmetric_identifier_handle_ref.into_owned());

                            let hm_for_conn = tcp_connection_map.clone();

                            send_response_to_tcp_client(
                                tcp_connection_map,
                                InternalServiceResponse::PeerConnectSuccess { cid },
                                uuid,
                            )
                            .await;
                            let connection_read_stream = async move {
                                while let Some(message) = stream.next().await {
                                    let message = InternalServiceResponse::MessageReceived {
                                        message: message.into_buffer(),
                                        cid: connection_cid,
                                        peer_cid,
                                    };
                                    match hm_for_conn.lock().await.get(&uuid) {
                                        Some(entry) => match entry.send(message) {
                                            Ok(res) => res,
                                            Err(_) => error!(target: "citadel", "tx not sent"),
                                        },
                                        None => {
                                            info!(target:"citadel","Hash map connection not found")
                                        }
                                    }
                                }
                            };
                            tokio::spawn(connection_read_stream);
                        }

                        Err(err) => {
                            send_response_to_tcp_client(
                                tcp_connection_map,
                                InternalServiceResponse::PeerConnectFailure {
                                    cid,
                                    message: err.into_string(),
                                },
                                uuid,
                            )
                            .await;
                        }
                    }
                }

                Err(err) => {
                    send_response_to_tcp_client(
                        tcp_connection_map,
                        InternalServiceResponse::PeerConnectFailure {
                            cid,
                            message: err.into_string(),
                        },
                        uuid,
                    )
                    .await;
                }
            }
        }
        InternalServicePayload::PeerDisconnect {
            uuid,
            cid,
            peer_cid
        } => {

            let request = NodeRequest::PeerCommand(PeerCommand {
                implicated_cid: cid,
                command: PeerSignal::Disconnect(PeerConnectionType::LocalGroupPeer { implicated_cid: cid, peer_cid}, None)
            });

            match server_connection_map.get_mut(&cid) {
                None => {
                    send_response_to_tcp_client(
                        tcp_connection_map,
                        InternalServiceResponse::PeerDisconnectFailure { cid, message: "Server connection not found".to_string() },
                        uuid
                    ).await;
                }
                Some(conn) => {
                    match conn.peers.get_mut(&cid) {
                        None => {}
                        Some(target_peer) => {
                            match target_peer.remote.send(request) {
                                Ok(ticket) => {
                                    conn.clear_peer_connection(peer_cid);
                                    let peer_disconnect_success = InternalServiceResponse::PeerDisconnectSuccess { cid, ticket };
                                    send_response_to_tcp_client(tcp_connection_map, peer_disconnect_success, uuid).await;
                                    info!(target: "citadel", "Disconnected Peer{ticket:?}")
                                },
                                Err(network_error) => {
                                    let error_message = format!("Failed to disconnect {network_error:?}");
                                    info!(target: "citadel", "{error_message}");
                                    let peer_disconnect_failure = InternalServiceResponse::PeerDisconnectFailure {
                                        cid,
                                        message: error_message,
                                    };
                                    send_response_to_tcp_client(tcp_connection_map, peer_disconnect_failure, uuid).await;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn create_client_server_remote(
    conn_type: VirtualTargetType,
    remote: NodeRemote,
) -> ClientServerRemote {
    ClientServerRemote::new(conn_type, remote)
}

pub fn wrap_tcp_conn(conn: TcpStream) -> Framed<TcpStream, LengthDelimitedCodec> {
    LengthDelimitedCodec::builder()
        .length_field_offset(0) // default value
        .max_frame_length(1024 * 1024 * 64) // 64 MB
        .length_field_type::<u32>()
        .length_adjustment(0) // default value
        .new_framed(conn)
}

fn serialize_payload(payload: &InternalServiceResponse) -> Vec<u8> {
    bincode2::serialize(&payload).unwrap()
}

async fn sink_send_payload(
    payload: &InternalServiceResponse,
    sink: &mut SplitSink<Framed<TcpStream, LengthDelimitedCodec>, Bytes>,
) {
    let payload = serialize_payload(payload);
    match sink.send(payload.into()).await {
        Ok(_) => (),
        Err(_) => info!(target: "citadel", "w task: sink send err"),
    }
}

fn deserialize(message: &[u8]) -> InternalServicePayload {
    bincode2::deserialize(message).unwrap()
}

fn send_to_kernel(payload_to_send: &[u8], sender: &UnboundedSender<InternalServicePayload>) {
    let payload = deserialize(payload_to_send);
    sender.send(payload).unwrap();
}

fn handle_connection(
    conn: TcpStream,
    to_kernel: UnboundedSender<InternalServicePayload>,
    mut from_kernel: tokio::sync::mpsc::UnboundedReceiver<InternalServiceResponse>,
    conn_id: Uuid,
) {
    tokio::task::spawn(async move {
        let framed = wrap_tcp_conn(conn);
        let (mut sink, mut stream) = framed.split();

        let write_task = async move {
            let response = InternalServiceResponse::ServiceConnectionAccepted { id: conn_id };

            sink_send_payload(&response, &mut sink).await;

            while let Some(kernel_response) = from_kernel.recv().await {
                sink_send_payload(&kernel_response, &mut sink).await;
            }
        };

        let read_task = async move {
            while let Some(message) = stream.next().await {
                send_to_kernel(&message.unwrap(), &to_kernel);
            }
            info!(target: "citadel", "Disconnected");
        };

        tokio::select! {
            res0 = write_task => res0,
            res1 = read_task => res1,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::stream::SplitSink;
    use std::error::Error;
    use std::time::Duration;
    use tokio::net::TcpStream;

    async fn send(
        sink: &mut SplitSink<Framed<TcpStream, LengthDelimitedCodec>, Bytes>,
        command: InternalServicePayload,
    ) -> Result<(), Box<dyn Error>> {
        let command = bincode2::serialize(&command)?;
        sink.send(command.into()).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_citadel_workspace_service() -> Result<(), Box<dyn Error>> {
        citadel_logging::setup_log();
        info!(target: "citadel", "above server spawn");
        let bind_address_internal_service: SocketAddr = "127.0.0.1:55556".parse().unwrap();

        // TCP client (GUI, CLI) -> internal service -> empty kernel server(s)
        let (server, server_bind_address) = citadel_sdk::test_common::server_info();

        tokio::task::spawn(server);
        info!(target: "citadel", "sub server spawn");
        let internal_service_kernel = CitadelWorkspaceService {
            remote: None,
            bind_address: bind_address_internal_service,
        };
        let internal_service = NodeBuilder::default()
            .with_node_type(NodeType::Peer)
            .with_backend(BackendType::InMemory)
            .build(internal_service_kernel)?;

        tokio::task::spawn(internal_service);

        // give time for both the server and internal service to run

        tokio::time::sleep(Duration::from_millis(2000)).await;

        info!(target: "citadel", "about to connect to internal service");

        // begin mocking the GUI/CLI access
        let conn = TcpStream::connect(bind_address_internal_service).await?;
        info!(target: "citadel", "connected to the TCP stream");
        let framed = wrap_tcp_conn(conn);
        info!(target: "citadel", "wrapped tcp connection");

        let (mut sink, mut stream) = framed.split();

        let first_packet = stream.next().await.unwrap()?;
        info!(target: "citadel", "First packet");
        let greeter_packet: InternalServiceResponse = bincode2::deserialize(&*first_packet)?;

        info!(target: "citadel", "Greeter packet {greeter_packet:?}");

        if let InternalServiceResponse::ServiceConnectionAccepted { id } = greeter_packet {
            let register_command = InternalServicePayload::Register {
                uuid: id,
                server_addr: server_bind_address,
                full_name: String::from("John"),
                username: String::from("john_doe"),
                proposed_password: "test12345".into(),
                default_security_settings: Default::default(),
            };
            send(&mut sink, register_command).await?;

            let second_packet = stream.next().await.unwrap()?;
            let response_packet: InternalServiceResponse = bincode2::deserialize(&*second_packet)?;
            if let InternalServiceResponse::RegisterSuccess { id } = response_packet {
                // now, connect to the server
                let command = InternalServicePayload::Connect {
                    username: String::from("john_doe"),
                    password: "test12345".into(),
                    connect_mode: Default::default(),
                    udp_mode: Default::default(),
                    keep_alive_timeout: None,
                    uuid: id,
                    session_security_settings: Default::default(),
                };

                send(&mut sink, command).await?;

                let next_packet = stream.next().await.unwrap()?;
                let response_packet: InternalServiceResponse =
                    bincode2::deserialize(&*next_packet)?;
                if let InternalServiceResponse::ConnectSuccess { cid } = response_packet {
                    let disconnect_command = InternalServicePayload::Disconnect { uuid: id, cid };

                    send(&mut sink, disconnect_command).await?;
                    let next_packet = stream.next().await.unwrap()?;
                    let response_disconnect_packet: InternalServiceResponse =
                        bincode2::deserialize(&*next_packet)?;

                    if let InternalServiceResponse::DisconnectSuccess { cid } =
                        response_disconnect_packet
                    {
                        info!(target:"citadel", "Disconnected {cid}");
                        Ok(())
                    } else {
                        panic!("Disconnection failed");
                    }
                } else {
                    panic!("Connection to server was not a success")
                }
            } else {
                panic!("Registration to server was not a success")
            }
        } else {
            panic!("Wrong packet type");
        }
    }
    // test
    #[tokio::test]
    async fn message_test() -> Result<(), Box<dyn Error>> {
        citadel_logging::setup_log();
        info!(target: "citadel", "above server spawn");
        let bind_address_internal_service: SocketAddr = "127.0.0.1:55568".parse().unwrap();

        // TCP client (GUI, CLI) -> internal service -> empty kernel server(s)
        let (server, server_bind_address) = citadel_sdk::test_common::server_info_reactive(
            |conn, _remote| async move {
                let (sink, mut stream) = conn.channel.split();

                while let Some(_message) = stream.next().await {
                    let send_message = "pong".into();
                    sink.send_message(send_message).await.unwrap();
                    info!("MessageSent");
                    ()
                }
                Ok(())
            },
            |_| (),
        );

        tokio::task::spawn(server);
        info!(target: "citadel", "sub server spawn");
        let internal_service_kernel = CitadelWorkspaceService {
            remote: None,
            bind_address: bind_address_internal_service,
        };
        let internal_service = NodeBuilder::default()
            .with_node_type(NodeType::Peer)
            .with_backend(BackendType::InMemory)
            .build(internal_service_kernel)?;

        tokio::task::spawn(internal_service);

        // give time for both the server and internal service to run

        tokio::time::sleep(Duration::from_millis(2000)).await;

        info!(target: "citadel", "about to connect to internal service");

        // begin mocking the GUI/CLI access
        let conn = TcpStream::connect(bind_address_internal_service).await?;
        info!(target: "citadel", "connected to the TCP stream");
        let framed = wrap_tcp_conn(conn);
        info!(target: "citadel", "wrapped tcp connection");

        let (mut sink, mut stream) = framed.split();

        let first_packet = stream.next().await.unwrap()?;
        info!(target: "citadel", "First packet");
        let greeter_packet: InternalServiceResponse = bincode2::deserialize(&*first_packet)?;

        info!(target: "citadel", "Greeter packet {greeter_packet:?}");

        if let InternalServiceResponse::ServiceConnectionAccepted { id } = greeter_packet {
            let register_command = InternalServicePayload::Register {
                uuid: id,
                server_addr: server_bind_address,
                full_name: String::from("John"),
                username: String::from("john_doe"),
                proposed_password: String::from("test12345").into_bytes().into(),
                default_security_settings: Default::default(),
            };
            send(&mut sink, register_command).await?;

            let second_packet = stream.next().await.unwrap()?;
            let response_packet: InternalServiceResponse = bincode2::deserialize(&*second_packet)?;
            if let InternalServiceResponse::RegisterSuccess { id } = response_packet {
                // now, connect to the server
                let command = InternalServicePayload::Connect {
                    // server_addr: server_bind_address,
                    username: String::from("john_doe"),
                    password: String::from("test12345").into_bytes().into(),
                    connect_mode: Default::default(),
                    udp_mode: Default::default(),
                    keep_alive_timeout: None,
                    uuid: id,
                    session_security_settings: Default::default(),
                };

                send(&mut sink, command).await?;

                let next_packet = stream.next().await.unwrap()?;
                let response_packet: InternalServiceResponse =
                    bincode2::deserialize(&*next_packet)?;
                if let InternalServiceResponse::ConnectSuccess { cid } = response_packet {
                    let serialized_message = bincode2::serialize("hi").unwrap();
                    let message_command = InternalServicePayload::Message {
                        uuid: id,
                        message: serialized_message,
                        cid,
                        user_cid: cid,
                        security_level: SecurityLevel::Standard,
                    };

                    send(&mut sink, message_command).await?;

                    info!(target:"citadel", "Message sent to sink from client");

                    let next_packet = stream.next().await.unwrap()?;
                    let response_message_packet: InternalServiceResponse =
                        bincode2::deserialize(&*next_packet)?;
                    info!(target: "citadel","{response_message_packet:?}");

                    if let InternalServiceResponse::MessageSent { cid } = response_message_packet {
                        info!(target:"citadel", "Message {cid}");
                        let next_packet = stream.next().await.unwrap()?;
                        let response_message_packet: InternalServiceResponse =
                            bincode2::deserialize(&*next_packet)?;
                        if let InternalServiceResponse::MessageReceived {
                            message,
                            cid,
                            peer_cid: _,
                        } = response_message_packet
                        {
                            println!("{message:?}");
                            assert_eq!(SecBuffer::from("pong"), message);
                            info!(target:"citadel", "Message sending success {cid}");
                            Ok(())
                        } else {
                            panic!("Message sending is not right");
                        }
                    } else {
                        panic!("Message sending failed");
                    }
                } else {
                    panic!("Connection to server was not a success")
                }
            } else {
                panic!("Registration to server was not a success")
            }
        } else {
            panic!("Wrong packet type");
        }
    }
    #[tokio::test]
    async fn test_citadel_workspace_service_peer_test() -> Result<(), Box<dyn Error>> {
        citadel_logging::setup_log();
        info!(target: "citadel", "above server spawn");
        let bind_address_internal_service: SocketAddr = "127.0.0.1:55556".parse().unwrap();

        // TCP client (GUI, CLI) -> internal service -> empty kernel server(s)
        let (server, server_bind_address) = citadel_sdk::test_common::server_info();

        tokio::task::spawn(server);
        info!(target: "citadel", "sub server spawn");
        let internal_service_kernel = CitadelWorkspaceService {
            remote: None,
            bind_address: bind_address_internal_service,
        };
        let internal_service = NodeBuilder::default()
            .with_node_type(NodeType::Peer)
            .with_backend(BackendType::InMemory)
            .build(internal_service_kernel)?;

        tokio::task::spawn(internal_service);

        // give time for both the server and internal service to run

        tokio::time::sleep(Duration::from_millis(2000)).await;

        info!(target: "citadel", "about to connect to internal service");

        let peer_execute = async move {
            // begin mocking the GUI/CLI access
            let conn = TcpStream::connect(bind_address_internal_service).await?;
            info!(target: "citadel", "connected to the TCP stream");
            let framed = wrap_tcp_conn(conn);
            info!(target: "citadel", "wrapped tcp connection");

            let (mut sink, mut stream) = framed.split();

            let first_packet = stream.next().await.unwrap()?;
            info!(target: "citadel", "First packet");
            let greeter_packet: InternalServiceResponse = bincode2::deserialize(&*first_packet)?;

            info!(target: "citadel", "Greeter packet {greeter_packet:?}");

            if let InternalServiceResponse::ServiceConnectionAccepted { id } = greeter_packet {
                let register_command = InternalServicePayload::Register {
                    uuid: id,
                    server_addr: server_bind_address,
                    full_name: String::from("Perry"),
                    username: String::from("peer_test"),
                    proposed_password: "test12345".into(),
                    default_security_settings: Default::default(),
                };
                send(&mut sink, register_command).await?;

                let second_packet = stream.next().await.unwrap()?;
                let response_packet: InternalServiceResponse = bincode2::deserialize(&*second_packet)?;
                if let InternalServiceResponse::RegisterSuccess { id } = response_packet {
                    // now, connect to the server
                    let command = InternalServicePayload::Connect {
                        username: String::from("peer_test"),
                        password: "test12345".into(),
                        connect_mode: Default::default(),
                        udp_mode: Default::default(),
                        keep_alive_timeout: None,
                        uuid: id,
                        session_security_settings: Default::default(),
                    };

                    send(&mut sink, command).await?;

                    let next_packet = stream.next().await.unwrap()?;
                    let response_packet: InternalServiceResponse =
                        bincode2::deserialize(&*next_packet)?;
                    if let InternalServiceResponse::ConnectSuccess { cid } = response_packet {

                        let disconnect_command = InternalServicePayload::Disconnect { uuid: id, cid };

                        send(&mut sink, disconnect_command).await?;
                        let next_packet = stream.next().await.unwrap()?;
                        let response_disconnect_packet: InternalServiceResponse =
                            bincode2::deserialize(&*next_packet)?;

                        if let InternalServiceResponse::DisconnectSuccess { cid } =
                            response_disconnect_packet
                        {
                            info!(target:"citadel", "Disconnected {cid}");
                            Ok(())
                        } else {
                            panic!("Disconnection failed");
                        }
                    } else {
                        panic!("Connection to server was not a success")
                    }
                } else {
                    panic!("Registration to server was not a success")
                }
            } else {
                panic!("Wrong packet type");
            }
        };

        // begin mocking the GUI/CLI access
        let conn = TcpStream::connect(bind_address_internal_service).await?;
        info!(target: "citadel", "connected to the TCP stream");
        let framed = wrap_tcp_conn(conn);
        info!(target: "citadel", "wrapped tcp connection");

        let (mut sink, mut stream) = framed.split();

        let first_packet = stream.next().await.unwrap()?;
        info!(target: "citadel", "First packet");
        let greeter_packet: InternalServiceResponse = bincode2::deserialize(&*first_packet)?;

        info!(target: "citadel", "Greeter packet {greeter_packet:?}");

        if let InternalServiceResponse::ServiceConnectionAccepted { id } = greeter_packet {
            let register_command = InternalServicePayload::Register {
                uuid: id,
                server_addr: server_bind_address,
                full_name: String::from("John"),
                username: String::from("john_doe"),
                proposed_password: "test12345".into(),
                default_security_settings: Default::default(),
            };
            send(&mut sink, register_command).await?;

            let second_packet = stream.next().await.unwrap()?;
            let response_packet: InternalServiceResponse = bincode2::deserialize(&*second_packet)?;
            if let InternalServiceResponse::RegisterSuccess { id } = response_packet {
                // now, connect to the server
                let command = InternalServicePayload::Connect {
                    username: String::from("john_doe"),
                    password: "test12345".into(),
                    connect_mode: Default::default(),
                    udp_mode: Default::default(),
                    keep_alive_timeout: None,
                    uuid: id,
                    session_security_settings: Default::default(),
                };

                send(&mut sink, command).await?;

                let next_packet = stream.next().await.unwrap()?;
                let response_packet: InternalServiceResponse =
                    bincode2::deserialize(&*next_packet)?;
                if let InternalServiceResponse::ConnectSuccess { cid } = response_packet {

                    // Peer Register
                    let peer_register_command = InternalServicePayload::PeerRegister {
                        uuid: id,
                        cid,
                        username: String::from("john_doe"),
                        peer_username: String::from("peer_test"),
                    };

                    send(&mut sink, peer_register_command).await?;
                    let peer_register_response = stream.next().await.unwrap()?;
                    let peer_response_packet: InternalServiceResponse = bincode2::deserialize(&*next_packet)?;
                    if let InternalServiceResponse::PeerRegisterSuccess { cid, peer_cid, username } = peer_response_packet {

                        info!(target:"citadel", "User {cid} Registered Peer {peer_cid}");

                        // Peer Connect
                        let peer_connect_command = InternalServicePayload::PeerConnect {
                            uuid: id,
                            cid,
                            username,
                            peer_cid,
                            peer_username: String::from("peer_test"),
                            udp_mode: Default::default(),
                            session_security_settings: Default::default(),
                        };

                        send(&mut sink, peer_connect_command).await?;
                        let peer_connect_response = stream.next().await.unwrap()?;
                        let peer_response_packet: InternalServiceResponse = bincode2::deserialize(&*next_packet)?;
                        if let InternalServiceResponse::PeerConnectSuccess { cid} = peer_response_packet {

                            info!(target:"citadel", "User {cid} Connected to Peer {peer_cid}");

                            // Disconnect
                            let disconnect_command = InternalServicePayload::Disconnect { uuid: id, cid };

                            send(&mut sink, disconnect_command).await?;
                            let next_packet = stream.next().await.unwrap()?;
                            let response_disconnect_packet: InternalServiceResponse =
                                bincode2::deserialize(&*next_packet)?;

                            if let InternalServiceResponse::DisconnectSuccess { cid } =
                                response_disconnect_packet
                            {
                                info!(target:"citadel", "Disconnected {cid}");
                                Ok(())
                            } else {
                                panic!("Disconnection failed");
                            }
                        }
                        else {
                            panic!("Peer Connect Failed!");
                        }
                    }
                    else {
                        panic!("Peer Register Failed!");
                    }
                } else {
                    panic!("Connection to server was not a success")
                }
            } else {
                panic!("Registration to server was not a success")
            }
        } else {
            panic!("Wrong packet type");
        }


    }
}
