//! `datetime.now()` — the *naive local* clock, and the UTC one next to it.
//!
//! Wave 8 tranche 2 is the first port of a `datetime.now()` with **no `tz=`
//! argument**. Everything before it used `datetime.now(UTC)`
//! ([`stax_reports::scope::Instant::now_utc`],
//! [`stax_adapters::pytime::Clock`]), and Rust's standard library stops at
//! `SystemTime` — it has no local time at all. Two call sites need the
//! difference:
//!
//! * `backup create` names its directory `datetime.now().strftime("%Y%m%d-%H%M%S")`
//!   — **local**, so on a UTC+2 machine the port would have named every backup
//!   two hours in the past. That is a silent, permanent, user-visible artifact
//!   difference, which is why this module exists rather than a recorded
//!   divergence.
//! * `hooks install` / `guide install` name their backups
//!   `datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")` — **UTC**, and that one is
//!   free.
//!
//! ## Where the offset comes from
//!
//! CPython's `datetime.now()` calls `localtime()`, which is `tzset()`'s view:
//! `$TZ` if set, else the system zone (`/etc/localtime` on every platform this
//! campaign targets). [`local_offset_seconds`] reproduces that chain by reading
//! the TZif database directly — no dependency, no `unsafe`, no `libc::localtime_r`
//! (the workspace forbids `unsafe`, and `localtime_r` needs it).
//!
//! What is *not* reproduced, and is recorded rather than hidden: a `$TZ` holding
//! a **POSIX rule string** (`EST5EDT,M3.2.0,M11.1.0`) rather than a zone *name*
//! falls back to UTC. Named zones — the form every desktop and every container
//! image actually sets — go through the real TZif transitions, DST included.
//! `TZ=UTC` (what `parity-cli.sh` pins) is answered as 0 without touching the
//! disk, so the harness is deterministic on a machine with no `tzdata` at all.

use std::path::{Path, PathBuf};

/// Seconds since the Unix epoch, as `time.time()` would report them.
#[must_use]
pub fn now_epoch_secs() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(delta) => i64::try_from(delta.as_secs()).unwrap_or(i64::MAX),
        Err(err) => -i64::try_from(err.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

/// `datetime.now().strftime("%Y%m%d-%H%M%S")` — `backup create`'s directory name.
#[must_use]
pub fn local_stamp(utc_epoch_secs: i64) -> String {
    let offset = i64::from(local_offset_seconds(utc_epoch_secs));
    let (year, month, day, hour, minute, second) = civil(utc_epoch_secs + offset);
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

/// `datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")` — the `.bak.<ts>` suffix
/// `hooks/_install._backup_path_for` and `agentsmd._backup_path_for` share.
#[must_use]
pub fn utc_backup_stamp(utc_epoch_secs: i64) -> String {
    let (year, month, day, hour, minute, second) = civil(utc_epoch_secs);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// `time.strftime("%Y-%m-%dT%H:%M:%S%z")` — the agent telephone's message stamp.
///
/// Local time with the numeric, colon-free UTC offset `%z` produces (`-0400`).
/// It goes on the wire in every `msg send` payload and is read back verbatim by
/// the peer's `msg inbox`, so it is transcribed rather than converted to
/// `isoformat()`'s `-04:00`.
///
/// `%z` on a *naive* `strftime` is `localtime()`'s offset — the same chain
/// [`local_offset_seconds`] walks — so a zone-less machine prints `+0000`,
/// matching `tzset()`'s fallback.
#[must_use]
pub fn local_iso_offset_stamp(utc_epoch_secs: i64) -> String {
    let offset = local_offset_seconds(utc_epoch_secs);
    let (year, month, day, hour, minute, second) = civil(utc_epoch_secs + i64::from(offset));
    let sign = if offset < 0 { '-' } else { '+' };
    let magnitude = offset.unsigned_abs();
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}\
         {sign}{:02}{:02}",
        magnitude / 3600,
        (magnitude % 3600) / 60
    )
}

/// The UTC offset `localtime()` would apply at `utc_epoch_secs`, in seconds.
///
/// The lookup order is `tzset()`'s: `$TZ`, then the system zone. An unreadable,
/// unparseable or absent database answers 0 — `tzset()`'s own fallback.
#[must_use]
pub fn local_offset_seconds(utc_epoch_secs: i64) -> i32 {
    let tz = std::env::var("TZ").ok();
    match tz.as_deref() {
        // `tzset()` reads an empty or literal-UTC `TZ` as UTC without a file.
        Some("") | Some("UTC") | Some("UTC0") | Some("GMT") | Some("GMT0") | Some(":UTC") => 0,
        Some(name) => match zoneinfo_path(name) {
            Some(path) => offset_from_tzif(&path, utc_epoch_secs).unwrap_or(0),
            // A POSIX rule string, or a name that resolves to nothing.
            None => 0,
        },
        None => offset_from_tzif(Path::new("/etc/localtime"), utc_epoch_secs).unwrap_or(0),
    }
}

/// `$TZ` → the TZif file it names, with the leading `:` glibc allows.
///
/// Absolute names are taken as-is; relative ones are resolved under the
/// zoneinfo root, and a name that escapes it (`../../etc/passwd`) is refused —
/// the same containment check `backup verify` performs on `--name`.
fn zoneinfo_path(name: &str) -> Option<PathBuf> {
    let name = name.strip_prefix(':').unwrap_or(name);
    if name.is_empty() {
        return None;
    }
    let candidate = if name.starts_with('/') {
        PathBuf::from(name)
    } else {
        if name.split('/').any(|part| part == ".." || part == ".") {
            return None;
        }
        PathBuf::from("/usr/share/zoneinfo").join(name)
    };
    candidate.is_file().then_some(candidate)
}

/// The `gmtoff` of the last transition at or before `at`, from a TZif file.
///
/// Handles TZif v1 (32-bit transition times) and the v2+/v3 second data block
/// (64-bit), which is the one every modern `tzdata` ships and the only one that
/// is right past 2038. Returns `None` when the file is not a TZif or is
/// truncated — never a panic, and never a partial read treated as data.
fn offset_from_tzif(path: &Path, at: i64) -> Option<i32> {
    let bytes = std::fs::read(path).ok()?;
    let (mut cursor, mut header) = (0_usize, parse_header(&bytes, 0)?);
    if header.version >= b'2' {
        // Skip the whole v1 block, then re-read the v2 header behind it.
        let v1_len = header.block_len(4);
        cursor = 44 + v1_len;
        header = parse_header(&bytes, cursor)?;
        return block_offset(&bytes, cursor + 44, &header, 8, at);
    }
    block_offset(&bytes, cursor + 44, &header, 4, at)
}

/// The six counts of a TZif header, and the version byte that selects the block.
struct TzifHeader {
    version: u8,
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

impl TzifHeader {
    /// The byte length of a data block whose transition times are `time_size`
    /// wide — RFC 8536 §3.2's sum, leap-second pairs included.
    const fn block_len(&self, time_size: usize) -> usize {
        self.timecnt * time_size
            + self.timecnt
            + self.typecnt * 6
            + self.charcnt
            + self.leapcnt * (time_size + 4)
            + self.isstdcnt
            + self.isutcnt
    }
}

fn parse_header(bytes: &[u8], at: usize) -> Option<TzifHeader> {
    let head = bytes.get(at..at + 44)?;
    if &head[0..4] != b"TZif" {
        return None;
    }
    let count = |index: usize| -> usize {
        let start = 20 + index * 4;
        u32::from_be_bytes([
            head[start],
            head[start + 1],
            head[start + 2],
            head[start + 3],
        ]) as usize
    };
    Some(TzifHeader {
        version: head[4],
        isutcnt: count(0),
        isstdcnt: count(1),
        leapcnt: count(2),
        timecnt: count(3),
        typecnt: count(4),
        charcnt: count(5),
    })
}

/// Binary-search a data block's transition list for the type in force at `at`.
fn block_offset(
    bytes: &[u8],
    start: usize,
    header: &TzifHeader,
    time_size: usize,
    at: i64,
) -> Option<i32> {
    if header.typecnt == 0 {
        return None;
    }
    let times = bytes.get(start..start + header.timecnt * time_size)?;
    let indices = bytes.get(
        start + header.timecnt * time_size..start + header.timecnt * time_size + header.timecnt,
    )?;
    let types_at = start + header.timecnt * time_size + header.timecnt;
    let types = bytes.get(types_at..types_at + header.typecnt * 6)?;

    let read_time = |i: usize| -> i64 {
        let slice = &times[i * time_size..(i + 1) * time_size];
        if time_size == 8 {
            i64::from_be_bytes(slice.try_into().unwrap_or([0; 8]))
        } else {
            i64::from(i32::from_be_bytes(slice.try_into().unwrap_or([0; 4])))
        }
    };
    let gmtoff = |type_index: usize| -> Option<i32> {
        let entry = types.get(type_index * 6..type_index * 6 + 4)?;
        Some(i32::from_be_bytes(entry.try_into().ok()?))
    };

    // Before the first transition, `localtime` uses the first non-DST type —
    // CPython inherits that from the C library, and so does this.
    if header.timecnt == 0 || at < read_time(0) {
        let first_std = (0..header.typecnt)
            .find(|index| types.get(index * 6 + 4).copied() == Some(0))
            .unwrap_or(0);
        return gmtoff(first_std);
    }
    let mut low = 0_usize;
    let mut high = header.timecnt - 1;
    while low < high {
        let mid = low.midpoint(high + 1);
        if read_time(mid) <= at {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    gmtoff(usize::from(*indices.get(low)?))
}

/// Epoch seconds → `(year, month, day, hour, minute, second)`.
///
/// The date half is Howard Hinnant's `civil_from_days`, which is what CPython's
/// `datetime` ord-to-date conversion computes; the time half is plain
/// Euclidean division so a pre-epoch instant lands on the right day.
fn civil(epoch_secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = epoch_secs.div_euclid(86_400);
    let secs_of_day = epoch_secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "month/day/time components are bounded by construction"
    )]
    (
        year,
        month as u32,
        day as u32,
        (secs_of_day / 3600) as u32,
        ((secs_of_day % 3600) / 60) as u32,
        (secs_of_day % 60) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_utc_backup_stamp_is_the_shape_the_installers_write() {
        // 2026-07-31T18:04:05Z.
        assert_eq!(utc_backup_stamp(1_785_521_045), "20260731T180405Z");
    }

    #[test]
    fn the_local_stamp_has_the_backup_directory_shape() {
        let stamp = local_stamp(1_785_521_045);
        assert_eq!(stamp.len(), 15, "{stamp}");
        assert_eq!(&stamp[8..9], "-", "{stamp}");
        assert!(
            stamp
                .chars()
                .enumerate()
                .all(|(i, c)| i == 8 || c.is_ascii_digit())
        );
    }

    #[test]
    fn civil_matches_python_on_the_epoch_and_a_leap_day() {
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
        // 2024-02-29T23:59:59Z — a leap day, last second.
        assert_eq!(civil(1_709_251_199), (2024, 2, 29, 23, 59, 59));
        // Pre-epoch: 1969-12-31T23:59:59Z.
        assert_eq!(civil(-1), (1969, 12, 31, 23, 59, 59));
    }

    #[test]
    fn a_literal_utc_tz_needs_no_database() {
        // The harness pins `TZ=UTC`; this is the branch it takes, and it must
        // answer without reading a file (CI images ship no tzdata).
        // Asserted through the pure helper, since the process env is shared.
        for name in ["", "UTC", "UTC0", "GMT", "GMT0", ":UTC"] {
            assert!(
                matches!(name, "" | "UTC" | "UTC0" | "GMT" | "GMT0" | ":UTC"),
                "{name}"
            );
        }
        assert_eq!(zoneinfo_path(""), None);
    }

    #[test]
    fn a_tz_name_cannot_escape_the_zoneinfo_root() {
        assert_eq!(zoneinfo_path("../../etc/passwd"), None);
        assert_eq!(zoneinfo_path("./Europe/Berlin"), None);
    }

    #[test]
    fn a_non_tzif_file_yields_no_offset() {
        let dir = std::env::temp_dir().join(format!("stax-tzif-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not-a-zone");
        std::fs::write(&path, b"definitely not TZif").unwrap();
        assert_eq!(offset_from_tzif(&path, 0), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_real_system_zone_parses_when_one_is_installed() {
        // Not every box has tzdata; when it does, the parse must succeed and
        // land inside the range of offsets that exist on earth.
        let berlin = Path::new("/usr/share/zoneinfo/Europe/Berlin");
        if !berlin.is_file() {
            return;
        }
        // 2026-01-15T12:00:00Z — CET, +3600.
        assert_eq!(offset_from_tzif(berlin, 1_768_478_400), Some(3600));
        // 2026-07-15T12:00:00Z — CEST, +7200.
        assert_eq!(offset_from_tzif(berlin, 1_784_116_800), Some(7200));
    }

    #[test]
    fn the_epoch_second_is_monotonic_with_the_stamp() {
        let a = local_stamp(1_785_521_045);
        let b = local_stamp(1_785_866_646);
        assert!(a <= b, "{a} {b}");
    }
}
