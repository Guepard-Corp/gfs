//! Helpers for current user identification.

#[cfg(unix)]
use std::process::Command;

#[cfg(unix)]
fn id_value(flag: &str) -> Option<String> {
    let output = Command::new("id").args([flag]).output().ok()?.stdout;
    let value = String::from_utf8_lossy(&output).trim().to_string();
    if value.is_empty() {
        return None;
    }
    Some(value)
}

/// Return current Unix uid and gid as "uid:gid" for `docker run --user`.
/// Returns `None` if id cannot be determined (e.g. on Windows or when `id` is unavailable).
///
/// `GFS_CONTAINER_UID_GID` overrides the detected value: set it to a `uid:gid`
/// pair to force that user, or to an empty string to run the container as the
/// image default. This is an escape hatch for runtimes whose bind mounts are not
/// owned by the host uid — e.g. Colima/Lima `virtiofs` presents them as
/// root-owned, so pinning the container to the host uid makes `initdb`'s chmod
/// fail; running as the image default (which chmods as root, then drops
/// privileges) works there, as it already does on Kubernetes.
#[cfg(unix)]
pub fn current_user_uid_gid() -> Option<String> {
    if let Ok(override_value) = std::env::var("GFS_CONTAINER_UID_GID") {
        let trimmed = override_value.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_string());
    }
    let uid = id_value("-u")?;
    let gid = id_value("-g")?;
    Some(format!("{uid}:{gid}"))
}

#[cfg(unix)]
pub fn current_user_name() -> Option<String> {
    id_value("-un")
}

#[cfg(not(unix))]
pub fn current_user_uid_gid() -> Option<String> {
    None
}

#[cfg(not(unix))]
pub fn current_user_name() -> Option<String> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::current_user_uid_gid;

    /// `GFS_CONTAINER_UID_GID` overrides the detected uid:gid — a pair forces that
    /// user, a blank value runs as the image default, and its absence falls back to
    /// the host uid:gid. Serialised in one test since the environment is process-wide.
    #[test]
    fn container_uid_gid_override_forces_value_blank_or_falls_back() {
        // SAFETY: single-threaded test; nothing else reads the environment concurrently.
        unsafe { std::env::set_var("GFS_CONTAINER_UID_GID", "1234:5678") };
        assert_eq!(current_user_uid_gid().as_deref(), Some("1234:5678"));

        unsafe { std::env::set_var("GFS_CONTAINER_UID_GID", "   ") };
        assert_eq!(
            current_user_uid_gid(),
            None,
            "a blank override runs the container as the image default"
        );

        unsafe { std::env::remove_var("GFS_CONTAINER_UID_GID") };
        assert!(
            current_user_uid_gid().is_some_and(|v| v.contains(':')),
            "without the override the detected host uid:gid is used"
        );
    }
}
