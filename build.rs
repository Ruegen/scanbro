fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.git/HEAD");

    let sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "nogit".into());
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{:x}", d.as_secs()))
        .unwrap_or_else(|_| "0".into());
    let build = format!("{sha}-{uniq}");
    println!("cargo:rustc-env=SCANBRO_BUILD={build}");
}

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
