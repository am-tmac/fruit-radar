//! Query-only persistent profile ownership. Never inspect personal browser data.
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::Path;

#[derive(Debug)]
pub(crate) struct ProfileOwner {
    file: File,
}
impl ProfileOwner {
    pub(crate) fn claim(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(dir.join(".fruit-radar-query-owner"))?;
        #[cfg(unix)]
        {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        #[cfg(not(unix))]
        {
            file.try_lock()?;
        }
        Ok(Self { file })
    }
}
impl Drop for ProfileOwner {
    fn drop(&mut self) {
        // Explicit unlock: child spawn can briefly inherit an open-file description.
        #[cfg(unix)]
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
        #[cfg(not(unix))]
        {
            let _ = self.file.unlock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exclusive_owner_releases_even_with_an_inherited_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let owner = ProfileOwner::claim(dir.path()).unwrap();
        let inherited = owner.file.try_clone().unwrap();
        assert!(ProfileOwner::claim(dir.path()).is_err());
        drop(owner);
        let next = ProfileOwner::claim(dir.path()).unwrap();
        drop(inherited);
        assert!(ProfileOwner::claim(dir.path()).is_err());
        drop(next);
        assert!(ProfileOwner::claim(dir.path()).is_ok());
    }
    #[test]
    fn owner_preserves_cookies_and_unknown_old_data() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("query-only");
        std::fs::create_dir_all(&profile).unwrap();
        let cookie = profile.join("Cookies");
        std::fs::write(&cookie, b"offline fixture").unwrap();
        let old = root.path().join("unknown-old-profile");
        std::fs::create_dir(&old).unwrap();
        drop(ProfileOwner::claim(&profile).unwrap());
        drop(ProfileOwner::claim(&profile).unwrap());
        assert_eq!(std::fs::read(cookie).unwrap(), b"offline fixture");
        assert!(old.is_dir());
    }
}
