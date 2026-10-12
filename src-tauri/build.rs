use std::{
    env,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

fn valid_dns(value: &str) -> bool {
    let name = value.strip_suffix('.').unwrap_or(value);
    !name.is_empty()
        && name.len() <= 253
        && name.parse::<IpAddr>().is_err()
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
}

fn validate_exclusions() {
    for (name, valid) in [
        ("FLEET_NAS_DNS", valid_dns as fn(&str) -> bool),
        ("FLEET_STUDIO_DNS", valid_dns),
        ("FLEET_NAS_IPV4", |value| value.parse::<Ipv4Addr>().is_ok()),
        ("FLEET_NAS_IPV6", |value| value.parse::<Ipv6Addr>().is_ok()),
    ] {
        println!("cargo:rerun-if-env-changed={name}");
        match env::var(name) {
            Ok(value) => assert!(valid(&value), "Invalid compile-time {name}"),
            Err(env::VarError::NotPresent) => {}
            Err(env::VarError::NotUnicode(_)) => panic!("Invalid compile-time {name}: not Unicode"),
        }
    }
}

fn main() {
    validate_exclusions();
    const COMMANDS: &[&str] = &[
        "app_info",
        "list_hosts",
        "save_host",
        "delete_host",
        "discover",
        "allocate_connection",
        "connect",
        "terminal_input",
        "terminal_resize",
        "terminal_disconnect",
        "answer_challenge",
        "acknowledge_output",
        "list_sessions",
        "check_session",
        "choose_backup_directory",
        "export_backup",
        "choose_restore",
        "commit_restore",
        "cancel_restore",
        "diagnostics_preview",
        "export_diagnostics",
        "open_link",
        "choose_setup_checkout",
        "prepare_host_setup",
        "smoke_result",
    ];
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("Fleet native build configuration is invalid");
}
