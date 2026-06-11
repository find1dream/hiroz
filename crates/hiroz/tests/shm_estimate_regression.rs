//! Regression tests for the SHM size-estimate dispatch bug.
//!
//! `ZPub::publish` is generic over `T: ZMessage` and sizes the SHM buffer via
//! `msg.estimated_serialized_size()`. hiroz-codegen emits an accurate estimate
//! for messages with dynamic fields (`PointCloud2`, `Image`, …), but only as an
//! *inherent* method plus `impl SizeEstimation` — never overriding the
//! `ZMessage` trait method. With `ZMessage` supplied by a blanket
//! `impl<T> ZMessage for T`, the generic publish context resolved the trait
//! DEFAULT (`size_of::<Self>() * 2 + 4` ≈ 244 B for `PointCloud2`), so large
//! sensor messages estimated below the SHM threshold and silently skipped
//! zero-copy; lowering the threshold to admit them overflowed the undersized
//! SHM buffer instead.
//!
//! The fix overrides `estimated_serialized_size` in the blanket impl to use the
//! accurate `CdrSerializedSize` walk. These tests pin that behavior.

use std::time::Duration;

use hiroz::context::ZContextBuilder;
use hiroz::msg::ZMessage;
use hiroz::{Builder, ZBuf};
use hiroz_msgs::sensor_msgs::{PointCloud2, PointField};
use hiroz_msgs::std_msgs::Header;
use zenoh_buffers::buffer::Buffer; // brings `ZBuf::len` into scope

/// Mirror the generic publish path exactly: a function bounded by `T: ZMessage`
/// can only see the trait method, never the concrete type's inherent one.
fn publish_path_estimate<T: ZMessage>(msg: &T) -> usize {
    msg.estimated_serialized_size()
}

fn xyz_fields() -> Vec<PointField> {
    ["x", "y", "z"]
        .iter()
        .enumerate()
        .map(|(i, name)| PointField {
            name: (*name).into(),
            offset: (i * 4) as u32,
            datatype: 7, // FLOAT32
            count: 1,
        })
        .collect()
}

fn make_cloud(num_points: usize) -> PointCloud2 {
    let point_step = 12u32; // x, y, z as f32
    let data = vec![0xABu8; num_points * point_step as usize];
    PointCloud2 {
        header: Header {
            frame_id: "lidar".into(),
            ..Default::default()
        },
        height: 1,
        width: num_points as u32,
        fields: xyz_fields(),
        is_bigendian: false,
        point_step,
        row_step: num_points as u32 * point_step,
        data: ZBuf::from(data),
        is_dense: true,
    }
}

/// Core regression: the generic estimate must cover the real payload.
///
/// Before the fix this returned ~244 B regardless of payload; after it, it is a
/// safe upper bound on the full serialized size.
#[test]
fn pointcloud_publish_estimate_includes_payload() {
    let num_points = 5_000; // 5_000 * 12 = 60_000 payload bytes
    let payload = num_points * 12;
    let cloud = make_cloud(num_points);

    let est = publish_path_estimate(&cloud);

    assert!(
        est >= payload,
        "generic publish-path estimate {est} B must cover the {payload} B payload \
         (regression: the ZMessage trait default returned ~244 B and SHM was skipped)"
    );
    // Sanity: a conservative upper bound, not wildly larger than the payload.
    assert!(
        est <= payload + 4096,
        "estimate {est} B unexpectedly large for a {payload} B payload"
    );
}

/// The estimate must scale with the payload (not be a constant like the
/// trait default), and must always remain >= the metadata-only floor.
#[test]
fn pointcloud_estimate_scales_with_payload() {
    let small = publish_path_estimate(&make_cloud(10));
    let large = publish_path_estimate(&make_cloud(10_000));
    assert!(
        large > small + 100_000,
        "estimate must grow with payload: small={small} large={large}"
    );
}

/// End-to-end: a 60 KB cloud with a 1 KB threshold must take the SHM path,
/// not panic on a too-small buffer, and round-trip intact.
#[test]
fn pointcloud_roundtrips_via_shm() {
    let ctx = ZContextBuilder::default()
        .with_shm_pool_size(8 * 1024 * 1024)
        .expect("enable SHM pool")
        .with_shm_threshold(1_000)
        .build()
        .expect("build context");

    let node = ctx.create_node("pc_shm_test").build().expect("node");
    let publisher = node
        .create_pub::<PointCloud2>("pc_shm_estimate_topic")
        .build()
        .expect("publisher");
    let subscriber = node
        .create_sub::<PointCloud2>("pc_shm_estimate_topic")
        .build()
        .expect("subscriber");

    std::thread::sleep(Duration::from_millis(500)); // discovery

    let cloud = make_cloud(5_000);
    let expect_len = cloud.data.len();
    let expect_width = cloud.width;

    // Before the fix, lowering the threshold to admit a large cloud overflowed
    // the SHM buffer here; now it allocates a correct upper bound.
    publisher.publish(&cloud).expect("publish must not overflow SHM");

    let received = subscriber
        .recv_timeout(Duration::from_secs(3))
        .expect("receive cloud");
    assert_eq!(received.data.len(), expect_len, "payload length mismatch");
    assert_eq!(received.width, expect_width, "width mismatch");
}
