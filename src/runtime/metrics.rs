use anyhow::Result;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};
use sysinfo::System;

use crate::core::StepResources;

const MAX_WALK_DEPTH: usize = 8;
const MAX_WALK_ENTRIES: usize = 20_000;
const MAX_WALK_TIME: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy)]
pub struct ResourceSnapshot {
    pub cpu_percent: f32,
    pub memory_mb: u64,
    pub disk_mb: u64,
}

pub fn sample(work_dir: &Path) -> Result<ResourceSnapshot> {
    let mut system = System::new_all();
    system.refresh_all();

    let cpu = system.global_cpu_info().cpu_usage();
    let memory_mb = system.used_memory() / 1024 / 1024;
    let disk_mb = dir_size_mb(work_dir)?;

    Ok(ResourceSnapshot {
        cpu_percent: cpu,
        memory_mb,
        disk_mb,
    })
}

pub fn to_step_resources(before: ResourceSnapshot, after: ResourceSnapshot) -> StepResources {
    StepResources {
        cpu_percent: ((before.cpu_percent + after.cpu_percent) / 2.0 * 10.0).round() / 10.0,
        memory_mb: after.memory_mb.saturating_sub(before.memory_mb),
        disk_mb: after.disk_mb.saturating_sub(before.disk_mb),
    }
}

fn dir_size_mb(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let bytes = dir_size_bytes_limited(
        path,
        MAX_WALK_DEPTH,
        MAX_WALK_ENTRIES,
        Instant::now(),
        MAX_WALK_TIME,
    )?;
    Ok(bytes / 1024 / 1024)
}

fn dir_size_bytes_limited(
    path: &Path,
    max_depth: usize,
    max_entries: usize,
    started: Instant,
    max_time: Duration,
) -> Result<u64> {
    let mut total = 0u64;
    let mut visited = 0usize;
    let mut stack = vec![(path.to_path_buf(), 0usize)];

    while let Some((current, depth)) = stack.pop() {
        if visited >= max_entries || started.elapsed() >= max_time {
            break;
        }

        let entries = match fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries {
            if visited >= max_entries || started.elapsed() >= max_time {
                break;
            }

            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            visited += 1;

            let meta = match fs::symlink_metadata(entry.path()) {
                Ok(meta) => meta,
                Err(_) => continue,
            };

            if meta.is_symlink() {
                continue;
            }

            if meta.is_file() {
                total = total.saturating_add(meta.len());
                continue;
            }

            if meta.is_dir() && depth < max_depth {
                stack.push((entry.path(), depth + 1));
            }
        }
    }

    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::dir_size_mb;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn dir_size_mb_returns_zero_for_missing_paths() {
        let path = temp_dir("metrics-missing").join("missing");
        assert_eq!(dir_size_mb(&path).expect("size"), 0);
    }

    #[test]
    fn dir_size_mb_counts_nested_files() {
        let root = temp_dir("metrics-nested");
        let nested = root.join("a").join("b");
        fs::create_dir_all(&nested).expect("nested dirs");
        fs::write(root.join("root.bin"), vec![0u8; 600_000]).expect("root file");
        fs::write(nested.join("nested.bin"), vec![0u8; 700_000]).expect("nested file");

        let size_mb = dir_size_mb(&root).expect("size");
        assert!(size_mb >= 1);

        let _ = fs::remove_dir_all(root);
    }

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("sendbuilds-{prefix}-{unique}"))
    }
}
