use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Locale {
    #[default]
    En,
    Ko,
}

impl std::str::FromStr for Locale {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "ko" | "korean" | "한국어" | "2" => Locale::Ko,
            _ => Locale::En,
        })
    }
}

impl Locale {
    pub fn from_env() -> Self {
        match std::env::var("RCC_LOCALE").as_deref() {
            Ok("ko") | Ok("KO") => Locale::Ko,
            _ => Locale::En,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Locale::En => "en",
            Locale::Ko => "ko",
        }
    }

    // ── Setup ──────────────────────────────────────────────────────────────

    pub fn setup_choose_language(&self) -> &'static str {
        match self {
            Locale::Ko => "언어를 선택하세요 (Choose language): [1] English  [2] 한국어",
            Locale::En => "Choose language: [1] English  [2] 한국어 (Korean)",
        }
    }

    pub fn setup_completion_message(
        &self,
        install_path: &Path,
        profile_path: &Path,
        installer_script_path: &Path,
    ) -> String {
        match self {
            Locale::Ko => format!(
                "설치 완료. `sh {}`를 실행해 `rcc`를 {}에 설치하고 {}를 업데이트하세요. 이후 포그라운드 실행은 `rcc`, 상시 실행은 `rcc service install && rcc service start`를 사용하세요.",
                installer_script_path.display(),
                install_path.display(),
                profile_path.display(),
            ),
            Locale::En => format!(
                "Setup complete. Run the generated installer script with `sh {}` to install `rcc` at {} and update {} if needed. After that, use `rcc` for foreground execution or `rcc service install && rcc service start` for background execution.",
                installer_script_path.display(),
                install_path.display(),
                profile_path.display(),
            ),
        }
    }

    pub fn setup_run_installer_prompt(&self) -> &'static str {
        match self {
            Locale::Ko => "설치 스크립트를 지금 실행할까요? [Y/n]",
            Locale::En => "Run the installer script now? [Y/n]",
        }
    }

    pub fn setup_installer_success(&self) -> &'static str {
        match self {
            Locale::Ko => "설치 스크립트 실행 완료.",
            Locale::En => "Installer script executed successfully.",
        }
    }

    pub fn setup_installer_run_later(&self, path: &Path) -> String {
        match self {
            Locale::Ko => format!("나중에 직접 실행하려면: sh {}", path.display()),
            Locale::En => format!("Run this later with: sh {}", path.display()),
        }
    }

    // ── Doctor ─────────────────────────────────────────────────────────────

    pub fn doctor_token_configured(&self, key: &str) -> String {
        match self {
            Locale::Ko => format!("{key}가 설정되어 있습니다"),
            Locale::En => format!("{key} is configured"),
        }
    }

    pub fn doctor_env_file(&self, path: &str) -> String {
        match self {
            Locale::Ko => format!("환경 파일 경로: {path}"),
            Locale::En => format!("env file path: {path}"),
        }
    }

    pub fn doctor_tmux_ok(&self) -> &'static str {
        match self {
            Locale::Ko => "tmux가 PATH에 있습니다",
            Locale::En => "tmux is available on PATH",
        }
    }

    pub fn doctor_state_db(&self, path: &str) -> String {
        match self {
            Locale::Ko => format!("상태 DB 경로: {path}"),
            Locale::En => format!("state db path: {path}"),
        }
    }

    pub fn doctor_hook_events_dir(&self, path: &str) -> String {
        match self {
            Locale::Ko => format!("훅 이벤트 디렉토리: {path}"),
            Locale::En => format!("hook events dir: {path}"),
        }
    }

    pub fn doctor_manifest(&self, path: &str) -> String {
        match self {
            Locale::Ko => format!("manifest 경로: {path}"),
            Locale::En => format!("manifest path: {path}"),
        }
    }

    pub fn doctor_channel_mapping(&self, path: &str) -> String {
        match self {
            Locale::Ko => format!("채널-프로젝트 매핑: {path}"),
            Locale::En => format!("channel project mapping: {path}"),
        }
    }

    pub fn doctor_failures_header(&self) -> &'static str {
        match self {
            Locale::Ko => "설치가 완료됐지만 아래 항목을 확인해주세요:",
            Locale::En => "Setup completed, but these items still need attention:",
        }
    }

    pub fn doctor_fix_tmux(&self) -> &'static str {
        match self {
            Locale::Ko => "tmux를 설치한 뒤 다시 doctor를 실행하세요.",
            Locale::En => "Install tmux and run doctor again.",
        }
    }

    pub fn doctor_fix_channel_mapping(&self, path: &str) -> String {
        match self {
            Locale::Ko => format!(
                "{path}에 채널-프로젝트 매핑을 추가하세요. Bot 유저를 대상 채널에 초대했는지 확인하세요."
            ),
            Locale::En => format!(
                "Add the channel-project mapping to {path}. Invite the bot user to the target channel before testing thread replies."
            ),
        }
    }

    // ── Service ────────────────────────────────────────────────────────────

    pub fn service_installed(&self, label: &str, plist: &Path, log: &Path) -> String {
        match self {
            Locale::Ko => format!(
                "서비스 등록 완료: {label}\nPlist: {}\n로그: {}",
                plist.display(),
                log.display()
            ),
            Locale::En => format!(
                "Service installed and loaded: {label}\nPlist: {}\nLog:   {}",
                plist.display(),
                log.display()
            ),
        }
    }

    pub fn service_uninstalled(&self, label: &str) -> String {
        match self {
            Locale::Ko => format!("서비스 제거 완료: {label}"),
            Locale::En => format!("Service uninstalled: {label}"),
        }
    }

    pub fn service_not_installed(&self, plist: &Path) -> String {
        match self {
            Locale::Ko => format!("서비스가 설치되어 있지 않습니다 (plist 없음: {})", plist.display()),
            Locale::En => format!("Service is not installed (plist not found: {})", plist.display()),
        }
    }

    pub fn service_removed_path(&self, path: &Path) -> String {
        match self {
            Locale::Ko => format!("삭제됨: {}", path.display()),
            Locale::En => format!("Removed: {}", path.display()),
        }
    }

    pub fn service_removed_path_entry(&self, profile: &Path) -> String {
        match self {
            Locale::Ko => format!("PATH 항목 제거됨: {}", profile.display()),
            Locale::En => format!("Removed PATH entry from: {}", profile.display()),
        }
    }

    pub fn service_uninstall_complete(&self) -> &'static str {
        match self {
            Locale::Ko => "\n제거 완료. 프로젝트 설정 파일(.env.local, data/)은 유지됩니다.",
            Locale::En => "\nUninstall complete. Project config files (.env.local, data/) are kept.",
        }
    }

    pub fn service_binary_not_found(&self, path: &Path) -> String {
        match self {
            Locale::Ko => format!("rcc 바이너리를 찾을 수 없습니다: {}. 먼저 setup을 실행하세요.", path.display()),
            Locale::En => format!("rcc binary not found at {}. Run setup first.", path.display()),
        }
    }

    pub fn service_started(&self, label: &str) -> String {
        match self {
            Locale::Ko => format!("서비스 시작됨: {label}"),
            Locale::En => format!("Service started: {label}"),
        }
    }

    pub fn service_stopped(&self, label: &str) -> String {
        match self {
            Locale::Ko => format!("서비스 중지됨: {label}"),
            Locale::En => format!("Service stopped: {label}"),
        }
    }

    pub fn service_installed_not_running(&self, label: &str) -> String {
        match self {
            Locale::Ko => format!("서비스가 설치됐지만 실행 중이 아닙니다: {label}"),
            Locale::En => format!("Service is installed but not running: {label}"),
        }
    }

    pub fn service_not_installed_hint(&self) -> &'static str {
        match self {
            Locale::Ko => "서비스가 설치되어 있지 않습니다. `rcc service install`을 먼저 실행하세요.",
            Locale::En => "Service is not installed. Run `rcc service install` first.",
        }
    }

    // ── CLI help ───────────────────────────────────────────────────────────

    pub fn help_text(&self) -> &'static str {
        match self {
            Locale::Ko => "사용법: rcc [setup|doctor|service <install|uninstall|start|stop|status>|--help|--version]",
            Locale::En => "Usage: rcc [setup|doctor|service <install|uninstall|start|stop|status>|--help|--version]",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_from_str_parses_korean_variants() {
        assert_eq!("ko".parse::<Locale>().unwrap(), Locale::Ko);
        assert_eq!("2".parse::<Locale>().unwrap(), Locale::Ko);
        assert_eq!("한국어".parse::<Locale>().unwrap(), Locale::Ko);
    }

    #[test]
    fn locale_from_str_defaults_to_english() {
        assert_eq!("1".parse::<Locale>().unwrap(), Locale::En);
        assert_eq!("en".parse::<Locale>().unwrap(), Locale::En);
        assert_eq!("".parse::<Locale>().unwrap(), Locale::En);
    }

    #[test]
    fn locale_from_env_reads_rcc_locale() {
        let previous = std::env::var_os("RCC_LOCALE");
        unsafe { std::env::set_var("RCC_LOCALE", "ko") };
        assert_eq!(Locale::from_env(), Locale::Ko);
        match previous {
            Some(v) => unsafe { std::env::set_var("RCC_LOCALE", v) },
            None => unsafe { std::env::remove_var("RCC_LOCALE") },
        }
    }

    #[test]
    fn service_messages_differ_by_locale() {
        let label = "com.remote-claude-code.rcc";
        let en = Locale::En.service_started(label);
        let ko = Locale::Ko.service_started(label);
        assert_ne!(en, ko);
        assert!(en.contains("started"));
        assert!(ko.contains("시작"));
    }
}
