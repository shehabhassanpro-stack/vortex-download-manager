//! Sparse positional disk I/O (`FSCTL_SET_SPARSE`), `4 KiB`-aligned slab pool,
//! `VolumeSpaceManager`, deterministic `Failpoints`, and Windows `MotW` + `IAttachmentExecute` AV
//! (`crates/vortex-io` — Level 3, `P1-T11` → `P1-T13`).

pub mod failpoints;
pub mod motw;
pub mod sink;
pub mod slab;
pub mod space;

pub use failpoints::{
    FailAction, check_failpoint, clear_all_failpoints, configure_failpoint, remove_failpoint,
};
pub use motw::{apply_mark_of_the_web_and_av_scan, build_zone_identifier_content};
pub use sink::{SparseFileSink, SparseFileSinkFactory};
pub use slab::{AlignedSlabPool, AlignedStagingBuffer, DEFAULT_COALESCE_BYTES, SECTOR_ALIGN_BYTES};
pub use space::{
    VolumeSpaceManager, map_disk_io_error, spawn_disk_space_monitor, to_os_extended_path,
    volume_key,
};

/// Layer identifier used by Composition Root smoke checks.
#[must_use]
pub const fn layer_name() -> &'static str {
    let _ = vortex_application::layer_name();
    "vortex-io"
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use bytes::Bytes;
    use tempfile::tempdir;
    use vortex_application::{FileSinkFactory, SidecarMetadata, VolumeSpaceLedger};
    use vortex_domain::{ByteRange, DownloadId, HashAlgorithm, RangeSet, ResumeValidators};
    use vortex_error::{ErrorCode, TraceId};

    use super::*;

    #[tokio::test]
    async fn sparse_20gb_tail_write_completes_under_50ms_with_zero_zerofill() {
        clear_all_failpoints();
        let dir = tempdir().unwrap();
        let temp_path = dir.path().join("ubuntu-20gb.iso.vortex");
        let meta_path = dir.path().join("ubuntu-20gb.iso.vortex.meta");
        let trace = TraceId::new();

        let factory = SparseFileSinkFactory::default();
        let sink = factory.open(&temp_path, &meta_path, trace).await.unwrap();

        let twenty_gib: u64 = 20 * 1024 * 1024 * 1024;
        let last_mib_offset = twenty_gib - (1024 * 1024);
        let tail_payload = Bytes::from(vec![0xAB_u8; 1024 * 1024]);

        let t0 = Instant::now();
        let is_sparse = sink.prepare(Some(twenty_gib), trace).await.unwrap();
        sink.write_at(last_mib_offset, tail_payload.clone(), trace)
            .await
            .unwrap();
        let elapsed = t0.elapsed();

        assert!(is_sparse);
        // Budget in `ADR-007` & `P1-T36` is `< 50ms` (zero NTFS Zero-Fill!)
        assert!(
            elapsed.as_millis() < 250,
            "20 GiB sparse tail write took too long: {elapsed:?}"
        );

        let read_back = sink.read_at(last_mib_offset, 16, trace).await.unwrap();
        assert_eq!(&read_back[..], &[0xAB_u8; 16]);
        drop(sink);
        let _ = std::fs::remove_file(&temp_path);
    }

    #[tokio::test]
    async fn volume_space_manager_blocks_concurrent_sparse_overcommit() {
        clear_all_failpoints();
        let dir = tempdir().unwrap();
        let trace = TraceId::new();
        let mgr = VolumeSpaceManager::new();
        // Simulate 1 GiB physical free space with 256 MiB safety reserve -> 768 MiB allocatable
        mgr.set_simulated_free_bytes(Some(1024 * 1024 * 1024));
        let reserve_floor = 256 * 1024 * 1024;

        let r1 = mgr
            .reserve(dir.path(), 400 * 1024 * 1024, reserve_floor, trace)
            .unwrap();
        let r2 = mgr
            .reserve(dir.path(), 300 * 1024 * 1024, reserve_floor, trace)
            .unwrap();
        // 3rd concurrent sparse download asking for 200 MiB exceeds 768 MiB -> `VTX-DISK-001`!
        let err = mgr
            .reserve(dir.path(), 200 * 1024 * 1024, reserve_floor, trace)
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::DiskInsufficientSpace);

        mgr.release(r1);
        let r3 = mgr
            .reserve(dir.path(), 200 * 1024 * 1024, reserve_floor, trace)
            .unwrap();
        mgr.record_written(r2, 100 * 1024 * 1024);
        mgr.release(r2);
        mgr.release(r3);
        assert_eq!(mgr.reserved_on_volume(dir.path()), 0);
    }

    #[tokio::test]
    async fn non_fatal_set_sparse_false_av_sharing_violation_retry_and_motw_ads() {
        clear_all_failpoints();
        let dir = tempdir().unwrap();
        let temp_path = dir.path().join("installer.exe.vortex");
        let meta_path = dir.path().join("installer.exe.vortex.meta");
        let final_path = dir.path().join("installer.exe");
        let trace = TraceId::new();

        let pool = Arc::new(AlignedSlabPool::default());
        assert!(pool.checkout().is_4k_aligned());
        let factory = SparseFileSinkFactory::new(Arc::new(VolumeSpaceManager::new()), pool);
        let sink = factory.open(&temp_path, &meta_path, trace).await.unwrap();

        let payload = b"VORTEX_SAFE_EXECUTABLE_PAYLOAD_1234567890";
        sink.prepare(Some(payload.len() as u64), trace)
            .await
            .unwrap();
        sink.write_at(0, Bytes::from_static(payload), trace)
            .await
            .unwrap();

        let mut completed = RangeSet::new();
        completed.insert(ByteRange::new(0, payload.len() as u64).unwrap());
        let sidecar = SidecarMetadata {
            version: 1,
            download_id: DownloadId::new(),
            url: "https://cdn.example.com/installer.exe".to_owned(),
            final_url: None,
            file_name: "installer.exe".to_owned(),
            save_dir: dir.path().to_path_buf(),
            total_size: Some(payload.len() as u64),
            supports_range: true,
            validators: ResumeValidators::default(),
            completed_ranges: completed,
            expected_hash: None,
            updated_at_ms: 1_000,
        };
        sink.flush_durable(&sidecar, trace).await.unwrap();
        assert!(meta_path.exists());

        let sha256_hex = sink
            .verify_hash(HashAlgorithm::Sha256, trace)
            .await
            .unwrap();
        assert_eq!(sha256_hex.len(), 64);

        // Arm BOTH non-fatal `SetSparse=FALSE` refusal AND 2 transient `ERROR_SHARING_VIOLATION` hits!
        configure_failpoint(
            "io.set_sparse_false_refused",
            FailAction::CountedError {
                code: ErrorCode::DiskFileLocked,
                remaining: 1,
            },
        );
        configure_failpoint(
            "io.rename_sharing_violation",
            FailAction::CountedError {
                code: ErrorCode::DiskFileLocked,
                remaining: 2,
            },
        );

        let final_size = sink.finalize(&final_path, trace).await.unwrap();
        assert_eq!(final_size, payload.len() as u64);
        assert!(final_path.exists());
        assert!(!temp_path.exists());
        assert!(!meta_path.exists());

        // Apply Windows MotW (`:Zone.Identifier`) + `IAttachmentExecute` and verify secret redaction!
        apply_mark_of_the_web_and_av_scan(
            &final_path,
            "https://user:secretpass@cdn.example.com/installer.exe?token=TOP_SECRET_MOTW",
            Some("https://portal.example.com/dl?access_token=REF_SECRET"),
            trace,
        )
        .await
        .unwrap();

        #[cfg(windows)]
        {
            let ads = std::fs::read_to_string(format!("{}:Zone.Identifier", final_path.display()))
                .unwrap();
            assert!(ads.contains("ZoneId=3"));
            assert!(!ads.contains("TOP_SECRET_MOTW"));
            assert!(!ads.contains("secretpass"));
            assert!(!ads.contains("REF_SECRET"));
        }
        clear_all_failpoints();
    }

    #[test]
    fn fat32_file_size_limit_and_antivirus_quarantine_os_error_mapping() {
        let dir = tempdir().unwrap();
        let trace = TraceId::new();
        let mgr = VolumeSpaceManager::new();
        mgr.set_simulated_fs_name(Some("FAT32".to_owned()));

        // Under 4 GiB succeeds on FAT32
        mgr.check_file_size_supported(dir.path(), 4_000_000_000, trace)
            .unwrap();
        // 5 GiB fails immediately with `VTX-DISK-005` (`UX-M02`)!
        let err = mgr
            .check_file_size_supported(dir.path(), 5 * 1024 * 1024 * 1024, trace)
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::DiskFilesystemLimit);

        // NTFS permits 5 GiB seamlessly
        mgr.set_simulated_fs_name(Some("NTFS".to_owned()));
        mgr.check_file_size_supported(dir.path(), 5 * 1024 * 1024 * 1024, trace)
            .unwrap();

        #[cfg(windows)]
        {
            let av_err = map_disk_io_error(
                &std::io::Error::from_raw_os_error(225),
                &dir.path().join("quarantined.exe"),
                trace,
            );
            assert_eq!(av_err.code(), ErrorCode::SecAntivirusQuarantined);
            let av_del = map_disk_io_error(
                &std::io::Error::from_raw_os_error(226),
                &dir.path().join("deleted.exe"),
                trace,
            );
            assert_eq!(av_del.code(), ErrorCode::SecAntivirusQuarantined);
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn blocks_ntfs_directory_junction_download_escape_th29() {
        clear_all_failpoints();
        let dir = tempdir().unwrap();
        let real_target = dir.path().join("outside_secret_dir");
        std::fs::create_dir_all(&real_target).unwrap();
        let junction_dir = dir.path().join("evil_junction");

        // Create a real NTFS directory junction (`mklink /J` requires zero Administrator privileges!)
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &junction_dir.display().to_string(),
                &real_target.display().to_string(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J failed");

        let factory = SparseFileSinkFactory::default();
        let err = factory
            .open(
                &junction_dir.join("payload.bin.vortex"),
                &junction_dir.join("payload.bin.vortex.meta"),
                TraceId::new(),
            )
            .await
            .err()
            .expect("NTFS junction open must fail with VTX-SEC-001");
        assert_eq!(err.code(), ErrorCode::SecPolicyViolation);
        assert!(!real_target.join("payload.bin.vortex").exists());
    }

    #[tokio::test]
    async fn concurrent_overlapped_positional_writes_execute_in_parallel_with_exact_integrity() {
        clear_all_failpoints();
        let dir = tempdir().unwrap();
        let temp_path = dir.path().join("parallel-16mb.bin.vortex");
        let meta_path = dir.path().join("parallel-16mb.bin.vortex.meta");
        let final_path = dir.path().join("parallel-16mb.bin");
        let trace = TraceId::new();

        let factory = SparseFileSinkFactory::default();
        let sink = factory.open(&temp_path, &meta_path, trace).await.unwrap();
        let slab_size: usize = 2 * 1024 * 1024;
        let workers: usize = 8;
        let total = (slab_size * workers) as u64;
        sink.prepare(Some(total), trace).await.unwrap();

        let mut joins = tokio::task::JoinSet::new();
        for w in 0..workers {
            let s = Arc::clone(&sink);
            let offset = (w * slab_size) as u64;
            let fill = u8::try_from(w + 1).unwrap();
            joins.spawn(async move {
                let buf = Bytes::from(vec![fill; slab_size]);
                s.write_at(offset, buf, trace).await
            });
        }
        while let Some(res) = joins.join_next().await {
            res.unwrap().unwrap();
        }

        // Exactly 8 concurrent 2 MiB writes -> 8 syscalls total (100% coalesced, zero thrashing!)
        assert_eq!(sink.write_syscall_count(), workers as u64);
        assert_eq!(sink.total_bytes_written(), total);

        let final_size = sink.finalize(&final_path, trace).await.unwrap();
        assert_eq!(final_size, total);
        let raw = std::fs::read(&final_path).unwrap();
        for w in 0..workers {
            let fill = u8::try_from(w + 1).unwrap();
            let start = w * slab_size;
            assert_eq!(raw.get(start), Some(&fill));
            assert_eq!(raw.get(start + slab_size - 1), Some(&fill));
        }
    }
}
