//! Large GeoJSON FeatureCollections read in place, a view at a time.
//!
//! Opening reads the file once, a feature at a time, keeping each feature's place in the file
//! and bounding box (28 bytes a feature) and working out the schema; a read picks the features
//! whose boxes meet the request and parses just those from the file again. Nothing of the file
//! is held between reads. Smaller files are loaded whole by `io::read_path`.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::{Batch, FeatureSource, Request, SourceInfo, clamp_limit, stride};
use crate::io::{FeatureSink, Schema, geojson_attrs, geojson_geometry};
use crate::{DataError, Feature, GeometryType, Result};
use rustgis_geom::Extent;
use serde_json::Value as Json;

fn bad(msg: impl std::fmt::Display) -> DataError {
    DataError::Format(format!("GeoJSON: {msg}"))
}

/// GeoJSON files larger than this are read in place (smaller ones load whole faster).
pub const IN_PLACE_ABOVE: u64 = 32 << 20;

/// The largest GeoJSON file read: it is read whole once, when opened.
pub const MAX_FILE: u64 = 4 << 30;

/// Where a feature's text is in the file.
#[derive(Clone, Copy)]
struct Place {
    offset: u64,
    len: u32,
}

/// A GeoJSON FeatureCollection read in place.
pub struct GeoJsonSource {
    info: SourceInfo,
    path: PathBuf,
    places: Vec<Place>,
    /// Each feature's box, rounded outward (NaN for a feature without geometry).
    boxes: Vec<[f32; 4]>,
}

/// The features of a file as it's scanned.
#[derive(Default)]
struct Scan {
    schema: Schema,
    places: Vec<Place>,
    boxes: Vec<[f32; 4]>,
    extent: Extent,
    /// Points, lines and polygons seen.
    kinds: [usize; 3],
    skipped: usize,
}

impl FeatureSink for Scan {
    fn restart(&mut self) {
        *self = Scan::default();
    }

    fn feature(&mut self, raw: &str, offset: usize) {
        let Ok(f) = serde_json::from_str::<Json>(raw) else { return };
        // Every feature's properties count in the schema, those with unreadable geometry too.
        if let Some(props) = f.get("properties").and_then(Json::as_object) {
            for (k, v) in props {
                self.schema.see(k, v);
            }
        }
        let Ok(geom) = geojson_geometry(&f) else {
            self.skipped += 1;
            return;
        };
        let (Ok(len), Ok(offset)) = (u32::try_from(raw.len()), u64::try_from(offset)) else {
            self.skipped += 1;
            return;
        };
        let b = match &geom {
            Some(g) => {
                let e = g.extent();
                self.extent = self.extent.union(&e);
                if let Some(slot) = self.kinds.get_mut(match g.kind() {
                    GeometryType::Point => 0,
                    GeometryType::Polyline => 1,
                    _ => 2,
                }) {
                    *slot += 1;
                }
                outward(&e)
            }
            None => [f32::NAN; 4],
        };
        self.places.push(Place { offset, len });
        self.boxes.push(b);
    }
}

/// An extent as `f32`s, rounded outward (NaN when it isn't finite, so no request meets it).
fn outward(e: &Extent) -> [f32; 4] {
    if e.is_empty() || ![e.min_x, e.min_y, e.max_x, e.max_y].iter().all(|v| v.is_finite()) {
        return [f32::NAN; 4];
    }
    let down = |v: f64| {
        let f = v as f32;
        if f64::from(f) > v { f.next_down() } else { f }
    };
    let up = |v: f64| {
        let f = v as f32;
        if f64::from(f) < v { f.next_up() } else { f }
    };
    [down(e.min_x), down(e.min_y), up(e.max_x), up(e.max_y)]
}

impl GeoJsonSource {
    /// Opens a GeoJSON FeatureCollection. Other GeoJSON documents (a single feature, a bare
    /// geometry) and TopoJSON aren't read in place: they fail here, to be read whole.
    pub fn open_path(path: &Path, name: &str) -> Result<GeoJsonSource> {
        let len = std::fs::metadata(path)?.len();
        if len > MAX_FILE {
            return Err(bad(format!("the file is too large ({len} bytes)")));
        }
        let text = std::fs::read_to_string(path)?;
        let mut scan = Scan::default();
        let rest = crate::io::stream_features(&text, &mut scan).map_err(bad)?;
        drop(text);
        if rest.get("type").and_then(Json::as_str) != Some("FeatureCollection") {
            return Err(DataError::Unsupported("only GeoJSON FeatureCollections are read in place".into()));
        }
        let mut notes = Vec::new();
        if scan.skipped > 0 {
            notes.push(format!("{} features had unreadable geometry", scan.skipped));
        }
        // The kind most features have, as a layer read whole takes.
        let geometry_type = match scan.kinds.iter().enumerate().max_by_key(|(_, n)| **n) {
            Some((_, 0)) | None => GeometryType::None,
            Some((0, _)) => GeometryType::Point,
            Some((1, _)) => GeometryType::Polyline,
            Some(_) => GeometryType::Polygon,
        };
        let info = SourceInfo {
            name: name.to_string(),
            format: "GeoJSON",
            geometry_type,
            fields: scan.schema.fields(),
            extent: scan.extent,
            count: Some(scan.places.len() as u64),
            crs: "EPSG:4326".into(),
            indexed: false,
            notes,
        };
        Ok(GeoJsonSource { info, path: path.to_path_buf(), places: scan.places, boxes: scan.boxes })
    }

    /// The features a request matches, in file order: every one without an extent; otherwise
    /// those whose boxes meet it (features without geometry never do).
    fn matches(&self, req: &Request) -> Vec<u32> {
        let Some(e) = req.extent else {
            return (0..self.places.len() as u32).collect();
        };
        if e.is_empty() {
            return Vec::new();
        }
        let (x0, y0, x1, y1) = (e.min_x as f32, e.min_y as f32, e.max_x as f32, e.max_y as f32);
        self.boxes.iter().enumerate().filter(|(_, b)| b[0] <= x1 && b[2] >= x0 && b[1] <= y1 && b[3] >= y0).map(|(i, _)| i as u32).collect()
    }
}

/// Bytes read ahead from the file: features near each other in a read share a fill.
const BUFFER: usize = 1 << 20;

impl FeatureSource for GeoJsonSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn read(&self, req: &Request) -> Result<Batch> {
        let limit = clamp_limit(req.limit);
        let matched = self.matches(req);
        let step = stride(matched.len() as u64, limit).max(1) as usize;
        let mut f = BufReader::with_capacity(BUFFER, File::open(&self.path)?);
        let mut pos = 0u64;
        let mut buf = Vec::new();
        let mut features = Vec::with_capacity(matched.len().div_ceil(step).min(limit));
        let mut skipped = 0;
        for &i in matched.iter().step_by(step).take(limit) {
            let Some(place) = self.places.get(i as usize) else { continue };
            // Ahead and near: through the buffer; otherwise a seek.
            let read = (|| -> std::io::Result<()> {
                match i64::try_from(place.offset.wrapping_sub(pos)) {
                    Ok(0) => {}
                    Ok(d) if place.offset > pos && d < BUFFER as i64 => f.seek_relative(d)?,
                    _ => {
                        f.seek(SeekFrom::Start(place.offset))?;
                    }
                }
                buf.resize(place.len as usize, 0);
                f.read_exact(&mut buf)
            })();
            if read.is_err() {
                // The file changed since it was opened: where it is now is unknown.
                pos = u64::MAX;
                skipped += 1;
                continue;
            }
            pos = place.offset + u64::from(place.len);
            let Ok(json) = serde_json::from_slice::<Json>(&buf) else {
                skipped += 1;
                continue;
            };
            let Ok(geometry) = geojson_geometry(&json) else {
                skipped += 1;
                continue;
            };
            features.push(Feature { id: u64::from(i) + 1, geometry, attrs: geojson_attrs(&json, &self.info.fields) });
        }
        Ok(Batch { features, matched: matched.len() as u64, complete: step == 1, skipped })
    }
}
