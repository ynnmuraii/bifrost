use crate::{menu::Action, State};
use ksni::TrayMethods;
use tokio::sync::{mpsc, watch};

struct Tray {
    state: State,
    commands: mpsc::UnboundedSender<Action>,
}
impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "multideskflow".into()
    }
    fn title(&self) -> String {
        "MultiDeskFlow".into()
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
}

pub(super) fn run(
    mut state: watch::Receiver<State>,
    commands: mpsc::UnboundedSender<Action>,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async move {
        let tray = Tray {
            state: state.borrow().clone(),
            commands,
        };
        let handle = tray.spawn().await.map_err(|e| e.to_string())?;
        while state.changed().await.is_ok() {
            let latest = state.borrow_and_update().clone();
            if handle.update(|tray| tray.state = latest).await.is_none() {
                break;
            }
        }
        handle.shutdown().await;
        Ok(())
    })
}
