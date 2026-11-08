pub mod error;

#[cfg(feature = "receive")]
use super::tasks::udp_rx;
use super::{
    crypto::Cipher,
    tasks::{
        message::*,
        ws::{self as ws_task, AuxNetwork},
    },
    Config,
    CryptoMode,
};
use crate::{
    constants::*,
    model::{
        payload::{Identify, Resume, SelectProtocol},
        Event as GatewayEvent,
        ProtocolData,
    },
    ws::WsStream,
    ConnectionInfo,
};
use discortp::discord::{IpDiscoveryPacket, IpDiscoveryType, MutableIpDiscoveryPacket};
use error::{Error, Result};
use flume::Sender;
use serenity_voice_model::payload::DaveMlsKeyPackage;
use socket2::Socket;
use std::sync::{atomic::AtomicU16, Arc, RwLock};
use std::{net::IpAddr, num::NonZeroU16, str::FromStr};
use tokio::{net::UdpSocket, spawn, time::timeout};
use tracing::{debug, info, instrument, warn};
use url::Url;

pub(crate) struct Connection {
    pub(crate) info: ConnectionInfo,
    pub(crate) ssrc: u32,
    pub(crate) ws: Sender<WsMessage>,
    /// tinycord patch: this is a video (Go Live) connection.
    pub(crate) video: u8,
}

impl Connection {
    pub(crate) async fn new(
        info: ConnectionInfo,
        interconnect: &Interconnect,
        config: &Config,
        idx: usize,
    ) -> Result<Connection> {
        if let Some(t) = config.driver_timeout {
            timeout(
                t.into(),
                Connection::new_inner(info, interconnect, config, idx),
            )
            .await?
        } else {
            Connection::new_inner(info, interconnect, config, idx).await
        }
    }

    pub(crate) async fn new_inner(
        info: ConnectionInfo,
        interconnect: &Interconnect,
        config: &Config,
        idx: usize,
    ) -> Result<Connection> {
        let url = generate_url(&info.endpoint)?;

        let mut client = WsStream::connect(url).await?;
        let (ws_msg_tx, ws_msg_rx) = flume::unbounded();

        let mut hello = None;
        let mut ready = None;

        if config.video_receive || config.video_send {
            // tinycord patch: a Go Live connection. Same as a voice identify,
            // plus "this client handles video".
            client.set_tap(if config.video_send { 2 } else { 1 });
            let identify = serde_json::json!({
                "op": 0,
                "d": {
                    "server_id": info.guild_id.0.to_string(),
                    "session_id": info.session_id,
                    "token": info.token,
                    "user_id": info.user_id.0.to_string(),
                    "max_dave_protocol_version": davey::DAVE_PROTOCOL_VERSION,
                    "video": true,
                    "streams": [{
                        "type": if config.video_send { "screen" } else { "video" },
                        "rid": "100",
                        "quality": 100,
                    }],
                }
            });
            client.send_raw(identify.to_string()).await?;
        } else {
            client
                .send_json(&GatewayEvent::from(Identify {
                    server_id: info.guild_id.into(),
                    session_id: info.session_id.clone(),
                    token: info.token.clone(),
                    user_id: info.user_id.into(),
                    max_dave_protocol_version: Some(davey::DAVE_PROTOCOL_VERSION),
                }))
                .await?;
        }

        loop {
            let Some(value) = client.recv_event().await? else {
                continue;
            };

            match value {
                GatewayEvent::Ready(r) => {
                    ready = Some(r);
                    if hello.is_some() {
                        break;
                    }
                },
                GatewayEvent::Hello(h) => {
                    hello = Some(h);
                    if ready.is_some() {
                        break;
                    }
                },
                other => {
                    // Discord hold back per-user connection state until after this handshake.
                    // There's no guarantee that will remain the case, so buffer it like all
                    // subsequent steps where we know they *do* send these packets.
                    debug!("Expected ready/hello; got: {:?}", other);
                    ws_msg_tx.send(WsMessage::Deliver(other))?;
                },
            }
        }

        let hello =
            hello.expect("Hello packet expected in connection initialisation, but not found.");
        let ready =
            ready.expect("Ready packet expected in connection initialisation, but not found.");

        let chosen_crypto = CryptoMode::negotiate(&ready.modes, Some(config.crypto_mode))?;

        info!(
            "Crypto scheme negotiation -- wanted {:?}. Chose {:?} from modes {:?}.",
            config.crypto_mode, chosen_crypto, ready.modes
        );

        let udp = UdpSocket::bind("0.0.0.0:0").await?;

        // tinycord patch: a keyframe arrives as a burst of ~100+ packets; a
        // small OS receive buffer drops some and the picture can't start.
        let udp = if config.video_receive || config.video_send {
            let socket = Socket::from(udp.into_std()?);
            if let Err(e) = socket.set_recv_buffer_size(4 * 1024 * 1024) {
                warn!("could not enlarge UDP receive buffer: {e}");
            }
            // Sending a stream: a keyframe leaves as a burst too, and a full
            // send buffer would drop packets on our own PC.
            if config.video_send {
                if let Err(e) = socket.set_send_buffer_size(4 * 1024 * 1024) {
                    warn!("could not enlarge UDP send buffer: {e}");
                }
            }
            UdpSocket::from_std(socket.into())?
        } else {
            udp
        };

        // Optimisation for non-receive case: set rx buffer size to zero.
        let udp = if cfg!(feature = "receive") {
            udp
        } else {
            let socket = Socket::from(udp.into_std()?);

            // Some operating systems do not allow setting the recv buffer to 0.
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            socket.set_recv_buffer_size(0)?;

            UdpSocket::from_std(socket.into())?
        };

        udp.connect((ready.ip, ready.port)).await?;

        // Follow Discord's IP Discovery procedures, in case NAT tunnelling is needed.
        let mut bytes = [0; IpDiscoveryPacket::const_packet_size()];
        {
            let mut view = MutableIpDiscoveryPacket::new(&mut bytes[..]).expect(
                "Too few bytes in 'bytes' for IPDiscovery packet.\
                    (Blame: IpDiscoveryPacket::const_packet_size()?)",
            );
            view.set_pkt_type(IpDiscoveryType::Request);
            view.set_length(70);
            view.set_ssrc(ready.ssrc);
        }

        udp.send(&bytes).await?;

        let (len, _addr) = udp.recv_from(&mut bytes).await?;
        {
            let view =
                IpDiscoveryPacket::new(&bytes[..len]).ok_or(Error::IllegalDiscoveryResponse)?;

            if view.get_pkt_type() != IpDiscoveryType::Response {
                return Err(Error::IllegalDiscoveryResponse);
            }

            // We could do something clever like binary search,
            // but possibility of UDP spoofing precludes us from
            // making the assumption we can find a "left edge" of '\0's.
            let nul_byte_index = view
                .get_address_raw()
                .iter()
                .position(|&b| b == 0)
                .ok_or(Error::IllegalIp)?;

            let address_str = std::str::from_utf8(&view.get_address_raw()[..nul_byte_index])
                .map_err(|_| Error::IllegalIp)?;

            let address = IpAddr::from_str(address_str).map_err(|_| Error::IllegalIp)?;

            if config.video_receive || config.video_send {
                if config.video_receive {
                    crate::video_hook::SELF_SSRC
                        .store(ready.ssrc, std::sync::atomic::Ordering::Relaxed);
                }
                // tinycord patch: offer Opus + H.264 (we encode only on our
                // own stream's connection).
                let select = serde_json::json!({
                    "op": 1,
                    "d": {
                        "protocol": "udp",
                        "data": {
                            "address": address.to_string(),
                            "port": view.get_port(),
                            "mode": chosen_crypto.to_request_str(),
                        },
                        "codecs": [
                            {"name": "opus", "type": "audio", "priority": 1000, "payload_type": 120},
                            {"name": "H264", "type": "video", "priority": 1000, "payload_type": 103,
                             "rtx_payload_type": 104, "encode": config.video_send, "decode": true},
                        ],
                    }
                });
                client.send_raw(select.to_string()).await?;
            } else {
                client
                    .send_json(&GatewayEvent::from(SelectProtocol {
                        protocol: "udp".into(),
                        data: ProtocolData {
                            address,
                            mode: chosen_crypto.to_request_str().into(),
                            port: view.get_port(),
                        },
                    }))
                    .await?;
            }
        }

        let (cipher, dave_session, dave_protocol_version) =
            init_cipher(&mut client, &info, chosen_crypto, &ws_msg_tx).await?;

        if config.video_receive && !config.video_send {
            // tinycord patch: ask the server to send us video at full quality.
            client
                .send_raw(serde_json::json!({"op": 15, "d": {"any": 100}}).to_string())
                .await?;
        }
        let dave_session = Arc::new(RwLock::new(dave_session));
        let dave_protocol_version = Arc::new(dave_protocol_version);

        info!("Connected to: {}", info.endpoint);

        info!("WS heartbeat duration {}ms.", hello.heartbeat_interval);

        #[cfg(feature = "receive")]
        let (udp_receiver_msg_tx, udp_receiver_msg_rx) = flume::unbounded();

        // NOTE: This causes the UDP Socket on "receive" to be non-blocking,
        // and the standard to be blocking. A UDP send should only WouldBlock if
        // you're sending more data than the OS can handle (not likely, and
        // at that point you should scale horizontally).
        //
        // If this is a problem for anyone, we can make non-blocking sends
        // queue up a delayed send up to a limit.
        #[cfg(feature = "receive")]
        let (udp_rx, udp_tx) = {
            let udp_tx = udp.into_std()?;
            let udp_rx = UdpSocket::from_std(udp_tx.try_clone()?)?;
            (udp_rx, udp_tx)
        };
        #[cfg(not(feature = "receive"))]
        let udp_tx = udp.into_std()?;

        let ssrc = ready.ssrc;

        if config.video_send {
            // tinycord patch: hand tinycord a way to send its own media.
            let sender = crate::video_hook::MediaSender::new(
                udp_tx.try_clone()?,
                cipher.clone(),
                dave_session.clone(),
                dave_protocol_version.clone(),
                ws_msg_tx.clone(),
                ready.ssrc,
                crate::video_hook::take_send_ssrcs(),
            );
            if let Ok(mut s) = crate::video_hook::SENDER.write() {
                *s = Some(Arc::new(sender));
            }
        }

        let mix_conn = MixerConnection {
            #[cfg(feature = "receive")]
            cipher: cipher.clone(),
            #[cfg(not(feature = "receive"))]
            cipher,
            dave_session: dave_session.clone(),
            dave_protocol_version: dave_protocol_version.clone(),
            crypto_state: chosen_crypto.into(),
            #[cfg(feature = "receive")]
            udp_rx: udp_receiver_msg_tx,
            udp_tx,
        };

        interconnect
            .mixer
            .send(MixerMessage::Ws(Some(ws_msg_tx.clone())))?;

        interconnect
            .mixer
            .send(MixerMessage::SetConn(mix_conn, ready.ssrc))?;

        #[cfg(feature = "receive")]
        let ssrc_tracker = Arc::new(SsrcTracker::default());

        let ws_state = AuxNetwork::new(
            ws_msg_rx,
            client,
            ssrc,
            hello.heartbeat_interval,
            idx,
            info.clone(),
            #[cfg(not(feature = "receive"))]
            dave_session,
            #[cfg(not(feature = "receive"))]
            dave_protocol_version,
            #[cfg(feature = "receive")]
            dave_session.clone(),
            #[cfg(feature = "receive")]
            dave_protocol_version.clone(),
            #[cfg(feature = "receive")]
            ssrc_tracker.clone(),
        );

        spawn(ws_task::runner(interconnect.clone(), ws_state));

        #[cfg(feature = "receive")]
        spawn(udp_rx::runner(
            interconnect.clone(),
            udp_receiver_msg_rx,
            cipher,
            chosen_crypto,
            config.clone(),
            udp_rx,
            ssrc_tracker,
            dave_session,
            dave_protocol_version,
        ));

        Ok(Connection {
            info,
            ssrc,
            ws: ws_msg_tx,
            video: if config.video_send { 2 } else if config.video_receive { 1 } else { 0 },
        })
    }

    #[instrument(skip(self))]
    pub async fn reconnect(&mut self, config: &Config) -> Result<()> {
        if let Some(t) = config.driver_timeout {
            timeout(t.into(), self.reconnect_inner()).await?
        } else {
            self.reconnect_inner().await
        }
    }

    #[instrument(skip(self))]
    pub async fn reconnect_inner(&mut self) -> Result<()> {
        let url = generate_url(&self.info.endpoint)?;

        // Thread may have died, we want to send to prompt a clean exit
        // (if at all possible) and then proceed as normal.
        let mut client = WsStream::connect(url).await?;
        client.set_tap(self.video);

        client
            .send_json(&GatewayEvent::from(Resume {
                server_id: self.info.guild_id.into(),
                session_id: self.info.session_id.clone(),
                token: self.info.token.clone(),
            }))
            .await?;

        let mut hello = None;
        let mut resumed = None;

        loop {
            let Some(value) = client.recv_event().await? else {
                continue;
            };

            match value {
                GatewayEvent::Resumed => {
                    resumed = Some(());
                    if hello.is_some() {
                        break;
                    }
                },
                GatewayEvent::Hello(h) => {
                    hello = Some(h);
                    if resumed.is_some() {
                        break;
                    }
                },
                other => {
                    self.ws.send(WsMessage::Deliver(other))?;
                },
            }
        }

        let hello =
            hello.expect("Hello packet expected in connection initialisation, but not found.");

        self.ws
            .send(WsMessage::SetKeepalive(hello.heartbeat_interval))?;
        self.ws.send(WsMessage::Ws(Box::new(client)))?;

        info!("Reconnected to: {}", &self.info.endpoint);
        Ok(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        info!("Disconnected");
    }
}

fn generate_url(endpoint: &str) -> Result<Url> {
    Url::parse(&format!("wss://{endpoint}/?v={VOICE_GATEWAY_VERSION}")).or(Err(Error::EndpointUrl))
}

#[inline]
async fn init_cipher(
    client: &mut WsStream,
    info: &ConnectionInfo,
    mode: CryptoMode,
    tx: &Sender<WsMessage>,
) -> Result<(Cipher, Option<davey::DaveSession>, AtomicU16)> {
    loop {
        let Some(value) = client.recv_event().await? else {
            continue;
        };

        match value {
            GatewayEvent::SessionDescription(desc) => {
                if desc.mode != mode.to_request_str() {
                    return Err(Error::CryptoModeInvalid);
                }

                let dave_session =
                    if let Some(version) = NonZeroU16::new(desc.dave_protocol_version) {
                        let mut session = davey::DaveSession::new(
                            version,
                            info.user_id.0.into(),
                            info.channel_id.0.into(),
                            None,
                        )
                        .map_err(Error::DaveInitializationError)?;

                        client
                            .send_binary(&GatewayEvent::DaveMlsKeyPackage(DaveMlsKeyPackage {
                                key_package: session
                                    .create_key_package()
                                    .map_err(Error::DaveCreateKeyPackageError)?,
                            }))
                            .await?;

                        Some(session)
                    } else {
                        None
                    };

                return Ok((
                    mode.cipher_from_key(&desc.secret_key)
                        .map_err(|_| Error::CryptoInvalidLength)?,
                    dave_session,
                    AtomicU16::new(desc.dave_protocol_version),
                ));
            },
            other => {
                // Discord can and will send user-specific payload packets during this time
                // which are needed to map SSRCs to `UserId`s.
                tx.send(WsMessage::Deliver(other))?;
            },
        }
    }
}
