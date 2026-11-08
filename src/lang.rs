//! File language from its name, for the "colour by language" mode.
//!
//! Deliberately coarse: a dozen families that matter for systems and app repos, plus
//! "other". Detection is by extension or well-known file name only (no content
//! sniffing), so it costs nothing at load time.

/// (name, colour). Index = language id. The last entry is the fallback.
pub const LANGS: [(&str, u32); 15] = [
    ("C", 0x4e79a7),
    ("C++", 0x86bcb6),
    ("Rust", 0xe15759),
    ("Assembly", 0xf28e2b),
    ("Python", 0x76b7b2),
    ("Shell", 0xedc948),
    ("Perl", 0xb07aa1),
    ("Go", 0x00add8),
    ("JS/TS", 0xf1ce63),
    ("Java/Kotlin", 0xd37295),
    ("Devicetree", 0x59a14f),
    ("Build", 0xff9da7),
    ("Data", 0x9c755f),
    ("Docs", 0xbab0ac),
    ("Other", 0x52525b),
];

pub const OTHER: u8 = (LANGS.len() - 1) as u8;

pub fn lang_of(path: &str) -> u8 {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    if lower == "makefile"
        || lower.starts_with("kconfig")
        || lower == "kbuild"
        || lower == "cmakelists.txt"
        || lower == "dockerfile"
        || lower == "cargo.toml"
        || lower == "build.gradle"
        || lower == "meson.build"
    {
        return 11;
    }
    let ext = match lower.rsplit_once('.') {
        Some((stem, e)) if !stem.is_empty() => e,
        _ => return OTHER,
    };
    match ext {
        "c" | "h" => 0,
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "inl" => 1,
        "rs" => 2,
        "s" | "asm" | "inc" => 3,
        "py" | "pyi" => 4,
        "sh" | "bash" | "zsh" => 5,
        "pl" | "pm" => 6,
        "go" => 7,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "svelte" | "vue" => 8,
        "java" | "kt" | "kts" | "scala" => 9,
        "dts" | "dtsi" | "dtso" => 10,
        "mk" | "make" | "cmake" | "gradle" | "bazel" | "bzl" => 11,
        "json" | "yaml" | "yml" | "toml" | "xml" | "csv" | "ini" | "conf" | "lock" => 12,
        "md" | "rst" | "txt" | "adoc" | "tex" | "html" | "css" => 13,
        _ => OTHER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_files() {
        let name = |p: &str| LANGS[lang_of(p) as usize].0;
        assert_eq!(name("kernel/sched/core.c"), "C");
        assert_eq!(name("include/linux/sched.h"), "C");
        assert_eq!(name("rust/kernel/lib.rs"), "Rust");
        assert_eq!(name("arch/x86/entry/entry_64.S"), "Assembly");
        assert_eq!(name("arch/arm64/boot/dts/apple/t8103.dtsi"), "Devicetree");
        assert_eq!(name("drivers/net/Kconfig"), "Build");
        assert_eq!(name("drivers/net/Makefile"), "Build");
        assert_eq!(name("Documentation/process/howto.rst"), "Docs");
        assert_eq!(name("scripts/checkpatch.pl"), "Perl");
        assert_eq!(name("web/app.tsx"), "JS/TS");
        assert_eq!(name(".gitignore"), "Other");
        assert_eq!(name("COPYING"), "Other");
    }
}
