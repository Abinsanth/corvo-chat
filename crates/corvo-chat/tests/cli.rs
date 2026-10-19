use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_corvo-chat")).args(args).output().expect("the binary runs")
}

#[test]
fn version_flag_prints_name_and_version() {
    let out = run(&["--version"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "corvo-chat 0.1.0");
}

#[test]
fn help_says_what_it_is() {
    let out = run(&["--help"]);
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("End-to-end encrypted chat in your terminal"), "{help}");
    assert!(help.contains("--version"), "{help}");
    assert!(!help.contains("--recover"), "recovery is a sign-in step now: {help}");
}

#[test]
fn help_lists_update_and_hides_uninstall() {
    let out = run(&["--help"]);
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--update"), "{help}");
    assert!(!help.contains("--uninstall"), "{help}");
}
