fn main() {
    // Bundle the gettext `.po` translations under `lang/` so the UI's `@tr(...)`
    // strings can switch language at runtime via slint::select_bundled_translation.
    // The `.slint` source (msgids) is English; `lang/zh/LC_MESSAGES/rudder.po`
    // maps msgids to Chinese and `lang/en/...` keeps the identity mapping.  No
    // per-component context, so msgids are the raw English strings.
    println!("cargo:rerun-if-changed=lang");
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            .with_style("fluent".into())
            .with_bundled_translations("lang")
            .with_default_translation_context(slint_build::DefaultTranslationContext::None),
    )
    .expect("Slint build failed");

    // Embed the application icon into the Windows executable so it shows up in
    // Explorer, the taskbar and shortcuts. No-op on non-Windows targets.
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/rudder.ico");
        println!("cargo:rerun-if-changed=assets/rudder.exe.manifest");
        // winresource 的 `new()` 会用 `reg query` 读 `KitsRoot10` 来定位 SDK。进程里
        // `reg` 起不来（受限沙箱 / PATH 缺 System32）时它退化成相对路径 `bin\x64\rc.exe`，
        // 报 "系统找不到指定的路径。 (os error 3)"，图标与 DPI manifest 一起丢失。
        // 先把 rc.exe 自己找出来写进 `RC_PATH`：`new()` 见到它就短路，不再走注册表。
        let mut rc = std::env::var_os("RC_PATH").map(std::path::PathBuf::from);
        if rc.is_none() {
            rc = rc_exe();
        }
        if let Some(path) = rc.as_deref() {
            // build.rs 此处是单线程的，没有并发访问环境变量的风险。
            unsafe { std::env::set_var("RC_PATH", path) };
        }
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/rudder.ico");
        // Embed an application manifest declaring Per-Monitor DPI Awareness V2.
        // Without it the DPI-awareness level depends on winit's runtime
        // SetProcessDpiAwarenessContext call, which races: if anything touches a
        // DPI API first the call silently fails and the window jumps in size /
        // cursor offset when dragged across monitors with different scaling (#194).
        // The manifest is authoritative and applied before any code runs.
        res.set_manifest_file("assets/rudder.exe.manifest");
        if let Err(e) = res.compile() {
            match rc.as_deref() {
                Some(path) => {
                    println!(
                        "cargo:warning=failed to embed Windows icon (rc.exe: {}): {e}",
                        path.display()
                    );
                }
                None => {
                    println!(
                        "cargo:warning=failed to embed Windows icon: {e}; \
                         rc.exe not found — install the Windows SDK or set RC_PATH"
                    );
                }
            }
        }
    }

    // --- Embed resolved dependency versions for the About dialog -----------
    generate_dep_versions();
}

/// 定位 `rc.exe`。
///
/// 顺序：`WindowsSdkVerBinPath`（开发者命令提示符）→ `WindowsSdkDir` →
/// `%ProgramFiles(x86)%\Windows Kits\10\bin`。只用环境变量与文件系统，
/// 不碰注册表：受限进程里 `reg.exe` 起不来，winresource 自带的探测会整体失败。
#[cfg(windows)]
fn rc_exe() -> Option<std::path::PathBuf> {
    use std::path::{Path, PathBuf};

    // 构建脚本跑在宿主架构上；ARM64 可用 x64 / x86 工具模拟执行。
    let archs: &[&str] = match std::env::var("HOST")
        .unwrap_or_default()
        .split('-')
        .next()
    {
        Some("aarch64") => &["arm64", "x64", "x86"],
        Some("x86_64") => &["x64", "x86"],
        _ => &["x86"],
    };

    // `bin` 下可能是 `10.0.26100.0\x64`，也可能是 `x86`（8.1 及更早的扁平布局）。
    fn pick(bin: &Path, archs: &[&str]) -> Option<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(bin) {
            let mut versioned: Vec<(Vec<u32>, PathBuf)> = entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| {
                    let name = entry.file_name();
                    let key: Vec<u32> = name
                        .to_str()?
                        .split('.')
                        .map(|part| part.parse::<u32>().ok())
                        .collect::<Option<Vec<u32>>>()?;
                    (key.len() == 4).then(|| (key, entry.path()))
                })
                .collect();
            versioned.sort_by(|a, b| b.0.cmp(&a.0));
            dirs.extend(versioned.into_iter().map(|(_, path)| path));
        }
        dirs.push(bin.to_path_buf());
        dirs.iter()
            .find_map(|dir| {
                archs
                    .iter()
                    .map(|arch| dir.join(arch))
                    .find(|sub| sub.join("rc.exe").is_file())
            })
            .map(|dir| dir.join("rc.exe"))
    }

    if let Some(bin) = std::env::var_os("WindowsSdkVerBinPath")
        && let Some(found) = pick(Path::new(&bin), archs)
    {
        return Some(found);
    }
    if let Some(root) = std::env::var_os("WindowsSdkDir")
        && let Some(found) = pick(&Path::new(&root).join("bin"), archs)
    {
        return Some(found);
    }
    ["ProgramFiles(x86)", "ProgramFiles"]
        .iter()
        .filter_map(std::env::var_os)
        .find_map(|base| pick(&Path::new(&base).join("Windows Kits").join("10").join("bin"), archs))
}

/// Parse Cargo.lock, extract every `[[package]]` name + version pair
/// (all resolved deps, not just direct ones), and write them into a
/// generated Rust source file that the app includes at compile time.
fn generate_dep_versions() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let lock_path = std::path::Path::new(&manifest_dir).join("Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock_path.display());

    let lock = std::fs::read_to_string(&lock_path).unwrap_or_else(|e| {
        println!("cargo:warning=cannot read Cargo.lock: {e}");
        String::new()
    });

    let deps: Vec<(String, String)> = parse_lock_packages(&lock);

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let dest = std::path::Path::new(&out_dir).join("deps.rs");
    let mut f = std::fs::File::create(dest).expect("create deps.rs");

    use std::io::Write as _;
    writeln!(f, "// Generated by build.rs — DO NOT EDIT").unwrap();
    writeln!(f, "pub(crate) const DEP_VERSIONS: &[(&str, &str)] = &[").unwrap();
    for (name, version) in &deps {
        writeln!(f, r#"    ("{name}", "{version}"),"#).unwrap();
    }
    writeln!(f, "];").unwrap();
}

/// Minimal Cargo.lock parser — each `[[package]]` block has `name` and
/// `version` keys on the following lines.  We only need those two fields.
fn parse_lock_packages(lock: &str) -> Vec<(String, String)> {
    let mut deps = Vec::new();
    let mut name = String::new();
    let mut version = String::new();

    for line in lock.lines() {
        if line.trim() == "[[package]]" {
            if !name.is_empty() {
                deps.push((std::mem::take(&mut name), std::mem::take(&mut version)));
            }
        } else if let Some(v) = line.trim().strip_prefix("name = ") {
            name = v.trim().trim_matches('"').to_string();
        } else if let Some(v) = line.trim().strip_prefix("version = ") {
            version = v.trim().trim_matches('"').to_string();
        }
    }
    if !name.is_empty() {
        deps.push((name, version));
    }
    deps
}
