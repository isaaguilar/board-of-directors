use crate::files;
use chrono::Local;
use regex::Regex;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

/// Check if a string is a collision suffix: `~N` (new format) or `-N` (legacy format).
fn is_collision_suffix(rest: &str) -> bool {
    rest.strip_prefix('~')
        .or_else(|| rest.strip_prefix('-'))
        .map_or(false, |digits| {
            !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
        })
}

/// Sanitize a branch name for use in filenames.
/// Returns `None` if the branch name contains no alphanumeric characters.
pub fn sanitize_branch_name(branch: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    static RE_MULTI: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"[^a-zA-Z0-9_\-]").unwrap());
    let sanitized = re.replace_all(branch, "-").to_string();
    let re_multi = RE_MULTI.get_or_init(|| Regex::new(r"-{2,}").unwrap());
    let result = re_multi.replace_all(&sanitized, "-").to_string();
    let result = result.trim_matches('-').to_string();
    if result.is_empty() || !result.chars().any(|c| c.is_ascii_alphanumeric()) {
        None
    } else {
        Some(result)
    }
}

/// Return a unique round identifier: `YYYYmmddHHMMSSn{8 hex chars}`.
///
/// The 14-digit timestamp provides calendar ordering. The `n{hex}` nonce
/// is sourced from OS entropy via `getrandom`, making round IDs practically
/// unique across concurrent invocations even when timestamps collide.
pub fn timestamp_now() -> String {
    let now = Local::now();
    let ts = now.format("%Y%m%d%H%M%S").to_string();
    // Embed milliseconds in the nonce high bits so that same-second round IDs
    // sort in temporal order. The low 16 bits are random for uniqueness.
    let millis = (now.timestamp_subsec_millis() % 1000) as u32;
    let mut rand_buf = [0u8; 4];
    if let Err(_) = getrandom::fill(&mut rand_buf) {
        eprintln!(
            "Warning: OS entropy unavailable; falling back to weak nonce. \
             Round ID collisions are more likely."
        );
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let pid = std::process::id();
        rand_buf = (nanos ^ pid.wrapping_shl(16)).to_le_bytes();
    }
    let random = u32::from_le_bytes(rand_buf) as u64;
    let nonce: u64 = ((millis as u64) << 32) | random;
    format!("{}n{:012x}", ts, nonce)
}

/// RAII guard that deletes a reserved (0-byte) file on drop unless disarmed.
/// Used to clean up atomically reserved files when the operation that would
/// populate them fails partway through.
#[must_use = "dropping a ReservedFile immediately deletes the reserved file"]
pub struct ReservedFile {
    path: Option<PathBuf>,
}

impl ReservedFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Disarm the guard so the file is NOT deleted on drop.
    /// Call this after the file has been successfully populated.
    pub fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for ReservedFile {
    fn drop(&mut self) {
        if let Some(ref path) = self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Given a base stem (without `.md`), atomically create a file in `bod_dir`
/// that does not collide with existing files. Uses `O_CREAT | O_EXCL`
/// (via `OpenOptions::create_new`) to eliminate the TOCTOU race between
/// checking existence and writing. Returns `(filename, File)`.
///
/// If `{base}.md` is free, creates it. Otherwise tries `{base}~2.md`,
/// `{base}~3.md`, etc. The `~` delimiter cannot appear in sanitized branch
/// names (only `[a-zA-Z0-9_\-]`), so there is no ambiguity.
fn create_atomic(bod_dir: &Path, base: &str) -> std::io::Result<(String, std::fs::File)> {
    let candidate = format!("{}.md", base);
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(bod_dir.join(&candidate))
    {
        Ok(file) => return Ok((candidate, file)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }

    // Scan existing collision suffixes to start from the highest.
    let prefix = format!("{}~", base);
    let mut max_n: u32 = 1;

    if let Ok(entries) = std::fs::read_dir(bod_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix(&prefix)
                && let Some(num_str) = rest.strip_suffix(".md")
                && let Ok(n) = num_str.parse::<u32>()
            {
                max_n = max_n.max(n);
            }
        }
    }

    // Retry with incrementing suffix until we win the race.
    for n in (max_n + 1)..=(max_n + 100) {
        let candidate = format!("{}~{}.md", base, n);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(bod_dir.join(&candidate))
        {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("Too many collisions for base '{}'", base),
    ))
}

/// Legacy non-atomic collision resolution. Only used in tests.
#[cfg(test)]
fn resolve_collision(bod_dir: &Path, base: &str) -> String {
    let candidate = format!("{}.md", base);
    if !bod_dir.join(&candidate).exists() {
        return candidate;
    }

    let prefix = format!("{}~", base);
    let mut max_n: u32 = 1;

    if let Ok(entries) = std::fs::read_dir(bod_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix(&prefix)
                && let Some(num_str) = rest.strip_suffix(".md")
                && let Ok(n) = num_str.parse::<u32>()
            {
                max_n = max_n.max(n);
            }
        }
    }

    format!("{}~{}.md", base, max_n + 1)
}

/// Atomically create a review file with collision avoidance.
/// Returns `(filename, ReservedFile)`. The RAII guard will delete the file
/// on drop unless `disarm()` is called, preventing orphaned 0-byte files.
///
/// Returns an error if `sanitized_branch` is empty.
pub fn create_review_file(
    bod_dir: &Path,
    codename: &str,
    sanitized_branch: &str,
    timestamp: &str,
) -> std::io::Result<(String, ReservedFile)> {
    if sanitized_branch.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sanitized_branch must not be empty",
        ));
    }
    let base = format!("{}-{}-{}", timestamp, codename, sanitized_branch);
    let (filename, file) = create_atomic(bod_dir, &base)?;
    drop(file);
    let guard = ReservedFile::new(bod_dir.join(&filename));
    Ok((filename, guard))
}

/// Atomically create a consolidated report file with collision avoidance.
/// Returns `(filename, ReservedFile)`. The RAII guard will delete the file
/// on drop unless `disarm()` is called, preventing orphaned 0-byte files.
///
/// Returns an error if `sanitized_branch` is empty.
pub fn create_consolidated_file(
    bod_dir: &Path,
    sanitized_branch: &str,
    timestamp: &str,
) -> std::io::Result<(String, ReservedFile)> {
    if sanitized_branch.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sanitized_branch must not be empty",
        ));
    }
    let base = format!("{}-consolidated-{}", timestamp, sanitized_branch);
    let (filename, file) = create_atomic(bod_dir, &base)?;
    drop(file);
    let guard = ReservedFile::new(bod_dir.join(&filename));
    Ok((filename, guard))
}

/// Remove review round artifacts older than the most recent `keep` rounds.
/// Never removes bugfix logs or consolidated reports.
///
/// Consolidated reports are preserved because they are the useful synthesis output;
/// removing them would lose the human-readable summary while the raw reviews are
/// already gone.
///
/// Only timestamped review files are eligible for automatic cleanup. Legacy (pre-timestamp)
/// files are not cleaned up because the `{codename}-{branch}` naming pattern is
/// ambiguous with user-created files (e.g. `opus-my-notes.md`). Legacy files are
/// finite and will not grow; users can remove them manually if needed.
pub fn cleanup_old_rounds(
    bod_dir: &Path,
    keep: usize,
) -> Result<u32, String> {
    // Collect all timestamped review round artifacts, excluding consolidated reports,
    // bugfix logs, and unrelated user files. Consolidated reports share the same round
    // prefix as their source reviews but should be preserved -- they are the useful
    // synthesis output.
    let mut files_with_ts: Vec<(String, String)> = Vec::new();

    let entries = std::fs::read_dir(bod_dir)
        .map_err(|e| format!("Failed to read directory {}: {}", bod_dir.display(), e))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if files::is_bugfix_log(&name) {
            continue;
        }
        let path = Path::new(&name);
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };
        if extension == "md" && is_consolidated_file(&name) {
            continue;
        }
        if let Some((round_id, rest)) = split_round_prefix(stem)
            && is_cleanup_candidate(rest, extension)
        {
            files_with_ts.push((round_id.to_string(), name));
        }
    }

    let mut timestamps: Vec<String> = files_with_ts.iter().map(|(ts, _)| ts.clone()).collect();
    // Sort by the calendar-order prefix only (first 14 or 12 digits), not the
    // random nonce. This ensures chronological ordering even when two rounds
    // share the same second but have different nonces.
    //
    // Dedup collapses same-second rounds (different nonces) into one logical round.
    // This is intentional: sub-second ordering between concurrent invocations is
    // undefined, and treating them as one round avoids surprising cleanup behavior
    // where `keep=10` keeps fewer than 10 _invocations_. The tradeoff is that
    // truly concurrent runs in the same second share a cleanup lifetime.
    timestamps.sort_by(|a, b| round_id_sort_key(a).cmp(round_id_sort_key(b)));
    timestamps.dedup_by(|a, b| round_id_sort_key(a) == round_id_sort_key(b));

    let total_rounds = timestamps.len();

    if total_rounds <= keep {
        return Ok(0);
    }

    let rounds_to_remove = total_rounds - keep;
    let remove_timestamps = &timestamps[..rounds_to_remove];
    let remove_keys: std::collections::HashSet<&str> = remove_timestamps
        .iter()
        .map(|r| round_id_sort_key(r))
        .collect();
    let mut removed = 0u32;

    for (ts, filename) in &files_with_ts {
        if remove_keys.contains(round_id_sort_key(ts)) {
            let path = bod_dir.join(filename);
            if let Err(e) = std::fs::remove_file(&path) {
                eprintln!("  Warning: failed to remove {}: {}", filename, e);
            } else {
                removed += 1;
            }
        }
    }

    if removed > 0 {
        eprintln!("  Cleaned up {} old review artifact(s).", removed);
    }

    Ok(removed)
}

/// Return the number of days in the given month, accounting for leap years.
fn days_in_month(month: u32, year: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Check if a timestamp string represents a realistic date/time.
/// Expects all characters to already be verified as ASCII digits by the caller.
/// Validates day-of-month against the actual calendar (e.g. rejects Feb 31).
fn is_valid_timestamp_range(ts: &str) -> bool {
    let len = ts.len();
    if len != 14 && len != 12 {
        return false;
    }
    let year: u32 = ts[0..4].parse().unwrap_or(0);
    let month: u32 = ts[4..6].parse().unwrap_or(0);
    let day: u32 = ts[6..8].parse().unwrap_or(0);
    let hour: u32 = ts[8..10].parse().unwrap_or(0);
    let minute: u32 = ts[10..12].parse().unwrap_or(0);
    if !(2000..=2099).contains(&year)
        || !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(month, year)
        || hour > 23
        || minute > 59
    {
        return false;
    }
    if len == 14 {
        let second: u32 = ts[12..14].parse().unwrap_or(0);
        if second > 59 {
            return false;
        }
    }
    true
}

/// Split a filename stem into the round-identifier prefix and the remainder.
///
/// Recognizes three formats:
/// - New: `{14digits}n{hex}-...` (round ID with nonce)
/// - Standard: `{14digits}-...` (timestamp-only, 14 digits)
/// - Legacy: `{12digits}-...` (old minute-precision, 12 digits)
///
/// Returns `(round_id, rest_after_dash)` or `None`.
fn split_round_prefix(stem: &str) -> Option<(&str, &str)> {
    // Try 14-digit base
    if stem.len() >= 14
        && stem.as_bytes()[..14].iter().all(|b| b.is_ascii_digit())
        && is_valid_timestamp_range(&stem[..14])
    {
        // With nonce: `{14digits}n{hex}-`
        if stem.len() > 14 && stem.as_bytes()[14] == b'n' {
            if let Some(dash_offset) = stem[15..].find('-') {
                let nonce_end = 15 + dash_offset;
                if nonce_end > 15 && stem[15..nonce_end].bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Some((&stem[..nonce_end], &stem[nonce_end + 1..]));
                }
            }
            // Standalone nonce (no dash after) -- e.g. the full stem is just the round ID
            if stem[15..].bytes().all(|b| b.is_ascii_hexdigit()) && stem.len() > 15 {
                return Some((stem, ""));
            }
        }
        // Without nonce: `{14digits}-`
        if stem.len() > 14 && stem.as_bytes()[14] == b'-' {
            return Some((&stem[..14], &stem[15..]));
        }
        // Standalone 14-digit timestamp
        if stem.len() == 14 {
            return Some((&stem[..14], ""));
        }
    }
    // Legacy 12-digit
    if stem.len() >= 12
        && stem.as_bytes()[..12].iter().all(|b| b.is_ascii_digit())
        && is_valid_timestamp_range(&stem[..12])
    {
        if stem.len() > 12 && stem.as_bytes()[12] == b'-' {
            return Some((&stem[..12], &stem[13..]));
        }
        if stem.len() == 12 {
            return Some((&stem[..12], ""));
        }
    }
    None
}

/// Extract the round identifier prefix from a filename.
/// Returns the full round ID (timestamp + optional nonce) for grouping/matching.
/// Also accepts legacy 12-digit timestamps for backwards compatibility.
pub fn extract_timestamp(filename: &str) -> Option<&str> {
    let stem = filename.strip_suffix(".md")?;
    split_round_prefix(stem).map(|(round_id, _)| round_id)
}

/// Extract only the calendar-order timestamp prefix (14 or 12 digits) from a filename,
/// ignoring the random nonce. Use this for ordering rounds chronologically --
/// `extract_timestamp()` includes the nonce which sorts randomly within the same second.
#[cfg(test)]
fn extract_timestamp_prefix(filename: &str) -> Option<&str> {
    let stem = filename.strip_suffix(".md")?;
    let round_id = split_round_prefix(stem).map(|(round_id, _)| round_id)?;
    Some(round_id_sort_key(round_id))
}

/// Return the calendar-order portion of a round ID (stripping the random nonce).
/// For IDs like `20260316153045n003d1a2b3c4d`, returns `20260316153045`.
/// For plain timestamps (`20260316153045`, `202603161530`), returns the full ID.
pub fn round_id_sort_key(round_id: &str) -> &str {
    if round_id.len() > 14 && round_id.as_bytes()[14] == b'n' {
        &round_id[..14]
    } else {
        round_id
    }
}

/// Check if a timestamped filename has the review file naming structure:
/// `{round_id}-{codename}-{branch}[~N]` (after stripping `.md`).
/// The `rest` parameter is the portion after the `{round_id}-` prefix.
/// Requires at least one dash (separating codename from branch) and only
/// characters valid in sanitized codenames/branches (`[a-zA-Z0-9_-]` plus `~` for collision).
fn is_review_file_structure(rest: &str) -> bool {
    if rest.is_empty() {
        return false;
    }
    // Must contain at least one dash separating codename from branch
    if !rest.contains('-') {
        return false;
    }
    // Strip optional ~N collision suffix before validating characters
    let base = if let Some(tilde_pos) = rest.rfind('~') {
        let suffix = &rest[tilde_pos + 1..];
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
            &rest[..tilde_pos]
        } else {
            rest
        }
    } else {
        rest
    };
    // Base (codename-branch) must be non-empty and only contain valid chars
    !base.is_empty() && base.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_cleanup_candidate(rest: &str, extension: &str) -> bool {
    match extension {
        "md" => is_review_file_structure(rest),
        "patch" => is_review_context_artifact(rest, "diff-"),
        "txt" => {
            is_review_context_artifact(rest, "diffstat-")
                || is_review_context_artifact(rest, "files-")
        }
        _ => false,
    }
}

fn is_review_context_artifact(rest: &str, prefix: &str) -> bool {
    let Some(branch_part) = rest.strip_prefix(prefix) else {
        return false;
    };
    !branch_part.is_empty()
        && branch_part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Check if a filename is a consolidated report. Matches both new-format
/// (timestamp-prefixed, e.g. `20260316153045-consolidated-feature.md`,
/// `20260316153045n1a2b3c4d-consolidated-feature.md`) and legacy format
/// (e.g. `consolidated-feature.md`, `consolidated-feature-2.md`).
fn is_consolidated_file(filename: &str) -> bool {
    let stem = filename.strip_suffix(".md").unwrap_or(filename);

    // Legacy format: `consolidated-*`
    if stem.starts_with("consolidated-") {
        return true;
    }

    split_round_prefix(stem)
        .map(|(_, rest)| rest.starts_with("consolidated-"))
        .unwrap_or(false)
}

/// List all review .md files in the state directory, excluding consolidated reports and bugfix logs.
pub fn list_review_files(bod_dir: &Path) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(bod_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy().to_string();
            if name.ends_with(".md")
                && !files::is_bugfix_log(&name)
                && !is_consolidated_file(&name)
            {
                files.push(name);
            }
        }
    }

    files.sort();
    files
}

/// List timestamped review files produced by review rounds across all branches.
pub fn list_timestamped_review_files(bod_dir: &Path) -> Vec<String> {
    let mut files = Vec::new();

    if let Ok(entries) = std::fs::read_dir(bod_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if files::is_bugfix_log(&name) || is_consolidated_file(&name) {
                continue;
            }
            let path = Path::new(&name);
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
                continue;
            };
            if extension != "md" {
                continue;
            }
            if let Some((_, rest)) = split_round_prefix(stem)
                && is_review_file_structure(rest)
            {
                files.push(name);
            }
        }
    }

    files.sort();
    files
}

/// List all consolidated reports in the state directory across all branches.
pub fn list_consolidated_files(bod_dir: &Path) -> Vec<String> {
    let mut files = Vec::new();

    if let Ok(entries) = std::fs::read_dir(bod_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".md") && is_consolidated_file(&name) {
                files.push(name);
            }
        }
    }

    files.sort();
    files
}

/// List all diff/context artifacts created for review rounds across all branches.
pub fn list_review_context_artifact_files(bod_dir: &Path) -> Vec<String> {
    let mut files = Vec::new();

    if let Ok(entries) = std::fs::read_dir(bod_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = Path::new(&name);
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
                continue;
            };
            let Some((_, rest)) = split_round_prefix(stem) else {
                continue;
            };
            if extension != "md" && is_cleanup_candidate(rest, extension) {
                files.push(name);
            }
        }
    }

    files.sort();
    files
}

/// List review files for the current branch only.
/// Uses configured codenames so branch suffixes are parsed without ambiguity.
pub fn list_review_files_for_branch(
    bod_dir: &Path,
    sanitized_branch: &str,
    codenames: &[String],
) -> Vec<String> {
    list_review_files(bod_dir)
        .into_iter()
        .filter(|file| review_file_matches_branch(file, codenames, sanitized_branch))
        .collect()
}

/// Check whether a review filename belongs to the given branch.
/// Handles both timestamped and legacy review filenames.
pub fn review_file_matches_branch(
    filename: &str,
    codenames: &[String],
    sanitized_branch: &str,
) -> bool {
    if sanitized_branch.is_empty() {
        return false;
    }

    let sorted_cn = sort_codenames_longest_first(codenames);
    review_file_matches_branch_with_sorted_codenames(filename, &sorted_cn, sanitized_branch)
}

/// List consolidated reports for the current branch only.
/// Supports both timestamped and legacy consolidated filenames.
pub fn list_consolidated_files_for_branch(bod_dir: &Path, sanitized_branch: &str) -> Vec<String> {
    if sanitized_branch.is_empty() {
        return Vec::new();
    }

    list_consolidated_files(bod_dir)
        .into_iter()
        .filter(|name| {
            let stem = name.strip_suffix(".md").unwrap_or(name);
            consolidated_stem_matches_branch(stem, sanitized_branch)
        })
        .collect()
}

/// Sort codenames longest-first for prefix-ambiguity resolution.
/// Call once and pass the result to `stem_matches_branch` to avoid
/// re-sorting on every invocation.
fn sort_codenames_longest_first(codenames: &[String]) -> Vec<&str> {
    let mut sorted: Vec<&str> = codenames.iter().map(|s| s.as_str()).collect();
    sorted.sort_by(|a, b| b.len().cmp(&a.len()));
    sorted
}

/// Check if a filename stem matches a specific branch after stripping a `{prefix}{codename}-` pattern.
/// Accepts optional `~N` collision suffixes (new format). Used by both
/// `list_review_files_for_round_id` and `latest_review_files` to avoid
/// duplicating branch-matching logic.
///
/// `sorted_codenames` must be sorted longest-first (via `sort_codenames_longest_first`)
/// so that when one codename is a dash-prefix of another (e.g. `opus` vs `opus-pro`),
/// the longer match takes priority. If a longer codename matches the prefix but the
/// branch portion does not match, we fall through to try shorter codenames.
fn stem_matches_branch(stem: &str, prefix: &str, sorted_codenames: &[&str], branch: &str) -> bool {
    for cn in sorted_codenames {
        let expected_prefix = format!("{}{}-", prefix, cn);
        if !stem.starts_with(&expected_prefix) {
            continue;
        }
        let remaining = &stem[expected_prefix.len()..];
        let matches = remaining == branch
            || remaining
                .strip_prefix(branch)
                .and_then(|rest| rest.strip_prefix('~'))
                .map_or(false, |digits| {
                    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
                });
        if matches {
            return true;
        }
        // Longer codename matched the prefix but branch did not match;
        // fall through to try shorter codenames.
    }
    false
}

fn legacy_review_file_matches_branch(
    stem: &str,
    sorted_codenames: &[&str],
    sanitized_branch: &str,
) -> bool {
    for cn in sorted_codenames {
        let expected_prefix = format!("{}-", cn);
        if !stem.starts_with(&expected_prefix) {
            continue;
        }

        let remaining = &stem[expected_prefix.len()..];
        return remaining == sanitized_branch
            || remaining
                .strip_prefix(sanitized_branch)
                .map_or(false, |rest| is_collision_suffix(rest));
    }

    false
}

fn review_file_matches_branch_with_sorted_codenames(
    filename: &str,
    sorted_codenames: &[&str],
    sanitized_branch: &str,
) -> bool {
    let stem = filename.strip_suffix(".md").unwrap_or(filename);
    if let Some(round_id) = extract_timestamp(filename) {
        let prefix = format!("{}-", round_id);
        return stem_matches_branch(stem, &prefix, sorted_codenames, sanitized_branch);
    }

    legacy_review_file_matches_branch(stem, sorted_codenames, sanitized_branch)
}

fn consolidated_name_matches_branch(rest: &str, sanitized_branch: &str) -> bool {
    rest == sanitized_branch
        || rest
            .strip_prefix(sanitized_branch)
            .map_or(false, |suffix| is_collision_suffix(suffix))
}

fn consolidated_stem_matches_branch(stem: &str, sanitized_branch: &str) -> bool {
    if sanitized_branch.is_empty() {
        return false;
    }

    if let Some(rest) = stem.strip_prefix("consolidated-") {
        return consolidated_name_matches_branch(rest, sanitized_branch);
    }

    split_round_prefix(stem)
        .and_then(|(_, rest)| rest.strip_prefix("consolidated-"))
        .map_or(false, |rest| {
            consolidated_name_matches_branch(rest, sanitized_branch)
        })
}

/// List review files that match a specific round ID and optionally a branch name.
/// When `sanitized_branch` is provided, only files for that branch are returned,
/// preventing cross-branch contamination in concurrent runs.
///
/// `round_id` is the full round identifier (timestamp + optional nonce) as returned
/// by `timestamp_now()` or `extract_timestamp()`. Callers must pass the full ID,
/// not just the 14-digit calendar prefix.
///
/// `codenames` is required when `sanitized_branch` is `Some` to correctly parse
/// the `{codename}-{branch}` portion of filenames. Codenames may contain dashes
/// (e.g. `claude-opus`, `gpt-4`), so splitting on the first dash is incorrect.
pub fn list_review_files_for_round_id(
    bod_dir: &Path,
    round_id: &str,
    sanitized_branch: Option<&str>,
    codenames: &[String],
) -> Vec<String> {
    let sorted_cn = sort_codenames_longest_first(codenames);
    list_review_files(bod_dir)
        .into_iter()
        .filter(|f| {
            if extract_timestamp(f) != Some(round_id) {
                return false;
            }
            if let Some(branch) = sanitized_branch {
                let stem = f.strip_suffix(".md").unwrap_or(f);
                let prefix = format!("{}-", round_id);
                stem_matches_branch(stem, &prefix, &sorted_cn, branch)
            } else {
                true
            }
        })
        .collect()
}

/// Group review files by their timestamp prefix.
/// Files without a valid timestamp (legacy naming) are grouped under a
/// synthetic `"legacy"` key so they remain accessible to callers.
pub fn group_reviews_by_round(files: &[String]) -> HashMap<String, Vec<String>> {
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();

    for file in files {
        if let Some(ts) = extract_timestamp(file) {
            groups
                .entry(ts.to_string())
                .or_default()
                .push(file.clone());
        } else {
            groups
                .entry("legacy".to_string())
                .or_default()
                .push(file.clone());
        }
    }

    groups
}

/// Find the review files from the most recent round for a given branch.
/// Uses codenames to parse the branch name from each filename.
/// Falls back to legacy (pre-timestamp) files when no timestamped round matches.
///
/// **Legacy fallback limitation**: Legacy files have no timestamp to distinguish
/// separate review runs. When the fallback activates, all legacy files matching
/// `{codename}-{branch}` are returned as a single undifferentiated group, which
/// may mix reviews from different historical runs. Users with only legacy files
/// should run `bod init` and `bod review` to generate timestamped rounds.
pub fn latest_review_files(
    files: &[String],
    codenames: &[String],
    sanitized_branch: &str,
) -> Option<Vec<String>> {
    // Guard against empty branch name: would match all files
    if sanitized_branch.is_empty() {
        return None;
    }

    let groups = group_reviews_by_round(files);
    let sorted_cn = sort_codenames_longest_first(codenames);

    // First try timestamped rounds.
    let timestamped_result = groups
        .iter()
        .filter(|(ts, _)| ts.as_str() != "legacy")
        .filter_map(|(ts, round_files)| {
            let matching: Vec<String> = round_files
                .iter()
                .filter(|f| {
                    let stem = f.strip_suffix(".md").unwrap_or(f);
                    let prefix = format!("{}-", ts);
                    stem_matches_branch(stem, &prefix, &sorted_cn, sanitized_branch)
                })
                .cloned()
                .collect();

            if matching.is_empty() {
                None
            } else {
                let mut sorted = matching;
                sorted.sort();
                Some((ts.clone(), sorted))
            }
        })
        // Primary: calendar-order prefix (14/12 digits). Tiebreaker: full round
        // ID including nonce. The tiebreaker is deterministic but arbitrary --
        // sub-second ordering between concurrent invocations is undefined.
        .max_by(|(a, _), (b, _)| {
            round_id_sort_key(a)
                .cmp(round_id_sort_key(b))
                .then_with(|| a.cmp(b))
        })
        .map(|(_, round_files)| round_files);

    if timestamped_result.is_some() {
        return timestamped_result;
    }

    // Fallback: check legacy (pre-timestamp) files with `{codename}-{branch}` pattern.
    // Accept both `~N` (new) and `-N` (pre-iteration-10) collision suffixes.
    // Reuse sorted_cn computed above.
    if let Some(legacy_files) = groups.get("legacy") {
        let matching: Vec<String> = legacy_files
            .iter()
            .filter(|f| {
                let stem = f.strip_suffix(".md").unwrap_or(f);
                legacy_review_file_matches_branch(stem, &sorted_cn, sanitized_branch)
            })
            .cloned()
            .collect();

        if !matching.is_empty() {
            let mut sorted = matching;
            sorted.sort();
            return Some(sorted);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn timestamp_now_contains_nonce() {
        let ts = timestamp_now();
        // Format: 14 digits + 'n' + 12 hex chars = 27 chars
        assert_eq!(ts.len(), 27);
        assert!(ts[..14].chars().all(|c| c.is_ascii_digit()));
        assert_eq!(ts.as_bytes()[14], b'n');
        assert!(ts[15..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn timestamp_now_unique_across_calls() {
        // Call 100 times and assert all are unique. With 16 bits of randomness
        // plus millisecond bucketing, a single pair has ~1/65536 collision
        // probability. Collecting many samples and checking the full set is
        // robust against same-millisecond collisions.
        let ids: std::collections::HashSet<String> =
            (0..100).map(|_| timestamp_now()).collect();
        assert_eq!(ids.len(), 100, "expected 100 unique round IDs");
    }

    #[test]
    fn resolve_collision_no_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_collision(dir.path(), "20260316153045-opus-feature");
        assert_eq!(result, "20260316153045-opus-feature.md");
    }

    #[test]
    fn resolve_collision_with_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        let result = resolve_collision(dir.path(), "20260316153045-opus-feature");
        assert_eq!(result, "20260316153045-opus-feature~2.md");
    }

    #[test]
    fn resolve_collision_with_multiple_existing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature~2.md"), "").unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature~3.md"), "").unwrap();
        let result = resolve_collision(dir.path(), "20260316153045-opus-feature");
        assert_eq!(result, "20260316153045-opus-feature~4.md");
    }

    #[test]
    fn extract_timestamp_14_digit() {
        assert_eq!(
            extract_timestamp("20260316153045-opus-feature.md"),
            Some("20260316153045")
        );
    }

    #[test]
    fn extract_timestamp_14_digit_with_collision_suffix() {
        assert_eq!(
            extract_timestamp("20260316153045-opus-feature~2.md"),
            Some("20260316153045")
        );
    }

    #[test]
    fn extract_timestamp_with_nonce() {
        assert_eq!(
            extract_timestamp("20260316153045n1a2b3c4d-opus-feature.md"),
            Some("20260316153045n1a2b3c4d")
        );
    }

    #[test]
    fn extract_timestamp_with_nonce_collision_suffix() {
        assert_eq!(
            extract_timestamp("20260316153045n1a2b3c4d-opus-feature~2.md"),
            Some("20260316153045n1a2b3c4d")
        );
    }

    #[test]
    fn extract_timestamp_prefix_strips_nonce() {
        assert_eq!(
            extract_timestamp_prefix("20260316153045n1a2b3c4d-opus-feature.md"),
            Some("20260316153045")
        );
        // Plain 14-digit timestamp (no nonce)
        assert_eq!(
            extract_timestamp_prefix("20260316153045-opus-feature.md"),
            Some("20260316153045")
        );
        // Legacy 12-digit
        assert_eq!(
            extract_timestamp_prefix("202603161530-opus-feature.md"),
            Some("202603161530")
        );
        // No timestamp
        assert_eq!(extract_timestamp_prefix("opus-feature.md"), None);
    }

    #[test]
    fn latest_review_files_sorts_by_timestamp_not_nonce() {
        // Two rounds in the same second with different nonces.
        // Round A has a higher nonce but should NOT be preferred over Round B
        // which has a later timestamp.
        let files = vec![
            "20260316153045nffffffff-opus-feature.md".to_string(),
            "20260316153046n00000001-opus-feature.md".to_string(),
        ];
        let codenames = vec!["opus".to_string()];

        let latest = latest_review_files(&files, &codenames, "feature").unwrap();
        // Should pick the later timestamp (153046), not the higher nonce
        assert_eq!(
            latest,
            vec!["20260316153046n00000001-opus-feature.md"]
        );
    }

    #[test]
    fn extract_timestamp_legacy_12_digit() {
        assert_eq!(
            extract_timestamp("202603161530-opus-feature.md"),
            Some("202603161530")
        );
    }

    #[test]
    fn extract_timestamp_invalid() {
        assert_eq!(extract_timestamp("opus-feature-1.md"), None);
        assert_eq!(extract_timestamp("bugfix-feature.log.md"), None);
    }

    #[test]
    fn extract_timestamp_rejects_out_of_range() {
        // All 9s -- nonsense timestamp
        assert_eq!(extract_timestamp("99999999999999-opus-feature.md"), None);
        // Invalid month (13)
        assert_eq!(extract_timestamp("20261316153045-opus-feature.md"), None);
        // Invalid day (32)
        assert_eq!(extract_timestamp("20260332153045-opus-feature.md"), None);
        // Invalid hour (25)
        assert_eq!(extract_timestamp("20260316253045-opus-feature.md"), None);
        // Invalid minute (61)
        assert_eq!(extract_timestamp("20260316156145-opus-feature.md"), None);
        // Invalid second (61)
        assert_eq!(extract_timestamp("20260316153061-opus-feature.md"), None);
        // Year before 2000
        assert_eq!(extract_timestamp("19990316153045-opus-feature.md"), None);
        // Month 00
        assert_eq!(extract_timestamp("20260016153045-opus-feature.md"), None);
        // Day 00
        assert_eq!(extract_timestamp("20260300153045-opus-feature.md"), None);
        // Legacy 12-digit with invalid month
        assert_eq!(extract_timestamp("202613161530-opus-feature.md"), None);
        // Feb 31 (invalid day for February)
        assert_eq!(extract_timestamp("20260231153045-opus-feature.md"), None);
        // Apr 31 (April has 30 days)
        assert_eq!(extract_timestamp("20260431153045-opus-feature.md"), None);
        // Jun 31 (June has 30 days)
        assert_eq!(extract_timestamp("20260631153045-opus-feature.md"), None);
        // Feb 29 on non-leap year (2025)
        assert_eq!(extract_timestamp("20250229153045-opus-feature.md"), None);
    }

    #[test]
    fn extract_timestamp_accepts_leap_year_feb29() {
        // Feb 29 on leap year (2024) is valid
        assert_eq!(
            extract_timestamp("20240229153045-opus-feature.md"),
            Some("20240229153045")
        );
    }

    #[test]
    fn is_consolidated_file_true_14_digit() {
        assert!(is_consolidated_file("20260316153045-consolidated-feature.md"));
        assert!(is_consolidated_file("20260316153045-consolidated-feature~2.md"));
    }

    #[test]
    fn is_consolidated_file_true_with_nonce() {
        assert!(is_consolidated_file("20260316153045n1a2b3c4d-consolidated-feature.md"));
        assert!(is_consolidated_file("20260316153045ndeadbeef-consolidated-feature~2.md"));
    }

    #[test]
    fn is_consolidated_file_true_legacy_12_digit() {
        assert!(is_consolidated_file("202603161530-consolidated-feature.md"));
        assert!(is_consolidated_file("202603161530-consolidated-feature~2.md"));
    }

    #[test]
    fn is_consolidated_file_false() {
        assert!(!is_consolidated_file("20260316153045-opus-feature.md"));
        assert!(!is_consolidated_file("bugfix-feature.log.md"));
    }

    #[test]
    fn is_consolidated_file_rejects_invalid_timestamp() {
        // All-9s timestamp is out of range -- should not be classified as consolidated
        assert!(!is_consolidated_file("99999999999999-consolidated-feature.md"));
        // Invalid month
        assert!(!is_consolidated_file("20261316153045-consolidated-feature.md"));
    }

    #[test]
    fn group_reviews_by_round_groups_by_timestamp() {
        let files = vec![
            "20260316153045-codex-feature.md".to_string(),
            "20260316153045-gemini-feature.md".to_string(),
            "20260316153045-opus-feature.md".to_string(),
            "20260316200015-codex-feature.md".to_string(),
            "20260316200015-gemini-feature.md".to_string(),
        ];

        let groups = group_reviews_by_round(&files);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups["20260316153045"].len(), 3);
        assert_eq!(groups["20260316200015"].len(), 2);
    }

    #[test]
    fn list_review_files_excludes_consolidated() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(
            dir.path().join("20260316153045-consolidated-feature.md"),
            "",
        )
        .unwrap();
        fs::write(dir.path().join("bugfix-feature.log.md"), "").unwrap();

        let files = list_review_files(dir.path());
        assert_eq!(files, vec!["20260316153045-opus-feature.md"]);
    }

    #[test]
    fn review_file_matches_branch_avoids_branch_suffix_false_positive() {
        let codenames = vec!["opus".to_string()];
        assert!(!review_file_matches_branch(
            "20260316153045-opus-my-feature.md",
            &codenames,
            "feature"
        ));
        assert!(review_file_matches_branch(
            "20260316153045-opus-my-feature.md",
            &codenames,
            "my-feature"
        ));
    }

    #[test]
    fn list_review_files_for_branch_filters_current_branch() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316153045-codex-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316153045-opus-other.md"), "").unwrap();
        fs::write(dir.path().join("opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316153045-consolidated-feature.md"), "").unwrap();
        fs::write(dir.path().join("bugfix-feature.log.md"), "").unwrap();

        let files = list_review_files_for_branch(
            dir.path(),
            "feature",
            &["codex".to_string(), "opus".to_string()],
        );
        assert_eq!(
            files,
            vec![
                "20260316153045-codex-feature.md",
                "20260316153045-opus-feature.md",
                "opus-feature.md",
            ]
        );
    }

    #[test]
    fn list_consolidated_files_for_branch_filters_current_branch() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-consolidated-feature.md"), "").unwrap();
        fs::write(
            dir.path().join("20260316153045n000000000001-consolidated-feature~2.md"),
            "",
        )
        .unwrap();
        fs::write(dir.path().join("consolidated-feature-2.md"), "").unwrap();
        fs::write(dir.path().join("20260316153045-consolidated-other.md"), "").unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();

        let files = list_consolidated_files_for_branch(dir.path(), "feature");
        assert_eq!(
            files,
            vec![
                "20260316153045-consolidated-feature.md",
                "20260316153045n000000000001-consolidated-feature~2.md",
                "consolidated-feature-2.md",
            ]
        );
    }

    #[test]
    fn list_consolidated_files_includes_legacy_and_timestamped() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-consolidated-feature.md"), "").unwrap();
        fs::write(dir.path().join("consolidated-main.md"), "").unwrap();
        fs::write(dir.path().join("bugfix-main.log.md"), "").unwrap();

        let files = list_consolidated_files(dir.path());

        assert_eq!(
            files,
            vec![
                "20260316153045-consolidated-feature.md".to_string(),
                "consolidated-main.md".to_string(),
            ]
        );
    }

    #[test]
    fn latest_review_files_picks_highest_round_for_branch() {
        let files = vec![
            "20260316153045-codex-feature.md".to_string(),
            "20260316153045-gemini-feature.md".to_string(),
            "20260316153045-opus-feature.md".to_string(),
            "20260316200015-codex-feature.md".to_string(),
            "20260316200015-gemini-feature.md".to_string(),
            "20260316200015-opus-feature.md".to_string(),
            "20260316210030-codex-other.md".to_string(),
        ];
        let codenames = vec![
            "codex".to_string(),
            "gemini".to_string(),
            "opus".to_string(),
        ];

        let latest = latest_review_files(&files, &codenames, "feature").unwrap();

        assert_eq!(
            latest,
            vec![
                "20260316200015-codex-feature.md".to_string(),
                "20260316200015-gemini-feature.md".to_string(),
                "20260316200015-opus-feature.md".to_string(),
            ]
        );
    }

    #[test]
    fn latest_review_files_returns_none_when_branch_has_no_reviews() {
        let files = vec!["20260316153045-codex-other.md".to_string()];
        let codenames = vec!["codex".to_string()];

        assert!(latest_review_files(&files, &codenames, "feature").is_none());
    }

    #[test]
    fn latest_review_files_returns_none_for_empty_branch() {
        let files = vec!["20260316153045-codex-feature.md".to_string()];
        let codenames = vec!["codex".to_string()];

        assert!(latest_review_files(&files, &codenames, "").is_none());
    }

    #[test]
    fn latest_review_files_no_false_positive_on_branch_substring() {
        // Branch "fix" must not match files for branch "foo-fix"
        let files = vec![
            "20260316153045-opus-foo-fix.md".to_string(),
            "20260316153045-opus-fix.md".to_string(),
        ];
        let codenames = vec!["opus".to_string()];

        let latest = latest_review_files(&files, &codenames, "fix").unwrap();
        assert_eq!(latest, vec!["20260316153045-opus-fix.md"]);
    }

    #[test]
    fn latest_review_files_matches_collision_suffix() {
        let files = vec![
            "20260316153045-opus-feature.md".to_string(),
            "20260316153045-opus-feature~2.md".to_string(),
        ];
        let codenames = vec!["opus".to_string()];

        let latest = latest_review_files(&files, &codenames, "feature").unwrap();
        assert_eq!(
            latest,
            vec![
                "20260316153045-opus-feature.md",
                "20260316153045-opus-feature~2.md",
            ]
        );
    }

    #[test]
    fn latest_review_files_falls_back_to_legacy() {
        // Only legacy files exist (no timestamped rounds)
        let files = vec![
            "opus-feature.md".to_string(),
            "gemini-feature.md".to_string(),
            "opus-other.md".to_string(),
        ];
        let codenames = vec!["opus".to_string(), "gemini".to_string()];

        let latest = latest_review_files(&files, &codenames, "feature").unwrap();
        assert_eq!(
            latest,
            vec!["gemini-feature.md", "opus-feature.md"]
        );
    }

    #[test]
    fn latest_review_files_prefers_timestamped_over_legacy() {
        let files = vec![
            "opus-feature.md".to_string(),
            "20260316153045-opus-feature.md".to_string(),
        ];
        let codenames = vec!["opus".to_string()];

        let latest = latest_review_files(&files, &codenames, "feature").unwrap();
        assert_eq!(latest, vec!["20260316153045-opus-feature.md"]);
    }

    #[test]
    fn latest_review_files_legacy_dash_collision_suffix() {
        // Pre-iteration-10 files used -N collision suffixes
        let files = vec![
            "codex-feature.md".to_string(),
            "codex-feature-1.md".to_string(),
            "opus-feature.md".to_string(),
        ];
        let codenames = vec!["codex".to_string(), "opus".to_string()];

        let latest = latest_review_files(&files, &codenames, "feature").unwrap();
        assert_eq!(
            latest,
            vec![
                "codex-feature-1.md",
                "codex-feature.md",
                "opus-feature.md",
            ]
        );
    }

    #[test]
    fn cleanup_old_rounds_ignores_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        // Create a timestamped file and a legacy file
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("opus-feature-1.md"), "").unwrap();
        // bugfix log should never be removed
        fs::write(dir.path().join("bugfix-feature.log.md"), "").unwrap();

        // Legacy files are no longer cleaned up (ambiguous naming pattern).
        // Only the timestamped round exists; keep=1 means nothing is removed.
        let removed = cleanup_old_rounds(dir.path(), 1).unwrap();
        assert_eq!(removed, 0);
        assert!(dir.path().join("20260316153045-opus-feature.md").exists());
        // Legacy file is preserved (not eligible for cleanup)
        assert!(dir.path().join("opus-feature-1.md").exists());
        assert!(dir.path().join("bugfix-feature.log.md").exists());
    }

    #[test]
    fn cleanup_old_rounds_preserves_files_within_keep() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("opus-feature-1.md"), "").unwrap();
        fs::write(dir.path().join("bugfix-feature.log.md"), "").unwrap();

        // keep=10: only 1 timestamped round, well within keep limit.
        let removed = cleanup_old_rounds(dir.path(), 10).unwrap();
        assert_eq!(removed, 0);
        assert!(dir.path().join("20260316153045-opus-feature.md").exists());
        assert!(dir.path().join("opus-feature-1.md").exists());
        assert!(dir.path().join("bugfix-feature.log.md").exists());
    }

    #[test]
    fn cleanup_old_rounds_keep_zero_removes_timestamped() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316200015-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("opus-feature-1.md"), "").unwrap();
        fs::write(dir.path().join("bugfix-feature.log.md"), "").unwrap();

        // keep=0 removes all timestamped rounds; legacy files are preserved.
        let removed = cleanup_old_rounds(dir.path(), 0).unwrap();
        assert_eq!(removed, 2);
        assert!(!dir.path().join("20260316153045-opus-feature.md").exists());
        assert!(!dir.path().join("20260316200015-opus-feature.md").exists());
        // Legacy file is preserved (not eligible for cleanup)
        assert!(dir.path().join("opus-feature-1.md").exists());
        // bugfix log is never removed
        assert!(dir.path().join("bugfix-feature.log.md").exists());
    }

    #[test]
    fn cleanup_old_rounds_preserves_consolidated_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("consolidated-feature.md"), "").unwrap();
        fs::write(dir.path().join("consolidated-main.md"), "").unwrap();
        fs::write(dir.path().join("bugfix-feature.log.md"), "").unwrap();

        // keep=1: only the timestamped round exists; nothing to evict.
        let removed = cleanup_old_rounds(dir.path(), 1).unwrap();
        assert_eq!(removed, 0);
        assert!(dir.path().join("20260316153045-opus-feature.md").exists());
        assert!(dir.path().join("consolidated-feature.md").exists());
        assert!(dir.path().join("consolidated-main.md").exists());
        assert!(dir.path().join("bugfix-feature.log.md").exists());
    }

    #[test]
    fn cleanup_old_rounds_preserves_timestamped_consolidated_files() {
        let dir = tempfile::tempdir().unwrap();
        // Two rounds of reviews
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316200015-opus-feature.md"), "").unwrap();
        // Timestamped consolidated reports sharing the same round prefix
        fs::write(dir.path().join("20260316153045-consolidated-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316200015-consolidated-feature.md"), "").unwrap();
        fs::write(dir.path().join("bugfix-feature.log.md"), "").unwrap();

        // keep=1: evict the oldest round's reviews but preserve consolidated reports.
        let removed = cleanup_old_rounds(dir.path(), 1).unwrap();
        assert_eq!(removed, 1); // only the old review file is removed
        assert!(!dir.path().join("20260316153045-opus-feature.md").exists());
        assert!(dir.path().join("20260316200015-opus-feature.md").exists());
        // Both consolidated reports are preserved
        assert!(dir.path().join("20260316153045-consolidated-feature.md").exists());
        assert!(dir.path().join("20260316200015-consolidated-feature.md").exists());
        assert!(dir.path().join("bugfix-feature.log.md").exists());
    }

    #[test]
    fn cleanup_old_rounds_preserves_user_files() {
        let dir = tempfile::tempdir().unwrap();
        // Timestamped files
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        // User-created files that should NOT be deleted
        fs::write(dir.path().join("notes.md"), "my notes").unwrap();
        fs::write(dir.path().join("todo.md"), "my todos").unwrap();
        fs::write(dir.path().join("scratch.md"), "scratch").unwrap();
        fs::write(dir.path().join("my-notes.md"), "my notes").unwrap();
        fs::write(dir.path().join("release-notes.md"), "release").unwrap();
        fs::write(dir.path().join("project-summary.md"), "summary").unwrap();
        // Codename-prefixed user files (the former false-positive problem)
        fs::write(dir.path().join("opus-my-notes.md"), "my notes").unwrap();

        let removed = cleanup_old_rounds(dir.path(), 10).unwrap();
        assert_eq!(removed, 0);
        assert!(dir.path().join("notes.md").exists());
        assert!(dir.path().join("todo.md").exists());
        assert!(dir.path().join("scratch.md").exists());
        assert!(dir.path().join("my-notes.md").exists());
        assert!(dir.path().join("release-notes.md").exists());
        assert!(dir.path().join("project-summary.md").exists());
        assert!(dir.path().join("opus-my-notes.md").exists());
    }

    #[test]
    fn cleanup_old_rounds_preserves_user_timestamped_files() {
        let dir = tempfile::tempdir().unwrap();
        // Valid review files (two rounds)
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316200015-opus-feature.md"), "").unwrap();
        // User-created file with a valid timestamp prefix but no codename-branch structure
        fs::write(dir.path().join("20260316153045-notes.md"), "my notes").unwrap();
        // User-created file with timestamp and single segment (no second dash)
        fs::write(dir.path().join("20260316153045-scratch.md"), "scratch").unwrap();
        fs::write(dir.path().join("bugfix-feature.log.md"), "").unwrap();

        // keep=1: evicts oldest review round but preserves user files
        let removed = cleanup_old_rounds(dir.path(), 1).unwrap();
        assert_eq!(removed, 1);
        assert!(!dir.path().join("20260316153045-opus-feature.md").exists());
        assert!(dir.path().join("20260316200015-opus-feature.md").exists());
        // User-created timestamped files are preserved
        assert!(dir.path().join("20260316153045-notes.md").exists());
        assert!(dir.path().join("20260316153045-scratch.md").exists());
    }

    #[test]
    fn cleanup_old_rounds_removes_large_diff_artifacts_with_old_rounds() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045n000000000001-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316153045n000000000001-diff-feature.patch"), "").unwrap();
        fs::write(dir.path().join("20260316153045n000000000001-diffstat-feature.txt"), "").unwrap();
        fs::write(dir.path().join("20260316153045n000000000001-files-feature.txt"), "").unwrap();
        fs::write(dir.path().join("20260316200015n000000000002-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316200015n000000000002-diff-feature.patch"), "").unwrap();

        let removed = cleanup_old_rounds(dir.path(), 1).unwrap();
        assert_eq!(removed, 4);
        assert!(!dir.path().join("20260316153045n000000000001-opus-feature.md").exists());
        assert!(!dir.path().join("20260316153045n000000000001-diff-feature.patch").exists());
        assert!(!dir.path().join("20260316153045n000000000001-diffstat-feature.txt").exists());
        assert!(!dir.path().join("20260316153045n000000000001-files-feature.txt").exists());
        assert!(dir.path().join("20260316200015n000000000002-opus-feature.md").exists());
        assert!(dir.path().join("20260316200015n000000000002-diff-feature.patch").exists());
    }

    #[test]
    fn list_review_context_artifact_files_returns_known_artifacts_only() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045n000000000001-diff-feature.patch"), "").unwrap();
        fs::write(dir.path().join("20260316153045n000000000001-diffstat-feature.txt"), "").unwrap();
        fs::write(dir.path().join("20260316153045n000000000001-files-feature.txt"), "").unwrap();
        fs::write(dir.path().join("keep-me.txt"), "").unwrap();

        let files = list_review_context_artifact_files(dir.path());

        assert_eq!(
            files,
            vec![
                "20260316153045n000000000001-diff-feature.patch".to_string(),
                "20260316153045n000000000001-diffstat-feature.txt".to_string(),
                "20260316153045n000000000001-files-feature.txt".to_string(),
            ]
        );
    }

    #[test]
    fn cleanup_old_rounds_preserves_user_timestamped_txt_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316200015-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316153045-notes.txt"), "notes").unwrap();
        fs::write(dir.path().join("20260316153045-summary.txt"), "summary").unwrap();

        let removed = cleanup_old_rounds(dir.path(), 1).unwrap();
        assert_eq!(removed, 1);
        assert!(!dir.path().join("20260316153045-opus-feature.md").exists());
        assert!(dir.path().join("20260316200015-opus-feature.md").exists());
        assert!(dir.path().join("20260316153045-notes.txt").exists());
        assert!(dir.path().join("20260316153045-summary.txt").exists());
    }

    #[test]
    fn list_review_files_excludes_legacy_consolidated() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("consolidated-feature.md"), "").unwrap();
        fs::write(dir.path().join("consolidated-feature-2.md"), "").unwrap();

        let files = list_review_files(dir.path());
        assert_eq!(files, vec!["20260316153045-opus-feature.md"]);
    }

    #[test]
    fn list_timestamped_review_files_skips_unrelated_markdown() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("keep-me.md"), "").unwrap();
        fs::write(dir.path().join("notes.md"), "").unwrap();

        let files = list_timestamped_review_files(dir.path());

        assert_eq!(files, vec!["20260316153045-opus-feature.md"]);
    }

    #[test]
    fn list_review_files_excludes_both_bugfix_log_forms() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "review").unwrap();
        // Both the legacy and branch-scoped bugfix logs must be excluded.
        fs::write(dir.path().join("bugfix.log.md"), "legacy log").unwrap();
        fs::write(dir.path().join("bugfix-feature.log.md"), "branch log").unwrap();

        let files = list_review_files(dir.path());
        assert_eq!(files, vec!["20260316153045-opus-feature.md"]);
    }

    #[test]
    fn cleanup_old_rounds_preserves_both_bugfix_log_forms() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316200015-opus-feature.md"), "").unwrap();
        // Both bugfix log forms must survive cleanup.
        fs::write(dir.path().join("bugfix.log.md"), "legacy").unwrap();
        fs::write(dir.path().join("bugfix-feature.log.md"), "branch").unwrap();

        let removed = cleanup_old_rounds(dir.path(), 1).unwrap();
        assert_eq!(removed, 1);
        assert!(dir.path().join("bugfix.log.md").exists());
        assert!(dir.path().join("bugfix-feature.log.md").exists());
    }

    #[test]
    fn is_consolidated_file_matches_legacy_format() {
        assert!(is_consolidated_file("consolidated-feature.md"));
        assert!(is_consolidated_file("consolidated-feature-2.md"));
        assert!(is_consolidated_file("consolidated-main.md"));
    }

    #[test]
    fn group_reviews_by_round_includes_legacy_files() {
        let files = vec![
            "20260316153045-opus-feature.md".to_string(),
            "opus-feature-1.md".to_string(),
            "gemini-feature.md".to_string(),
        ];

        let groups = group_reviews_by_round(&files);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups["20260316153045"].len(), 1);
        assert_eq!(groups["legacy"].len(), 2);
    }

    #[test]
    fn create_review_file_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let (name1, mut f1) = create_review_file(
            dir.path(), "opus", "feature", "20260316153045",
        )
        .unwrap();
        f1.disarm(); // Keep the file so the second call gets a collision suffix
        assert_eq!(name1, "20260316153045-opus-feature.md");

        // Second call with same params gets collision suffix
        let (name2, mut f2) = create_review_file(
            dir.path(), "opus", "feature", "20260316153045",
        )
        .unwrap();
        f2.disarm();
        assert_eq!(name2, "20260316153045-opus-feature~2.md");
    }

    #[test]
    fn create_review_file_rejects_empty_branch() {
        let dir = tempfile::tempdir().unwrap();
        let result = create_review_file(dir.path(), "opus", "", "20260316153045");
        assert!(result.is_err());
    }

    #[test]
    fn create_consolidated_file_rejects_empty_branch() {
        let dir = tempfile::tempdir().unwrap();
        let result = create_consolidated_file(dir.path(), "", "20260316153045");
        assert!(result.is_err());
    }

    #[test]
    fn sanitize_branch_name_all_special_chars_produces_none() {
        assert_eq!(sanitize_branch_name("@@@"), None);
        assert_eq!(sanitize_branch_name("!!!"), None);
    }

    #[test]
    fn list_review_files_for_round_id_matches_exact_round() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "r1").unwrap();
        fs::write(dir.path().join("20260316200015-opus-feature.md"), "r2").unwrap();
        fs::write(dir.path().join("bugfix-feature.log.md"), "").unwrap();

        let codenames = vec!["opus".to_string()];
        let result = list_review_files_for_round_id(
            dir.path(), "20260316153045", None, &codenames,
        );
        assert_eq!(result, vec!["20260316153045-opus-feature.md"]);
    }

    #[test]
    fn list_review_files_for_round_id_mismatched_nonce() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045n1a2b3c4d-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316153045ndeadbeef-opus-feature.md"), "").unwrap();

        let codenames = vec!["opus".to_string()];
        // Only files matching the exact round ID (including nonce) should be returned.
        let result = list_review_files_for_round_id(
            dir.path(), "20260316153045n1a2b3c4d", None, &codenames,
        );
        assert_eq!(result, vec!["20260316153045n1a2b3c4d-opus-feature.md"]);
    }

    #[test]
    fn list_review_files_for_round_id_filters_by_branch() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316153045-opus-bugfix.md"), "").unwrap();

        let codenames = vec!["opus".to_string()];
        let result = list_review_files_for_round_id(
            dir.path(), "20260316153045", Some("feature"), &codenames,
        );
        assert_eq!(result, vec!["20260316153045-opus-feature.md"]);
    }

    #[test]
    fn list_review_files_for_round_id_wrong_branch() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();

        let codenames = vec!["opus".to_string()];
        let result = list_review_files_for_round_id(
            dir.path(), "20260316153045", Some("bugfix"), &codenames,
        );
        assert!(result.is_empty());
    }

    #[test]
    fn list_review_files_for_round_id_collision_suffix() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature~2.md"), "").unwrap();

        let codenames = vec!["opus".to_string()];
        let result = list_review_files_for_round_id(
            dir.path(), "20260316153045", Some("feature"), &codenames,
        );
        assert_eq!(result, vec![
            "20260316153045-opus-feature.md",
            "20260316153045-opus-feature~2.md",
        ]);
    }

    #[test]
    fn list_review_files_for_round_id_dash_codename() {
        // Codenames with dashes (e.g. claude-opus) must be parsed correctly.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-claude-opus-feature.md"), "").unwrap();

        let codenames = vec!["claude-opus".to_string()];
        let result = list_review_files_for_round_id(
            dir.path(), "20260316153045", Some("feature"), &codenames,
        );
        assert_eq!(result, vec!["20260316153045-claude-opus-feature.md"]);
    }

    #[test]
    fn list_review_files_for_round_id_no_branch_filter() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260316153045-opus-feature.md"), "").unwrap();
        fs::write(dir.path().join("20260316153045-opus-bugfix.md"), "").unwrap();
        fs::write(dir.path().join("20260316200015-opus-feature.md"), "").unwrap();

        let codenames = vec!["opus".to_string()];
        // None branch filter: return all files for the round.
        let result = list_review_files_for_round_id(
            dir.path(), "20260316153045", None, &codenames,
        );
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"20260316153045-opus-feature.md".to_string()));
        assert!(result.contains(&"20260316153045-opus-bugfix.md".to_string()));
    }

    #[test]
    fn is_review_file_structure_validates_correctly() {
        // Valid review file remainders (after round-id prefix)
        assert!(is_review_file_structure("opus-feature"));
        assert!(is_review_file_structure("opus-feature~2"));
        assert!(is_review_file_structure("claude-opus-4-6-my-branch"));
        assert!(is_review_file_structure("codex-fix_thing"));

        // Invalid: no dash (single segment, not codename-branch)
        assert!(!is_review_file_structure("notes"));
        assert!(!is_review_file_structure("scratch"));

        // Invalid: empty
        assert!(!is_review_file_structure(""));

        // Invalid: contains characters outside [a-zA-Z0-9_-~digits]
        assert!(!is_review_file_structure("opus-feature branch"));
    }
}
