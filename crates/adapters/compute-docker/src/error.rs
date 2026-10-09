use std::path::PathBuf;

use gfs_domain::ports::compute::ComputeError;

fn is_connection_error(err: &bollard::errors::Error) -> bool {
    if let bollard::errors::Error::IOError { err: io_err } = err {
        let kind = io_err.kind();
        if matches!(
            kind,
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::PermissionDenied
        ) {
            return true;
        }
    }
    err.to_string().to_ascii_lowercase().contains("connect")
}

/// Classify a bollard error into the appropriate [`ComputeError`] variant.
///
/// Bollard surfaces Docker daemon errors as [`bollard::errors::Error::DockerResponseServerError`]
/// with an HTTP status code and a message string. We inspect both the status and
/// the message body to produce the most specific `ComputeError`.
pub(crate) fn classify(container_id: &str, err: bollard::errors::Error) -> ComputeError {
    if is_connection_error(&err) {
        return ComputeError::NotAvailable(crate::DockerCompute::format_connection_error(&err));
    }
    classify_with_mount_path(container_id, err, None)
}

/// Classify a bollard error with optional mount path context for better error messages.
pub(crate) fn classify_with_mount_path(
    container_id: &str,
    err: bollard::errors::Error,
    mount_path: Option<PathBuf>,
) -> ComputeError {
    match &err {
        bollard::errors::Error::DockerResponseServerError {
            status_code,
            message,
        } => {
            let msg = message.to_ascii_lowercase();

            // Check for mount-related errors
            if msg.contains("invalid mount config")
                || msg.contains("mount denied")
                // Docker Desktop writes "Mounts denied:" -- plural. "mount denied"
                // is not a substring of it, so without this line the macOS case,
                // which is the common one, fell through to a bare Internal error.
                || msg.contains("mounts denied")
                || msg.contains("not shared from the host")
                || msg.contains("cannot mount")
                || msg.contains("invalid volume specification")
                || msg.contains("invalid mode")
                || msg.contains("invalid bind mount")
                || msg.contains("invalid mount")
            {
                let path = mount_path.unwrap_or_else(|| {
                    // Try to extract path from error message
                    extract_path_from_error(message).unwrap_or_else(|| PathBuf::from("unknown"))
                });
                return ComputeError::docker_mount_failed(path, message.clone());
            }

            match status_code {
                404 => ComputeError::NotFound(if container_id.is_empty() {
                    message.clone()
                } else {
                    container_id.to_owned()
                }),
                409 => {
                    if msg.contains("already started")
                        || msg.contains("is already running")
                        || msg.contains("container already running")
                    {
                        ComputeError::AlreadyRunning(container_id.to_owned())
                    } else if msg.contains("is not running") || msg.contains("not running") {
                        ComputeError::NotRunning(container_id.to_owned())
                    } else if msg.contains("already paused") {
                        ComputeError::AlreadyPaused(container_id.to_owned())
                    } else if msg.contains("is not paused") || msg.contains("not paused") {
                        ComputeError::NotPaused(container_id.to_owned())
                    } else {
                        ComputeError::Internal(message.clone())
                    }
                }
                _ => {
                    // Rootless Podman / cgroup v1 hosts cannot freeze container
                    // processes.  The daemon surfaces this as a 500 with a message
                    // that is semantically about *pausing* being unsupported.
                    //
                    // We require the message to be explicitly about pause/freeze
                    // to avoid false-positives from unrelated 500 errors that
                    // happen to mention "cgroup" or "not supported" in other contexts
                    // (e.g. "cgroup memory limit exceeded", "network feature not supported").
                    let is_about_pause =
                        msg.contains("pause") || msg.contains("freeze") || msg.contains("freezing");
                    let is_unsupported_reason = msg.contains("cgroup v1")
                        || msg.contains("rootless")
                        || msg.contains("pause is not")
                        || msg.contains("cannot pause")
                        || (msg.contains("cgroup")
                            && (msg.contains("freeze") || msg.contains("pause")))
                        || (msg.contains("not supported") && is_about_pause);
                    if is_about_pause && is_unsupported_reason {
                        ComputeError::PauseUnsupported(message.clone())
                    } else {
                        ComputeError::Internal(message.clone())
                    }
                }
            }
        }
        bollard::errors::Error::IOError { err } => ComputeError::Internal(err.to_string()),
        other => ComputeError::Internal(other.to_string()),
    }
}

/// Try to extract a file path from a Docker error message.
fn extract_path_from_error(message: &str) -> Option<PathBuf> {
    // Common patterns in Docker mount error messages:
    // - "invalid mount config for type 'bind': source path '/path/to/dir' must be a directory"
    // - "mount denied: the path /path/to/dir is not shared"
    // - "invalid volume specification: '/path/to/dir:/container/path'"

    // Look for paths in quotes or after common keywords
    let patterns = [
        ("source path '", "'"),
        ("source path \"", "\""),
        ("the path ", " "),
        ("path '", "'"),
        ("path \"", "\""),
        ("'", "'"),
        ("\"", "\""),
    ];

    // Matched case-insensitively: Docker Desktop capitalises ("The path ...")
    // while the daemon's own messages do not, and a lowercase-only search
    // silently yields "unknown" for the Desktop wording.
    let haystack = message.to_ascii_lowercase();
    for (start, end) in patterns {
        if let Some(start_idx) = haystack.find(start) {
            let path_start = start_idx + start.len();
            // Slice the ORIGINAL, not the lowercased copy: a path is
            // case-sensitive and /Users/Foo must not come back as /users/foo.
            // ASCII-lowercasing preserves byte offsets, so the indices align.
            let remaining = &message[path_start..];
            if let Some(end_idx) = remaining.to_ascii_lowercase().find(end) {
                let path_str = &remaining[..end_idx];
                if !path_str.is_empty() && (path_str.starts_with('/') || path_str.contains(':')) {
                    return Some(PathBuf::from(path_str));
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn docker_err(status_code: u16, message: impl Into<String>) -> bollard::errors::Error {
        bollard::errors::Error::DockerResponseServerError {
            status_code,
            message: message.into(),
        }
    }

    #[test]
    fn classify_404_with_container_id() {
        let err = classify("cid-123", docker_err(404, "not found"));
        assert!(matches!(err, ComputeError::NotFound(s) if s == "cid-123"));
    }

    #[test]
    fn classify_404_empty_container_id_uses_message() {
        let err = classify("", docker_err(404, "No such container"));
        assert!(matches!(err, ComputeError::NotFound(s) if s == "No such container"));
    }

    #[test]
    fn classify_409_already_running() {
        let err = classify("c1", docker_err(409, "Container is already running"));
        assert!(matches!(err, ComputeError::AlreadyRunning(s) if s == "c1"));
    }

    #[test]
    fn classify_409_not_running() {
        let err = classify("c1", docker_err(409, "Container is not running"));
        assert!(matches!(err, ComputeError::NotRunning(s) if s == "c1"));
    }

    #[test]
    fn classify_409_already_paused() {
        let err = classify("c1", docker_err(409, "Container already paused"));
        assert!(matches!(err, ComputeError::AlreadyPaused(s) if s == "c1"));
    }

    #[test]
    fn classify_409_not_paused() {
        let err = classify("c1", docker_err(409, "Container is not paused"));
        assert!(matches!(err, ComputeError::NotPaused(s) if s == "c1"));
    }

    #[test]
    fn classify_409_fallback_to_internal() {
        let err = classify("c1", docker_err(409, "Some other conflict"));
        assert!(matches!(err, ComputeError::Internal(_)));
    }

    #[test]
    fn classify_500_internal() {
        let err = classify("c1", docker_err(500, "Server error"));
        assert!(matches!(err, ComputeError::Internal(s) if s == "Server error"));
    }

    #[test]
    fn classify_500_cgroup_v1_freeze_is_pause_unsupported() {
        let err = classify(
            "c1",
            docker_err(
                500,
                "OCI: cgroup v1 does not support freezing a single process",
            ),
        );
        assert!(matches!(err, ComputeError::PauseUnsupported(_)));
    }

    #[test]
    fn classify_500_pause_not_supported_rootless_is_pause_unsupported() {
        let err = classify("c1", docker_err(500, "pause is not supported on rootless"));
        assert!(matches!(err, ComputeError::PauseUnsupported(_)));
    }

    #[test]
    fn classify_500_unrelated_cgroup_error_is_internal() {
        let err = classify("c1", docker_err(500, "cgroup memory limit exceeded"));
        assert!(
            matches!(err, ComputeError::Internal(_)),
            "unrelated cgroup error must not be classified as PauseUnsupported"
        );
    }

    #[test]
    fn classify_500_unrelated_not_supported_is_internal() {
        let err = classify("c1", docker_err(500, "network feature not supported"));
        assert!(
            matches!(err, ComputeError::Internal(_)),
            "unrelated 'not supported' message must not be classified as PauseUnsupported"
        );
    }

    #[test]
    fn classify_io_error_connection_refused_is_not_available() {
        let err = classify(
            "c1",
            bollard::errors::Error::IOError {
                err: io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused"),
            },
        );
        assert!(matches!(err, ComputeError::NotAvailable(_)));
    }

    #[test]
    fn classify_io_error_permission_denied_is_not_available() {
        let err = classify(
            "c1",
            bollard::errors::Error::IOError {
                err: io::Error::new(io::ErrorKind::PermissionDenied, "permission denied"),
            },
        );
        assert!(matches!(err, ComputeError::NotAvailable(_)));
    }

    #[test]
    fn classify_io_error_other_is_internal() {
        let err = classify(
            "c1",
            bollard::errors::Error::IOError {
                err: io::Error::new(io::ErrorKind::TimedOut, "timed out"),
            },
        );
        assert!(matches!(err, ComputeError::Internal(_)));
    }

    #[test]
    fn classify_mount_error_with_path() {
        let path = PathBuf::from("/tmp/test");
        let err = classify_with_mount_path(
            "c1",
            docker_err(
                500,
                "invalid mount config for type 'bind': source path '/tmp/test' must be a directory",
            ),
            Some(path.clone()),
        );
        match err {
            ComputeError::DockerMountFailed {
                path: err_path,
                reason,
                suggestion,
            } => {
                assert_eq!(err_path, path);
                assert!(reason.contains("invalid mount config"));
                assert!(suggestion.contains("Solutions:"));
                assert!(suggestion.contains("--output-dir .gfs/exports"));
            }
            _ => panic!("Expected DockerMountFailed, got {:?}", err),
        }
    }

    #[test]
    fn classify_mount_error_extract_path() {
        let err = classify(
            "c1",
            docker_err(
                500,
                "invalid mount config for type 'bind': source path '/tmp/test' must be a directory",
            ),
        );
        match err {
            ComputeError::DockerMountFailed {
                path,
                reason,
                suggestion,
            } => {
                assert_eq!(path, PathBuf::from("/tmp/test"));
                assert!(reason.contains("invalid mount config"));
                assert!(suggestion.contains("Solutions:"));
            }
            _ => panic!("Expected DockerMountFailed, got {:?}", err),
        }
    }

    #[test]
    fn classify_mount_error_mount_denied() {
        let err = classify(
            "c1",
            docker_err(500, "mount denied: the path /tmp/test is not shared"),
        );
        match err {
            ComputeError::DockerMountFailed {
                path,
                reason,
                suggestion,
            } => {
                assert_eq!(path, PathBuf::from("/tmp/test"));
                assert!(reason.contains("mount denied"));
                assert!(suggestion.contains("Solutions:"));
            }
            _ => panic!("Expected DockerMountFailed, got {:?}", err),
        }
    }

    /// Docker Desktop's own wording, verbatim. The other tests in this module
    /// use invented strings that happen to match the condition list, so a
    /// condition list that never fires in practice still passed all of them.
    #[test]
    fn classify_mount_error_docker_desktop_macos_wording() {
        let err = classify(
            "c1",
            docker_err(
                500,
                "Mounts denied: \nThe path /tmp/gfs-export is not shared from the host \
                 and is not known to Docker.\nYou can configure shared paths from \
                 Docker -> Preferences... -> Resources -> File Sharing.",
            ),
        );
        match err {
            ComputeError::DockerMountFailed {
                path, suggestion, ..
            } => {
                assert_eq!(path, PathBuf::from("/tmp/gfs-export"));
                assert!(suggestion.contains("Solutions:"));
            }
            other => panic!("Expected DockerMountFailed, got {:?}", other),
        }
    }

    /// The marker is matched case-insensitively, but the path is sliced from
    /// the original: /Users/MAC must not come back as /users/mac, or the
    /// suggestion tells the reader to share a directory that does not exist.
    #[test]
    fn an_extracted_path_keeps_its_original_case() {
        let err = classify(
            "c1",
            docker_err(
                500,
                "Mounts denied: The path /Users/MAC/Desktop/GFS is not shared from the host.",
            ),
        );
        match err {
            ComputeError::DockerMountFailed { path, .. } => {
                assert_eq!(path, PathBuf::from("/Users/MAC/Desktop/GFS"));
            }
            other => panic!("Expected DockerMountFailed, got {:?}", other),
        }
    }

    #[test]
    fn classify_mount_error_invalid_volume() {
        let err = classify(
            "c1",
            docker_err(
                500,
                "invalid volume specification: '/tmp/test:/container/path'",
            ),
        );
        match err {
            ComputeError::DockerMountFailed { .. } => {}
            _ => panic!("Expected DockerMountFailed, got {:?}", err),
        }
    }
}
