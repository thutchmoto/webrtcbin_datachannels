use anyhow::{anyhow, bail, Result};
use crossbeam_channel::unbounded;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use log::{debug, info, trace};
use std::thread;
use std::time::Duration;

fn main() -> Result<()> {
    env_logger::init();
    gst::init()?;

    let (pipeline1, peer1) = create_pipeline_with_datachannel();
    produce_mulaw(&pipeline1, &peer1);

    let (pipeline2, peer2) = create_pipeline_with_media();

    log_states("peer1", &peer1);
    log_states("peer2", &peer2);

    exchange_ice(&peer1, &peer2);

    terminate_incoming_stream(&pipeline1, &peer1);
    terminate_incoming_stream(&pipeline2, &peer2);

    thread::sleep(Duration::from_millis(1000));

    info!("Negotiating SDP");
    negotiate_sdp(&peer1, &peer2);
    thread::sleep(Duration::from_millis(500));
    info!("SDP Negotiated");

    info!("Waiting for both peers to be connected");
    wait_for_connected(&peer1, &peer2, Duration::from_secs(5)).unwrap();

    info!("Peers connected");
    thread::sleep(Duration::from_millis(3000));

    produce_mulaw(&pipeline1, &peer1);
    negotiate_sdp(&peer1, &peer2);
    
    Ok(())
}

fn create_pipeline_with_datachannel() -> (gst::Pipeline, gst::Element) {
    let pipeline = gst::Pipeline::new(None);

    let webrtcbin = gst::ElementFactory::make("webrtcbin", None).unwrap();
    webrtcbin.set_property_from_str("bundle-policy", "max-bundle");
    pipeline.add(&webrtcbin).unwrap();

    pipeline
        .set_state(gst::State::Playing)
        .expect("Pipeline should be playing");

    webrtcbin
        .emit(
            "create-data-channel",
            &[&"data-channel", &None::<gst::Structure>],
        )
        .unwrap();

    (pipeline, webrtcbin)
}

fn create_pipeline_with_media() -> (gst::Pipeline, gst::Element) {
    let pipeline_source = "audiotestsrc is-live=true wave=sine ! audioconvert ! queue ! opusenc ! \
                           rtpopuspay ! webrtcbin name=wrtc bundle-policy=max-bundle";
    let pipeline = gst::parse_launch(pipeline_source)
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();

    let webrtcbin = pipeline.get_by_name("wrtc").expect("Webrtcbin null");

    pipeline
        .set_state(gst::State::Playing)
        .expect("Pipeline should be playing");

    (pipeline, webrtcbin)
}

fn exchange_ice(peer1: &gst::Element, peer2: &gst::Element) {
    let peer1_ref = peer1.downgrade();
    let peer2_ref = peer2.downgrade();

    peer1
        .connect("on-ice-candidate", false, move |values| {
            let peer2 = match peer2_ref.upgrade() {
                Some(p) => p,
                _ => return None,
            };

            let _webrtc = values[0]
                .get::<gst::Element>()
                .expect("Invalid argument")
                .expect("Should never be null.");
            let mlineindex = values[1].get_some::<u32>().expect("Invalid argument");
            let candidate_raw = values[2]
                .get::<String>()
                .expect("Invalid argument")
                .unwrap();

            trace!("Ice candidate gathered on peer1. Sending to peer2.");
            peer2
                .emit("add-ice-candidate", &[&mlineindex, &candidate_raw])
                .unwrap();
            None
        })
        .unwrap();

    peer2
        .connect("on-ice-candidate", false, move |values| {
            let peer1 = match peer1_ref.upgrade() {
                Some(p) => p,
                _ => return None,
            };

            let _webrtc = values[0]
                .get::<gst::Element>()
                .expect("Invalid argument")
                .expect("Should never be null.");
            let mlineindex = values[1].get_some::<u32>().expect("Invalid argument");
            let candidate_raw = values[2]
                .get::<String>()
                .expect("Invalid argument")
                .unwrap();

            trace!("Ice candidate gathered on peer2. Sending to peer1.");
            peer1
                .emit("add-ice-candidate", &[&mlineindex, &candidate_raw])
                .unwrap();
            None
        })
        .unwrap();
}

fn negotiate_sdp(peer1: &gst::Element, peer2: &gst::Element) {
    let peer1_ref = peer1.downgrade();
    let peer2_ref = peer2.downgrade();

    let (offer_tx, offer_rx) = unbounded();
    let (answer_tx, answer_rx) = unbounded();

    info!("Creating offer for peer1");
    let create_offer_promise = gst::Promise::with_change_func(move |reply| {
        let webrtcbin = match peer1_ref.upgrade() {
            Some(w) => w,
            _ => return,
        };

        if let Ok(Some(r)) = reply {
            let offer = r
                .get_value("offer")
                .unwrap()
                .get::<gst_webrtc::WebRTCSessionDescription>()
                .expect("Invalid argument")
                .unwrap();

            let raw_offer = offer.get_sdp().as_text().unwrap();
            webrtcbin
                .emit("set-local-description", &[&offer, &None::<gst::Promise>])
                .unwrap();
            info!("Set offer as peer1's local description");
            offer_tx.send(raw_offer).unwrap();
        };
    });

    peer1
        .emit(
            "create-offer",
            &[&None::<gst::Structure>, &create_offer_promise],
        )
        .unwrap();

    let created_offer = offer_rx.recv().unwrap();
    info!("Peer 1 offer: {}", created_offer);

    let remote_offer_sdp = gst_sdp::SDPMessage::parse_buffer(created_offer.as_bytes()).unwrap();
    let remote_offer = gst_webrtc::WebRTCSessionDescription::new(
        gst_webrtc::WebRTCSDPType::Offer,
        remote_offer_sdp,
    );

    peer2
        .emit(
            "set-remote-description",
            &[&remote_offer, &None::<gst::Promise>],
        )
        .unwrap();
    info!("Set peer2's remote description from offer");

    info!("Creating answer for peer2");
    let create_answer_promise = gst::Promise::with_change_func(move |reply| {
        let webrtcbin = match peer2_ref.upgrade() {
            Some(w) => w,
            None => return,
        };

        if let Ok(Some(r)) = reply {
            let answer = r
                .get_value("answer")
                .unwrap()
                .get::<gst_webrtc::WebRTCSessionDescription>()
                .expect("Invalid argument")
                .unwrap();

            let raw_answer = answer.get_sdp().as_text().unwrap();

            webrtcbin
                .emit("set-local-description", &[&answer, &None::<gst::Promise>])
                .unwrap();
            info!("Set peer2's local description from answer");

            answer_tx.send(raw_answer).unwrap();
        };
    });

    peer2
        .emit(
            "create-answer",
            &[&None::<gst::Structure>, &create_answer_promise],
        )
        .unwrap();

    let created_answer = answer_rx.recv().unwrap();
    info!("Peer 2 answer: {}", created_answer);

    let remote_answer_sdp = gst_sdp::SDPMessage::parse_buffer(created_answer.as_bytes()).unwrap();
    let remote_answer = gst_webrtc::WebRTCSessionDescription::new(
        gst_webrtc::WebRTCSDPType::Answer,
        remote_answer_sdp,
    );

    // without this random sleep, sometimes we get crashes from webrtcbin because
    // rtpfunnel cannot be setup correctly?
    thread::sleep(Duration::from_millis(500));
    info!("Informing peer1 about peer2's answer");
    peer1
        .emit(
            "set-remote-description",
            &[&remote_answer, &None::<gst::Promise>],
        )
        .unwrap();
}

fn log_states(id: &str, element: &gst::Element) {
    log_connection_state(id, element);
    log_ice_connection_state(id, element);
    log_signalling_state(id, element);
}

fn log_connection_state(id: &str, element: &gst::Element) {
    let peer_id = id.to_string();
    element.connect_notify(Some("connection-state"), move |source, _| {
        let v = get_connection_state(source);
        debug!("{} connection state: {:?}", peer_id, v)
    });
}

fn log_ice_connection_state(id: &str, element: &gst::Element) {
    let peer_id = id.to_string();
    element.connect_notify(Some("ice-connection-state"), move |source, _| {
        let v = get_ice_connection_state(source);
        debug!("{} ice-connection state: {:?}", peer_id, v)
    });
}

fn log_signalling_state(id: &str, element: &gst::Element) {
    let peer_id = id.to_string();
    element.connect_notify(Some("signaling-state"), move |source, _| {
        let v = get_signalling_state(source);
        debug!("{} signalling state: {:?}", peer_id, v)
    });
}

fn get_connection_state(element: &gst::Element) -> gst_webrtc::WebRTCPeerConnectionState {
    element
        .get_property("connection-state")
        .unwrap()
        .get::<gst_webrtc::WebRTCPeerConnectionState>()
        .unwrap()
        .unwrap()
}

fn get_ice_connection_state(element: &gst::Element) -> gst_webrtc::WebRTCICEConnectionState {
    element
        .get_property("ice-connection-state")
        .unwrap()
        .get::<gst_webrtc::WebRTCICEConnectionState>()
        .unwrap()
        .unwrap()
}

fn get_ice_gathering_state(element: &gst::Element) -> gst_webrtc::WebRTCICEGatheringState {
    element
        .get_property("ice-gathering-state")
        .unwrap()
        .get::<gst_webrtc::WebRTCICEGatheringState>()
        .unwrap()
        .unwrap()
}

fn get_signalling_state(element: &gst::Element) -> gst_webrtc::WebRTCSignalingState {
    element
        .get_property("signaling-state")
        .unwrap()
        .get::<gst_webrtc::WebRTCSignalingState>()
        .unwrap()
        .unwrap()
}

fn is_connected(element: &gst::Element) -> bool {
    get_connection_state(element) == gst_webrtc::WebRTCPeerConnectionState::Connected
}

fn terminate_incoming_stream(parent: &gst::Pipeline, peer: &gst::Element) {
    let parent_ref = parent.downgrade();

    peer.connect_pad_added(move |_from, pad| {
        if pad.get_direction() == gst::PadDirection::Src {
            let parent = match parent_ref.upgrade() {
                Some(p) => p,
                _ => return,
            };

            info!("Terminating new incoming stream from {}", _from.get_name());
            let queue = gst::ElementFactory::make("queue", None).unwrap();
            let sink = gst::ElementFactory::make("fakesink", None).unwrap();

            parent.add(&queue).unwrap();
            parent.add(&sink).unwrap();

            queue.link(&sink).unwrap();
            let sink_pad = queue.get_static_pad("sink").unwrap();
            pad.link(&sink_pad).unwrap();

            sink.sync_state_with_parent().unwrap();
            queue.sync_state_with_parent().unwrap();
        }
    });
}

fn wait_for_connected(
    peer1: &gst::Element,
    peer2: &gst::Element,
    duration: Duration,
) -> Result<()> {
    let after = std::time::Instant::now() + duration;

    loop {
        if std::time::Instant::now() > after {
            bail!("Too much time elapsed".to_string())
        }
        let peer1_connected = is_connected(peer1);
        let peer2_connected = is_connected(peer2);

        debug!(
            "Peer1 connected: {}, peer2 connected: {}",
            peer1_connected, peer2_connected
        );

        if peer1_connected && peer2_connected {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub fn get_caps_field_as_str(caps: &gst::Caps, field: &str) -> Result<String> {
    let value = caps
        .get_structure(0)
        .ok_or_else(|| anyhow!("No embedded structure"))?
        .get_value(field);

    match value {
        Ok(v) => v.get::<String>()?.ok_or_else(|| anyhow!("Field was null")),
        _ => Err(anyhow!("Could not ready field {} from caps", field)),
    }
}

fn produce_mulaw(pipeline: &gst::Pipeline, peer: &gst::Element) {
    let bin_source = "audiotestsrc is-live=true wave=white-noise ! audioconvert ! queue ! 
        mulawenc ! rtppcmupay";
    let bin = gst::parse_bin_from_description(bin_source, true).unwrap();
    pipeline.add(&bin).unwrap();

    let (tx, rx) = unbounded();

    let src_pad = bin.get_static_pad("src").unwrap();
    let sink_pad = peer.get_request_pad("sink_%u").unwrap();

    sink_pad.connect_property_caps_notify(move |_pad| {
        if _pad.has_current_caps() {
            let _ = tx.send(());
        }
    });

    src_pad
        .link_maybe_ghosting_full(&sink_pad, gst::PadLinkCheck::DEFAULT)
        .unwrap();
    bin.sync_state_with_parent().unwrap();

    rx.recv_timeout(Duration::from_millis(100)).unwrap();
}
