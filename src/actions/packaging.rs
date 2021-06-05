use anyhow::{anyhow, Result};
use chrono::Duration;
use console::style;
use std::{
    fs,
    io::{BufRead, BufReader},
    path::Path,
    time::Instant,
};
use walkdir::WalkDir;

use crate::{common::create_spinner, config, error, info, repo, warn};

use super::{
    container::{get_output_directory, mount_fs, rollback_container, run_in_container},
    UPDATE_SCRIPT,
};

#[inline]
fn format_duration(duration: Duration) -> String {
    let seconds = duration.num_seconds();
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60
    )
}

fn read_package_list<P: AsRef<Path>>(filename: P, depth: usize) -> Result<Vec<String>> {
    if depth > 32 {
        return Err(anyhow!(
            "Nested group exceeded 32 levels! Potential infinite loop."
        ));
    }
    let f = fs::File::open(filename)?;
    let reader = BufReader::new(f);
    let mut results = Vec::new();
    for line in reader.lines() {
        let line = line?;
        // skip comment
        if line.starts_with('#') {
            continue;
        }
        // trim whitespace
        let trimmed = line.trim();
        // process nested groups
        if trimmed.starts_with("groups/") {
            let path = Path::new("./TREE").join(trimmed);
            let nested = read_package_list(&path, depth + 1)?;
            results.extend(nested);
            continue;
        }
        results.push(trimmed.to_owned());
    }

    Ok(results)
}

/// Expand the packages list to an array of packages
fn expand_package_list<'a, I: IntoIterator<Item = &'a str>>(packages: I) -> Vec<String> {
    let mut expanded = Vec::new();
    for package in packages {
        if !package.starts_with("groups/") {
            expanded.push(package.to_string());
            continue;
        }
        let list_file = Path::new("./TREE").join(&package);
        match read_package_list(list_file, 0) {
            Ok(list) => {
                info!("Read {} packages from {}", list.len(), package);
                expanded.extend(list);
            }
            Err(e) => {
                warn!("Unable to read package group `{}`: {}", package, e);
            }
        }
    }

    expanded
}

/// Fetch all the source packages in one go
pub fn package_fetch<'a, K: ExactSizeIterator<Item = &'a str>>(
    instance: &str,
    packages: K,
) -> Result<i32> {
    let conf = config::read_config();
    if conf.is_err() {
        return Err(anyhow!("Please configure this workspace first!"));
    }
    let conf = conf.unwrap();
    if !conf.local_sources {
        warn!("Using this function without local sources caching is probably meaningless.");
    }

    mount_fs(instance)?;
    rollback_container(instance)?;

    let mut cmd = vec!["/bin/acbs-build", "-g", "--"];
    cmd.extend(packages.into_iter());
    let status = run_in_container(instance, &cmd)?;

    Ok(status)
}

/// Build packages in the container
pub fn package_build<'a, K: Clone + ExactSizeIterator<Item = &'a str>>(
    instance: &str,
    packages: K,
    offline: bool,
) -> Result<i32> {
    let conf = config::read_config();
    if conf.is_err() {
        return Err(anyhow!("Please configure this workspace first!"));
    }
    let conf = conf.unwrap();

    if offline || std::env::var("CIEL_OFFLINE").is_ok() {
        info!("Preparing offline mode. Fetching source packages first ...");
        package_fetch(&instance, packages.clone())?;
        std::env::set_var("CIEL_OFFLINE", "ON");
        // FIXME: does not work with current version of systemd
        info!("Running in offline mode. Network access disabled.");
    }

    mount_fs(instance)?;
    rollback_container(instance)?;

    if !conf.local_repo {
        let mut cmd = vec!["/bin/acbs-build", "--"];
        cmd.extend(packages.into_iter());
        let status = run_in_container(instance, &cmd)?;
        return Ok(status);
    }

    let output_dir = get_output_directory(conf.sep_mount);
    let root = std::env::current_dir()?.join(output_dir);
    let packages = expand_package_list(packages);
    let total = packages.len();
    let start = Instant::now();
    for (index, package) in packages.into_iter().enumerate() {
        // set terminal title, \r is for hiding the message if the terminal does not support the sequence
        eprint!("\x1b]0;ciel: [{}/{}] {}\x07\r", index + 1, total, package);
        // hopefully the sequence gets flushed together with the `info!` below
        info!("[{}/{}] Building {}...", index + 1, total, package);
        mount_fs(&instance)?;
        info!("Refreshing local repository...");
        repo::init_repo(&root, Path::new(instance))?;
        let status = run_in_container(&instance, &["/bin/bash", "-ec", UPDATE_SCRIPT])?;
        if status != 0 {
            error!("Failed to update the OS before building packages");
            return Ok(status);
        }
        let status = run_in_container(instance, &["/bin/acbs-build", "--", &package])?;
        if status != 0 {
            error!("Build failed with status: {}", status);
            return Ok(status);
        }
        rollback_container(instance)?;
    }
    let duration = Duration::from_std(start.elapsed())?;
    eprintln!(
        "{} - {} packages in {}",
        style("BUILD SUCCESSFUL").bold().green(),
        total,
        format_duration(duration)
    );

    Ok(0)
}

/// Clean up output directories
pub fn cleanup_outputs() -> Result<()> {
    let spinner = create_spinner("Removing output directories ...", 200);
    for entry in WalkDir::new(".").max_depth(1) {
        let entry = entry?;
        if entry.file_type().is_dir() && entry.file_name().to_string_lossy().starts_with("OUTPUT-")
        {
            fs::remove_dir_all(entry.path())?;
        }
    }
    spinner.finish_with_message("Done.");

    Ok(())
}

#[test]
fn test_time_format() {
    let test_dur = Duration::seconds(3661);
    assert_eq!(format_duration(test_dur), "01:01:01");
}
