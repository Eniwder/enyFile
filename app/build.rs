// Embeds icon.ico as the exe icon, enyfile.manifest and the version resource with the Windows SDK's rc.exe,
// avoiding an extra build dependency (builds are offline). Without rc.exe the exe just has
// neither.
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=icon.rc");
    println!("cargo:rerun-if-changed=icon.ico");
    println!("cargo:rerun-if-changed=enyfile.manifest");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=RC");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    // The loader resolves the exe's own imports (dwmapi, uxtheme, ...) from System32 only,
    // never from the exe's folder; shell::harden_dll_search covers later loads.
    println!("cargo:rustc-link-arg-bin=enyfile=/DEPENDENTLOADFLAG:0x800");
    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe not found; enyfile.exe will have no icon or manifest");
        return;
    };
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // The version resource takes its numbers from Cargo.toml.
    let version = |part: &str| std::env::var(format!("CARGO_PKG_VERSION_{part}")).unwrap();
    let header = format!(
        "#define VER_MAJOR {}\n#define VER_MINOR {}\n#define VER_PATCH {}\n#define VER_STRING \"{}\"\n",
        version("MAJOR"),
        version("MINOR"),
        version("PATCH"),
        std::env::var("CARGO_PKG_VERSION").unwrap(),
    );
    std::fs::write(out.join("version.h"), header).unwrap();
    let res = out.join("icon.res");
    let ok = Command::new(&rc)
        .arg("/nologo")
        .arg("/i")
        .arg(&out)
        .arg("/fo")
        .arg(&res)
        .arg("icon.rc")
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        println!("cargo:warning=rc.exe failed; enyfile.exe will have no icon or manifest");
        return;
    }
    // link.exe accepts compiled .res files as inputs.
    println!("cargo:rustc-link-arg-bin=enyfile={}", res.display());
}

fn find_rc() -> Option<PathBuf> {
    if let Some(rc) = std::env::var_os("RC") {
        return Some(rc.into());
    }
    if Command::new("rc.exe").arg("/?").output().is_ok() {
        return Some("rc.exe".into());
    }
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86") => "x86",
        _ => "x64",
    };
    let pf = std::env::var_os("ProgramFiles(x86)").unwrap_or_else(|| r"C:\Program Files (x86)".into());
    let bin = PathBuf::from(pf).join(r"Windows Kits\10\bin");
    let mut found: Vec<PathBuf> = std::fs::read_dir(bin)
        .ok()?
        .flatten()
        .map(|e| e.path().join(arch).join("rc.exe"))
        .filter(|p| p.is_file())
        .collect();
    // SDK folders are named by version (10.0.22621.0); the newest sorts last for same-width parts.
    found.sort();
    found.pop()
}
