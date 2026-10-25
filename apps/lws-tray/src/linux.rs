use crate::{menu::Action, State};
use ksni::TrayMethods;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::{mpsc, watch};

struct Tray {
    state: State,
    commands: mpsc::UnboundedSender<Action>,
    ready: lws_desktop::TrayReady,
    watcher_up: Arc<AtomicBool>,
}
impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "bifrost".into()
    }
    fn title(&self) -> String {
        self.state
            .status
            .as_ref()
            .filter(|status| !status.game_guard_reason.is_empty())
            .map(|status| {
                ui_text::game_guard_state(
                    super::language_of(self.state.look_file.as_deref()),
                    &status.game_guard_reason,
                )
            })
            .unwrap_or_else(|| "Bifrost".into())
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Bifrost".into(),
            description: self.title(),
            ..Default::default()
        }
    }
    fn activate(&mut self, _: i32, _: i32) {
        let _ = self.commands.send(Action::Open);
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let (width, height, rgba) = lws_desktop::assets::icon_rgba();
        let mut argb = Vec::with_capacity(rgba.len());
        for pixel in rgba.as_chunks::<4>().0 {
            argb.extend_from_slice(&[pixel[3], pixel[0], pixel[1], pixel[2]]);
        }
        vec![ksni::Icon {
            width: width as i32,
            height: height as i32,
            data: argb,
        }]
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        self.state
            .items()
            .into_iter()
            .map(|item| {
                let action = item.action;
                ksni::menu::StandardItem {
                    label: item.label,
                    enabled: action.is_some(),
                    activate: Box::new(move |tray: &mut Self| {
                        if let Some(action) = &action {
                            let _ = tray.commands.send(action.clone());
                        }
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect()
    }
    fn watcher_online(&self) {
        self.watcher_up.store(true, Ordering::Release);
        self.ready.set(true);
    }
    fn watcher_offline(&self, _: ksni::OfflineReason) -> bool {
        // No StatusNotifierWatcher (yet): keep running, the icon appears when one does.
        self.watcher_up.store(false, Ordering::Release);
        self.ready.set(false);
        true
    }
}

pub(super) fn run(
    mut state: watch::Receiver<State>,
    commands: mpsc::UnboundedSender<Action>,
    ready: lws_desktop::TrayReady,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async move {
        let watcher_up = Arc::new(AtomicBool::new(true));
        let tray = Tray {
            state: state.borrow().clone(),
            commands,
            ready: ready.clone(),
            watcher_up: watcher_up.clone(),
        };
        // A desktop whose tray host starts after us (KDE autostart before plasmashell, GNOME
        // without the extension yet) must not make the tray give up: the soft error goes to
        // `watcher_offline`, and `watcher_online` raises the flag when the host appears.
        let handle = tray
            .assume_sni_available(true)
            .spawn()
            .await
            .map_err(|e| e.to_string())?;
        ready.set(watcher_up.load(Ordering::Acquire));
        while state.changed().await.is_ok() {
            let latest = state.borrow_and_update().clone();
            if handle.update(|tray| tray.state = latest).await.is_none() {
                break;
            }
        }
        ready.set(false);
        handle.shutdown().await;
        Ok(())
    })
}
