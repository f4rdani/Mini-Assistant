use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Only ONE file is compiled. App.slint pulls in the three window
    // definitions, so the design-token globals (theme.slint) are emitted into
    // the generated Rust module exactly once.
    slint_build::compile("ui/App.slint").unwrap();

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_icon();
    }
}

fn embed_windows_icon() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let ico_path = manifest_dir.join("assets").join("app-icon.ico");
    println!("cargo:rerun-if-changed={}", ico_path.display());
    if !ico_path.is_file() {
        println!("cargo:warning=assets/app-icon.ico not found; skipping .exe icon embed");
        return;
    }

    let Some(rc_exe) = find_rc_exe() else {
        println!("cargo:warning=Windows SDK rc.exe not found; skipping .exe icon embed");
        return;
    };

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let rc_file = out_dir.join("app_icon.rc");
    let res_file = out_dir.join("app_icon.res");
    let ico_escaped = ico_path.to_string_lossy().replace('\\', "/");
    let rc_src = format!(
        r#"1 ICON "{ico_escaped}"
1 VERSIONINFO
FILEVERSION 1,0,0,0
PRODUCTVERSION 1,0,0,0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "Startorch Labs"
            VALUE "FileDescription", "Mini Assistant Portable"
            VALUE "FileVersion", "1.0.0.0"
            VALUE "InternalName", "MiniAssistant"
            VALUE "OriginalFilename", "MiniAssistant.exe"
            VALUE "ProductName", "Mini Assistant"
            VALUE "ProductVersion", "1.0.0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#
    );
    if let Err(e) = std::fs::write(&rc_file, rc_src) {
        println!("cargo:warning=failed writing app_icon.rc: {e}");
        return;
    }

    let status = Command::new(&rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .status();

    match status {
        Ok(s) if s.success() && res_file.is_file() => {
            println!("cargo:rustc-link-arg-bin=mini-assistant-ui={}", res_file.display());
        }
        Ok(s) => println!("cargo:warning=rc.exe exited with {s}; skipping .exe icon embed"),
        Err(e) => println!("cargo:warning=failed running rc.exe: {e}"),
    }
}

fn find_rc_exe() -> Option<PathBuf> {
    if Command::new("rc.exe").arg("/?").output().is_ok() {
        return Some(PathBuf::from("rc.exe"));
    }
    let kits_bin = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(kits_bin)
        .ok()?
        .filter_map(|e| e.ok().map(|x| x.path()))
        .filter(|p| p.is_dir())
        .collect();
    versions.sort();
    for dir in versions.into_iter().rev() {
        for arch in ["x64", "x86"] {
            let cand = dir.join(arch).join("rc.exe");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}
