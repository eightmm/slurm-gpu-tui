use crate::{Result, job_view, runtime};
use serde::{Deserialize, Serialize};
use std::{
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub version: u32,
    pub tab: usize,
    pub sort: usize,
    pub reverse: bool,
    pub job_sort: job_view::Sort,
    pub job_reverse: bool,
    pub job_filter: job_view::Filter,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            version: 1,
            tab: 0,
            sort: 0,
            reverse: false,
            job_sort: job_view::Sort::default(),
            job_reverse: false,
            job_filter: job_view::Filter::default(),
        }
    }
}

impl Preferences {
    fn validate(&self) -> Result<()> {
        if self.version != 1 || self.tab >= 8 || self.sort >= 4 {
            return Err("unsupported or invalid view settings".into());
        }
        Ok(())
    }
}

pub fn path() -> PathBuf {
    runtime::home().join(".sgpu/ui/preferences.json")
}

pub fn load(path: &Path) -> Result<Preferences> {
    let (bytes, metadata) = match runtime::read_regular(path, 16 * 1024, Some(runtime::euid())) {
        Ok(value) => value,
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
        {
            return Ok(Preferences::default());
        }
        Err(error) => return Err(error),
    };
    if metadata.mode() & 0o022 != 0 {
        return Err("view settings must not be writable by other accounts".into());
    }
    let preferences: Preferences = serde_json::from_slice(&bytes)?;
    preferences.validate()?;
    Ok(preferences)
}

pub fn save(path: &Path, preferences: &Preferences) -> Result<()> {
    preferences.validate()?;
    runtime::secure_dir(
        path.parent().ok_or("missing view settings directory")?,
        0o700,
    )?;
    runtime::atomic_write(path, &serde_json::to_vec_pretty(preferences)?, 0o600, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    #[test]
    fn settings_round_trip_in_a_private_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ui/preferences.json");
        assert_eq!(load(&path).unwrap(), Preferences::default());
        let preferences = Preferences {
            tab: 3,
            sort: 2,
            reverse: true,
            job_sort: job_view::Sort::Wait,
            job_reverse: true,
            job_filter: job_view::Filter::Pending,
            ..Default::default()
        };
        save(&path, &preferences).unwrap();
        assert_eq!(load(&path).unwrap(), preferences);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        save(
            &path,
            &Preferences {
                tab: 7,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(load(&path).unwrap().tab, 7);
        save(&path, &Preferences::default()).unwrap();
        assert_eq!(load(&path).unwrap(), Preferences::default());
    }

    #[test]
    fn invalid_future_oversized_and_unsafe_settings_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("preferences.json");
        for bytes in [
            r#"{"version":2}"#,
            r#"{"tab":8}"#,
            r#"{"sort":4}"#,
            r#"{"job_sort":"unknown"}"#,
            "[",
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(load(&path).is_err(), "{bytes}");
        }
        fs::write(&path, vec![b' '; 16 * 1024 + 1]).unwrap();
        assert!(load(&path).is_err());
        fs::write(&path, "{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(load(&path).is_err());
        let link = tmp.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(load(&link).is_err());
        let directory_link = tmp.path().join("ui-link");
        symlink(tmp.path(), &directory_link).unwrap();
        assert!(save(&directory_link.join("other.json"), &Preferences::default()).is_err());
    }
}
