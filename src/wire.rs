#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    Empty,
    Absolute,
    ParentRef,
    Nul,
    TooLong,
}

/// Maximum accepted path length in bytes. Comfortably above any real path,
/// low enough that a hostile sender cannot make us allocate wildly.
const MAX_PATH_LEN: usize = 4096;

/// Split a sender-supplied relative path into components, rejecting anything
/// that could escape the destination directory at the string level.
///
/// Passing this check is necessary but NOT sufficient — see `sys::walk_dirs`,
/// which prevents escape via symlinks that these string rules cannot see.
pub fn sanitize(path: &str) -> Result<Vec<&str>, PathError> {
    if path.len() > MAX_PATH_LEN {
        return Err(PathError::TooLong);
    }
    if path.contains('\0') {
        return Err(PathError::Nul);
    }
    if path.starts_with('/') {
        return Err(PathError::Absolute);
    }

    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => continue,
            ".." => return Err(PathError::ParentRef),
            other => parts.push(other),
        }
    }

    if parts.is_empty() {
        return Err(PathError::Empty);
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_normal_nested_path() {
        assert_eq!(
            sanitize("photos/2019/img.heic"),
            Ok(vec!["photos", "2019", "img.heic"])
        );
    }

    #[test]
    fn accepts_a_bare_filename() {
        assert_eq!(sanitize("archive.dmg"), Ok(vec!["archive.dmg"]));
    }

    #[test]
    fn drops_dot_and_empty_segments() {
        assert_eq!(sanitize("a/./b"), Ok(vec!["a", "b"]));
        assert_eq!(sanitize("a//b"), Ok(vec!["a", "b"]));
    }

    #[test]
    fn rejects_absolute_paths() {
        assert_eq!(sanitize("/etc/passwd"), Err(PathError::Absolute));
    }

    #[test]
    fn rejects_parent_references_anywhere() {
        assert_eq!(sanitize("../../etc/passwd"), Err(PathError::ParentRef));
        assert_eq!(sanitize("a/../../b"), Err(PathError::ParentRef));
        assert_eq!(sanitize("a/b/.."), Err(PathError::ParentRef));
    }

    #[test]
    fn rejects_embedded_nul() {
        assert_eq!(sanitize("a\0b"), Err(PathError::Nul));
    }

    #[test]
    fn rejects_paths_that_resolve_to_nothing() {
        assert_eq!(sanitize(""), Err(PathError::Empty));
        assert_eq!(sanitize("."), Err(PathError::Empty));
        assert_eq!(sanitize("./"), Err(PathError::Empty));
    }

    #[test]
    fn rejects_overlong_paths() {
        let long = "a/".repeat(3000);
        assert_eq!(sanitize(&long), Err(PathError::TooLong));
    }
}
