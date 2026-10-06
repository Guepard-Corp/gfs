//! What a branch may be called.
//!
//! A branch name becomes a path: `refs/heads/<name>`. Unchecked, that is a
//! filesystem write with a caller-supplied path in it — `gfs branch
//! ../../../../../escape` created a file outside the repository, invisible to
//! `gfs branch` afterwards.
//!
//! Environments already had a rule, in `cmd_env`: a single path segment, no
//! slashes, because an environment IS a directory name. Branches cannot borrow
//! it — `feat/x` is legitimate and supported the whole way down, since
//! `refs/heads/feat/x` is a real nested path and the Kubernetes volume name
//! hashes the exact branch string, so nesting stays unambiguous.
//!
//! So the rule here is closer to git's `check-ref-format`: interior slashes are
//! fine, and everything that would make the name escape, vanish, or corrupt the
//! output is not.
//!
//! It lives in the domain rather than the CLI because it protects the ref store,
//! and the CLI is only one of the ways in.

use crate::model::errors::RepoError;
use crate::model::layout::DETACHED_WORKSPACE_SEGMENT;

/// Characters git refuses in a ref name, and so do we.
///
/// `~^:?*[` and `\` are refspec and glob syntax; a space makes a name that
/// cannot be passed back to the tool that printed it.
const FORBIDDEN: &[char] = &['~', '^', ':', '?', '*', '[', '\\', ' '];

/// Check a branch name, or say why it cannot be used.
///
/// The message names the actual reason. Before this existed, `gfs branch ""`,
/// `gfs branch .` and `gfs branch ..` all failed with "branch '<x>' already
/// exists" — true of the directory being described, and meaningless to a reader
/// who had not created anything.
pub fn validate_branch_name(name: &str) -> Result<(), RepoError> {
    let invalid = |why: &str| {
        Err(RepoError::InvalidName(format!(
            "invalid branch name: {why}"
        )))
    };

    if name.is_empty() {
        return invalid("it is empty");
    }
    // Trailing whitespace survives into the ref file and makes two branches that
    // look identical in a listing.
    if name != name.trim() {
        return invalid("it has leading or trailing whitespace");
    }
    if let Some(bad) = name.chars().find(|c| c.is_ascii_control()) {
        return invalid(&format!(
            "it contains a control character ({:#04x}); that would break the branch listing",
            bad as u32
        ));
    }
    if let Some(bad) = name.chars().find(|c| FORBIDDEN.contains(c)) {
        return invalid(&format!("it contains '{bad}'"));
    }
    if name.starts_with('-') {
        return invalid("it starts with '-', which makes it unusable as a command argument");
    }
    if name.starts_with('/') || name.ends_with('/') {
        return invalid("it starts or ends with '/'");
    }
    if name.contains("//") {
        return invalid("it contains an empty path segment ('//')");
    }
    if name.ends_with(".lock") {
        return invalid("it ends with '.lock', which is reserved");
    }
    // `detached` names the layout segment that holds every detached working copy,
    // and a branch's workspace is `workspaces/<branch>` -- so a branch of this
    // name owns the namespace root rather than a directory inside it. Deleting
    // such a branch removed `workspaces/detached` entirely, taking every detached
    // working copy and any uncommitted work in them, silently, at exit 0.
    //
    // Refused at creation rather than only guarded at deletion, because the
    // collision is what makes the deletion dangerous. The guard exists too, for a
    // repository that already carries such a branch.
    if name == DETACHED_WORKSPACE_SEGMENT {
        return invalid(
            "'detached' is reserved: it names the directory holding detached working copies, \
             so a branch of that name would own the whole namespace",
        );
    }
    // The escape. Each segment is a directory name under refs/heads, so a `..`
    // segment walks out of the repository.
    for segment in name.split('/') {
        if segment == "." || segment == ".." {
            return invalid(
                "it contains a '.' or '..' path segment, which would point outside the \
                 repository's refs",
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod reserved_name_tests {
    /// Both validators must refuse the reserved name, and this test exists because
    /// one of them did not.
    ///
    /// There are two: `branch_name::validate_branch_name`, which `gfs branch`
    /// reaches, and `repo_layout::validate_branch_name`, which a filesystem
    /// `gfs checkout -b` reaches instead. Adding the rule to the first left
    /// `checkout -b detached` creating the branch anyway -- the fix looked complete
    /// and the defect was still reachable by one of its two doors. Asserting
    /// against both is what makes the pair impossible to half-fix again.
    #[test]
    fn a_reserved_name_is_refused_by_both_validators() {
        let reserved = crate::model::layout::DETACHED_WORKSPACE_SEGMENT;

        assert!(
            super::validate_branch_name(reserved).is_err(),
            "branch_name:: must refuse '{reserved}'"
        );
        assert!(
            crate::repo_utils::repo_layout::validate_branch_name(reserved).is_err(),
            "repo_layout:: must refuse '{reserved}' -- this is the validator a \
             filesystem `checkout -b` reaches, and it is the one that was missed"
        );

        // Controls. Without these, a validator that refused everything would pass
        // the assertions above.
        for ok in [
            "main",
            "feature/detached-thing",
            "detached-work",
            "my-detached",
        ] {
            assert!(
                super::validate_branch_name(ok).is_ok(),
                "branch_name:: must still accept '{ok}'"
            );
            assert!(
                crate::repo_utils::repo_layout::validate_branch_name(ok).is_ok(),
                "repo_layout:: must still accept '{ok}'"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(name: &str) -> String {
        match validate_branch_name(name) {
            Ok(()) => panic!("expected '{name}' to be rejected, but it was accepted"),
            Err(e) => e.to_string(),
        }
    }

    /// The finding this exists for: a name that walks out of the repository.
    #[test]
    fn a_name_that_escapes_the_repository_is_refused() {
        let why = rejected("../../../../../gfs-escape-test");
        assert!(
            why.contains("outside the repository"),
            "the message must say what it prevents: {why}"
        );
        rejected("..");
        rejected("a/../../b");
        rejected("./x");
    }

    /// These used to fail with "branch '<x>' already exists", which describes the
    /// directory and tells the reader nothing.
    #[test]
    fn empty_and_dot_names_say_what_is_actually_wrong() {
        assert!(rejected("").contains("empty"));
        assert!(rejected(".").contains("'.' or '..'"));
        for name in ["", ".", ".."] {
            assert!(
                !rejected(name).contains("already exists"),
                "'{name}' must not be explained as an existing branch"
            );
        }
    }

    /// A newline was accepted and broke the plain-text listing.
    #[test]
    fn control_characters_are_refused() {
        assert!(rejected("feat\nx").contains("control character"));
        rejected("feat\tx");
        rejected("feat\r\nx");
    }

    #[test]
    fn names_that_cannot_be_passed_back_are_refused() {
        assert!(rejected("-x").contains("command argument"));
        rejected("a b");
        rejected("a:b");
        rejected("a?b");
        rejected("a*b");
        rejected("a[b");
        rejected("a^b");
        rejected("a~b");
        rejected("a\\b");
        rejected("/leading");
        rejected("trailing/");
        rejected("a//b");
        rejected("x.lock");
        rejected(" padded");
        rejected("padded ");
    }

    /// The half that stops this from being a rule that refuses everything. A
    /// one-directional test would pass a validator that rejected all input.
    #[test]
    fn legitimate_names_are_accepted() {
        for name in [
            "main",
            "feat/thing",
            "feat/sub/deep",
            "release-1.2.3",
            "FEAT/Thing",
            "fix.bug",
            "a",
            "ünïcødé",
            "2026-09-14",
            "user@host",
        ] {
            validate_branch_name(name)
                .unwrap_or_else(|e| panic!("'{name}' should be allowed, got: {e}"));
        }
    }
}
