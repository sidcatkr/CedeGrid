//! Subordinate user IDs (`/etc/subuid`). A rootless container's processes run under user IDs
//! from their owner's range, so each counts as that owner, one person with one share, not as
//! a person of its own (FR-059). Read only; while the file or an owner's name cannot be read,
//! a user ID stays itself, which keeps it a person.

use std::path::PathBuf;
use std::time::SystemTime;

/// The subordinate ranges, read again whenever the file changes.
#[derive(Debug)]
pub(super) struct SubordinateIds {
    path: PathBuf,
    modified: Option<SystemTime>,
    /// Each range as its first user ID, last user ID and owner's user ID.
    ranges: Vec<(u32, u32, u32)>,
}

impl SubordinateIds {
    pub(super) fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            modified: None,
            ranges: Vec::new(),
        }
    }

    /// Reads the file again when its modification time changed; an unreadable file, or an empty
    /// path, maps nothing.
    pub(super) fn refresh(&mut self) {
        if self.path.as_os_str().is_empty() {
            self.ranges.clear();
            return;
        }
        let modified = std::fs::metadata(&self.path)
            .and_then(|meta| meta.modified())
            .ok();
        if modified.is_some() && modified == self.modified {
            return;
        }
        self.modified = modified;
        self.ranges = std::fs::read_to_string(&self.path)
            .map(|text| parse(&text, user_id))
            .unwrap_or_default();
    }

    /// The user ID a process counts as: its range's owner, else itself.
    pub(super) fn owner(&self, uid: u32) -> u32 {
        self.ranges
            .iter()
            .find(|(first, last, _)| (*first..=*last).contains(&uid))
            .map_or(uid, |(_, _, owner)| *owner)
    }
}

/// `name_or_uid:first:count` lines; a line that does not parse, or whose owner has no user ID,
/// maps nothing.
fn parse(text: &str, resolve: impl Fn(&str) -> Option<u32>) -> Vec<(u32, u32, u32)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.trim().split(':');
            let (owner, first, count) = (fields.next()?, fields.next()?, fields.next()?);
            let first: u32 = first.parse().ok()?;
            let count: u32 = count.parse().ok()?;
            let last = first.checked_add(count.checked_sub(1)?)?;
            let owner = owner.parse().ok().or_else(|| resolve(owner))?;
            Some((first, last, owner))
        })
        .collect()
}

/// A user name's ID through the system's user database, directory users included.
fn user_id(name: &str) -> Option<u32> {
    #[cfg(unix)]
    {
        let name = std::ffi::CString::new(name).ok()?;
        let mut buffer = vec![0 as libc::c_char; 16 * 1024];
        // SAFETY: zeroed passwd is a valid out-parameter for getpwnam_r.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call and the buffer's length is passed with it.
        let status = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                &mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut found,
            )
        };
        (status == 0 && !found.is_null()).then_some(entry.pw_uid)
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subordinate_id_counts_as_its_owner_and_others_stay_themselves() {
        let names = |name: &str| (name == "alice").then_some(1500);
        let ranges = parse(
            "alice:100000:65536\n2000:165536:65536\nghost:231072:65536\nbroken line\n",
            names,
        );
        assert_eq!(ranges, [(100_000, 165_535, 1500), (165_536, 231_071, 2000)]);
        let ids = SubordinateIds {
            path: PathBuf::new(),
            modified: None,
            ranges,
        };
        assert_eq!(ids.owner(100_998), 1500);
        assert_eq!(ids.owner(165_536), 2000);
        // An owner without a user ID maps nothing; nor does an ID outside every range.
        assert_eq!(ids.owner(231_072), 231_072);
        assert_eq!(ids.owner(60_100), 60_100);
    }

    #[test]
    fn an_unreadable_file_maps_nothing() {
        let mut ids = SubordinateIds::at("/nonexistent/cedegrid/subuid");
        ids.refresh();
        assert_eq!(ids.owner(100_998), 100_998);
    }
}
