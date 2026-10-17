//! Permission-independent logical output discovery through Wayland's xdg-output.
use std::collections::BTreeMap;
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

/// An output in compositor logical coordinates (the coordinate space used by EIS).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Display {
    pub id: String,
    pub name: String,
    pub logical: [i32; 4],
    pub scale: i32,
    /// Current mode before compositor transform, in pixels.
    pub native: Option<[i32; 2]>,
    pub rotated: bool,
}

/// Selects one coordinate source, never combines incompatible snapshots.
/// Mirrored outputs sharing a rectangle represent one logical screen.
pub fn discover(
    zones: Vec<Display>,
    fallback: impl FnOnce() -> Result<Vec<Display>, String>,
) -> Result<(&'static str, Vec<Display>), String> {
    let zones = merge_mirrors(zones);
    if !zones.is_empty() {
        let outputs = fallback().unwrap_or_default();
        return Ok(("input-capture-zones", enrich(zones, &outputs)));
    }
    let outputs = merge_mirrors(fallback()?);
    if outputs.is_empty() {
        return Err("no InputCapture zones and no valid xdg-output logical geometry".to_owned());
    }
    Ok(("wayland-xdg-output", outputs))
}

fn merge_mirrors(outputs: Vec<Display>) -> Vec<Display> {
    let mut result: Vec<Display> = Vec::with_capacity(outputs.len());
    for output in outputs {
        if output.logical[2] > 0
            && output.logical[3] > 0
            && !result
                .iter()
                .any(|existing| existing.logical == output.logical)
        {
            result.push(output);
        }
    }
    result
}

/// Adds physical-mode metadata without changing capture-zone ids or logical geometry.
pub fn enrich(mut zones: Vec<Display>, outputs: &[Display]) -> Vec<Display> {
    for zone in &mut zones {
        if let Some(output) = outputs.iter().find(|output| output.logical == zone.logical) {
            zone.native = output.native;
            zone.rotated = output.rotated;
            zone.name.clone_from(&output.name);
            zone.scale = output.scale;
        }
    }
    zones
}

impl Display {
    pub fn native_size(&self) -> Option<[i32; 2]> {
        self.native.map(|[width, height]| {
            if self.rotated {
                [height, width]
            } else {
                [width, height]
            }
        })
    }

    pub fn logical_scale(&self) -> f64 {
        self.native_size()
            .filter(|_| self.logical[2] > 0)
            .map_or(f64::from(self.scale.max(1)), |native| {
                f64::from(native[0]) / f64::from(self.logical[2])
            })
    }
}

#[derive(Default)]
struct Outputs {
    outputs: BTreeMap<u32, (wl_output::WlOutput, Display)>,
    manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    logical_outputs: Vec<zxdg_output_v1::ZxdgOutputV1>,
}

/// Enumerates without requesting capture, screencast, or remote-desktop permission.
pub fn enumerate() -> Result<Vec<Display>, String> {
    let connection = Connection::connect_to_env().map_err(|error| error.to_string())?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut state = Outputs::default();
    queue
        .roundtrip(&mut state)
        .map_err(|error| error.to_string())?;
    let manager = state
        .manager
        .as_ref()
        .ok_or("xdg-output logical geometry is unavailable")?;
    for (id, (output, _)) in &state.outputs {
        state
            .logical_outputs
            .push(manager.get_xdg_output(output, &qh, *id));
    }
    queue
        .roundtrip(&mut state)
        .map_err(|error| error.to_string())?;
    Ok(state
        .outputs
        .into_values()
        .map(|(_, output)| output)
        .filter(|output| output.logical[2] > 0 && output.logical[3] > 0)
        .collect())
}

impl Dispatch<wl_registry::WlRegistry, ()> for Outputs {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_output" => {
                    let output = registry.bind(name, version.min(4), qh, name);
                    state.outputs.insert(
                        name,
                        (
                            output,
                            Display {
                                id: format!("output-{name}"),
                                scale: 1,
                                ..Display::default()
                            },
                        ),
                    );
                }
                "zxdg_output_manager_v1" => {
                    state.manager = Some(registry.bind(name, version.min(3), qh, ()))
                }
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_output::WlOutput, u32> for Outputs {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some((_, output)) = state.outputs.get_mut(id) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => output.id = name,
            wl_output::Event::Description { description } => output.name = description,
            wl_output::Event::Scale { factor } => output.scale = factor.max(1),
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } if flags
                .into_result()
                .is_ok_and(|flags| flags.contains(wl_output::Mode::Current)) =>
            {
                output.native = (width > 0 && height > 0).then_some([width, height]);
            }
            wl_output::Event::Geometry {
                transform: wayland_client::WEnum::Value(transform),
                ..
            } => {
                output.rotated = matches!(
                    transform,
                    wl_output::Transform::_90
                        | wl_output::Transform::_270
                        | wl_output::Transform::Flipped90
                        | wl_output::Transform::Flipped270
                );
            }
            _ => {}
        }
    }
}
impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for Outputs {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some((_, output)) = state.outputs.get_mut(id) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                output.logical[0] = x;
                output.logical[1] = y;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.logical[2] = width;
                output.logical[3] = height;
            }
            zxdg_output_v1::Event::Name { name } => output.id = name,
            zxdg_output_v1::Event::Description { description } => output.name = description,
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(Outputs: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);

#[cfg(test)]
mod tests {
    use super::*;

    fn output(id: &str, logical: [i32; 4], scale: i32) -> Display {
        Display {
            id: id.into(),
            logical,
            scale,
            ..Display::default()
        }
    }

    #[test]
    fn capture_zones_keep_priority_when_output_metadata_is_unavailable() {
        let zones = vec![output("zone-0", [0, 0, 1920, 1080], 1)];
        let (source, displays) = discover(zones.clone(), || Err("no xdg-output".into())).unwrap();
        assert_eq!(source, "input-capture-zones");
        assert_eq!(displays, zones);
    }

    #[test]
    fn physical_metadata_enriches_zones_without_moving_or_renaming_them() {
        let zone = output("zone-0", [0, 0, 1745, 1091], 1);
        let mut screen = output("eDP-1", zone.logical, 2);
        screen.native = Some([2560, 1600]);
        let (source, displays) = discover(vec![zone.clone()], || Ok(vec![screen])).unwrap();
        assert_eq!(source, "input-capture-zones");
        assert_eq!(displays[0].id, zone.id);
        assert_eq!(displays[0].logical, zone.logical);
        assert_eq!(displays[0].native_size(), Some([2560, 1600]));
        assert!((displays[0].logical_scale() - 2560.0 / 1745.0).abs() < 1e-10);
        let mut rotated = displays[0].clone();
        rotated.rotated = true;
        assert_eq!(rotated.native_size(), Some([1600, 2560]));
        // A rotated output is measured against the logical rectangle the compositor handed
        // back, so a 2560×1600 panel turned by 90° still reports a scale of one.
        let mut turned = output("DP-1", [0, 0, 2560, 1600], 2);
        turned.native = Some([1600, 2560]);
        turned.rotated = true;
        assert_eq!(turned.native_size(), Some([2560, 1600]));
        assert!((turned.logical_scale() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn absent_or_invalid_zones_fall_back_and_merge_mirrors() {
        let screen = output("DP-1", [-1920, 0, 1920, 1080], 2);
        let mirror = output("HDMI-1", screen.logical, 2);
        let (source, displays) = discover(vec![Display::default()], || {
            Ok(vec![screen.clone(), mirror])
        })
        .unwrap();
        assert_eq!(source, "wayland-xdg-output");
        assert_eq!(displays, vec![screen]);
        // xdg-output is already logical: do not divide by wl_output.scale again.
        assert_eq!(displays[0].logical, [-1920, 0, 1920, 1080]);
        assert_eq!(displays[0].scale, 2);
    }

    #[test]
    fn an_empty_chain_reports_the_failure() {
        assert!(discover(Vec::new(), || Ok(Vec::new()))
            .unwrap_err()
            .contains("no valid"));
        assert_eq!(
            discover(Vec::new(), || Err("Wayland unavailable".into())).unwrap_err(),
            "Wayland unavailable"
        );
    }
}
