//! Research-only, opt-in file control; no persisted schema or public API.
//! Fixed source rectangles/coverage are retained. This measures sampling and
//! table cost, not physical footprint calibration or arbitrary polygon seams.

use super::{
    blend::{transfer_at, Coverage, SlicerSpec},
    warp::Warp,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

#[derive(Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Settings {
    #[serde(default)]
    outputs: BTreeMap<String, Pins>,
    #[serde(default = "default_workers")]
    workers: usize,
}
fn default_workers() -> usize {
    1
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Pins {
    corners: [[f64; 2]; 4],
    #[serde(default = "neutral")]
    center: [f64; 2],
    #[serde(default)]
    force_general: bool,
}
fn neutral() -> [f64; 2] {
    [0.5, 0.5]
}

pub struct Output {
    pub index: usize,
    pub warp: Option<Warp>,
    pub table: Vec<(u16, u8)>,
    pub build_ms: f64,
    geometry: Geometry,
}
pub struct Prepared {
    pub outputs: Vec<Output>,
    pub build_ms: f64,
    pub workers: usize,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Geometry {
    corners: [[u64; 2]; 4],
    center: [u64; 2],
    force_general: bool,
}

#[derive(Clone)]
struct Request {
    settings: Settings,
    geometry: Vec<Geometry>,
}

type PendingBuild = (Vec<u8>, mpsc::Receiver<Result<Prepared, String>>);

pub struct Controller {
    path: PathBuf,
    spec: SlicerSpec,
    sizes: Vec<(u32, u32)>,
    last_read: Instant,
    seen: Option<Vec<u8>>,
    pending: Option<PendingBuild>,
    installed: Vec<Option<Geometry>>,
    next_revision: u64,
}

pub const POLL_INTERVAL: Duration = Duration::from_millis(10);

impl Controller {
    pub fn from_env(spec: &SlicerSpec, sizes: Vec<(u32, u32)>) -> Option<Self> {
        let path = std::env::var_os("SUEDE_WARP_FILE")?;
        Some(Self {
            path: path.into(),
            spec: spec.clone(),
            sizes,
            last_read: Instant::now() - Duration::from_secs(1),
            seen: None,
            pending: None,
            installed: vec![None; spec.slices.len()],
            next_revision: 0,
        })
    }

    /// Acknowledges only data that the caller successfully uploaded and installed.
    pub fn installed(&mut self, prepared: &Prepared) {
        for output in &prepared.outputs {
            if let Some(slot) = self.installed.get_mut(output.index) {
                *slot = Some(output.geometry.clone());
            }
        }
    }

    /// Poll periodically even on static content. One build runs at a time;
    /// the newest file is read after each completed installation.
    pub fn poll(&mut self) -> Option<Result<Prepared, String>> {
        let mut completed = None;
        if let Some((_bytes, rx)) = &self.pending {
            match rx.try_recv() {
                Ok(result) => {
                    completed = Some(result);
                    self.pending = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    completed = Some(Err("warp worker terminated".into()));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if completed.is_some() {
            return completed;
        }
        if self.pending.is_some() || self.last_read.elapsed() < POLL_INTERVAL {
            return completed;
        }
        self.last_read = Instant::now();
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) if bytes.len() <= 1024 * 1024 => bytes,
            Ok(_) => return Some(Err("warp file exceeds 1 MiB".into())),
            Err(error) => return Some(Err(format!("warp file: {error}"))),
        };
        if self.seen.as_ref() == Some(&bytes) {
            return completed;
        }
        let request = match validate(&bytes, &self.spec, &self.sizes) {
            Ok(request) => request,
            Err(error) => {
                self.seen = Some(bytes);
                return Some(Err(error));
            }
        };
        let indices = self.changed_indices(&request.geometry);
        self.seen = Some(bytes.clone());
        if indices.is_empty() {
            return completed;
        }
        self.next_revision += 1;
        let revision = self.next_revision;
        let spec = self.spec.clone();
        let sizes = self.sizes.clone();
        let build_bytes = bytes.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = tx.send(build_selected(&build_bytes, &spec, &sizes, &indices).map(
                |mut prepared| {
                    prepared.revision = revision;
                    prepared
                },
            ));
        });
        self.pending = Some((bytes, rx));
        completed
    }

    fn changed_indices(&self, geometry: &[Geometry]) -> Vec<usize> {
        geometry
            .iter()
            .enumerate()
            .filter_map(|(index, desired)| {
                (self.installed[index].as_ref() != Some(desired)).then_some(index)
            })
            .collect()
    }
}

fn make_geometry(pins: Option<&Pins>, width: u32, height: u32) -> Geometry {
    let (corners, center, force_general) = match pins {
        Some(pins) => (pins.corners, pins.center, pins.force_general),
        None => (
            [
                [0.0, 0.0],
                [f64::from(width), 0.0],
                [f64::from(width), f64::from(height)],
                [0.0, f64::from(height)],
            ],
            [0.5, 0.5],
            false,
        ),
    };
    Geometry {
        corners: corners
            .map(|row| row.map(|value| (if value == 0.0 { 0.0 } else { value }).to_bits())),
        center: center.map(|value| (if value == 0.0 { 0.0 } else { value }).to_bits()),
        force_general,
    }
}

fn validate(bytes: &[u8], spec: &SlicerSpec, sizes: &[(u32, u32)]) -> Result<Request, String> {
    if sizes.len() != spec.slices.len() {
        return Err("output count mismatch".into());
    }
    let settings: Settings = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if !(1..=8).contains(&settings.workers) {
        return Err("workers must be in 1..=8".into());
    }
    if let Some(name) = settings
        .outputs
        .keys()
        .find(|name| !spec.slices.iter().any(|s| &s.output == *name))
    {
        return Err(format!("unknown output {name}"));
    }
    let mut geometry = Vec::with_capacity(spec.slices.len());
    for (slice, &(width, height)) in spec.slices.iter().zip(sizes) {
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 33_554_432 {
            return Err("spike supports nonzero outputs up to 32 megapixels".into());
        }
        if width != slice.source.width as u32 || height != slice.source.height as u32 {
            return Err(
                "spike requires unit scale and output size equal to source rectangle".into(),
            );
        }
        if let Some(pins) = settings.outputs.get(&slice.output) {
            // Validate every requested output before dispatching any worker.
            Warp::new(pins.corners, pins.center, width, height)?;
        }
        geometry.push(make_geometry(
            settings.outputs.get(&slice.output),
            width,
            height,
        ));
    }
    Ok(Request { settings, geometry })
}

fn build(bytes: &[u8], spec: &SlicerSpec, sizes: &[(u32, u32)]) -> Result<Prepared, String> {
    let indices: Vec<_> = (0..spec.slices.len()).collect();
    build_selected(bytes, spec, sizes, &indices)
}

fn build_selected(
    bytes: &[u8],
    spec: &SlicerSpec,
    sizes: &[(u32, u32)],
    indices: &[usize],
) -> Result<Prepared, String> {
    let started = Instant::now();
    let request = validate(bytes, spec, sizes)?;
    let coverage = Coverage::new(spec.slices.iter().map(|s| s.source));
    let mut outputs = Vec::new();
    for &index in indices {
        let slice = &spec.slices[index];
        let (width, height) = sizes[index];
        let start = Instant::now();
        let pins = request.settings.outputs.get(&slice.output);
        let warp = match pins {
            Some(pins) => {
                let w = Warp::new(pins.corners, pins.center, width, height)?;
                if !w.is_identity() || pins.force_general {
                    Some(w)
                } else {
                    None
                }
            }
            None => None,
        };
        let mut table = vec![(0, 0); width as usize * height as usize];
        let rows = (height as usize).div_ceil(request.settings.workers);
        std::thread::scope(|scope| {
            for (chunk, dest) in table.chunks_mut(rows * width as usize).enumerate() {
                let warp = &warp;
                let coverage = &coverage;
                scope.spawn(move || {
                    for (offset, value) in dest.iter_mut().enumerate() {
                        let x = (offset % width as usize) as u32;
                        let y = (chunk * rows + offset / width as usize) as u32;
                        let edge = warp.as_ref().map_or(1.0, |w| w.coverage(x, y));
                        if edge == 0.0 {
                            continue;
                        }
                        let Some([sx, sy]) = warp
                            .as_ref()
                            .map_or(Some([x as f64 + 0.5, y as f64 + 0.5]), |w| {
                                w.source_at(x as f64 + 0.5, y as f64 + 0.5)
                            })
                        else {
                            continue;
                        };
                        // Pixel centers at partially covered edges may map just
                        // outside the source. Extend only the edge texel into AA.
                        let sx = sx.clamp(0.5, width as f64 - 0.5);
                        let sy = sy.clamp(0.5, height as f64 - 0.5);
                        let cx = slice.source.x as f64 + sx;
                        let cy = slice.source.y as f64 + sy;
                        if cx < 0.0
                            || cy < 0.0
                            || cx >= spec.canvas_width as f64
                            || cy >= spec.canvas_height as f64
                        {
                            continue;
                        }
                        let lift = coverage.lift(spec.black_lift, cx, cy);
                        *value = transfer_at(&slice.ramps, spec.gamma, lift, sx, sy, edge);
                    }
                });
            }
        });
        outputs.push(Output {
            index,
            warp,
            table,
            build_ms: start.elapsed().as_secs_f64() * 1000.0,
            geometry: request.geometry[index].clone(),
        });
    }
    Ok(Prepared {
        outputs,
        build_ms: started.elapsed().as_secs_f64() * 1000.0,
        workers: request.settings.workers,
        revision: 0,
    })
}

/// Pure table timing on the reference machine, before any Wayland setup.
/// Opt in via SUEDE_WARP_BENCH; the supplied slice sizes are authoritative.
pub fn benchmark(spec: &SlicerSpec) -> Result<(), String> {
    let sizes: Vec<_> = spec
        .slices
        .iter()
        .map(|s| (s.source.width as u32, s.source.height as u32))
        .collect();
    for workers in [1, 4, 8] {
        for mode in ["off", "identity", "keystone"] {
            let mut outputs = serde_json::Map::new();
            if mode != "off" {
                for s in &spec.slices {
                    let w = f64::from(s.source.width);
                    let h = f64::from(s.source.height);
                    let corners = if mode == "identity" {
                        [[0.0, 0.0], [w, 0.0], [w, h], [0.0, h]]
                    } else {
                        [
                            [w * 0.025, h * 0.03],
                            [w * 0.98, h * 0.01],
                            [w * 0.99, h * 0.98],
                            [w * 0.01, h * 0.96],
                        ]
                    };
                    outputs.insert(
                        s.output.clone(),
                        serde_json::json!({"corners":corners,"forceGeneral":true}),
                    );
                }
            }
            let bytes =
                serde_json::to_vec(&serde_json::json!({"outputs":outputs,"workers":workers}))
                    .map_err(|e| e.to_string())?;
            for iteration in 0..11 {
                let result = build(&bytes, spec, &sizes)?;
                if iteration > 0 {
                    println!(
                        "{}",
                        serde_json::json!({"mode":mode,"workers":workers,"iteration":iteration,"buildMs":result.build_ms,"sizes":sizes,"outputMs":result.outputs.iter().map(|o|o.build_ms).collect::<Vec<_>>()})
                    );
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::Rect,
        projection::blend::{pixel_transfer, SliceSpec},
    };
    fn spec() -> SlicerSpec {
        SlicerSpec {
            control_session: String::new(),
            source: "canvas".into(),
            canvas_width: 8,
            canvas_height: 8,
            gamma: 2.2,
            black_lift: 0.1,
            pattern: None,
            free_run: false,
            renderer: Default::default(),
            slices: vec![SliceSpec {
                geometry: None,
                output: "A".into(),
                source: Rect {
                    x: 0,
                    y: 0,
                    width: 8,
                    height: 8,
                },
                ramps: vec![],
            }],
        }
    }
    #[test]
    fn identity_and_parallel_build_match_legacy() {
        let s = spec();
        for workers in [1, 4] {
            let bytes = format!("{{\"workers\":{workers}}}");
            let b = build(bytes.as_bytes(), &s, &[(8, 8)]).unwrap();
            for y in 0..8 {
                for x in 0..8 {
                    assert_eq!(
                        b.outputs[0].table[(y * 8 + x) as usize],
                        pixel_transfer(&[], s.gamma, 0.0, x, y)
                    );
                }
            }
        }
    }

    #[test]
    fn selected_build_is_sparse_and_identity_is_normalized() {
        let mut s = spec();
        let mut second = s.slices[0].clone();
        second.output = "B".into();
        s.slices.push(second);
        let omitted = validate(b"{}", &s, &[(8, 8), (8, 8)]).unwrap();
        let explicit = validate(
            br#"{"outputs":{"A":{"corners":[[0,0],[8,0],[8,8],[0,8]]}}}"#,
            &s,
            &[(8, 8), (8, 8)],
        )
        .unwrap();
        assert_eq!(omitted.geometry[0], explicit.geometry[0]);
        let sparse = build_selected(b"{}", &s, &[(8, 8), (8, 8)], &[1]).unwrap();
        assert_eq!(sparse.outputs.len(), 1);
        assert_eq!(sparse.outputs[0].index, 1);
    }
    #[test]
    fn invalid_file_does_not_build() {
        assert!(build(b"{}", &spec(), &[]).is_err());
        assert!(build(br#"{"workers":0}"#, &spec(), &[(8, 8)]).is_err());
        assert!(build(
            br#"{"outputs":{"unknown":{"corners":[[0,0],[8,0],[8,8],[0,8]]}}}"#,
            &spec(),
            &[(8, 8)]
        )
        .is_err());
    }

    #[test]
    fn partial_picture_border_attenuates_lift_as_well_as_gain() {
        let mut s = spec();
        s.canvas_width = 12;
        let mut neighbor = s.slices[0].clone();
        neighbor.output = "B".into();
        neighbor.source.x = 4;
        s.slices.push(neighbor);
        let result = build(
            br#"{"outputs":{"A":{"corners":[[0.5,0],[8,0],[8,8],[0.5,8]]}}}"#,
            &s,
            &[(8, 8), (8, 8)],
        )
        .unwrap();
        // At x=0, one of two possible footprints covers the source. Half
        // pixel border coverage must halve the 0.1 black lift too.
        assert_eq!(result.outputs[0].table[8], (115, 13));
        assert_eq!(result.outputs[0].table[9], (230, 26));
        let outside = build(
            br#"{"outputs":{"A":{"corners":[[1.5,0],[8,0],[8,8],[1.5,8]]}}}"#,
            &s,
            &[(8, 8), (8, 8)],
        )
        .unwrap();
        assert_eq!(outside.outputs[0].table[8], (0, 0));
    }

    fn controller(path: PathBuf) -> Controller {
        let mut s = spec();
        let mut second = s.slices[0].clone();
        second.output = "B".into();
        s.slices.push(second);
        let initial = build(b"{}", &s, &[(8, 8), (8, 8)]).unwrap();
        std::fs::write(&path, b"{}").unwrap();
        let mut controller = Controller {
            path,
            spec: s,
            sizes: vec![(8, 8), (8, 8)],
            last_read: Instant::now() - Duration::from_secs(1),
            seen: Some(b"{}".to_vec()),
            pending: None,
            installed: vec![None; 2],
            next_revision: 0,
        };
        controller.installed(&initial);
        controller
    }

    fn finish(controller: &mut Controller) -> Prepared {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            controller.last_read = Instant::now() - Duration::from_secs(1);
            if let Some(result) = controller.poll() {
                return result.unwrap();
            }
            assert!(Instant::now() < deadline, "worker did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    const MOVED_B: &[u8] = br#"{"outputs":{"B":{"corners":[[1,1],[7,0],[8,7],[0,8]]}}}"#;

    #[test]
    fn one_output_edits_reset_and_effective_noops_stay_local() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = controller(dir.path().join("warp.json"));
        let initial = c.installed.clone();
        std::fs::write(&c.path, MOVED_B).unwrap();
        let changed = finish(&mut c);
        assert_eq!(changed.outputs.len(), 1);
        assert_eq!(changed.outputs[0].index, 1);
        let full = build(MOVED_B, &c.spec, &c.sizes).unwrap();
        assert_eq!(changed.outputs[0].table, full.outputs[1].table);
        assert_eq!(
            c.installed, initial,
            "worker completion is not installation"
        );
        c.installed(&changed);
        assert_eq!(c.installed[0], initial[0]);
        assert_ne!(c.installed[1], initial[1]);

        std::fs::write(&c.path, b"{}").unwrap();
        let reset = finish(&mut c);
        assert_eq!(reset.outputs.len(), 1);
        assert_eq!(reset.outputs[0].index, 1);
        assert!(reset.outputs[0].warp.is_none());
        c.installed(&reset);
        assert_eq!(c.installed, initial);

        // Signed zero, omitted center, explicit identity and workers all
        // normalize to the same installed pixels: no worker, no upload.
        std::fs::write(
            &c.path,
            br#"{"workers":8,"outputs":{"B":{"corners":[[-0.0,0],[8,0],[8,8],[0,8]]}}}"#,
        )
        .unwrap();
        c.last_read = Instant::now() - Duration::from_secs(1);
        assert!(c.poll().is_none());
        assert!(c.pending.is_none());
    }

    #[test]
    fn completed_intermediate_installs_then_revert_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = controller(dir.path().join("warp.json"));
        let initial = c.installed.clone();
        let mut prepared = build_selected(MOVED_B, &c.spec, &c.sizes, &[1]).unwrap();
        prepared.revision = 1;
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(Ok(prepared)).ok().unwrap();
        c.pending = Some((MOVED_B.to_vec(), rx));
        c.seen = Some(MOVED_B.to_vec());
        c.next_revision = 1;
        // The file has already reverted to identity while B was building.
        let intermediate = c.poll().unwrap().unwrap();
        assert_eq!(intermediate.outputs.len(), 1);
        assert!(c.pending.is_none(), "no new diff before installation ack");
        c.installed(&intermediate);
        let final_state = finish(&mut c);
        assert!(final_state.revision > intermediate.revision);
        assert_eq!(final_state.outputs.len(), 1);
        assert_eq!(final_state.outputs[0].index, 1);
        c.installed(&final_state);
        assert_eq!(c.installed, initial);
    }

    #[test]
    fn invalid_batch_rejects_before_any_worker_and_keeps_installed_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = controller(dir.path().join("warp.json"));
        let initial = c.installed.clone();
        std::fs::write(&c.path, br#"{"outputs":{"A":{"corners":[[1,1],[7,0],[8,7],[0,8]]},"B":{"corners":[[0,0],[8,0],[8,8],[0,8]],"center":[0,0.5]}}}"#).unwrap();
        assert!(c.poll().unwrap().is_err());
        assert!(c.pending.is_none());
        assert_eq!(c.installed, initial);
    }
}
