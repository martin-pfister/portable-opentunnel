//! The documented on-disk layout of one state directory.
//!
//! ```text
//! <state>/
//!   .lock                     wrapper lock, held for every modifying operation
//!   wrapper.json              wrapper metadata (format, owner, mode, API, route)
//!   wrapper.json.tmp-<pid>-<rand>   residue of an interrupted metadata write
//!   sdk/                      Storage root handed to the SDK
//!     default/                the one fixed profile this program uses
//!       token                 \
//!       private-key.pem        |
//!       certificate.pem        |  SDK final files
//!       chain.pem              |
//!       pending.json           |
//!       tunnel.json           /
//!       <stem>.tmp-<pid>      residue of an interrupted SDK atomic write
//! ```
//!
//! `Storage::at(root)` and `write_secret` build temporary names with
//! `Path::with_extension`, so `tunnel.json` becomes `tunnel.tmp-<pid>` and
//! `private-key.pem` becomes `private-key.tmp-<pid>`. Those names are
//! recognized here as *owned residue* only: a matching name never proves that
//! the writing process is gone, so residue is discarded only while holding
//! the state lock.

use std::path::Path;

pub const WRAPPER_FILE: &str = "wrapper.json";
pub const LOCK_FILE: &str = ".lock";
pub const SDK_DIR: &str = "sdk";
pub const PROFILE: &str = "default";

pub const TOKEN: &str = "token";
pub const PRIVATE_KEY: &str = "private-key.pem";
pub const CERTIFICATE: &str = "certificate.pem";
pub const CHAIN: &str = "chain.pem";
pub const PENDING: &str = "pending.json";
pub const TUNNEL: &str = "tunnel.json";

/// Final files the pinned SDK profile may contain.
pub const PROFILE_FINAL: &[&str] = &[TOKEN, PRIVATE_KEY, CERTIFICATE, CHAIN, PENDING, TUNNEL];

/// Stems the pinned SDK's `write_secret` derives temporary names from.
const PROFILE_TEMP_STEMS: &[&str] = &[
    TOKEN,
    "tunnel",
    "pending",
    "private-key",
    "certificate",
    "chain",
];

const WRAPPER_TEMP_PREFIX: &str = "wrapper.json.tmp-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootEntry {
    Lock,
    Wrapper,
    WrapperTemp,
    SdkDir,
    Unknown,
}

pub fn root_entry(name: &str) -> RootEntry {
    if name == LOCK_FILE {
        RootEntry::Lock
    } else if name == WRAPPER_FILE {
        RootEntry::Wrapper
    } else if is_wrapper_temp(name) {
        RootEntry::WrapperTemp
    } else if name == SDK_DIR {
        RootEntry::SdkDir
    } else {
        RootEntry::Unknown
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileEntry {
    Final,
    Temp,
    Unknown,
}

pub fn profile_entry(name: &str) -> ProfileEntry {
    if PROFILE_FINAL.contains(&name) {
        ProfileEntry::Final
    } else if is_profile_temp(name) {
        ProfileEntry::Temp
    } else {
        ProfileEntry::Unknown
    }
}

/// `wrapper.json.tmp-<pid>-<random>`, the one temporary name this program
/// creates for its own atomic metadata write.
pub fn is_wrapper_temp(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(WRAPPER_TEMP_PREFIX) else {
        return false;
    };
    let Some((pid, random)) = rest.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && !random.is_empty()
        && random
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

/// `<stem>.tmp-<pid>` for one of the pinned SDK's `write_secret` stems.
pub fn is_profile_temp(name: &str) -> bool {
    for stem in PROFILE_TEMP_STEMS {
        let Some(rest) = name.strip_prefix(stem) else {
            continue;
        };
        let Some(pid) = rest.strip_prefix(".tmp-") else {
            continue;
        };
        if !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()) {
            return true;
        }
    }
    false
}

/// The SDK profile directory inside a state root.
pub fn profile_dir(root: &Path) -> std::path::PathBuf {
    root.join(SDK_DIR).join(PROFILE)
}

/// The SDK storage root inside a state root.
pub fn sdk_root(root: &Path) -> std::path::PathBuf {
    root.join(SDK_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_the_pinned_sdk_temporary_names() {
        for name in [
            "token.tmp-1",
            "tunnel.tmp-4242",
            "pending.tmp-7",
            "private-key.tmp-99",
            "certificate.tmp-99",
            "chain.tmp-99",
        ] {
            assert!(is_profile_temp(name), "{name} should be SDK residue");
        }
    }

    #[test]
    fn rejects_near_miss_temporary_names() {
        for name in [
            "token.tmp-",
            "token.tmp-12a",
            "token.tmp-1.pem",
            "token.tmp-1/",
            "toke.tmp-1",
            "tokenTMP-1",
            "tunnel.json.tmp-1",
            "private-key.pem.tmp-1",
            "token.tmp-1-2",
        ] {
            assert!(!is_profile_temp(name), "{name} must not be SDK residue");
        }
        assert_eq!(profile_entry("token"), ProfileEntry::Final);
        assert_eq!(profile_entry("tunnel.json"), ProfileEntry::Final);
        assert_eq!(profile_entry("notes.txt"), ProfileEntry::Unknown);
    }

    #[test]
    fn recognizes_wrapper_temporary_names() {
        assert!(is_wrapper_temp("wrapper.json.tmp-123-abcdef"));
        assert!(is_wrapper_temp("wrapper.json.tmp-1-0"));
        for name in [
            "wrapper.json.tmp-abc-1",
            "wrapper.json.tmp-1-ABC",
            "wrapper.json.tmp-1-",
            "wrapper.json.tmp-1",
            "wrapper.json",
            ".wrapper.json.tmp-1-a",
        ] {
            assert!(!is_wrapper_temp(name), "{name} must not be wrapper residue");
        }
    }

    #[test]
    fn classifies_root_entries() {
        assert_eq!(root_entry(".lock"), RootEntry::Lock);
        assert_eq!(root_entry("wrapper.json"), RootEntry::Wrapper);
        assert_eq!(root_entry("sdk"), RootEntry::SdkDir);
        assert_eq!(root_entry("wrapper.json.tmp-9-abc"), RootEntry::WrapperTemp);
        assert_eq!(root_entry(".portable-opentunnel"), RootEntry::Unknown);
        assert_eq!(root_entry("token"), RootEntry::Unknown);
    }
}
