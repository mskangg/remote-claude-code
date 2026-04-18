use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Context, Result};

const SERVICE_LABEL: &str = "com.remote-claude-code.rcc";

pub fn service_plist_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{SERVICE_LABEL}.plist")))
}

pub fn default_rcc_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    Ok(home.join(".local").join("bin").join("rcc"))
}

pub fn default_log_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    Ok(home
        .join(".local")
        .join("share")
        .join("remote-claude-code")
        .join("rcc.log"))
}

pub fn build_plist(rcc_path: &Path, log_path: &Path, path_env: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{SERVICE_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{rcc}</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        rcc = rcc_path.display(),
        log = log_path.display(),
        path = path_env,
    )
}

pub fn install_service() -> Result<()> {
    let rcc_path = default_rcc_path()?;
    if !rcc_path.exists() {
        bail!(
            "rcc binary not found at {}. Run setup first.",
            rcc_path.display()
        );
    }

    let log_path = default_log_path()?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create log directory: {}", parent.display()))?;
    }

    let plist_path = service_plist_path()?;
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create LaunchAgents directory: {}", parent.display()))?;
    }

    let path_env = std::env::var("PATH").unwrap_or_else(|_| "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin".to_string());
    let plist = build_plist(&rcc_path, &log_path, &path_env);
    fs::write(&plist_path, &plist)
        .with_context(|| format!("write plist: {}", plist_path.display()))?;

    let status = Command::new("launchctl")
        .args(["load", "-w", &plist_path.display().to_string()])
        .status()
        .context("run launchctl load")?;

    if !status.success() {
        bail!("launchctl load failed with status {status}");
    }

    println!("Service installed and loaded: {SERVICE_LABEL}");
    println!("Plist: {}", plist_path.display());
    println!("Log:   {}", log_path.display());
    Ok(())
}

pub fn uninstall_service() -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;

    // 1. launchd 언로드 + plist 삭제
    let plist_path = service_plist_path()?;
    if plist_path.exists() {
        let _ = Command::new("launchctl")
            .args(["unload", "-w", &plist_path.display().to_string()])
            .status();
        fs::remove_file(&plist_path)
            .with_context(|| format!("remove plist: {}", plist_path.display()))?;
        println!("Removed: {}", plist_path.display());
    }

    // 2. 바이너리 삭제
    let bin_dir = home.join(".local").join("bin");
    for name in &["rcc", "rcc.bin"] {
        let path = bin_dir.join(name);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("remove binary: {}", path.display()))?;
            println!("Removed: {}", path.display());
        }
    }

    // 3. share 디렉토리 삭제
    let share_dir = home
        .join(".local")
        .join("share")
        .join("remote-claude-code");
    if share_dir.exists() {
        fs::remove_dir_all(&share_dir)
            .with_context(|| format!("remove share dir: {}", share_dir.display()))?;
        println!("Removed: {}", share_dir.display());
    }

    // 4. ~/.zshrc / ~/.bash_profile 등에서 PATH 줄 제거
    let profile_candidates = [
        home.join(".zshrc"),
        home.join(".bash_profile"),
        home.join(".bashrc"),
    ];
    let path_line = "export PATH=\"$HOME/.local/bin:$PATH\"";
    for profile in &profile_candidates {
        if !profile.exists() {
            continue;
        }
        let content = fs::read_to_string(profile)
            .with_context(|| format!("read profile: {}", profile.display()))?;
        let updated: String = content
            .lines()
            .filter(|line| line.trim() != path_line)
            .collect::<Vec<_>>()
            .join("\n");
        // 파일 끝 개행 유지
        let updated = if content.ends_with('\n') {
            format!("{updated}\n")
        } else {
            updated
        };
        if updated != content {
            fs::write(profile, &updated)
                .with_context(|| format!("update profile: {}", profile.display()))?;
            println!("Removed PATH entry from: {}", profile.display());
        }
    }

    println!("\nUninstall complete. Project config files (.env.local, data/) are kept.");
    Ok(())
}

pub fn start_service() -> Result<()> {
    let status = Command::new("launchctl")
        .args(["start", SERVICE_LABEL])
        .status()
        .context("run launchctl start")?;

    if !status.success() {
        bail!("launchctl start failed with status {status}. Is the service installed? Run `rcc service install` first.");
    }

    println!("Service started: {SERVICE_LABEL}");
    Ok(())
}

pub fn stop_service() -> Result<()> {
    let status = Command::new("launchctl")
        .args(["stop", SERVICE_LABEL])
        .status()
        .context("run launchctl stop")?;

    if !status.success() {
        bail!("launchctl stop failed with status {status}.");
    }

    println!("Service stopped: {SERVICE_LABEL}");
    Ok(())
}

pub fn status_service() -> Result<()> {
    let output = Command::new("launchctl")
        .args(["list", SERVICE_LABEL])
        .output()
        .context("run launchctl list")?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        println!("{stdout}");
    } else {
        let plist_path = service_plist_path()?;
        if plist_path.exists() {
            println!("Service is installed but not running: {SERVICE_LABEL}");
        } else {
            println!("Service is not installed. Run `rcc service install` first.");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn build_plist_contains_label_and_paths() {
        let rcc = PathBuf::from("/Users/demo/.local/bin/rcc");
        let log = PathBuf::from("/Users/demo/.local/share/remote-claude-code/rcc.log");
        let plist = build_plist(&rcc, &log, "/opt/homebrew/bin:/usr/bin:/bin");

        assert!(plist.contains("com.remote-claude-code.rcc"));
        assert!(plist.contains("/Users/demo/.local/bin/rcc"));
        assert!(plist.contains("/Users/demo/.local/share/remote-claude-code/rcc.log"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>PATH</key>"));
        assert!(plist.contains("/opt/homebrew/bin"));
    }

    #[test]
    fn build_plist_uses_same_path_for_stdout_and_stderr() {
        let rcc = PathBuf::from("/usr/local/bin/rcc");
        let log = PathBuf::from("/tmp/rcc.log");
        let plist = build_plist(&rcc, &log, "/usr/bin:/bin");

        let stdout_count = plist.matches("StandardOutPath").count();
        let stderr_count = plist.matches("StandardErrorPath").count();
        assert_eq!(stdout_count, 1);
        assert_eq!(stderr_count, 1);
        assert_eq!(plist.matches("/tmp/rcc.log").count(), 2);
    }

    #[test]
    fn service_plist_path_uses_home_library_launch_agents() {
        let previous = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", "/Users/demo") };

        let path = service_plist_path().expect("plist path");

        match previous {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert_eq!(
            path,
            PathBuf::from("/Users/demo/Library/LaunchAgents/com.remote-claude-code.rcc.plist")
        );
    }

    #[test]
    fn default_rcc_path_uses_home_local_bin() {
        let previous = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", "/Users/demo") };

        let path = default_rcc_path().expect("rcc path");

        match previous {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert_eq!(path, PathBuf::from("/Users/demo/.local/bin/rcc"));
    }

    #[test]
    fn default_log_path_uses_share_directory() {
        let previous = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", "/Users/demo") };

        let path = default_log_path().expect("log path");

        match previous {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert_eq!(
            path,
            PathBuf::from("/Users/demo/.local/share/remote-claude-code/rcc.log")
        );
    }
}
