//! Where Overnight sends people for help: its documentation, its source, and
//! a new GitHub issue. Only these fixed addresses are ever opened, never a URL
//! the popover page hands over.

use std::{process::Command, thread};

pub const DOCS: &str = "https://dienp.github.io/overnight/";
pub const REPO: &str = "https://github.com/dienp/overnight";

/// Opens the documentation, at one of its sections when given one.
pub fn docs(section: Option<&str>) {
    open(docs_url(section));
}

pub fn repo() {
    open(REPO.to_string());
}

/// A release's page on GitHub, for what changed in it.
pub fn release(version: &str) {
    if version.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
        open(format!("{REPO}/releases/tag/v{version}"));
    }
}

/// A new GitHub issue with the app and macOS versions filled in and nothing
/// else: no session titles, projects or logs leave the Mac this way.
pub fn report_problem(app_version: &str) {
    open(issue_url(app_version, &macos_version()));
}

/// In the default browser, without holding up the caller.
fn open(url: String) {
    thread::spawn(move || {
        let _ = Command::new("/usr/bin/open").arg(url).status();
    });
}

/// A section is an anchor on the docs page; anything else opens the top.
fn docs_url(section: Option<&str>) -> String {
    let anchor = section.filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    match anchor {
        Some(s) => format!("{DOCS}#{s}"),
        None => DOCS.to_string(),
    }
}

fn issue_url(app_version: &str, macos: &str) -> String {
    let body = format!("What happened?\n\n\nWhat did you expect instead?\n\n\nOvernight {app_version}, {macos}\n");
    format!("{REPO}/issues/new?body={}", encode(&body))
}

/// "macOS Version 26.0 (Build 25A354)".
fn macos_version() -> String {
    let v = objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersionString();
    format!("macOS {v}")
}

/// Percent-encodes everything but the unreserved characters.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docs_open_only_their_own_anchors() {
        assert_eq!(docs_url(Some("reset")), "https://dienp.github.io/overnight/#reset");
        assert_eq!(docs_url(Some("lid-lock")), "https://dienp.github.io/overnight/#lid-lock");
        assert_eq!(docs_url(None), DOCS);
        assert_eq!(docs_url(Some("")), DOCS);
        assert_eq!(docs_url(Some("x\" onload=\"y")), DOCS);
        assert_eq!(docs_url(Some("../../evil")), DOCS);
    }

    #[test]
    fn issue_carries_versions_and_nothing_else() {
        let url = issue_url("0.1.0", "macOS Version 26.0 (Build 25A354)");
        assert!(url.starts_with("https://github.com/dienp/overnight/issues/new?body=What%20happened%3F%0A"));
        assert!(url.ends_with("Overnight%200.1.0%2C%20macOS%20Version%2026.0%20%28Build%2025A354%29%0A"));
        assert!(!url.contains(' ') && !url.contains('\n'));
    }

    #[test]
    fn reads_the_macos_version() {
        assert!(macos_version().starts_with("macOS Version "));
    }
}
