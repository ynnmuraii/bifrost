use protocol::ipc::{MemberStatus, Status, TopologyDoc};
use ui_text::{fill, text, Key, Lang};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Open,
    Switch(String),
    Stop,
    Exit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Item {
    pub label: String,
    pub action: Option<Action>,
}

fn name(member: &MemberStatus, lang: Lang) -> String {
    if member.display_name.is_empty() {
        fill(
            lang,
            Key::ComputerNamed,
            &[&member.node_id.chars().take(8).collect::<String>()],
        )
    } else {
        member.display_name.clone()
    }
}

// Mirrors the GUI's transfer decision: remote returns via its active peer; local
// prefers an online topology neighbour, preserving Status member order on ties.
fn transfer(
    status: &Status,
    topology: Option<&TopologyDoc>,
    lang: Lang,
) -> Option<(String, String)> {
    if !status.transition_disabled_reason.is_empty() {
        return None;
    }
    let this = topology.and_then(|doc| {
        if status.is_owner
            && doc
                .members
                .iter()
                .any(|m| m.node_id == status.owner_node_id)
        {
            Some(status.owner_node_id.as_str())
        } else if doc.members.len() == 2 {
            let owner = doc.members.iter().find(|m| {
                status
                    .members
                    .iter()
                    .any(|s| s.node_id == m.node_id && s.is_owner)
            })?;
            doc.members
                .iter()
                .find(|m| m.node_id != owner.node_id)
                .map(|m| m.node_id.as_str())
        } else {
            None
        }
    });
    let peers: Vec<_> = status
        .members
        .iter()
        .filter(|m| {
            this.map_or(
                !(status.is_owner
                    && !status.owner_node_id.is_empty()
                    && m.node_id == status.owner_node_id),
                |id| m.node_id != id,
            )
        })
        .collect();
    if status.focus == "REMOTE" {
        let peer = peers
            .iter()
            .find(|m| name(m, lang) == status.active_peer)
            .or_else(|| peers.first())?;
        let local = this
            .and_then(|id| topology?.members.iter().find(|m| m.node_id == id))
            .map(|m| m.display_name.as_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(text(lang, Key::ThisComputer));
        return Some((
            fill(lang, Key::SendInputTo, &[&local]),
            peer.node_id.clone(),
        ));
    }
    if status.focus != "LOCAL" {
        return None;
    }
    let neighbours = topology
        .zip(this)
        .map(|(doc, this)| crate::geometry::neighbours(doc, this))
        .unwrap_or_default();
    let peer = peers
        .iter()
        .filter(|m| m.online)
        .min_by_key(|m| !neighbours.contains(&m.node_id))?;
    Some((
        fill(lang, Key::SendInputTo, &[&name(peer, lang)]),
        peer.node_id.clone(),
    ))
}

pub(crate) fn items(
    status: Option<&Status>,
    topology: Option<&TopologyDoc>,
    lang: Lang,
) -> Vec<Item> {
    let mut items = vec![Item {
        label: text(lang, Key::OpenBifrost).into(),
        action: Some(Action::Open),
    }];
    let label = match status {
        Some(s) if s.focus == "REMOTE" => fill(
            lang,
            Key::Input2,
            &[&if s.active_peer.is_empty() {
                text(lang, Key::TheOtherComputer)
            } else {
                &s.active_peer
            }],
        ),
        Some(_) => text(lang, Key::TheServiceIsRunning).into(),
        None => text(lang, Key::TheServiceIsStopped).into(),
    };
    items.push(Item {
        label,
        action: None,
    });
    if let Some((label, peer)) = status.and_then(|s| transfer(s, topology, lang)) {
        items.push(Item {
            label,
            action: Some(Action::Switch(peer)),
        });
    }
    items.push(Item {
        label: text(lang, Key::StopTheService).into(),
        action: status.map(|_| Action::Stop),
    });
    items.push(Item {
        label: text(lang, Key::QuitTheServiceKeepsRunning).into(),
        action: Some(Action::Exit),
    });
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    fn status() -> Status {
        Status {
            focus: "LOCAL".into(),
            is_owner: true,
            owner_node_id: "self".into(),
            members: vec![
                MemberStatus {
                    node_id: "self".into(),
                    is_owner: true,
                    online: true,
                    ..Default::default()
                },
                MemberStatus {
                    node_id: "peer".into(),
                    display_name: "Ноутбук".into(),
                    online: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }
    #[test]
    fn offline_has_no_service_actions() {
        let menu = items(None, None, Lang::Ru);
        assert!(!menu
            .iter()
            .any(|i| matches!(i.action, Some(Action::Switch(_) | Action::Stop))));
        assert_eq!(menu.last().unwrap().action, Some(Action::Exit));
    }
    #[test]
    fn local_and_remote_use_same_peer() {
        let mut s = status();
        assert_eq!(
            transfer(&s, None, Lang::Ru),
            Some(("Передать ввод → Ноутбук".into(), "peer".into()))
        );
        s.focus = "REMOTE".into();
        s.active_peer = "Ноутбук".into();
        assert_eq!(
            transfer(&s, None, Lang::Ru),
            Some(("Передать ввод → этот компьютер".into(), "peer".into()))
        );
        s.transition_disabled_reason = "Нет разрешения".into();
        assert_eq!(transfer(&s, None, Lang::Ru), None);
    }
    #[test]
    fn impossible_transitions_are_hidden() {
        let mut s = status();
        for focus in ["PREPARING", "BLOCKED"] {
            s.focus = focus.into();
            assert_eq!(transfer(&s, None, Lang::Ru), None);
        }
        s.focus = "LOCAL".into();
        s.members[1].online = false;
        assert_eq!(transfer(&s, None, Lang::Ru), None);
    }
}

#[cfg(test)]
mod topology_tests {
    use super::*;
    use protocol::ipc::{EndpointView, LinkView, MemberTopology};

    #[test]
    fn neighbour_precedes_first_online_member() {
        let s = Status {
            focus: "LOCAL".into(),
            is_owner: true,
            owner_node_id: "self".into(),
            members: ["self", "distant", "neighbour"]
                .into_iter()
                .map(|id| MemberStatus {
                    node_id: id.into(),
                    display_name: id.into(),
                    online: true,
                    is_owner: id == "self",
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let doc = TopologyDoc {
            members: ["self", "distant", "neighbour"]
                .into_iter()
                .map(|id| MemberTopology {
                    node_id: id.into(),
                    ..Default::default()
                })
                .collect(),
            links: vec![LinkView {
                from: Some(EndpointView {
                    node_id: "self".into(),
                    edge: "RIGHT".into(),
                    ..Default::default()
                }),
                to: Some(EndpointView {
                    node_id: "neighbour".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            transfer(&s, Some(&doc), Lang::Ru).map(|(_, id)| id),
            Some("neighbour".into())
        );
    }

    #[test]
    fn remote_return_does_not_require_online_member() {
        let s = Status {
            focus: "REMOTE".into(),
            active_peer: "peer".into(),
            members: vec![MemberStatus {
                node_id: "peer-id".into(),
                display_name: "peer".into(),
                online: false,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            transfer(&s, None, Lang::Ru).map(|(_, id)| id),
            Some("peer-id".into())
        );
    }
}

#[cfg(test)]
mod labels {
    use super::*;

    /// The same menu, in English: the language reaches every label of the tray's model.
    #[test]
    fn the_english_menu_is_the_same_menu_in_english() {
        let offline = items(None, None, Lang::En);
        assert_eq!(offline[0].label, "Open Bifrost");
        assert_eq!(offline[1].label, "The service is stopped");
        assert_eq!(
            offline.last().unwrap().label,
            "Quit (the service keeps running)"
        );
        let remote = Status {
            focus: "REMOTE".into(),
            active_peer: "LAPTOP".into(),
            ..Default::default()
        };
        assert_eq!(
            items(Some(&remote), None, Lang::En)[1].label,
            "Input → LAPTOP"
        );
        assert!(
            !offline[1]
                .label
                .chars()
                .any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)),
            "no Russian is left in the English menu"
        );
    }

    #[test]
    fn owner_menu_labels_are_stable() {
        let offline = items(None, None, Lang::Ru);
        assert_eq!(offline[0].label, "Открыть Bifrost");
        assert_eq!(offline[1].label, "Служба остановлена");
        assert_eq!(
            offline.last().unwrap().label,
            "Выход (служба продолжит работать)"
        );
        assert_eq!(
            offline.iter().filter(|item| item.action.is_none()).count(),
            2
        );
        let remote = Status {
            focus: "REMOTE".into(),
            active_peer: "Ноутбук".into(),
            ..Default::default()
        };
        assert_eq!(
            items(Some(&remote), None, Lang::Ru)[1].label,
            "Ввод → Ноутбук"
        );
    }
}
