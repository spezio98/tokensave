//! Kotlin Multiplatform (KMP) path conventions: derive the source-set, target
//! platform, and owning module from a file path. Pure path heuristics — no
//! Gradle parsing (that is out of scope; see the KMP design spec).

/// The compilation target a KMP source set belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KmpTarget {
    /// A shared source set (`commonMain`, `commonTest`).
    Common,
    /// A platform-specific target: the prefix before `Main`/`Test`
    /// (`"android"`, `"ios"`, `"jvm"`, `"js"`, `"native"`, `"wasmJs"`, ...).
    Platform(String),
}

/// Where a file sits in a KMP module layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KmpLocation {
    /// The source-set directory name, e.g. `"commonMain"`, `"androidMain"`.
    pub source_set: String,
    pub target: KmpTarget,
    /// The path up to (excluding) `src/`, e.g. `"shared"`.
    pub module_root: String,
}

/// Parse a KMP location from a file path, or `None` for non-KMP layouts.
///
/// Matches the first path segment of the shape `{prefix}Main` / `{prefix}Test`
/// that sits immediately inside a `src/` segment (`.../src/{segment}/...`).
pub fn kmp_location_from_path(file_path: &str) -> Option<KmpLocation> {
    let segments: Vec<&str> = file_path.split('/').collect();
    for (i, seg) in segments.iter().enumerate() {
        if i == 0 || segments[i - 1] != "src" {
            continue;
        }
        let Some(prefix) = seg
            .strip_suffix("Main")
            .or_else(|| seg.strip_suffix("Test"))
        else {
            continue;
        };
        // Require a lowercase-led, alphanumeric prefix (rejects e.g. "Main").
        if prefix.is_empty()
            || !prefix
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase())
        {
            continue;
        }
        if !prefix.chars().all(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        let target = if prefix == "common" {
            KmpTarget::Common
        } else {
            KmpTarget::Platform(prefix.to_string())
        };
        let module_root = segments[..i - 1].join("/");
        return Some(KmpLocation {
            source_set: (*seg).to_string(),
            target,
            module_root,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_main_is_common() {
        let loc = kmp_location_from_path("shared/src/commonMain/kotlin/com/x/Foo.kt").unwrap();
        assert_eq!(loc.source_set, "commonMain");
        assert!(matches!(loc.target, KmpTarget::Common));
        assert_eq!(loc.module_root, "shared");
    }

    #[test]
    fn android_main_is_platform() {
        let loc = kmp_location_from_path("shared/src/androidMain/kotlin/Foo.kt").unwrap();
        assert_eq!(loc.source_set, "androidMain");
        assert!(matches!(loc.target, KmpTarget::Platform(ref p) if p == "android"));
        assert_eq!(loc.module_root, "shared");
    }

    #[test]
    fn ios_test_source_set() {
        let loc = kmp_location_from_path("a/b/feature/src/iosTest/kotlin/FooTest.kt").unwrap();
        assert_eq!(loc.source_set, "iosTest");
        assert!(matches!(loc.target, KmpTarget::Platform(ref p) if p == "ios"));
        assert_eq!(loc.module_root, "a/b/feature");
    }

    #[test]
    fn non_kmp_layout_is_none() {
        assert!(kmp_location_from_path("app/src/main/kotlin/Foo.kt").is_none());
        assert!(kmp_location_from_path("src/lib.rs").is_none());
    }

    #[test]
    fn wasm_js_custom_target() {
        let loc = kmp_location_from_path("core/src/wasmJsMain/kotlin/Foo.kt").unwrap();
        assert_eq!(loc.source_set, "wasmJsMain");
        assert!(matches!(loc.target, KmpTarget::Platform(ref p) if p == "wasmJs"));
    }
}
