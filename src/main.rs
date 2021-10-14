//! WebRTC SFU with horizontal scale design

use anyhow::{Result, Context};
use log::{debug, info, warn, error};
use interceptor::registry::Registry;
use rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_OPUS, MIME_TYPE_VP8};
use webrtc::api::APIBuilder;
use webrtc::media::rtp::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType};
use webrtc::media::rtp::rtp_receiver::RTCRtpReceiver;
use webrtc::media::rtp::rtp_sender::RTCRtpSender;
use webrtc::media::track::track_remote::TrackRemote;
use webrtc::media::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::media::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::peer::peer_connection::RTCPeerConnection;
use webrtc::peer::peer_connection::{
    OnTrackHdlrFn,
    OnICEConnectionStateChangeHdlrFn,
    OnPeerConnectionStateChangeHdlrFn,
    OnDataChannelHdlrFn,
};
use webrtc::peer::configuration::RTCConfiguration;
use webrtc::peer::ice::ice_connection_state::RTCIceConnectionState;
use webrtc::peer::ice::ice_server::RTCIceServer;
use webrtc::peer::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer::sdp::session_description::RTCSessionDescription;
use webrtc::peer::sdp::sdp_type::RTCSdpType;
use webrtc::data::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data::data_channel::RTCDataChannel;
use std::sync::Arc;
use std::collections::HashMap;
use tokio::time::Duration;
use tokio::sync::oneshot;
use tokio::sync::mpsc;
use actix_web::{get, post, web, App, HttpServer, Responder, HttpResponse};
use actix_web_httpauth::extractors::bearer::BearerAuth;
// use actix_cors::Cors;
use actix_files::Files;
use serde::{Deserialize, Serialize};
use once_cell::sync::Lazy;
use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};
use tracing::{Instrument, info_span};
use std::sync::Mutex;
use std::sync::RwLock;


fn main() -> Result<()> {
    // logger
    // bridge "log" crate and "tracing" crate
    tracing_log::LogTracer::init()?;
    // create "logs" dir if not exist
    if !std::path::Path::new("./logs").is_dir() {
        std::fs::create_dir("logs")?;
    }
    // logfile writer
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
    let file = format!("rtc.{}.log", now);
    let file_appender = tracing_appender::rolling::never("logs", file);
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    // compose our complex logger
    // 1. filter via RUST_LOG env
    // 2. output to stdout
    // 3. output to logfile
    let subscriber = tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())    // RUST_LOG env filter
        .with(fmt::Layer::new().with_writer(std::io::stdout))
        .with(fmt::Layer::new().with_writer(non_blocking));
    // set our logger as global default
    tracing::subscriber::set_global_default(subscriber).context("Unable to set global collector")?;

    web_main()?;

    Ok(())
}


#[derive(Debug, Default)]
struct State {
    rooms: HashMap<String, Room>,
}

#[derive(Debug, Default)]
struct Room {
    name: String,
    /// (user, track id) -> mime type
    user_track_to_mime: HashMap<(String, String), String>,
    sub_peers: HashMap<String, PeerConnetionInfo>,
    /// user -> token
    pub_tokens: HashMap<String, String>,
    sub_token: String,
}

#[derive(Debug, Default)]
struct PeerConnetionInfo {
    name: String,
    // pc: Option<RTCPeerConnection>,
    notify_message: Option<Arc<mpsc::Sender<String>>>,  // TODO: special enum for all the cases
    // notify_close: ...,
}

static HACK_STATE: Lazy<Mutex<State>> = Lazy::new(|| Default::default());


/////////////
// Publisher
/////////////

struct PublisherDetails {
    user: String,
    room: String,
    pc: Arc<RTCPeerConnection>,
    nats: nats::asynk::Connection,
}

impl PublisherDetails {
    fn get_nats_subect(&self) -> String {
        format!("rtc.{}.{}", self.room, self.user)
    }

    async fn create_pc() -> Result<RTCPeerConnection, webrtc::Error> {
        // Create a MediaEngine object to configure the supported codec
        info!("creating MediaEngine");
        let mut m = MediaEngine::default();

        // Setup the codecs you want to use.
        // We'll use a VP8 and Opus but you can also define your own
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_VP8.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: "".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 96,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;

        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48000,
                    channels: 2,
                    sdp_fmtp_line: "".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )?;

        // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
        // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
        // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
        // for each PeerConnection.
        let mut registry = Registry::new();

        // Use the default set of Interceptors
        registry = register_default_interceptors(registry, &mut m)?;

        // Create the API object with the MediaEngine
        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();

        // Prepare the configuration
        info!("preparing RTCConfiguration");
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };

        info!("creating PeerConnection");
        // Create a new RTCPeerConnection
        api.new_peer_connection(config).await
    }

    async fn add_transceivers_based_on_sdp(&self, offer: &str) -> Result<()> {
        info!("add tranceivers based on SDP offer");

        // TODO: more efficent way?
        // TODO: limitation for safety?
        //
        // Allow us to receive N video/audio tracks based on SDP Offer
        // sample of SDP line: "m=video 9 UDP/TLS/RTP/SAVPF 96"
        for sdp_media in offer.rsplit_terminator("m=") {
            let msid = sdp_media.rsplit_once("msid:");
            if msid.is_none() {
                continue;
            }
            let (_, msid) = msid.unwrap();
            let id = msid.split_whitespace().take(2).skip(1).next();
            if id.is_none() {
                continue;
            }
            let id = id.unwrap();

            // TODO: global cache for media count
            if sdp_media.starts_with("video") {
                // FIXME: use better way
                // TODO: add id in
                {
                    let room_info = HACK_STATE.lock();
                    let mut room_info = room_info.unwrap();
                    let room_info = room_info.rooms.entry(self.room.clone()).or_default();
                    room_info.user_track_to_mime.insert((self.user.clone(), id.to_string()), "video".to_string());
                }
                self.pc
                    .add_transceiver_from_kind(RTPCodecType::Video, &[])
                    .await?;
            } else if sdp_media.starts_with("audio") {
                // FIXME: use better way
                // TODO: add id in
                {
                    let room_info = HACK_STATE.lock();
                    let mut room_info = room_info.unwrap();
                    let room_info = room_info.rooms.entry(self.room.clone()).or_default();
                    room_info.user_track_to_mime.insert((self.user.clone(), id.to_string()), "audio".to_string());
                }
                self.pc
                    .add_transceiver_from_kind(RTPCodecType::Audio, &[])
                    .await?;
            }
        }

        Ok(())
    }

    /// Handler for incoming streams
    fn on_track(&self) -> OnTrackHdlrFn {
        let span = tracing::Span::current();

        let nc = self.nats.clone();
        let pc = self.pc.clone();
        let subject = self.get_nats_subect();

        Box::new(move |track: Option<Arc<TrackRemote>>, _receiver: Option<Arc<RTCRtpReceiver>>| {
            let _enter = span.enter();  // populate user & room info in following logs

            if let Some(track) = track {
                // Send a PLI on an interval so that the publisher is pushing a keyframe every rtcpPLIInterval
                let media_ssrc = track.ssrc();
                let pc = Arc::clone(&pc);
                tokio::spawn(async move {
                    let mut result = Ok(0);
                    while result.is_ok() {
                        let timeout = tokio::time::sleep(Duration::from_secs(3));
                        tokio::pin!(timeout);

                        tokio::select! {
                            _ = timeout.as_mut() =>{
                                result = pc.write_rtcp(&PictureLossIndication{
                                        sender_ssrc: 0,
                                        media_ssrc,
                                }).await;
                            }
                        };
                    }
                }.instrument(span.clone()));

                let nc = nc.clone();
                let subject = subject.clone();
                // push RTP to NATS
                // use ID to disquish streams from same publisher
                // TODO: can we use SSRC?
                tokio::spawn(async move {
                    // FIXME: the id here generated from browser might be "{...}"
                    let subject = format!("{}.{}", subject, track.id().await);
                    info!("publish to {}", subject);
                    let mut b = vec![0u8; 1500];
                    while let Ok((n, _)) = track.read(&mut b).await {
                        nc.publish(&subject, &b[..n]).await?;
                    }
                    Result::<()>::Ok(())
                }.instrument(span.clone()));
            }

            Box::pin(async {})
        })
    }

    fn on_ice_connection_state_change(&self) -> OnICEConnectionStateChangeHdlrFn {
        let span = tracing::Span::current();
        Box::new(move |connection_state: RTCIceConnectionState| {
            let _enter = span.enter();  // populate user & room info in following logs
            info!("ICE Connection State has changed: {}", connection_state);
            // if connection_state == RTCIceConnectionState::Connected {
            // }
            Box::pin(async {})
        })
    }

    fn on_peer_connection_state_change(&self) -> OnPeerConnectionStateChangeHdlrFn {
        let span = tracing::Span::current();

        let room = self.room.clone();
        let user = self.user.clone();

        Box::new(move |s: RTCPeerConnectionState| {
            let _enter = span.enter();  // populate user & room info in following logs

            info!("PeerConnection State has changed: {}", s);

            let room = room.clone();   // TODO: avoid this?
            let user = user.clone();   // TODO: avoid this?

            if s == RTCPeerConnectionState::Failed {
                // Wait until PeerConnection has had no network activity for 30 seconds or another failure. It may be reconnected using an ICE Restart.
                // Use webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster timeout.
                // Note that the PeerConnection may come back from PeerConnectionStateDisconnected.
                info!("Peer Connection has gone to failed exiting: Done forwarding");

                // TODO: make sure we will cleanup related stuffs
            }

            if s == RTCPeerConnectionState::Disconnected {
                // TODO: also remove the media from state

                // tell subscribers a new publisher just leave
                // ask subscribers to renegotiation
                return Box::pin(async move {
                    let room = room.clone();   // TODO: avoid this?
                    let user = user.clone();   // TODO: avoid this?

                    // remove from global state
                    let mut tracks = vec![];    // TODO: better mechanism
                    {
                        let mut state = HACK_STATE.lock().unwrap();
                        let room_obj = state.rooms.get(&room).unwrap();
                        for ((pub_user, track_id), _) in room_obj.user_track_to_mime.iter() {
                            if pub_user == &user {
                                tracks.push(track_id.to_string());
                            }
                        }
                        let user_track_to_mime = &mut state.rooms.get_mut(&room).unwrap().user_track_to_mime;
                        for track in tracks {
                            user_track_to_mime.remove(&(user.to_string(), track.to_string()));
                        }
                    }

                    let subs = {
                        let state = HACK_STATE.lock().unwrap();
                        let room = state.rooms.get(&room).unwrap();
                        room.sub_peers.iter().map(|(_, sub)| sub.notify_message.as_ref().unwrap().clone()).collect::<Vec<_>>()
                    };
                    for sub in subs {
                        // TODO: special enum for all the cases
                        sub.send(format!("PUB_LEFT {}", user)).await;
                    }
                });

                // TODO: make sure we will cleanup related stuffs
            }

            // if s == RTCPeerConnectionState::Connected {
            //     info!("webrtc to nats connected!");
            // }

            Box::pin(async {})
        })
    }

    /// tell subscribers a new publisher just join
    /// ask subscribers to renegotiation
    async fn notify_subs_for_join(&self) {
        info!("notify subscribers for publisher join");
        let subs = {
            let state = HACK_STATE.lock().unwrap();
            let room = state.rooms.get(&self.room).unwrap();
            room.sub_peers.iter().map(|(_, sub)| sub.notify_message.as_ref().unwrap().clone()).collect::<Vec<_>>()
        };
        let user = self.user.clone();
        for sub in subs {
            // TODO: special enum for all the cases
            sub.send(format!("PUB_JOIN {}", user)).await.unwrap();
        }
    }
}

/// Extract RTP streams from WebRTC, and send it to NATS
///
/// based on [rtp-forwarder](https://github.com/webrtc-rs/webrtc/tree/master/examples/rtp-forwarder) example
#[tracing::instrument(name = "pub", skip(offer, answer_tx), level = "info")]  // following log will have "pub{room=..., user=...}" in INFO level
async fn webrtc_to_nats(room: String, user: String, offer: String, answer_tx: oneshot::Sender<String>, tid: u16) -> Result<()> {
    // NATS
    // TODO: share NATS connection
    info!("connecting NATS");
    let nc = nats::asynk::connect("localhost").await?;

    let peer_connection = Arc::new(PublisherDetails::create_pc().await?);
    let publisher = PublisherDetails {
        user: user.clone(),
        room: room.clone(),
        pc: peer_connection.clone(),
        nats: nc.clone()
    };  // TODO: remove clone

    publisher.add_transceivers_based_on_sdp(&offer).await?;

    // build SDP Offer type
    let mut sdp = RTCSessionDescription::default();
    sdp.sdp_type = RTCSdpType::Offer;
    sdp.sdp = offer;
    let offer = sdp;

    // Set a handler for when a new remote track starts, this handler will forward data to our UDP listeners.
    // In your application this is where you would handle/process audio/video
    peer_connection
        .on_track(publisher.on_track())
        .instrument(info_span!("pub"))
        .await;

    // Set the handler for ICE connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection
        .on_ice_connection_state_change(publisher.on_ice_connection_state_change())
        .await;

    // Set the handler for Peer connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection
        .on_peer_connection_state_change(publisher.on_peer_connection_state_change())
        .await;

    // Set the remote SessionDescription
    info!("PC set remote SDP");
    peer_connection.set_remote_description(offer).await?;

    // Create an answer
    info!("PC create local SDP");
    let answer = peer_connection.create_answer(None).await?;

    // Create channel that is blocked until ICE Gathering is complete
    let mut gather_complete = peer_connection.gathering_complete_promise().await;

    // Sets the LocalDescription, and starts our UDP listeners
    peer_connection.set_local_description(answer).await?;

    // Block until ICE Gathering is complete, disabling trickle ICE
    // we do this because we only can exchange one signaling message
    // in a production application you should exchange ICE Candidates via OnICECandidate
    let _ = gather_complete.recv().await;

    // Send out the SDP answer via Sender
    if let Some(local_desc) = peer_connection.local_description().await {
        info!("PC send local SDP");
        answer_tx.send(local_desc.sdp).unwrap();
    } else {
        // TODO: when will this happen?
        warn!("generate local_description failed!");
    }

    publisher.notify_subs_for_join().await;

    // limit a publisher to 3 hours for now
    // after 3 hours, we close the connection
    //
    // TODO: a signal to trigger it ealier when PC failed
    tokio::time::sleep(Duration::from_secs(3 * 60 * 60)).await;
    peer_connection.close().await?;
    info!("leaving main function");

    Ok(())
}


//////////////
// Subscriber
//////////////

struct SubscriberDetails {
    user: String,
    room: String,
    pc: Arc<RTCPeerConnection>,
    nats: nats::asynk::Connection,
    tracks: Arc<RwLock<HashMap<(String, String), Arc<TrackLocalStaticRTP>>>>,
    user_media_to_tracks: Arc<RwLock<HashMap<(String, String), Arc<TrackLocalStaticRTP>>>>,
    user_media_to_senders: Arc<RwLock<HashMap<(String, String), Arc<RTCRtpSender>>>>,
    rtp_senders: Arc<RwLock<HashMap<(String, String), Arc<RTCRtpSender>>>>,
    notify_sender: Option<mpsc::Sender<String>>,
    notify_receiver: Option<mpsc::Receiver<String>>,
}

impl SubscriberDetails {
    fn get_nats_subect(&self) -> String {
        format!("rtc.{}.*.*", self.room)
    }

    async fn create_pc() -> Result<RTCPeerConnection, webrtc::Error> {
        info!("creating MediaEngine");
        // Create a MediaEngine object to configure the supported codec
        let mut m = MediaEngine::default();

        // m.register_default_codecs()?;
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_VP8.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: "".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 96,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;

        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48000,
                    channels: 2,
                    sdp_fmtp_line: "".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )?;

        // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
        // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
        // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
        // for each PeerConnection.
        let mut registry = Registry::new();

        // Use the default set of Interceptors
        registry = register_default_interceptors(registry, &mut m)?;

        // Create the API object with the MediaEngine
        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();

        info!("preparing RTCConfiguration");
        // Prepare the configuration
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };

        info!("creating PeerConnection");
        // Create a new RTCPeerConnection
        api.new_peer_connection(config).await
    }

    async fn add_trasceivers_based_on_room(&self) -> Result<()> {
        // Create Track that we send video back to browser on
        // TODO: dynamic creation
        // TODO: how to handle video/audio from same publisher and send to different track?
        // HACK_MEDIA.lock().unwrap().push("video".to_string());

        let media = HACK_STATE.lock().unwrap().rooms.get(&self.room).unwrap().user_track_to_mime.clone(); // TODO: avoid this?
        for ((user, track_id), mime) in media {
            let app_id = match mime.as_ref() {
                "video" => "video0",
                "audio" => "audio0",
                _ => unreachable!(),
            };

            let mime = match mime.as_ref() {
                "video" => MIME_TYPE_VP8,
                "audio" => MIME_TYPE_OPUS,
                _ => unreachable!(),
            };

            let track = Arc::new(TrackLocalStaticRTP::new(
                RTCRtpCodecCapability {
                    mime_type: mime.to_owned(),
                    ..Default::default()
                },
                // id is the unique identifier for this Track.
                // This should be unique for the stream, but doesn’t have to globally unique.
                // A common example would be 'audio' or 'video' or 'desktop' or 'webcam'
                app_id.to_string(),         // msid, application id part
                // stream_id is the group this track belongs too.
                // This must be unique.
                user.to_string(),           // msid, group id part
            ));

            // for later dyanmic RTP dispatch from NATS
            self.tracks.write().unwrap().insert((user.clone(), track_id.clone()), track.clone());
            self.user_media_to_tracks.write().unwrap().insert((user.clone(), app_id.to_string()), track.clone());

            let rtp_sender = self.pc
                .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
                .await?;

            // for later cleanup
            self.rtp_senders.write().unwrap().insert((user.clone(), track_id.clone()), rtp_sender.clone());
            self.user_media_to_senders.write().unwrap().insert((user.clone(), app_id.to_string()), rtp_sender.clone());

            // Read incoming RTCP packets
            // Before these packets are returned they are processed by interceptors. For things
            // like NACK this needs to be called.
            tokio::spawn(async move {
                let mut rtcp_buf = vec![0u8; 1500];
                while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
                info!("sub: leaving RTP sender read");
                Result::<()>::Ok(())
            });
        }

        Ok(())
    }

    fn register_notify_message(&mut self) {
        // set notify
        let (sender, receiver) = mpsc::channel(10);
        let mut state = HACK_STATE.lock().unwrap();
        let room = state.rooms.get_mut(&self.room).unwrap();
        let user = room.sub_peers.entry(self.user.clone()).or_default();
        user.notify_message = Some(Arc::new(sender.clone()));
        self.notify_sender = Some(sender);
        self.notify_receiver = Some(receiver);
    }

    fn on_ice_connection_state_change(&self) -> OnICEConnectionStateChangeHdlrFn {
        let span = tracing::Span::current();
        Box::new(move |connection_state: RTCIceConnectionState| {
            let _enter = span.enter();  // populate user & room info in following logs
            info!("ICE Connection State has changed: {}", connection_state);
            // if connection_state == RTCIceConnectionState::Connected {
            // }
            Box::pin(async {})
        })
    }

    fn on_peer_connection_state_change(&self) -> OnPeerConnectionStateChangeHdlrFn {
        let span = tracing::Span::current();
        Box::new(move |s: RTCPeerConnectionState| {
            let _enter = span.enter();  // populate user & room info in following logs
            info!("PeerConnection State has changed: {}", s);
            if s == RTCPeerConnectionState::Failed {
                // Wait until PeerConnection has had no network activity for 30 seconds or another failure. It may be reconnected using an ICE Restart.
                // Use webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster timeout.
                // Note that the PeerConnection may come back from PeerConnectionStateDisconnected.
                info!("sub: Peer Connection has gone to failed exiting: Done forwarding");
                // TODO: make sure we cleanup related resource
            }

            // if s == RTCPeerConnectionState::Connected {
            // }

            Box::pin(async {})
        })
    }

    fn on_data_channel(&mut self) -> OnDataChannelHdlrFn {
        let span = tracing::Span::current();

        let pc = self.pc.clone();
        let tracks2 = self.tracks.clone();
        let rtp_senders = self.rtp_senders.clone();
        let notify_message = Arc::new(tokio::sync::Mutex::new(self.notify_receiver.take().unwrap()));
        let user_media_to_tracks2 = self.user_media_to_tracks.clone();
        let user_media_to_senders2 = self.user_media_to_senders.clone();
        let notify_sender = self.notify_sender.as_ref().unwrap().clone();
        let room = self.room.clone();
        // tokio::pin!(notify_message);
        // let mut notify_message = std::boxed::Box::pin(receiver);

        Box::new(move |dc: Arc<RTCDataChannel>| {
            let _enter = span.enter();  // populate user & room info in following logs

            let notify_message = Arc::clone(&notify_message);    // TODO: avoid this?
            let room = room.clone();    // TODO: avoid this?
            let pc = Arc::clone(&pc);  // TODO: avoid this?
            let pc2 = Arc::clone(&pc);  // TODO: avoid this?
            let tracks2 = Arc::clone(&tracks2);       // TODO: avoid this?
            let rtp_senders = rtp_senders.clone();
            let user_media_to_tracks2 = Arc::clone(&user_media_to_tracks2);    // TODO: avoid this?
            let user_media_to_senders2 = Arc::clone(&user_media_to_senders2);  // TODO: avoid this?
            let notify_sender = notify_sender.clone();

            let dc_label = dc.label().to_owned();

            // only accept data channel with label "control"
            if dc_label != "control" {
               return Box::pin(async {});
            }

            let dc_id = dc.id();
            info!("New DataChannel {} {}", dc_label, dc_id);

            // Register channel opening handling
            // let pc = pc.clone();
            Box::pin(async move {
                let span = tracing::Span::current();

                let dc2 = Arc::clone(&dc);
                let dc_label2 = dc_label.clone();
                let dc_id2 = dc_id.clone();
                let tracks2 = Arc::clone(&tracks2);       // TODO: avoid this?
                let rtp_senders2 = rtp_senders.clone();
                let user_media_to_tracks2 = Arc::clone(&user_media_to_tracks2);    // TODO: avoid this?
                let user_media_to_senders2 = Arc::clone(&user_media_to_senders2);  // TODO: avoid this?
                let pc = pc.clone();
                dc.on_open(Box::new(move || {
                    let _enter = span.enter();  // populate user & room info in following logs
                    info!("Data channel '{}'-'{}' open", dc_label2, dc_id2);

                    Box::pin(async move {
                        let tracks2 = Arc::clone(&tracks2);       // TODO: avoid this?
                        let rtp_senders2 = Arc::clone(&rtp_senders2);    // TODO: avoid this?
                        let mut result = Ok(0);
                        let mut notify_message = notify_message.lock().await;  // TODO: avoid this?
                        while result.is_ok() {
                            let timeout = tokio::time::sleep(Duration::from_secs(30));
                            tokio::pin!(timeout);

                            tokio::select! {
                                msg = notify_message.recv() => {
                                    let media = HACK_STATE.lock().unwrap().rooms.get(&room).unwrap().user_track_to_mime.clone(); // TODO: avoid this?

                                    let videos = media.iter().filter(|(_, mime)| mime.as_str() == "video").count();
                                    let audios = media.iter().filter(|(_, mime)| mime.as_str() == "audio").count();
                                    let msg = msg.unwrap();

                                    if msg.starts_with("PUB_LEFT ") {
                                        // delete old track for PUB_LEFT
                                        // let left_user = msg.splitn(2, " ").next().unwrap();
                                        // let mut remove_targets = vec![];
                                        // for ((pub_user, track_id), _) in tracks2.read().unwrap().iter() {
                                        //     if pub_user == left_user {
                                        //         info!("remove track for {} {}", pub_user, track_id);
                                        //         remove_targets.push((pub_user.to_string(), track_id.to_string()));
                                        //     }
                                        // }
                                        // for target in remove_targets {
                                        //     tracks2.write().unwrap().remove(&target);
                                        //     let rtp_sender = {
                                        //         let rtp_senders = rtp_senders2.read().unwrap();
                                        //         rtp_senders.get(&target).unwrap().clone()
                                        //     };
                                        //     // remove track from subscriber's PeerConnection
                                        //     pc.remove_track(&rtp_sender).await.unwrap();
                                        //     // rtp_sender.stop().await.unwrap();
                                        //     rtp_senders2.write().unwrap().remove(&target);
                                        //     // TODO: make sure RTCP task is killed
                                        // }
                                    }

                                    if msg.starts_with("PUB_JOIN ") {
                                        // let join_user = msg.splitn(2, " ").skip(1).next().unwrap();

                                        // add new track for PUB_JOIN
                                        for ((pub_user, track_id), mime) in media {
                                            if tracks2.read().unwrap().contains_key(&(pub_user.clone(), track_id.clone())) {
                                                continue;
                                            }

                                            // hardcode this for now
                                            // we may need to change it to support like screen sharing later
                                            let app_id = match mime.as_ref() {
                                                "video" => "video0",
                                                "audio" => "audio0",
                                                _ => unreachable!(),
                                            };

                                            let mime = match mime.as_ref() {
                                                "video" => MIME_TYPE_VP8,
                                                "audio" => MIME_TYPE_OPUS,
                                                _ => unreachable!(),
                                            };

                                            // info!("{} add new track for {} {}", user, pub_user, track_id);   // TODO: or use tracing's span
                                            info!("add new track for {} {}", pub_user, track_id);

                                            let track = Arc::new(TrackLocalStaticRTP::new(
                                                RTCRtpCodecCapability {
                                                    mime_type: mime.to_owned(),
                                                    ..Default::default()
                                                },
                                                // id is the unique identifier for this Track.
                                                // This should be unique for the stream, but doesn’t have to globally unique.
                                                // A common example would be 'audio' or 'video' or 'desktop' or 'webcam'
                                                app_id.to_string(),     // msid, application id part
                                                // stream_id is the group this track belongs too.
                                                // This must be unique.
                                                pub_user.to_string(),   // msid, group id part
                                            ));

                                            // for later dyanmic RTP dispatch from NATS
                                            tracks2.write().unwrap().insert((pub_user.to_string(), track_id.to_string()), track.clone());

                                            // TODO: cleanup old track
                                            user_media_to_tracks2.write().unwrap().entry((pub_user.to_string(), app_id.to_string()))
                                                .and_modify(|e| *e = track.clone())
                                                .or_insert(track.clone());

                                            // user_media_to_senders2
                                            let sender = {
                                                let user_media_to_senders = user_media_to_senders2.read().unwrap();
                                                user_media_to_senders.get(&(pub_user.to_string(), app_id.to_string())).cloned().clone()
                                            };

                                            if let Some(sender) = sender {
                                                // reuse RtcRTPSender
                                                // apply new track
                                                info!("switch track for {} {}", pub_user, track_id);
                                                sender.replace_track(Some(track.clone())).await.unwrap();
                                                info!("switch track for {} {} done", pub_user, track_id);
                                            } else {
                                                // add tracck to pc
                                                // insert rtp sender to cache
                                                let pc = Arc::clone(&pc);
                                                let pub_user = pub_user.clone();
                                                let track_id = track_id.clone();
                                                let track = track.clone();
                                                let rtp_senders2 = rtp_senders2.clone();
                                                let user_media_to_senders2 = user_media_to_senders2.clone();

                                                // Read incoming RTCP packets
                                                // Before these packets are returned they are processed by interceptors. For things
                                                // like NACK this needs to be called.
                                                tokio::spawn(async move {
                                                    info!("create new rtp sender for {} {}", pub_user, track_id);
                                                    let rtp_sender = pc
                                                        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
                                                        .await?;

                                                    rtp_senders2.write().unwrap().insert((pub_user.clone(), track_id.clone()), rtp_sender.clone());
                                                    user_media_to_senders2.write().unwrap().insert((pub_user.clone(), app_id.to_string()), rtp_sender.clone());
                                                    let mut rtcp_buf = vec![0u8; 1500];
                                                    while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
                                                    info!("sub: leaving RTP sender read");
                                                    Result::<()>::Ok(())
                                                }.instrument(tracing::Span::current()));
                                            }
                                        }
                                    }

                                    dc2.send_text(msg).await;
                                    dc2.send_text(format!("RENEGOTIATION videos {} audios {}", videos, audios)).await;
                                },
                                _ = timeout.as_mut() => {
                                    let message = "hello".to_string();
                                    info!("Sending '{}'", message);
                                    result = dc2.send_text(message).await;
                                }
                            };
                        }
                    }.instrument(span.clone()))
                })).await;

                // Register text message handling
                let span = tracing::Span::current();
                let dc3 = Arc::clone(&dc);  // TODO: avoid this?
                let notify_sender = notify_sender.clone();
                dc.on_message(Box::new(move |msg: DataChannelMessage| {
                    let _enter = span.enter();  // populate user & room info in following logs
                    let pc = Arc::clone(&pc2);  // TODO: avoid this?
                    let dc3 = Arc::clone(&dc3);  // TODO: avoid this?
                    let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
                    let notify_sender = notify_sender.clone();
                    info!("Message from DataChannel '{}': '{:.20}'", dc_label, msg_str);
                    if msg_str.starts_with("SDP_OFFER ") {
                        let offer = msg_str.splitn(2, " ").skip(1).next().unwrap();
                        debug!("got new SDP offer: {}", offer);
                        // build SDP Offer type
                        let mut sdp = RTCSessionDescription::default();
                        sdp.sdp_type = RTCSdpType::Offer;
                        sdp.sdp = offer.to_string();
                        let offer = sdp;
                        return Box::pin(async move {
                            let dc3 = Arc::clone(&dc3);
                            pc.set_remote_description(offer).await.unwrap();
                            info!("updated new SDP offer");
                            let answer = pc.create_answer(None).await.unwrap();
                            pc.set_local_description(answer.clone()).await.unwrap();
                            if let Some(answer) = pc.local_description().await {
                                info!("sent new SDP answer");
                                dc3.send_text(format!("SDP_ANSWER {}", answer.sdp)).await;
                            }
                        }.instrument(span.clone()));
                    }

                    // FIXME: remove this?
                    if msg_str.starts_with("RENEGOTIATION") {
                        let notify_sender = notify_sender.clone();
                        return Box::pin(async move {
                            notify_sender.send("RENEGOTIATION".to_string()).await;
                        });
                    }

                    Box::pin(async {})
                })).instrument(tracing::Span::current()).await;
            }.instrument(span.clone()))
        })
    }

    async fn spawn_rtp_foward_task(&self) -> Result<()> {
        // get RTP from NATS
        let subject = self.get_nats_subect();
        let sub = self.nats.subscribe(&subject).await?;
        let tracks = self.tracks.clone();

        // Read RTP packets forever and send them to the WebRTC Client
        tokio::spawn(async move {
            use webrtc::Error;
            while let Some(msg) = sub.next().await {
                let raw_rtp = msg.data;

                // TODO: real dyanmic dispatch for RTP
                // subject sample: "rtc.1234.user1.video1"
                let mut it = msg.subject.rsplitn(3, ".").take(2);
                let track_id = it.next().unwrap().to_string();
                let user = it.next().unwrap().to_string();
                let track = {
                    let tracks = tracks.read().unwrap();
                    let track = tracks.get(&(user, track_id));
                    // FIXME: we should always prepare all the tracks for sending RTP
                    // TODO: create new tracks in renegotiation case
                    if track.is_none() {
                        continue;
                    }
                    track.unwrap().clone()
                };

                if let Err(err) = track.write(&raw_rtp).await {
                    error!("nats forward err: {:?}", err);
                    if Error::ErrClosedPipe == err {
                        // The peerConnection has been closed.
                        return;
                    } else {
                        error!("track write err: {}", err);
                        // TODO: cleanup?
                        // std::process::exit(0);
                    }
                }
            }
        });

        Ok(())
    }

    // fn on_pub_join()
    // fn on_pub_left()
    // fn on_command()
    // fn on_renegotiation()
}


/// Pull RTP streams from NATS, and send it to WebRTC
///
// based on [rtp-to-webrtc](https://github.com/webrtc-rs/webrtc/tree/master/examples/rtp-to-webrtc)
#[tracing::instrument(name = "sub", skip(offer, answer_tx), level = "info")]  // following log will have "sub{room=..., user=...}" in INFO level
async fn nats_to_webrtc(room: String, user: String, offer: String, answer_tx: oneshot::Sender<String>, tid: u16) -> Result<()> {
    // build SDP Offer type
    let mut sdp = RTCSessionDescription::default();
    sdp.sdp_type = RTCSdpType::Offer;
    sdp.sdp = offer;
    let offer = sdp;

    // NATS
    // TODO: share NATS connection
    let nc = nats::asynk::connect("localhost").await?;
    let peer_connection = Arc::new(SubscriberDetails::create_pc().await?);

    let mut subscriber = SubscriberDetails {
        user: user.clone(),
        room: room.clone(),
        nats: nc.clone(),
        pc: peer_connection.clone(),
        tracks: Default::default(),
        user_media_to_tracks: Default::default(),
        user_media_to_senders: Default::default(),
        rtp_senders: Default::default(),
        notify_sender: None,
        notify_receiver: None,
    };

    subscriber.add_trasceivers_based_on_room().await?;
    subscriber.register_notify_message();

    // Set the handler for ICE connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection
        .on_ice_connection_state_change(subscriber.on_ice_connection_state_change())
        .await;

    // Register data channel creation handling
    peer_connection
        .on_data_channel(subscriber.on_data_channel())
        .await;

    // Set the handler for Peer connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection
        .on_peer_connection_state_change(subscriber.on_peer_connection_state_change())
        .await;

    // Set the remote SessionDescription
    peer_connection.set_remote_description(offer).await?;

    // Create an answer
    let answer = peer_connection.create_answer(None).await?;

    // Create channel that is blocked until ICE Gathering is complete
    let mut gather_complete = peer_connection.gathering_complete_promise().await;

    // Sets the LocalDescription, and starts our UDP listeners
    peer_connection.set_local_description(answer).await?;

    // Block until ICE Gathering is complete, disabling trickle ICE
    // we do this because we only can exchange one signaling message
    // in a production application you should exchange ICE Candidates via OnICECandidate
    let _ = gather_complete.recv().await;

    // Output the answer in base64 so we can paste it in browser
    if let Some(local_desc) = peer_connection.local_description().await {
        info!("PC send local SDP");
        answer_tx.send(local_desc.sdp).unwrap();
    } else {
        // TODO: when will this happen?
        warn!("generate local_description failed!");
    }

    subscriber.spawn_rtp_foward_task().await?;

    // limit a subscriber to 3 hours for now
    // after 3 hours, we close the connection
    //
    // TODO: a signal to trigger it ealier when PC failed
    tokio::time::sleep(Duration::from_secs(3 * 60 * 60)).await;
    peer_connection.close().await?;
    info!("leaving main function");

    Ok(())
}


/// Web server for communicating with web clients
#[actix_web::main]
async fn web_main() -> std::io::Result<()> {
    HttpServer::new(||
            App::new()
                // .wrap(Cors::default())
                // enable logger
                .wrap(actix_web::middleware::Logger::default())
                .service(Files::new("/static", "site").prefer_utf8(true))   // demo site
                .service(create_room)
                .service(create_pub)
                .service(publish)
                .service(subscribe_all)
                .service(list)
        )
        .bind("127.0.0.1:8080")?
        .run()
        .await
}

#[derive(Debug, Serialize, Deserialize)]
struct CreateRoomParams {
    room: String,
    token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CreatePubParams {
    room: String,
    id: String,
    token: Option<String>,
}


#[post("/create/room")]
async fn create_room(params: web::Json<CreateRoomParams>) -> impl Responder {
    // TODO: save to cache that shared across instances
    info!("create room: {:?}", params);
    if let Some(token) = params.token.clone() {
        let mut state = HACK_STATE.lock().unwrap();
        let mut room = state.rooms.entry(params.room.clone()).or_default();
        room.sub_token = token;
    }
    "room set"
}


#[post("/create/pub")]
async fn create_pub(params: web::Json<CreatePubParams>) -> impl Responder {
    // TODO: save to cache that shared across instances
    info!("create pub: {:?}", params);
    if let Some(token) = params.token.clone() {
        let mut state = HACK_STATE.lock().unwrap();
        let room = state.rooms.entry(params.room.clone()).or_default();
        let pub_token = room.pub_tokens.entry(params.id.clone()).or_default();
        *pub_token = token;
    }
    "pub set"
}


/// WebRTC WHIP compatible endpoint for publisher
#[post("/pub/{room}/{id}")]
async fn publish(auth: BearerAuth,
                 path: web::Path<(String, String)>,
                 sdp: web::Bytes) -> impl Responder {
                 // web::Json(sdp): web::Json<RTCSessionDescription>) -> impl Responder {
    let (room, id) = path.into_inner();

    // TODO: verify "Content-Type: application/sdp"

    // token verification
    {
        let mut state = HACK_STATE.lock().unwrap();
        let room = state.rooms.entry(room.clone()).or_default();
        let token = room.pub_tokens.get(&id);
        // if let Some(token) = token {
        //     if token != auth.token() {
        //         return HttpResponse::Unauthorized().body("bad token");
        //     }
        // }
    }

    let sdp = String::from_utf8(sdp.to_vec()).unwrap(); // FIXME: no unwrap
    debug!("pub: auth {} sdp {:.20?}", auth.token(), sdp);
    let (tx, rx) = tokio::sync::oneshot::channel();

    // get a time based id to represent following Tokio task for this user
    // if user call it again later
    // we will be able to identify in logs
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros();
    let tid = now.wrapping_div(10000) as u16;

    tokio::spawn(webrtc_to_nats(room.clone(), id.clone(), sdp, tx, tid));
    // TODO: timeout
    let sdp_answer = rx.await.unwrap();     // FIXME: no unwrap
    debug!("SDP answer: {:.20}", sdp_answer);
    HttpResponse::Created() // 201
        .content_type("application/sdp")
        .append_header(("Location", ""))    // TODO: what's the need?
        .body(sdp_answer)
}

#[post("/sub/{room}/{id}")]
async fn subscribe_all(auth: BearerAuth,
                       path: web::Path<(String, String)>,
                       sdp: web::Bytes) -> impl Responder {
    let (room, id) = path.into_inner();

    // TODO: verify "Content-Type: application/sdp"

    // token verification
    // TODO: per user token?
    {
        let mut state = HACK_STATE.lock().unwrap();
        let room = state.rooms.entry(room.clone()).or_default();
        // if room.sub_token != auth.token() {
        //     return HttpResponse::Unauthorized().body("bad token");
        // }
    }

    let sdp = String::from_utf8(sdp.to_vec()).unwrap(); // FIXME: no unwrap
    debug!("sub_all: auth {} sdp {:.20?}", auth.token(), sdp);
    let (tx, rx) = tokio::sync::oneshot::channel();

    // get a time based id to represent following Tokio task for this user
    // if user call it again later
    // we will be able to identify in logs
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros();
    let tid = now.wrapping_div(10000) as u16;

    tokio::spawn(nats_to_webrtc(room.clone(), id.clone(), sdp, tx, tid));
    // TODO: timeout
    let sdp_answer = rx.await.unwrap();     // FIXME: no unwrap
    debug!("SDP answer: {:.20}", sdp_answer);
    HttpResponse::Created() // 201
        .content_type("application/sdp")
        .body(sdp_answer)
}

#[post("/info/{room}/list")]
async fn list(auth: BearerAuth,
              path: web::Path<String>) -> impl Responder {
    let room = path.into_inner();
    // TODO: token verification
    // auth.token().to_string()
    // return participants
    unimplemented!();
    ""
}
