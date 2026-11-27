//! Exhaustive behavioral & edge-case test suite for `vortex-domain` (`P1-T01`..`P1-T05`),
//! verifying every FSM branch, RFC 9110 validator rule, sanitization defense, and math edge case.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::path::{Path, PathBuf};

use uuid::Uuid;
use vortex_domain::{
    ActiveSegment, BIN_COMPLETED, ByteRange, CategorizationPolicy, CategoryId, Checksum, Clock,
    DEFAULT_FALLBACK_FILENAME, Download, DownloadCommand, DownloadId, DualHorizonEwma,
    EtagValidator, EventId, FakeClock, FilenameSanitizer, HashAlgorithm, IfRangeDirective,
    LastModifiedValidator, LeaseDecision, MAX_FILENAME_BYTES, PostProcessStage, Priority, QueueId,
    RangeSet, Resource, ResumeValidators, SegmentMap, SegmentationPolicy, WorkerId, align_up_64kib,
    worth_splitting,
};
use vortex_error::{ApiError, ErrorCode, TraceId};

#[test]
fn value_objects_cover_all_constructors_formatting_and_validation_branches() {
    let raw_uuid = Uuid::now_v7();
    let did = DownloadId::from_uuid(raw_uuid);
    assert_eq!(did.as_uuid(), raw_uuid);
    assert_eq!(DownloadId::default().to_string().len(), 36);

    let eid = EventId::from_uuid(raw_uuid);
    assert_eq!(eid.to_string(), raw_uuid.to_string());
    assert_eq!(EventId::default().to_string().len(), 36);

    assert_eq!(QueueId::default().as_str(), "main");
    let q = QueueId::new("  night-queue  ").unwrap();
    assert_eq!(q.to_string(), "night-queue");
    assert!(QueueId::new("   ").is_err());
    assert!(QueueId::new("x".repeat(65)).is_err());

    assert_eq!(CategoryId::default().as_str(), "other");
    let c = CategoryId::new("  VIDEO ").unwrap();
    assert_eq!(c.to_string(), "video");
    assert!(CategoryId::new("").is_err());
    assert!(CategoryId::new("y".repeat(65)).is_err());

    let w0 = WorkerId::new(0);
    assert_eq!(w0.get(), 1);
    assert_eq!(w0.to_string(), "W1");

    let r1 = ByteRange::new(0, 10).unwrap();
    let r2 = ByteRange::new(10, 20).unwrap();
    let r3 = ByteRange::new(5, 15).unwrap();
    assert!(!r1.overlaps(r2));
    assert!(r1.touches_or_overlaps(r2));
    assert!(r1.overlaps(r3));
    assert_eq!(r1.to_string(), "[0, 10)");
    assert_eq!(
        r1.split_at(4),
        Some((
            ByteRange::new(0, 4).unwrap(),
            ByteRange::new(4, 10).unwrap()
        ))
    );
    assert_eq!(r1.split_at(0), None);
    assert_eq!(r1.split_at(10), None);
    assert_eq!(ByteRange::new(0, 1).unwrap().midpoint(), None);

    // Checksums: Blake3, Sha256, Sha1, Md5 + errors
    let b3 = Checksum::parse_prefixed(&format!("blake3:{}", "ab".repeat(32))).unwrap();
    assert_eq!(b3.algorithm, HashAlgorithm::Blake3);
    assert!(b3.to_string().starts_with("blake3:"));
    let s1 = Checksum::parse_prefixed(&format!("sha-1:{}", "cd".repeat(20))).unwrap();
    assert_eq!(s1.algorithm, HashAlgorithm::Sha1);
    let md5 = Checksum::parse_prefixed(&format!("md5:{}", "ef".repeat(16))).unwrap();
    assert_eq!(md5.algorithm, HashAlgorithm::Md5);
    assert!(!md5.matches_hex("short"));
    assert!(Checksum::parse_prefixed("no_colon").is_err());
    assert!(Checksum::parse_prefixed("crc32:12345678").is_err());

    // ETag & ResumeValidators branches
    assert!(EtagValidator::parse("   ").is_none());
    assert!(EtagValidator::parse("W/   ").is_none());
    let unquoted_weak = EtagValidator::parse("w/abc").unwrap();
    assert!(!unquoted_weak.is_strong());
    assert_eq!(unquoted_weak.as_str(), r#"W/"abc""#);
    let unquoted_strong = EtagValidator::parse("xyz").unwrap();
    assert!(unquoted_strong.is_strong());
    assert_eq!(unquoted_strong.as_str(), r#""xyz""#);

    let v_base = ResumeValidators {
        etag: Some(unquoted_strong.clone()),
        last_modified: Some(LastModifiedValidator {
            raw_http_date: "Mon, 01 Jan 2026 00:00:00 GMT".to_owned(),
            last_modified_epoch_secs: None,
            response_date_epoch_secs: None,
        }),
        content_length: Some(100),
    };
    assert_eq!(
        v_base.select_if_range(),
        IfRangeDirective::SendHeader(r#""xyz""#.to_owned())
    );
    // Size mismatch
    let v_diff_len = ResumeValidators {
        content_length: Some(200),
        ..v_base.clone()
    };
    assert_eq!(
        v_base.validate_on_resume(&v_diff_len).unwrap_err().code(),
        ErrorCode::ResResourceChanged
    );
    // Last-Modified mismatch
    let v_diff_lm = ResumeValidators {
        last_modified: Some(LastModifiedValidator {
            raw_http_date: "Tue, 02 Jan 2026 00:00:00 GMT".to_owned(),
            last_modified_epoch_secs: None,
            response_date_epoch_secs: None,
        }),
        ..v_base.clone()
    };
    assert_eq!(
        v_base.validate_on_resume(&v_diff_lm).unwrap_err().code(),
        ErrorCode::ResResourceChanged
    );

    assert_eq!(Priority::from_i32(-5), Priority::Low);
    assert_eq!(Priority::from_i32(2), Priority::Normal);
    assert_eq!(Priority::from_i32(99), Priority::Critical);

    let clock = FakeClock::new(100);
    assert_eq!(clock.now_unix_ms(), 100);
}

#[test]
fn download_fsm_covers_queueing_retries_splits_hash_failure_and_illegal_transitions() {
    let trace = TraceId::new();
    let expected_hash = Checksum::new(HashAlgorithm::Md5, &"aa".repeat(16)).unwrap();
    let (mut dl, added_evt) = Download::new_pending(
        DownloadId::new(),
        Resource::new("https://origin.example.com/archive.zip"),
        "https://origin.example.com/archive.zip".to_owned(),
        PathBuf::from("D:/Downloads/Compressed"),
        "archive.zip".to_owned(),
        CategoryId::builtin(CategoryId::COMPRESSED),
        QueueId::main_queue(),
        Priority::High,
        Some(expected_hash),
        500,
        trace,
    );
    assert_eq!(added_evt.download_id, dl.id);
    assert!(dl.final_path().ends_with("archive.zip"));
    assert!(dl.temp_path().ends_with("archive.zip.vortex"));
    assert!(dl.meta_sidecar_path().ends_with("archive.zip.vortex.meta"));
    assert!(!dl.state.is_active());

    // StartProbe + idempotent StartProbe
    dl.apply(DownloadCommand::StartProbe, 510, trace).unwrap();
    assert!(dl.state.is_active());
    assert_eq!(
        dl.apply(DownloadCommand::StartProbe, 511, trace)
            .unwrap()
            .len(),
        0
    );

    // CompleteProbe with queue_full = true -> Queued
    dl.apply(
        DownloadCommand::CompleteProbe {
            total_size: Some(4_000),
            supports_range: true,
            validators: ResumeValidators {
                etag: EtagValidator::parse(r#""tag1""#),
                last_modified: None,
                content_length: Some(4_000),
            },
            final_url: Some("https://cdn.example.com/archive.zip".to_owned()),
            resolved_file_name: Some("archive-v2.zip".to_owned()),
            queue_full: true,
            final_url_redacted: Some("https://cdn.example.com/archive.zip".to_owned()),
        },
        520,
        trace,
    )
    .unwrap();
    assert_eq!(dl.state.as_slug(), "queued");
    assert_eq!(dl.file_name, "archive-v2.zip");
    assert_eq!(
        dl.resource.effective_url(),
        "https://cdn.example.com/archive.zip"
    );

    // PromoteFromQueue -> Downloading
    dl.apply(
        DownloadCommand::PromoteFromQueue {
            supports_range: true,
        },
        530,
        trace,
    )
    .unwrap();
    assert_eq!(dl.state.as_slug(), "downloading");
    assert_eq!(
        dl.apply(DownloadCommand::Resume, 531, trace).unwrap().len(),
        0
    );

    // StartWorkerSegment + RecordSplit
    dl.apply(
        DownloadCommand::StartWorkerSegment {
            worker_id: WorkerId::new(1),
            assigned_range: ByteRange::new(0, 4_000).unwrap(),
            lease_end: 2_000,
        },
        535,
        trace,
    )
    .unwrap();
    dl.apply(
        DownloadCommand::RecordSplit {
            from_worker: WorkerId::new(1),
            new_worker: WorkerId::new(2),
            stolen_range: ByteRange::new(2_000, 4_000).unwrap(),
            lease_end: 4_000,
        },
        540,
        trace,
    )
    .unwrap();

    // ScheduleRetry -> BackoffElapsed
    dl.apply(
        DownloadCommand::ScheduleRetry {
            attempt: 1,
            retry_after_ms: 500,
            code: ErrorCode::NetConnectionLost,
        },
        550,
        trace,
    )
    .unwrap();
    assert_eq!(dl.state.as_slug(), "retrying");
    dl.apply(
        DownloadCommand::BackoffElapsed {
            supports_range: true,
        },
        1_050,
        trace,
    )
    .unwrap();
    assert_eq!(dl.state.as_slug(), "downloading");

    // Cannot BeginPostProcessing while incomplete
    dl.apply(
        DownloadCommand::RecordProgress {
            worker_id: WorkerId::new(1),
            chunk: ByteRange::new(0, 2_000).unwrap(),
            speed_fast_bps: 1000.0,
            speed_slow_bps: 1000.0,
            eta_secs: Some(2),
        },
        1_100,
        trace,
    )
    .unwrap();
    assert!(
        dl.apply(
            DownloadCommand::BeginPostProcessing {
                stage: PostProcessStage::FinalizeFile
            },
            1_105,
            trace
        )
        .is_err()
    );

    // MarkNeedsAttention -> RefreshLink -> Resume fails if server dropped Range support
    dl.apply(
        DownloadCommand::MarkNeedsAttention {
            reason_code: ErrorCode::ResLinkExpired,
            detail: "410 Gone".to_owned(),
        },
        1_110,
        trace,
    )
    .unwrap();
    assert_eq!(dl.state.as_slug(), "needs_attention");
    dl.apply(
        DownloadCommand::RefreshLink {
            new_url: "https://cdn.example.com/archive-new.zip".to_owned(),
            new_url_redacted: "https://cdn.example.com/archive-new.zip".to_owned(),
        },
        1_120,
        trace,
    )
    .unwrap();
    dl.apply(
        DownloadCommand::CompleteProbe {
            total_size: Some(4_000),
            supports_range: false, // server lost range support!
            validators: ResumeValidators::default(),
            final_url: None,
            resolved_file_name: None,
            queue_full: false,
            final_url_redacted: None,
        },
        1_130,
        trace,
    )
    .unwrap();
    assert_eq!(dl.state.as_slug(), "needs_attention");

    // Refresh again, finish remaining bytes, and fail checksum verification
    dl.apply(
        DownloadCommand::RefreshLink {
            new_url: "https://cdn.example.com/archive-good.zip".to_owned(),
            new_url_redacted: "https://cdn.example.com/archive-good.zip".to_owned(),
        },
        1_140,
        trace,
    )
    .unwrap();
    dl.apply(
        DownloadCommand::CompleteProbe {
            total_size: Some(4_000),
            supports_range: true,
            validators: ResumeValidators {
                etag: EtagValidator::parse(r#""tag1""#),
                last_modified: None,
                content_length: Some(4_000),
            },
            final_url: None,
            resolved_file_name: None,
            queue_full: false,
            final_url_redacted: None,
        },
        1_150,
        trace,
    )
    .unwrap();
    dl.apply(
        DownloadCommand::RecordProgress {
            worker_id: WorkerId::new(2),
            chunk: ByteRange::new(2_000, 4_000).unwrap(),
            speed_fast_bps: 2000.0,
            speed_slow_bps: 2000.0,
            eta_secs: Some(0),
        },
        1_160,
        trace,
    )
    .unwrap();
    dl.apply(
        DownloadCommand::BeginPostProcessing {
            stage: PostProcessStage::FinalizeFile,
        },
        1_170,
        trace,
    )
    .unwrap();
    dl.apply(
        DownloadCommand::AdvanceAfterPostProcessing {
            final_size_bytes: 4_000,
        },
        1_180,
        trace,
    )
    .unwrap();
    // Mismatched hash -> Failed(VTX-INTG-001)
    dl.apply(
        DownloadCommand::CompleteVerification {
            actual_hex: "bb".repeat(16),
            final_size_bytes: 4_000,
        },
        1_190,
        trace,
    )
    .unwrap();
    assert_eq!(dl.state.as_slug(), "failed");

    // Manual retry from Failed -> Probing -> Complete without checksum!
    let (mut dl2, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new("https://origin.example.com/tiny.bin"),
        "https://origin.example.com/tiny.bin".to_owned(),
        PathBuf::from("D:/Downloads"),
        "tiny.bin".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Low,
        None,
        2_000,
        trace,
    );
    assert!(
        dl2.apply(
            DownloadCommand::PromoteFromQueue {
                supports_range: false
            },
            2_001,
            trace
        )
        .is_err()
    );
    assert!(dl2.apply(DownloadCommand::Resume, 2_001, trace).is_err());
    dl2.apply(DownloadCommand::StartProbe, 2_010, trace)
        .unwrap();
    dl2.apply(
        DownloadCommand::CompleteProbe {
            total_size: Some(16),
            supports_range: false,
            validators: ResumeValidators::default(),
            final_url: None,
            resolved_file_name: None,
            queue_full: false,
            final_url_redacted: None,
        },
        2_020,
        trace,
    )
    .unwrap();
    dl2.apply(
        DownloadCommand::RecordProgress {
            worker_id: WorkerId::new(1),
            chunk: ByteRange::new(0, 16).unwrap(),
            speed_fast_bps: 160.0,
            speed_slow_bps: 160.0,
            eta_secs: Some(0),
        },
        2_030,
        trace,
    )
    .unwrap();
    dl2.apply(
        DownloadCommand::BeginPostProcessing {
            stage: PostProcessStage::SecurityScan,
        },
        2_040,
        trace,
    )
    .unwrap();
    dl2.apply(
        DownloadCommand::AdvanceAfterPostProcessing {
            final_size_bytes: 16,
        },
        2_050,
        trace,
    )
    .unwrap();
    assert_eq!(dl2.state.as_slug(), "completed");

    // Explicit Fail command
    let (mut dl3, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new("https://origin.example.com/404.bin"),
        "https://origin.example.com/404.bin".to_owned(),
        PathBuf::from("D:/Downloads"),
        "404.bin".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        3_000,
        trace,
    );
    dl3.apply(
        DownloadCommand::Fail {
            error: ApiError::new(ErrorCode::ResNotFound, trace),
        },
        3_010,
        trace,
    )
    .unwrap();
    assert_eq!(dl3.state.as_slug(), "failed");
}

#[test]
fn range_set_segmentation_and_sanitizer_edge_cases() {
    // Unknown-size & 0-byte RangeSet bins
    let empty_set = RangeSet::new();
    assert!(empty_set.is_complete(0));
    assert_eq!(empty_set.covered_within(0), 0);
    assert_eq!(empty_set.gaps(0).len(), 0);

    let active_unknown = [ActiveSegment::new(
        WorkerId::new(2),
        ByteRange::new(0, 1000).unwrap(),
        500,
    )];
    let bins_active = empty_set.render_512_bins(None, &active_unknown);
    assert_eq!(bins_active[0], WorkerId::new(2).as_bin_marker());

    let mut non_empty = RangeSet::new();
    non_empty.insert(ByteRange::new(0, 64).unwrap());
    let bins_done = non_empty.render_512_bins(None, &[]);
    assert_eq!(bins_done[0], BIN_COMPLETED);

    let mut sm = SegmentMap::new(Some(100));
    assert!(
        sm.record_written(WorkerId::new(1), ByteRange::new(90, 110).unwrap())
            .is_err()
    );
    assert_eq!(SegmentMap::new(None).unassigned_gaps().len(), 0);
    assert_eq!(SegmentMap::new(None).remaining_bytes(), None);

    // Segmentation mid-lease continuation & stalled worker worth_splitting
    assert_eq!(align_up_64kib(1), 65_536);
    assert!(worth_splitting(4 * 1024 * 1024, 0.0, 0.0, f64::NAN, false));
    let mut ewma = DualHorizonEwma::default();
    ewma.observe(1000, 0.0); // ignored when dt <= 1e-6
    assert_eq!(ewma.eta_secs(None), None);

    let mut map = SegmentMap::new(Some(4 * 1024 * 1024));
    let first =
        SegmentationPolicy::acquire_next_lease(&mut map, WorkerId::new(1), 512_000.0, 0.02, false);
    assert!(matches!(first, LeaseDecision::ClaimedUnassigned { .. }));
    // Partial progress inside current lease -> ContinuedExisting returns remaining part of current lease
    map.record_written(WorkerId::new(1), ByteRange::new(0, 256 * 1024).unwrap())
        .unwrap();
    let cont =
        SegmentationPolicy::acquire_next_lease(&mut map, WorkerId::new(1), 512_000.0, 0.02, true);
    assert!(matches!(cont, LeaseDecision::ContinuedExisting { .. }));

    // Sanitizer URL percent-decoding, truncation preserving compound extension, and MIME fallbacks
    let url = FilenameSanitizer::validate_download_url(
        "https://example.com/files/%D9%85%D9%84%D9%81%201.mp3",
    )
    .unwrap();
    assert_eq!(FilenameSanitizer::filename_from_url(&url), "ملف 1.mp3");
    let root_url = FilenameSanitizer::validate_download_url("https://example.com/").unwrap();
    assert_eq!(
        FilenameSanitizer::filename_from_url(&root_url),
        DEFAULT_FALLBACK_FILENAME
    );

    let very_long = format!("{}.tar.gz", "أ".repeat(160));
    let truncated = FilenameSanitizer::sanitize_untrusted(&very_long);
    assert!(truncated.len() <= MAX_FILENAME_BYTES);
    assert!(truncated.ends_with(".tar.gz"));

    assert_eq!(
        FilenameSanitizer::resolve_collision("README", |n| n == "README"),
        "README (1)"
    );
    assert!(FilenameSanitizer::safe_join(Path::new("D:/Downloads"), "..").is_err());

    // Categorization by extension & MIME fallback
    for (name, mime, expected) in [
        ("song.flac", None, CategoryId::AUDIO),
        ("setup.msi", None, CategoryId::PROGRAMS),
        ("photo.webp", None, CategoryId::IMAGES),
        ("blob", Some("audio/mpeg"), CategoryId::AUDIO),
        ("blob", Some("image/png"), CategoryId::IMAGES),
        ("blob", Some("application/zip"), CategoryId::COMPRESSED),
        ("blob", Some("application/pdf"), CategoryId::DOCUMENTS),
        ("blob", Some("application/x-msi"), CategoryId::PROGRAMS),
        ("blob", Some("application/octet-stream"), CategoryId::OTHER),
    ] {
        assert_eq!(
            CategorizationPolicy::classify(name, mime).id.as_str(),
            expected
        );
    }

    // URL validation errors + video/* MIME fallback
    assert!(FilenameSanitizer::validate_download_url("not a url").is_err());
    assert!(FilenameSanitizer::validate_download_url("http://").is_err());
    assert_eq!(
        CategorizationPolicy::classify("clip", Some("video/webm"))
            .id
            .as_str(),
        CategoryId::VIDEO
    );
}

#[test]
fn download_fsm_rejects_every_illegal_command_in_pending_state() {
    let trace = TraceId::new();
    let (mut dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new("https://origin.example.com/file.bin"),
        "https://origin.example.com/file.bin".to_owned(),
        PathBuf::from("D:/Downloads"),
        "file.bin".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        100,
        trace,
    );
    assert_eq!(dl.state.as_slug(), "pending");

    let illegal_cmds = vec![
        DownloadCommand::CompleteProbe {
            total_size: Some(10),
            supports_range: true,
            validators: ResumeValidators::default(),
            final_url: None,
            resolved_file_name: None,
            queue_full: false,
            final_url_redacted: None,
        },
        DownloadCommand::StartWorkerSegment {
            worker_id: WorkerId::new(1),
            assigned_range: ByteRange::new(0, 10).unwrap(),
            lease_end: 10,
        },
        DownloadCommand::RecordSplit {
            from_worker: WorkerId::new(1),
            new_worker: WorkerId::new(2),
            stolen_range: ByteRange::new(5, 10).unwrap(),
            lease_end: 10,
        },
        DownloadCommand::RecordProgress {
            worker_id: WorkerId::new(1),
            chunk: ByteRange::new(0, 5).unwrap(),
            speed_fast_bps: 1.0,
            speed_slow_bps: 1.0,
            eta_secs: None,
        },
        DownloadCommand::ScheduleRetry {
            attempt: 1,
            retry_after_ms: 100,
            code: ErrorCode::NetConnectionLost,
        },
        DownloadCommand::BackoffElapsed {
            supports_range: true,
        },
        DownloadCommand::MarkNeedsAttention {
            reason_code: ErrorCode::ResLinkExpired,
            detail: "expired".to_owned(),
        },
        DownloadCommand::RefreshLink {
            new_url: "https://origin.example.com/new.bin".to_owned(),
            new_url_redacted: "https://origin.example.com/new.bin".to_owned(),
        },
        DownloadCommand::BeginPostProcessing {
            stage: PostProcessStage::FinalizeFile,
        },
        DownloadCommand::AdvanceAfterPostProcessing {
            final_size_bytes: 10,
        },
        DownloadCommand::CompleteVerification {
            actual_hex: "00".repeat(32),
            final_size_bytes: 10,
        },
    ];

    for cmd in illegal_cmds {
        let err = dl.apply(cmd, 110, trace).unwrap_err();
        assert_eq!(err.code(), ErrorCode::BugInternalIsolated);
    }
}
