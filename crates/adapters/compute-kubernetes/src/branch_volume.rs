//! PVC names for a branch's data volume.
//!
//! A Kubernetes PVC cannot be renamed and cannot be moved between names, so
//! "which volume holds branch X's data" has to be answered by the name itself
//! plus the record in [`gfs_domain::repo_utils::branch_volumes`]. This module
//! owns the name half.
//!
//! Two properties matter, and the obvious naming scheme has neither:
//!
//! 1. **Injective on the branch name.** A PVC name must be a DNS-1123 label-ish
//!    string, and branch names are not: `feat/thing`, `feat-thing` and
//!    `FEAT/thing` all sanitize to `feat-thing`. If the name were the sanitized
//!    slug alone, three different branches would share one volume and each
//!    checkout would silently hand the next branch the previous one's data. So
//!    the name carries a hash of the EXACT branch string; the slug is there only
//!    to make `kubectl get pvc` readable.
//!
//! 2. **Attributable to an instance.** Reclamation and the mount guard both need
//!    to answer "is this PVC mine?" from the name, so every name an instance
//!    owns starts with the instance name — see [`pvc_belongs_to_instance`].

use sha2::{Digest, Sha256};

/// Suffix every data PVC carries, whichever branch it belongs to.
const DATA_SUFFIX: &str = "-data";

/// How much of the branch slug survives into the name.
///
/// Purely cosmetic — the hash carries the identity — so it is capped to keep the
/// name short. Kubernetes allows 253 characters for a PVC name, but these names
/// also show up in error messages and `kubectl` output.
const MAX_SLUG_LEN: usize = 24;

/// Hex characters of the branch-name digest included in the PVC name.
///
/// 8 hex characters is 32 bits. These names are only ever compared within one
/// instance's own set of branches, so the collision risk is over a handful of
/// names, not a global namespace.
const HASH_LEN: usize = 8;

/// The stable, pre-branch-aware PVC name: `{instance}-data`.
///
/// Repositories created before per-branch volumes have their data here, and a
/// newly initialised repository's first branch still starts here, so this name
/// stays meaningful rather than becoming legacy.
pub fn stable_data_pvc_name(instance: &str) -> String {
    format!("{}{DATA_SUFFIX}", instance.trim())
}

/// A DNS-safe, readable fragment of a branch name.
///
/// Lowercased, non-alphanumerics collapsed to single dashes, trimmed of leading
/// and trailing dashes, truncated. Lossy by design — [`branch_data_pvc`] adds
/// the hash that makes the whole name unambiguous.
fn branch_slug(branch: &str) -> String {
    let mut slug = String::with_capacity(MAX_SLUG_LEN);
    let mut last_was_dash = true; // suppress a leading dash
    for ch in branch.trim().chars() {
        if slug.len() >= MAX_SLUG_LEN {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        // A branch name of only punctuation still needs a valid label segment;
        // the hash below distinguishes it from any other such branch.
        "branch".to_string()
    } else {
        slug
    }
}

/// The PVC holding `branch`'s live data for `instance`.
///
/// `{instance}-{slug}-{hash}-data`. The hash is over the exact branch name, so
/// two branches whose slugs collide still get different volumes — the property
/// that keeps a checkout from handing one branch another's data.
pub fn branch_data_pvc(instance: &str, branch: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(branch.trim().as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!(
        "{}-{}-{}{DATA_SUFFIX}",
        instance.trim(),
        branch_slug(branch),
        &digest[..HASH_LEN]
    )
}

/// Whether `pvc` is one of `instance`'s data volumes.
///
/// True for the stable `{instance}-data` and for any `{instance}-…-data` this
/// module would produce. Used in two places that must not be lenient:
///
/// - the mount guard, which refuses to put a volume in front of a database
///   unless it belongs to that database. The guard used to be an equality check
///   against `{instance}-data`; relaxing it to "starts with the instance name"
///   keeps the protection that matters (never mount another repository's data)
///   while allowing an instance more than one volume.
/// - credentials adoption, which infers "this snapshot came from a different
///   instance" and would otherwise read a branch PVC of the SAME instance as a
///   foreign one, because `{instance}-feat-ab12cd34-data` minus `-data` is not
///   an instance name.
pub fn pvc_belongs_to_instance(pvc: &str, instance: &str) -> bool {
    let pvc = pvc.trim();
    let instance = instance.trim();
    if instance.is_empty() || !pvc.ends_with(DATA_SUFFIX) {
        return false;
    }
    if pvc == stable_data_pvc_name(instance) {
        return true;
    }
    // `{instance}-` prefix, and something between it and `-data`. The dash is
    // required so instance `gfs-pg-1` does not claim `gfs-pg-10-data`.
    let Some(rest) = pvc.strip_prefix(instance).and_then(|r| r.strip_prefix('-')) else {
        return false;
    };
    rest.len() > DATA_SUFFIX.len() && rest.ends_with(DATA_SUFFIX)
}

/// The `host_data_dir` value that makes the adapter mount `pvc` verbatim.
///
/// The adapter reads this `pvc:` prefix in `pvc_name_for`, and
/// `provision_with_instance` uses its presence to skip creating a PVC — which is
/// exactly right here, because the volume already exists.
pub fn mount_existing_pvc(pvc: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("pvc:{}", pvc.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSTANCE: &str = "gfs-pg-1780839025190";

    #[test]
    fn branches_that_sanitize_alike_still_get_different_volumes() {
        // The defect this exists to prevent: three distinct branches sharing one
        // PVC, so a checkout hands the next branch the previous one's data.
        let a = branch_data_pvc(INSTANCE, "feat/thing");
        let b = branch_data_pvc(INSTANCE, "feat-thing");
        let c = branch_data_pvc(INSTANCE, "FEAT/thing");
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        // ...while still being readable.
        assert!(a.contains("feat-thing"), "{a}");
    }

    #[test]
    fn the_same_branch_always_gets_the_same_volume() {
        assert_eq!(
            branch_data_pvc(INSTANCE, "main"),
            branch_data_pvc(INSTANCE, " main ")
        );
    }

    #[test]
    fn names_are_valid_dns_1123_subdomains() {
        let valid = |name: &str| {
            !name.is_empty()
                && name.len() <= 253
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !name.starts_with('-')
                && !name.ends_with('-')
                && !name.contains("--")
        };
        for branch in [
            "main",
            "feat/thing",
            "FEAT/Thing",
            "release/v1.2.3",
            "wip___spaces   here",
            "----",
            "ünïcödé",
            &"x".repeat(200),
        ] {
            let name = branch_data_pvc(INSTANCE, branch);
            assert!(
                valid(&name),
                "invalid PVC name {name:?} for branch {branch:?}"
            );
        }
    }

    #[test]
    fn a_punctuation_only_branch_still_yields_distinct_names() {
        let a = branch_data_pvc(INSTANCE, "///");
        let b = branch_data_pvc(INSTANCE, "___");
        assert!(a.contains("-branch-"), "{a}");
        assert_ne!(a, b, "the hash must still separate them");
    }

    #[test]
    fn the_guard_accepts_both_the_stable_and_the_branch_volumes() {
        assert!(pvc_belongs_to_instance(
            &stable_data_pvc_name(INSTANCE),
            INSTANCE
        ));
        assert!(pvc_belongs_to_instance(
            &branch_data_pvc(INSTANCE, "feat/thing"),
            INSTANCE
        ));
    }

    #[test]
    fn the_guard_rejects_another_instances_volume() {
        // The protection that must survive generalizing the old equality check.
        assert!(!pvc_belongs_to_instance(
            &stable_data_pvc_name("gfs-pg-9999999999999"),
            INSTANCE
        ));
        assert!(!pvc_belongs_to_instance(
            &branch_data_pvc("gfs-pg-9999999999999", "main"),
            INSTANCE
        ));
    }

    #[test]
    fn the_guard_is_not_fooled_by_an_instance_name_prefix() {
        // `gfs-pg-1` must not claim `gfs-pg-10-data`: without the required dash
        // separator, every instance would own its longer-named neighbours.
        assert!(!pvc_belongs_to_instance("gfs-pg-10-data", "gfs-pg-1"));
        assert!(pvc_belongs_to_instance("gfs-pg-1-data", "gfs-pg-1"));
    }

    #[test]
    fn the_guard_rejects_non_data_and_empty_inputs() {
        assert!(!pvc_belongs_to_instance(INSTANCE, INSTANCE));
        assert!(!pvc_belongs_to_instance("-data", ""));
        assert!(!pvc_belongs_to_instance("", INSTANCE));
        // `{instance}--data` has nothing between the prefix and the suffix.
        assert!(!pvc_belongs_to_instance(
            &format!("{INSTANCE}--data"),
            INSTANCE
        ));
    }

    #[test]
    fn mount_existing_pvc_uses_the_prefix_the_adapter_reads() {
        assert_eq!(
            mount_existing_pvc(" my-pvc "),
            std::path::PathBuf::from("pvc:my-pvc")
        );
    }
}
