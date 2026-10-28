mod support;

use protocol::ipc::gui_frame::Msg;
use support::{event, status_with, two_node_doc, view, PEER_NODE};

#[test]
fn audit_duplicate_names_do_not_guess_route_identity() {
    let mut vm = view();
    vm.on_connected("test");
    let mut doc = two_node_doc(1);
    for member in &mut doc.members {
        member.display_name = "WORKSTATION".into();
    }
    vm.apply(event(Msg::TopologyDoc(doc)));
    let mut status = status_with("REMOTE");
    status.active_peer = "WORKSTATION".into();
    status.active_peer_node_id.clear();
    vm.apply(event(Msg::Status(status.clone())));
    assert!(vm.map_model().nodes.iter().all(|node| !node.target));
    status.active_peer_node_id = PEER_NODE.into();
    vm.apply(event(Msg::Status(status.clone())));
    let target = vm
        .map_model()
        .nodes
        .into_iter()
        .find(|node| node.target)
        .unwrap();
    assert_eq!(target.node_id, PEER_NODE);
    status.active_peer_node_id = "cc".repeat(32);
    vm.apply(event(Msg::Status(status)));
    assert!(vm.map_model().nodes.iter().all(|node| !node.target));
}

#[test]
fn audit_active_peer_node_id_uses_an_appended_status_tag() {
    let status = protocol::ipc::Status {
        active_peer_node_id: PEER_NODE.into(),
        ..Default::default()
    };
    let bytes = protocol::encode_message(protocol::Channel::Ipc, &status).unwrap();
    assert_eq!(&bytes[4..6], &[0xf2, 0x02]); // field 46, length-delimited
    let decoded = protocol::decode_message::<protocol::ipc::Status>(
        protocol::Channel::Ipc,
        &bytes,
    )
    .unwrap()
    .unwrap();
    assert_eq!(decoded, status);
}
