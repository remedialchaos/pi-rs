#![forbid(unsafe_code)]

use pi_rs_durable::{MemoryStorage, Storage, SubmissionQuery, testing::storage_benchmark::*};

#[test]
fn seeded_workloads_cover_old_revisions_deep_forks_and_mixed_commits()
-> Result<(), Box<dyn std::error::Error>> {
    let read = MemoryStorage::new();
    let data = seed_storage_benchmark(&read, TIMING_SCALE)?;
    for case in STORAGE_READ_BENCHMARKS {
        assert_eq!(
            (case.run)(&read, &data)?,
            Some((case.expected)(&data)),
            "{}",
            case.name
        );
    }
    for case in STORAGE_WRITE_BENCHMARKS {
        let write = MemoryStorage::new();
        seed_storage_write_benchmark(&write)?;
        assert_eq!((case.run)(&write)?, case.expected, "{}", case.name);
        if case.expected == 4 {
            let page = write.scan_submissions(&SubmissionQuery::default(), 10, None)?;
            let submission = page
                .items
                .first()
                .ok_or("mixed commit omitted its submission")?;
            let id = submission
                .entry
                .ok_or("mixed submission omitted its transcript entry")?;
            assert!(write.entry(id)?.is_some());
        }
    }
    Ok(())
}
