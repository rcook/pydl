//! Maps the host platform onto the `python-build-standalone` target-triple
//! strings and tests whether a given asset triple is a match.
//!
//! Only baseline architectures are matched. For `x86_64` on Linux the upstream
//! publishes `x86_64`, `x86_64_v2`, `x86_64_v3` and `x86_64_v4` variants;
//! without runtime CPU feature detection we can't know which the user's CPU
//! supports, so we conservatively only accept the baseline `x86_64`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Darwin,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    I686,
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
}

/// The subset of an asset's attributes that define the "default" build.
///
/// For a given host: the flavour (always `install_only` today) plus, when the
/// platform has multiple equally-valid target triples, a preferred suffix to
/// pick one of them. `None` for `triple_suffix` means any triple the platform
/// filter accepts is fine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultAttrs {
    pub flavour: &'static str,
    pub triple_suffix: Option<&'static str>,
}

impl Platform {
    /// Detect the current host platform. Returns `None` if the host OS or
    /// architecture is outside the set the upstream actually publishes for.
    #[must_use]
    pub fn current() -> Option<Self> {
        let os = match std::env::consts::OS {
            "linux" => Os::Linux,
            "macos" => Os::Darwin,
            "windows" => Os::Windows,
            _ => return None,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => Arch::X86_64,
            "x86" => Arch::I686,
            "aarch64" | "arm64" => Arch::Aarch64,
            _ => return None,
        };
        Some(Self { os, arch })
    }

    /// The default redistributable attributes to pick for this platform when
    /// `--default-attrs` is on.
    #[must_use]
    pub const fn default_attrs(self) -> DefaultAttrs {
        match self.os {
            Os::Linux => DefaultAttrs {
                flavour: "install_only",
                // Prefer glibc over musl — musl builds are statically linked
                // and larger; `install_only` on glibc is the closest analogue
                // to an upstream python.org build.
                triple_suffix: Some("-unknown-linux-gnu"),
            },
            Os::Darwin => DefaultAttrs {
                flavour: "install_only",
                // Only one darwin triple per arch exists upstream.
                triple_suffix: None,
            },
            Os::Windows => DefaultAttrs {
                flavour: "install_only",
                // Prefer msvc over gnu to match what python.org ships.
                triple_suffix: Some("-pc-windows-msvc"),
            },
        }
    }

    /// Test whether an asset's target-triple is a match for this platform.
    /// The match is exact on the arch component and OR-of-suffixes on the
    /// OS component (e.g. Linux accepts both `-gnu` and `-musl`).
    #[must_use]
    pub fn matches_triple(self, triple: &str) -> bool {
        let arch = match self.arch {
            Arch::X86_64 => "x86_64",
            Arch::I686 => "i686",
            Arch::Aarch64 => "aarch64",
        };
        let Some(rest) = triple.strip_prefix(arch) else {
            return false;
        };
        let Some(rest) = rest.strip_prefix('-') else {
            // `x86_64` must be followed by `-`; this rejects `x86_64_v2-...`
            // because after stripping `x86_64` the next char is `_`, not `-`.
            return false;
        };
        match self.os {
            Os::Linux => rest == "unknown-linux-gnu" || rest == "unknown-linux-musl",
            Os::Darwin => rest == "apple-darwin",
            Os::Windows => rest == "pc-windows-msvc" || rest == "pc-windows-gnu",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINUX_X86_64: Platform = Platform {
        os: Os::Linux,
        arch: Arch::X86_64,
    };
    const MACOS_AARCH64: Platform = Platform {
        os: Os::Darwin,
        arch: Arch::Aarch64,
    };
    const WINDOWS_X86_64: Platform = Platform {
        os: Os::Windows,
        arch: Arch::X86_64,
    };
    const LINUX_AARCH64: Platform = Platform {
        os: Os::Linux,
        arch: Arch::Aarch64,
    };

    #[test]
    fn linux_x86_64_accepts_gnu_and_musl() {
        assert!(LINUX_X86_64.matches_triple("x86_64-unknown-linux-gnu"));
        assert!(LINUX_X86_64.matches_triple("x86_64-unknown-linux-musl"));
    }

    #[test]
    fn linux_x86_64_rejects_micro_arch_variants() {
        // These require newer CPU features; baseline x86_64 only.
        assert!(!LINUX_X86_64.matches_triple("x86_64_v2-unknown-linux-gnu"));
        assert!(!LINUX_X86_64.matches_triple("x86_64_v3-unknown-linux-gnu"));
        assert!(!LINUX_X86_64.matches_triple("x86_64_v4-unknown-linux-musl"));
    }

    #[test]
    fn linux_x86_64_rejects_other_arches() {
        assert!(!LINUX_X86_64.matches_triple("aarch64-unknown-linux-gnu"));
        assert!(!LINUX_X86_64.matches_triple("i686-unknown-linux-gnu"));
    }

    #[test]
    fn linux_x86_64_rejects_other_oses() {
        assert!(!LINUX_X86_64.matches_triple("x86_64-apple-darwin"));
        assert!(!LINUX_X86_64.matches_triple("x86_64-pc-windows-msvc"));
    }

    #[test]
    fn linux_aarch64_accepts_gnu_and_musl() {
        assert!(LINUX_AARCH64.matches_triple("aarch64-unknown-linux-gnu"));
        assert!(LINUX_AARCH64.matches_triple("aarch64-unknown-linux-musl"));
    }

    #[test]
    fn linux_aarch64_rejects_armv7() {
        // armv7 is a different arch string, not an alias for aarch64.
        assert!(!LINUX_AARCH64.matches_triple("armv7-unknown-linux-gnueabi"));
        assert!(!LINUX_AARCH64.matches_triple("armv7-unknown-linux-gnueabihf"));
    }

    #[test]
    fn macos_aarch64_accepts_darwin() {
        assert!(MACOS_AARCH64.matches_triple("aarch64-apple-darwin"));
    }

    #[test]
    fn macos_aarch64_rejects_x86_64_darwin() {
        assert!(!MACOS_AARCH64.matches_triple("x86_64-apple-darwin"));
    }

    #[test]
    fn windows_x86_64_accepts_msvc_and_gnu() {
        assert!(WINDOWS_X86_64.matches_triple("x86_64-pc-windows-msvc"));
        assert!(WINDOWS_X86_64.matches_triple("x86_64-pc-windows-gnu"));
    }

    #[test]
    fn windows_x86_64_rejects_i686() {
        assert!(!WINDOWS_X86_64.matches_triple("i686-pc-windows-msvc"));
    }

    #[test]
    fn rejects_prefix_only_match() {
        // "x86_64" is a prefix of "x86_64_v2" — strip_prefix plus the required
        // '-' separator prevents us from silently treating `_v2` as a match.
        assert!(!LINUX_X86_64.matches_triple("x86_64_v2-unknown-linux-gnu"));
    }

    #[test]
    fn rejects_empty_and_garbage() {
        assert!(!LINUX_X86_64.matches_triple(""));
        assert!(!LINUX_X86_64.matches_triple("x86_64"));
        assert!(!LINUX_X86_64.matches_triple("something-random"));
    }

    #[test]
    fn default_attrs_linux_prefers_glibc_install_only() {
        let attrs = LINUX_X86_64.default_attrs();
        assert_eq!(attrs.flavour, "install_only");
        assert_eq!(attrs.triple_suffix, Some("-unknown-linux-gnu"));
    }

    #[test]
    fn default_attrs_macos_install_only_no_triple_pref() {
        let attrs = MACOS_AARCH64.default_attrs();
        assert_eq!(attrs.flavour, "install_only");
        assert_eq!(attrs.triple_suffix, None);
    }

    #[test]
    fn default_attrs_windows_prefers_msvc_install_only() {
        let attrs = WINDOWS_X86_64.default_attrs();
        assert_eq!(attrs.flavour, "install_only");
        assert_eq!(attrs.triple_suffix, Some("-pc-windows-msvc"));
    }

    #[test]
    fn current_returns_some_on_supported_host() {
        let p = Platform::current();
        // This test runs on CI (Linux x86_64, macOS arm64, Windows x86_64)
        // so it must always resolve to a known platform.
        assert!(p.is_some());
    }
}
