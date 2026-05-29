fn main() {
    #[cfg(windows)]
    windows();

    #[cfg(all(unix, not(target_os = "macos")))]
    linux();
}

#[cfg(windows)]
fn windows() {
    println!(r"cargo:rustc-link-search=native=D:\php_sdk\phpdev\vs17");
    println!("cargo:rustc-link-lib=dylib=php8embed");
}

#[cfg(all(unix, not(target_os = "macos")))]
fn linux() {
    println!("cargo:rerun-if-env-changed=PHP_CONFIG");

    let php_config = std::env::var("PHP_CONFIG").unwrap_or_else(|_| "php-config".to_string());

    let ldflags = command_output(&php_config, "--ldflags");
    let libs = command_output(&php_config, "--libs");
    let version = command_output(&php_config, "--version");

    for token in ldflags.split_whitespace().chain(libs.split_whitespace()) {
        if let Some(path) = token.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        } else if let Some(lib) = token.strip_prefix("-l") {
            println!("cargo:rustc-link-lib=dylib={lib}");
        } else if token.starts_with("-Wl,") {
            println!("cargo:rustc-link-arg={token}");
        }
    }

    let major_minor = version
        .split('.')
        .take(2)
        .collect::<Vec<_>>()
        .join(".");

    if !major_minor.is_empty() {
        println!("cargo:rustc-link-lib=dylib=php{major_minor}");
    }

    println!("cargo:rustc-link-lib=dylib=php");
}

#[cfg(all(unix, not(target_os = "macos")))]
fn command_output(program: &str, arg: &str) -> String {
    let output = Command::new(program)
        .arg(arg)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {program} {arg}: {error}"));

    if !output.status.success() {
        panic!("{program} {arg} failed with status {}", output.status);
    }

    String::from_utf8(output.stdout)
        .unwrap_or_else(|error| panic!("{program} {arg} returned non-UTF8 output: {error}"))
        .trim()
        .to_string()
}
