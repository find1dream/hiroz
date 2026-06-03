//! Threshold selectivity: large camera images take the zero-copy SHM path,
//! small lidar clouds stay on the regular path.
//!
//! `ZPub::publish` records a `used_shm` boolean on its `publish` tracing span
//! (alongside the `topic`). We install a capturing `tracing` layer, publish one
//! camera `Image` (above threshold) and one lidar `PointCloud2` (below), and
//! assert the actual per-message decision — not just the estimate.
//!
//! This also exercises the estimate fix end to end: without it, the camera
//! image estimates at the `ZMessage` trait default (~200 B) and would wrongly
//! stay on the regular path, so `camera → used_shm=true` would fail.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use hiroz::context::ZContextBuilder;
use hiroz::{Builder, ZBuf};
use hiroz_msgs::sensor_msgs::{Image, PointCloud2, PointField};
use hiroz_msgs::std_msgs::Header;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;
use zenoh_buffers::buffer::Buffer; // ZBuf::len

/// Threshold sized between a lidar cloud and a camera frame.
const THRESHOLD: usize = 64 * 1024;

// ── tracing capture: span-id → topic, and topic → used_shm ──────────────────

#[derive(Clone, Default)]
struct ShmCapture {
    topics: Arc<Mutex<HashMap<u64, String>>>,
    used: Arc<Mutex<HashMap<String, bool>>>,
}

impl ShmCapture {
    fn used_shm_for(&self, topic_substr: &str) -> Option<bool> {
        self.used
            .lock()
            .unwrap()
            .iter()
            .find(|(t, _)| t.contains(topic_substr))
            .map(|(_, v)| *v)
    }
}

#[derive(Default)]
struct FieldGrab {
    topic: Option<String>,
    used_shm: Option<bool>,
}

impl Visit for FieldGrab {
    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "used_shm" {
            self.used_shm = Some(value);
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "topic" {
            self.topic = Some(value.to_string());
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `topic = %...` is recorded via Display→Debug; strip surrounding quotes.
        if field.name() == "topic" {
            self.topic = Some(format!("{value:?}").trim_matches('"').to_string());
        }
    }
}

impl<S> Layer<S> for ShmCapture
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
        if attrs.metadata().name() != "publish" {
            return;
        }
        let mut g = FieldGrab::default();
        attrs.record(&mut g);
        if let Some(topic) = g.topic {
            self.topics.lock().unwrap().insert(id.into_u64(), topic);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        let mut g = FieldGrab::default();
        values.record(&mut g);
        let Some(used) = g.used_shm else { return };
        if let Some(topic) = self.topics.lock().unwrap().get(&id.into_u64()).cloned() {
            self.used.lock().unwrap().insert(topic, used);
        }
    }
}

// ── message builders ────────────────────────────────────────────────────────

fn make_image(width: u32, height: u32) -> Image {
    let step = width * 3; // rgb8
    let data = vec![0x7Fu8; (step * height) as usize];
    Image {
        header: Header {
            frame_id: "camera".into(),
            ..Default::default()
        },
        height,
        width,
        encoding: "rgb8".into(),
        is_bigendian: 0,
        step,
        data: ZBuf::from(data),
        ..Default::default()
    }
}

fn make_cloud_xy(num_points: usize) -> PointCloud2 {
    let point_step = 8u32; // x, y as f32
    let data = vec![0xABu8; num_points * point_step as usize];
    let fields = ["x", "y"]
        .iter()
        .enumerate()
        .map(|(i, name)| PointField {
            name: (*name).into(),
            offset: (i * 4) as u32,
            datatype: 7, // FLOAT32
            count: 1,
        })
        .collect();
    PointCloud2 {
        header: Header {
            frame_id: "lidar".into(),
            ..Default::default()
        },
        height: 1,
        width: num_points as u32,
        fields,
        is_bigendian: false,
        point_step,
        row_step: num_points as u32 * point_step,
        data: ZBuf::from(data),
        is_dense: true,
    }
}

#[test]
fn camera_uses_shm_lidar_does_not() {
    let capture = ShmCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    // publish() is synchronous on this thread, so a thread-local default
    // subscriber sees its span + the used_shm record.
    let _guard = tracing::subscriber::set_default(subscriber);

    let ctx = ZContextBuilder::default()
        .with_shm_pool_size(8 * 1024 * 1024)
        .expect("enable SHM pool")
        .with_shm_threshold(THRESHOLD)
        .build()
        .expect("build context");
    let node = ctx.create_node("shm_selectivity").build().expect("node");

    let cam_pub = node
        .create_pub::<Image>("/cam/image")
        .build()
        .expect("camera pub");
    let lidar_pub = node
        .create_pub::<PointCloud2>("/lidar/points")
        .build()
        .expect("lidar pub");

    // 640x480 rgb8 = 921_600 B  ≫  64 KB threshold
    let img = make_image(640, 480);
    // 300 points * 8 B = 2_400 B  ≪  64 KB threshold
    let cloud = make_cloud_xy(300);

    // Self-document the straddle.
    assert!(
        img.data.len() > THRESHOLD,
        "camera payload {} must exceed threshold {THRESHOLD}",
        img.data.len()
    );
    assert!(
        cloud.data.len() < THRESHOLD,
        "lidar payload {} must be below threshold {THRESHOLD}",
        cloud.data.len()
    );

    cam_pub.publish(&img).expect("publish image");
    lidar_pub.publish(&cloud).expect("publish cloud");

    assert_eq!(
        capture.used_shm_for("/cam/image"),
        Some(true),
        "large camera image must use SHM zero-copy"
    );
    assert_eq!(
        capture.used_shm_for("/lidar/points"),
        Some(false),
        "small lidar cloud must stay on the regular (non-SHM) path"
    );
}
