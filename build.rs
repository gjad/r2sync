use std::{env, fs, path::PathBuf};

fn main() {
    let target = env::var("TARGET").expect("Cargo sets TARGET for build scripts");
    if target != "x86_64-win7-windows-msvc" {
        return;
    }

    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".cargo")))
        .expect("CARGO_HOME or USERPROFILE must be set");
    let registry_source = cargo_home.join("registry").join("src");
    let mut found = false;

    for registry in fs::read_dir(&registry_source)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", registry_source.display()))
        .flatten()
    {
        let Ok(packages) = fs::read_dir(registry.path()) else {
            continue;
        };
        for package in packages.flatten() {
            let package_name = package.file_name();
            let package_name = package_name.to_string_lossy();
            if package_name.starts_with("windows_x86_64_msvc-") {
                let import_lib_dir = package.path().join("lib");
                if import_lib_dir.is_dir() {
                    println!(
                        "cargo:rustc-link-search=native={}",
                        import_lib_dir.display()
                    );
                    found = true;
                }
            }
        }
    }

    if !found {
        panic!(
            "could not locate windows_x86_64_msvc import libraries in {}; run cargo fetch and retry",
            registry_source.display()
        );
    }
}
