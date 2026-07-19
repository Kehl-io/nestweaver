// build.rs – compile the bundled third-party static libraries that the
// prebuilt liblbug.a leaves as undefined external symbols.
//
// This is only needed when lbug is statically linked (the default).
// If LBUG_SHARED is set, the dynamic lbug already includes these.

mod build_support;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

const LBUG_ABI_VERSION: &str = "0.18.2";
const LBUG_SOURCE_MANIFEST_ENV: &str = "NESTWEAVER_LBUG_SOURCE_MANIFEST";

fn lbug_src_dir() -> Result<PathBuf, String> {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR")
            .ok_or_else(|| "CARGO_MANIFEST_DIR is not set".to_string())?,
    );
    let manifest_path = manifest_dir.join("Cargo.toml");

    if let Some(override_path) = std::env::var_os(LBUG_SOURCE_MANIFEST_ENV) {
        let override_path = PathBuf::from(override_path);
        let override_path = if override_path.is_absolute() {
            override_path
        } else {
            manifest_dir.join(override_path)
        };
        println!("cargo:rerun-if-changed={}", override_path.display());
        return build_support::validate_source_manifest_override(
            &override_path,
            "lbug",
            LBUG_ABI_VERSION,
            "lbug-src",
        );
    }

    let out_dir =
        PathBuf::from(std::env::var_os("OUT_DIR").ok_or_else(|| "OUT_DIR is not set".to_string())?);
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let resolution = build_support::cargo_metadata_for_dependency(
        cargo,
        &manifest_path,
        &out_dir,
        "lbug",
        LBUG_ABI_VERSION,
    )?;
    let dependency = build_support::resolved_dependency_source(
        &resolution.json,
        &resolution.resolution_manifest,
        "lbug",
        "lbug-src",
    )?;
    if dependency.version != LBUG_ABI_VERSION {
        return Err(format!(
            "Cargo resolved {} at version {}; expected the pinned ABI version {LBUG_ABI_VERSION}",
            dependency.package_id, dependency.version
        ));
    }

    println!("cargo:rerun-if-changed={}", manifest_path.display());
    println!(
        "cargo:rerun-if-changed={}",
        dependency.manifest_path.display()
    );
    if !resolution.used_isolated_resolver {
        println!("cargo:rerun-if-changed={}", resolution.lockfile.display());
    }
    Ok(dependency.source_dir)
}

fn emit_openssl_link_search() {
    for variable in ["OPENSSL_LIB_DIR", "OPENSSL_DIR", "OPENSSL_ROOT_DIR"] {
        println!("cargo:rerun-if-env-changed={variable}");
    }

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let explicit_lib_dir = std::env::var_os("OPENSSL_LIB_DIR").map(PathBuf::from);
    let explicit_root = std::env::var_os("OPENSSL_DIR")
        .or_else(|| std::env::var_os("OPENSSL_ROOT_DIR"))
        .map(PathBuf::from)
        .map(|path| path.join("lib"));
    let candidates = [
        explicit_lib_dir,
        explicit_root,
        Some(PathBuf::from("/opt/homebrew/opt/openssl@3/lib")),
        Some(PathBuf::from("/usr/local/opt/openssl@3/lib")),
    ];

    if let Some(lib_dir) = candidates
        .into_iter()
        .flatten()
        .find(|path| path.join("libssl.dylib").is_file())
    {
        println!("cargo:rustc-link-search=native={}", lib_dir.display());
    } else {
        println!(
            "cargo:warning=OpenSSL 3 library directory not found; set OPENSSL_LIB_DIR before linking lbug"
        );
    }
}

/// Glob all .c / .cpp / .cc files in `dir` (non-recursive unless `recursive`).
fn collect_sources(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    let mut out = vec![];
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && recursive {
            out.extend(collect_sources(&path, true));
        } else if let Some(ext) = path.extension() {
            let ext = ext.to_string_lossy();
            if ext == "c" || ext == "cpp" || ext == "cc" {
                out.push(path);
            }
        }
    }
    out
}

fn main() {
    println!("cargo:rerun-if-env-changed=LBUG_SHARED");
    println!("cargo:rerun-if-env-changed=LBUG_BUILD_FROM_SOURCE");
    // A validated source-manifest override supports vendored/offline builds
    // without falling back to Cargo cache layout assumptions.
    println!("cargo:rerun-if-env-changed={LBUG_SOURCE_MANIFEST_ENV}");
    // Only needed for the static link case.
    if std::env::var("LBUG_SHARED").is_ok() {
        return;
    }

    // lbug 0.18 links OpenSSL dynamically. Its pkg-config probe does not work
    // on a stock macOS developer machine where pkg-config is absent, so expose
    // the standard OpenSSL 3 locations to the final linker ourselves.
    emit_openssl_link_search();
    // When lbug is built FROM SOURCE (LBUG_BUILD_FROM_SOURCE), its own build
    // compiles AND links every one of these third_party libraries. Compiling them
    // here too duplicates each static global — e.g. antlr4's `COMPLETE_CHAR_SET`,
    // whose destructor then runs on a mismatched/duplicated allocation at exit and
    // aborts the process (`double free or corruption` → SIGABRT) on glibc. It only
    // works at all today because `--allow-multiple-definition` papers over the
    // duplicate *symbols* at link time — but that can't prevent the runtime
    // double-free. This build.rs is ONLY for satisfying the PREBUILT liblbug.a,
    // which ships these as undefined externals; skip it entirely for source builds.
    if std::env::var("LBUG_BUILD_FROM_SOURCE").is_ok() {
        return;
    }

    let lbug_src = lbug_src_dir().unwrap_or_else(|error| {
        panic!("nestweaver-store requires the bundled lbug libraries: {error}")
    });

    let tp = lbug_src.join("third_party");

    // ------------------------------------------------------------------ yyjson
    let yyjson_src = tp.join("yyjson/src");
    if yyjson_src.is_dir() {
        cc::Build::new()
            .files(collect_sources(&yyjson_src, false))
            .include(&yyjson_src)
            .compile("yyjson");
    }

    // ----------------------------------------------------------------- simsimd
    // simsimd is header-only in spirit; lbug ships a lib.c stub.
    let simsimd_dir = tp.join("simsimd");
    let simsimd_lib_c = simsimd_dir.join("lib.c");
    if simsimd_lib_c.is_file() {
        cc::Build::new()
            .file(&simsimd_lib_c)
            .include(simsimd_dir.join("include"))
            // Avoid AVX-512 / architecture-specific instructions that may not be available.
            .flag_if_supported("-DSIMSIMD_NATIVE_F16=0")
            .flag_if_supported("-DSIMSIMD_NATIVE_BF16=0")
            .compile("simsimd");
    }

    // ----------------------------------------------------------------- fastpfor
    let fastpfor_dir = tp.join("fastpfor/fastpfor");
    if fastpfor_dir.is_dir() {
        cc::Build::new()
            .files(collect_sources(&fastpfor_dir, false))
            .include(&fastpfor_dir)
            .flag_if_supported("-std=c++17")
            .compile("fastpfor");
    }

    // ----------------------------------------------------------------- roaring
    let roaring_dir = tp.join("roaring_bitmap");
    let roaring_c = roaring_dir.join("roaring.c");
    if roaring_c.is_file() {
        cc::Build::new()
            .file(&roaring_c)
            .include(&roaring_dir)
            .compile("roaring_bitmap");
    }

    // ----------------------------------------------------------------- mbedtls
    // Only the SHA-256 component is needed (mbedtls_sha256_*).
    let mbedtls_lib = tp.join("mbedtls/library");
    let mbedtls_inc = tp.join("mbedtls/include");
    if mbedtls_lib.is_dir() {
        cc::Build::new()
            .files(collect_sources(&mbedtls_lib, false))
            .include(&mbedtls_inc)
            .flag_if_supported("-std=c++17")
            .compile("mbedtls");
    }

    // ----------------------------------------------------------------- brotli
    let brotli_c = tp.join("brotli/c");
    if brotli_c.is_dir() {
        let mut build = cc::Build::new();
        for sub in &["common", "dec"] {
            build.files(collect_sources(&brotli_c.join(sub), false));
        }
        build.include(brotli_c.join("include")).compile("brotlidec");
    }

    // ----------------------------------------------------------------- lz4
    let lz4_dir = tp.join("lz4");
    let lz4_cpp = lz4_dir.join("lz4.cpp");
    if lz4_cpp.is_file() {
        cc::Build::new()
            .file(&lz4_cpp)
            .include(&lz4_dir)
            .flag_if_supported("-std=c++17")
            .compile("lz4");
    }

    // ----------------------------------------------------------------- zstd
    let zstd_dir = tp.join("zstd");
    if zstd_dir.is_dir() {
        let mut build = cc::Build::new();
        for sub in &["common", "compress", "decompress"] {
            build.files(collect_sources(&zstd_dir.join(sub), false));
        }
        build
            .include(&zstd_dir)
            .include(zstd_dir.join("common"))
            .include(zstd_dir.join("include"))
            .include(zstd_dir.join("include/zstd"))
            .include(zstd_dir.join("include/zstd/common"))
            .include(zstd_dir.join("include/zstd/compress"))
            .include(zstd_dir.join("include/zstd/decompress"))
            .flag_if_supported("-std=c++17")
            .compile("zstd");
    }

    // ----------------------------------------------------------------- snappy
    let snappy_dir = tp.join("snappy");
    if snappy_dir.is_dir() {
        cc::Build::new()
            .files(collect_sources(&snappy_dir, false))
            .include(&snappy_dir)
            .flag_if_supported("-std=c++17")
            .compile("snappy");
    }

    // ----------------------------------------------------------------- utf8proc
    let utf8proc_dir = tp.join("utf8proc");
    if utf8proc_dir.is_dir() {
        cc::Build::new()
            .files(collect_sources(&utf8proc_dir, false))
            .include(utf8proc_dir.join("include"))
            .flag_if_supported("-std=c++17")
            .compile("utf8proc");
    }

    // ----------------------------------------------------------------- antlr4_runtime
    let antlr4_rt_src = tp.join("antlr4_runtime/src");
    if antlr4_rt_src.is_dir() {
        cc::Build::new()
            .cpp(true)
            .files(collect_sources(&antlr4_rt_src, true))
            .include(&antlr4_rt_src)
            .flag("-std=c++17")
            .compile("antlr4_runtime");
    }

    // ----------------------------------------------------------------- antlr4_cypher
    let antlr4_cypher_dir = tp.join("antlr4_cypher");
    if antlr4_cypher_dir.is_dir() {
        cc::Build::new()
            .cpp(true)
            .files(collect_sources(&antlr4_cypher_dir, false))
            .include(antlr4_cypher_dir.join("include"))
            .include(&antlr4_rt_src)
            .flag("-std=c++17")
            .compile("antlr4_cypher");
    }

    // ----------------------------------------------------------------- miniz
    let miniz_dir = tp.join("miniz");
    let miniz_cpp = miniz_dir.join("miniz.cpp");
    if miniz_cpp.is_file() {
        cc::Build::new()
            .cpp(true)
            .file(&miniz_cpp)
            .include(&miniz_dir)
            .flag("-std=c++17")
            .compile("miniz");
    }

    // ----------------------------------------------------------------- thrift
    let thrift_dir = tp.join("thrift");
    if thrift_dir.is_dir() {
        cc::Build::new()
            .cpp(true)
            .files(collect_sources(&thrift_dir, true))
            .include(&thrift_dir)
            .flag("-std=c++17")
            .compile("thrift");
    }

    // ----------------------------------------------------------------- parquet
    let parquet_dir = tp.join("parquet");
    if parquet_dir.is_dir() {
        let thrift_dir2 = tp.join("thrift");
        cc::Build::new()
            .cpp(true)
            .files(collect_sources(&parquet_dir, false))
            .include(&parquet_dir)
            .include(&thrift_dir2)
            .flag("-std=c++17")
            .compile("parquet");
    }

    // ----------------------------------------------------------------- re2
    let re2_dir = tp.join("re2");
    if re2_dir.is_dir() {
        cc::Build::new()
            .cpp(true)
            .files(collect_sources(&re2_dir, false))
            .include(&re2_dir)
            .include(re2_dir.join("include"))
            .flag("-std=c++17")
            .compile("re2");
    }

    println!("cargo:rerun-if-changed=build.rs");
}
