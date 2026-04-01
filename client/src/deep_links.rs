use url::Url;

pub fn extract_invite_token_from_args() -> Option<String> {
    std::env::args()
        .skip(1)
        .find_map(|arg| extract_invite_token(&arg))
}

fn extract_invite_token(raw: &str) -> Option<String> {
    let arg = raw.trim().trim_matches('"');
    let url = Url::parse(arg).ok()?;
    if url.scheme() != "astrix" || url.host_str() != Some("invite") {
        return None;
    }
    if let Some(token) = url.path_segments().and_then(|mut segments| segments.next()) {
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    url.query_pairs()
        .find(|(key, _)| key == "token")
        .map(|(_, value)| value.into_owned())
}

#[cfg(target_os = "windows")]
pub fn register_protocol_handler() {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_WRITE};
    use winreg::RegKey;

    let Ok(exe_path) = std::env::current_exe() else {
        return;
    };
    let exe = exe_path.to_string_lossy().replace('"', "");
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok((classes, _)) = hkcu.create_subkey("Software\\Classes\\astrix") else {
        return;
    };
    let _ = classes.set_value("", &"URL:Astrix Protocol");
    let _ = classes.set_value("URL Protocol", &"");

    if let Ok((icon, _)) = classes.create_subkey("DefaultIcon") {
        let _ = icon.set_value("", &format!("\"{}\",0", exe));
    }
    if let Ok((command, _)) = classes.create_subkey_with_flags(
        "shell\\open\\command",
        KEY_WRITE,
    ) {
        let _ = command.set_value("", &format!("\"{}\" \"%1\"", exe));
    }
}

#[cfg(not(target_os = "windows"))]
pub fn register_protocol_handler() {}
