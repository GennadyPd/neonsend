// Внедряет иконку в exe: без этого в проводнике и панели задач виден стандартный значок.
// Нужен windres из MinGW; если его нет — сборка продолжается без иконки.
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let Some(out) = std::env::var_os("OUT_DIR").map(PathBuf::from) else {
        return;
    };
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let ico = root.join("icon.ico");
    if !ico.is_file() {
        return;
    }
    let rc = out.join("neonsend.rc");
    let body = format!("1 ICON \"{}\"\n", ico.display().to_string().replace('\\', "\\\\"));
    if std::fs::write(&rc, body).is_err() {
        return;
    }
    let obj = out.join("neonsend_res.o");

    for tool in ["windres", "llvm-windres"] {
        let ok = Command::new(tool)
            .arg(&rc)
            .arg("-O")
            .arg("coff")
            .arg("-o")
            .arg(&obj)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            println!("cargo:rustc-link-arg={}", obj.display());
            return;
        }
    }
    println!("cargo:warning=windres не найден: exe собран без иконки");
}
