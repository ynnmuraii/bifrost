//! `Status.local_node_id`: the window's own node id in a 3+ node workspace.
//!
//! The status screen has to name this computer in the monitor map, and `owner_node_id` is
//! the owner's id — on a follower in a three-node mesh it names somebody else. The tests
//! here drive the view model's fold with a `Status` the core now sends (and with one from a
//! core that predates the field).

mod support;

use protocol::ipc;
use protocol::ipc::gui_frame::Msg;
use support::{event, status_with, two_node_doc, view, SELF_NODE};

/// L2: `Status.local_node_id` names this computer on a follower in a three-node mesh.
///
/// `owner_node_id` names the *owner*, so without the new field the window could only answer
/// `self_node()` while this computer held input or when the mesh had exactly two members.
#[test]
fn a_third_node_mesh_names_this_computer_from_the_local_id() {
    const OWNER: &str = "cc33cc33cc33cc33cc33cc33cc33cc33cc33cc33cc33cc33cc33cc33cc33cc33";
    let mut vm = view();
    let mut doc = two_node_doc(7);
    let mut third = doc.members[1].clone();
    third.node_id = OWNER.to_owned();
    third.display_name = "htpc".to_owned();
    doc.members.push(third);
    vm.apply(event(Msg::TopologyDoc(doc)));

    let mut status = status_with("LOCAL");
    status.is_owner = false;
    status.owner_node_id = OWNER.to_owned();
    status.local_node_id = SELF_NODE.to_owned();
    status.members.push(ipc::MemberStatus {
        app_version: String::new(),
        remote_update: false,
        pairing_maybe_removed: false,
        node_id: OWNER.to_owned(),
        display_name: "htpc".to_owned(),
        online: true,
        is_owner: true,
        link_state: "READY".to_owned(),
    });
    vm.apply(event(Msg::Status(status)));

    assert_eq!(vm.self_node(), Some(SELF_NODE));
    assert_eq!(vm.short_id_of_self().as_deref(), Some("aa11aa11"));

    // A core from before the field keeps the old rule: three members, no local id, this
    // computer not the owner — the answer stays open instead of guessing.
    let mut old_core = status_with("LOCAL");
    old_core.is_owner = false;
    old_core.owner_node_id = OWNER.to_owned();
    old_core.local_node_id.clear();
    vm.apply(event(Msg::Status(old_core)));
    assert_eq!(vm.self_node(), None);
}
