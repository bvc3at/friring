//! What one WSL distro place is made of, as a pure function of its profile.
//!
//! Names, command lines, the hardened `/etc/wsl.conf` and the parsing of
//! `wsl.exe`'s own output live here; [`super::WslDistroBackend`] is the half
//! that runs something. Nothing in this file spawns a process, so the whole
//! shape of a distro — including the two commands that would destroy one — is
//! assertable without WSL being installed.

use crate::session::SandboxPolicy;

/// Prefix of every distro friring registers.
///
/// The first half of "friring touches only what it created". A distro carries no
/// labels the way a container does, so ownership is the prefix **plus** the
/// marker file [`MARKER_FILE`] inside it, and both are checked again immediately
/// before `wsl --unregister` — which destroys a distro's whole filesystem, so it
/// is the one command in this feature that must never name a stranger's.
///
/// Deliberately its own constant rather than the container backend's: a distro
/// name and a container name live in different registries, and tying them
/// together would mean an edit to one silently renaming the other's places.
pub const DISTRO_PREFIX: &str = "friring-sbx-";

/// The file inside a distro that says friring created it, and for which profile.
///
/// Under `/etc` rather than in the profile's own tree because it has to travel
/// *with* the distro: the question it answers is asked of a distro friring finds
/// on a machine, possibly after the friring that made it is long gone.
pub const MARKER_FILE: &str = "/etc/friring-sandbox";

/// WSL's own per-distro configuration, which the hardening is written to.
pub const WSL_CONF: &str = "/etc/wsl.conf";

/// The hardened `/etc/wsl.conf` every distro friring registers is given.
///
/// Both settings are boundary, not taste:
///
/// - **`automount.enabled = false`.** A distro mounts every Windows drive under
///   `/mnt/` by default, so the machine's whole filesystem — friring's config
///   and data directories among it, which is where the database lives (ADR-29) —
///   would be inside the sandbox before a single profile path was considered.
/// - **`interop.enabled = false`** (and with it `appendWindowsPath`). Interop
///   lets a Linux process `execve` a *Windows* binary, which runs outside the
///   VM, outside the distro and outside every boundary friring set. An agent
///   that can run `powershell.exe` is not sandboxed at all. Turning
///   `appendWindowsPath` off alone would only take those binaries off `PATH`.
///
/// It **replaces** whatever the template carried, rather than merging into it:
/// a boundary made of the settings friring did not find is not one friring can
/// state. The visible consequence is the template's `[user] default` going with
/// it, so a distro friring registers runs as root and `$HOME` is `/root` —
/// which is why the home is read back out of the distro
/// ([`super::WslDistroBackend::ensure_distro`]) instead of assumed. Root inside
/// a distro is not root on Windows; the boundary is the VM and the distro, and
/// bubblewrap inside it is what applies the profile.
pub const WSL_CONF_CONTENTS: &str = "# Written by friring: this distro is a sandbox place.\n\
                                     [automount]\n\
                                     enabled = false\n\
                                     \n\
                                     [interop]\n\
                                     enabled = false\n\
                                     appendWindowsPath = false\n";

/// The distro friring registers for `profile`.
///
/// One distro per profile, which is what makes the place shared by every session
/// that picks the profile (ADR-26). Profile names are already validated to
/// letters, digits, `-`, `_` and `.`, which is inside what WSL accepts as a
/// distro name; the sanitiser is the belt to that braces, because the name also
/// becomes a registry key and a directory.
pub fn distro_name(profile: &str) -> String {
    let cleaned: String = crate::sandbox::dirs::sanitize_component(profile)
        .chars()
        .take(32)
        .collect();
    format!("{DISTRO_PREFIX}{}", cleaned.trim_matches('-'))
}

/// Whether `distro` is one friring could have registered, by name alone.
///
/// The cheap half of the ownership check, and never the whole of it: a user is
/// free to `wsl --import friring-sbx-anything`, so the marker file is what
/// actually decides.
pub fn looks_like_ours(distro: &str) -> bool {
    distro.starts_with(DISTRO_PREFIX) && distro.len() > DISTRO_PREFIX.len()
}

/// One row of `wsl --list --verbose`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistroInfo {
    pub name: String,
    /// `Running`, `Stopped`, … — localised, so it is carried rather than
    /// compared.
    pub state: String,
    /// The WSL version this distro runs as: 2, or 1 for one friring refuses.
    pub version: u32,
    /// Whether this is the host's default distro, which is the template when a
    /// profile names none.
    pub default: bool,
}

/// Decode what `wsl.exe` wrote.
///
/// `wsl.exe` writes its *own* output as UTF-16LE, which reaches friring through
/// a lossy UTF-8 conversion as every ASCII character followed by a NUL, usually
/// behind a byte-order mark. Stripping both is what turns that back into the
/// text every parser here expects. Output that came from a process *inside* a
/// distro is ordinary UTF-8 and passes through untouched, because it carries
/// neither.
///
/// **A non-ASCII character does not survive**, and cannot be recovered here: the
/// bytes are lossily converted before this sees them, so `ü` is already a
/// replacement character. What that costs is bounded and stated rather than
/// hidden — a localised *state*, which nothing compares, and a distro whose
/// **name** carries one, which no lookup then matches. The second refuses a
/// launch naming that template rather than acting on a different distro, which
/// is the safe direction of the two.
pub fn decode(raw: &str) -> String {
    raw.chars()
        .filter(|c| *c != '\0' && *c != '\u{feff}')
        .collect()
}

/// Parse `wsl --list --verbose`.
///
/// Read **from the right**, and deliberately: the header row is localised on a
/// non-English Windows, so `NAME STATE VERSION` cannot be recognised by its
/// headings. The last column is the version, the one before it the state, and
/// everything between the default marker and those two is the name. A row whose
/// last column is not a number is not a distro row — which is how the header,
/// localised or not, drops out without friring knowing a word of it.
pub fn parse_list(raw: &str) -> Vec<DistroInfo> {
    let mut out = Vec::new();
    for line in decode(raw).lines() {
        let line = line.trim_end();
        let (default, rest) = match line.trim_start().strip_prefix('*') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        let fields = split_columns(rest);
        if fields.len() < 3 {
            continue;
        }
        let Ok(version) = fields[fields.len() - 1].parse::<u32>() else {
            continue;
        };
        let name = fields[..fields.len() - 2].join(" ");
        if name.is_empty() {
            continue;
        }
        out.push(DistroInfo {
            name,
            state: fields[fields.len() - 2].to_string(),
            version,
            default,
        });
    }
    out
}

/// Split one listed row into its columns.
///
/// On runs of **two or more** spaces, because both columns before the version
/// can carry a single one: a distro name may (`wsl --import` takes any name),
/// and a localised state does — a German Windows prints `Wird ausgeführt`.
/// Splitting on whitespace would read either as two columns and hand back a
/// name no lookup could match. A row that is not padded into columns at all
/// falls back to whitespace, which is the best guess left.
fn split_columns(row: &str) -> Vec<&str> {
    let columns: Vec<&str> = row
        .split("  ")
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .collect();
    if columns.len() >= 3 {
        columns
    } else {
        row.split_whitespace().collect()
    }
}

/// `wsl --export <template> <file> --format vhd`.
///
/// A VHD rather than a tar: it is what `--import --vhd` takes back in one copy,
/// where a tar is unpacked file by file into a fresh ext4 image — minutes
/// against seconds for a distro with a toolchain in it.
pub fn export_argv<'a>(template: &'a str, file: &'a str) -> Vec<&'a str> {
    vec!["--export", template, file, "--format", "vhd"]
}

/// `wsl --import <name> <install dir> <file> --vhd --version 2`.
///
/// `--version 2` is passed rather than inherited: WSL1 has no utility VM, no
/// namespaces and a translated filesystem, so a host whose default version is 1
/// would otherwise register a distro that cannot be a boundary at all.
pub fn import_argv<'a>(distro: &'a str, install_dir: &'a str, file: &'a str) -> Vec<&'a str> {
    vec![
        "--import",
        distro,
        install_dir,
        file,
        "--vhd",
        "--version",
        "2",
    ]
}

/// The `sh -c` script that hardens a freshly imported distro and marks it as
/// friring's, as one command so a half-written distro cannot exist.
///
/// `set -e` is what makes that true: a marker written after a failed
/// `/etc/wsl.conf` would claim a distro friring did not finish hardening, and
/// the next ensure would adopt it with the Windows filesystem still mounted
/// inside.
pub fn harden_script(profile: &str) -> String {
    let quote = crate::shell::posix_quote;
    format!(
        "set -e\nprintf '%s' {conf} > {conf_path}\nprintf '%s' {marker} > {marker_path}\nchmod \
         0644 {conf_path} {marker_path}\n",
        conf = quote(WSL_CONF_CONTENTS),
        conf_path = quote(WSL_CONF),
        marker = quote(profile),
        marker_path = quote(MARKER_FILE),
    )
}

/// The profile paths that name the Windows side of the machine rather than the
/// distro's own filesystem.
///
/// Warned about rather than refused, because the profile is not *wrong* — it is
/// slow and weaker. Two costs, both worth a sentence: DrvFs pays a 10–100×
/// metadata penalty, which a repository-walking agent feels on every command;
/// and a Windows path is outside the distro's filesystem, so the boundary around
/// it is the VM's rather than the distro's. The hardened template mounts no
/// Windows drive at all, so such a path is also simply *not there* — which is
/// the half the message has to say, or the user reads a warning and gets a dead
/// pane.
pub fn windows_side_paths(policy: &SandboxPolicy) -> Vec<String> {
    policy
        .rw_paths
        .iter()
        .chain(policy.ro_paths.iter())
        .filter(|path| is_windows_side(path))
        .map(|path| {
            format!(
                "'{path}' is on the Windows filesystem. A distro friring registers mounts no \
                 Windows drive — automount is off, which is what keeps the host's filesystem out \
                 of the sandbox — so that path is not there at all, and DrvFs would cost 10–100× \
                 on metadata if it were. Keep the repository on the distro's own ext4"
            )
        })
        .collect()
}

/// Whether `path` names the Windows side: a DrvFs mount point (`/mnt/c/…`) or a
/// path written in Windows' own spelling (`C:\…`, a UNC share).
fn is_windows_side(path: &str) -> bool {
    if path.starts_with("\\\\") || path.contains(":\\") {
        return true;
    }
    let Some(rest) = path.strip_prefix("/mnt/") else {
        return false;
    };
    let drive = rest.split('/').next().unwrap_or_default();
    drive.len() == 1 && drive.chars().all(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SandboxBackendKind, SandboxPath, SandboxProfile};

    /// The bytes `wsl.exe` really writes: UTF-16LE behind a byte-order mark,
    /// which a lossy UTF-8 read turns into this.
    fn as_utf16(text: &str) -> String {
        let mut bytes: Vec<u8> = vec![0xff, 0xfe];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn a_distro_is_named_after_its_profile_and_marked_as_friring_s() {
        assert_eq!(distro_name("dev"), "friring-sbx-dev");
        assert_eq!(distro_name("my.box_1"), "friring-sbx-my.box_1");
        assert!(looks_like_ours(&distro_name("dev")));
        // A stranger's distro is not, and neither is the bare prefix.
        assert!(!looks_like_ours("Ubuntu"));
        assert!(!looks_like_ours(DISTRO_PREFIX));
    }

    /// The output really is UTF-16, the header really is localised, and a
    /// distro name really can carry a space — so the parse is anchored on the
    /// two columns that are always numbers and words at the end of the row.
    #[test]
    fn the_distro_list_is_parsed_from_the_right_through_utf16() {
        let raw = as_utf16(
            "  NAME                   STATE           VERSION\n\
             * Ubuntu-24.04           Running         2\n\
             \x20 friring-sbx-dev        Stopped         2\n\
             \x20 Old Distro Name        Stopped         1\n",
        );
        let listed = parse_list(&raw);
        assert_eq!(listed.len(), 3, "{listed:?}");
        assert_eq!(listed[0].name, "Ubuntu-24.04");
        assert!(listed[0].default);
        assert_eq!(listed[0].version, 2);
        assert_eq!(listed[1].name, "friring-sbx-dev");
        assert!(!listed[1].default);
        // A name with spaces survives, and WSL1 is reported rather than hidden.
        assert_eq!(listed[2].name, "Old Distro Name");
        assert_eq!(listed[2].version, 1);

        // A localised header drops out for the same reason an English one does:
        // its last column is a word. And a localised *state* carries a space —
        // German prints `Wird ausgeführt` — so the columns are what the row is
        // split on, not the whitespace inside them. Its `ü` is a replacement
        // character by the time this runs, which is the documented cost of the
        // lossy conversion and is why nothing compares a state.
        let german =
            as_utf16("  NAME            STATUS          VERSION\n* Ubuntu  Wird ausgeführt  2\n");
        let listed = parse_list(&german);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].name, "Ubuntu");
        assert!(listed[0].state.starts_with("Wird ausgef"), "{listed:?}");
        assert_eq!(listed[0].version, 2);

        // And nothing at all is nothing, rather than a row of empty strings.
        assert!(parse_list("").is_empty());
        assert!(parse_list(&as_utf16("\r\n")).is_empty());
    }

    #[test]
    fn the_lifecycle_command_lines_are_the_ones_wsl_documents() {
        assert_eq!(
            export_argv("Ubuntu", "C:\\data\\dev\\template.vhdx"),
            [
                "--export",
                "Ubuntu",
                "C:\\data\\dev\\template.vhdx",
                "--format",
                "vhd"
            ]
        );
        assert_eq!(
            import_argv("friring-sbx-dev", "C:\\data\\dev\\distro", "C:\\t.vhdx"),
            [
                "--import",
                "friring-sbx-dev",
                "C:\\data\\dev\\distro",
                "C:\\t.vhdx",
                "--vhd",
                "--version",
                "2",
            ]
        );
    }

    /// The hardening is the boundary: a distro that mounts the Windows drives
    /// has friring's own database inside it (ADR-29), and one with interop on
    /// can `execve` a Windows binary that runs outside the VM entirely.
    #[test]
    fn the_hardened_template_mounts_no_windows_drive_and_runs_no_windows_binary() {
        for setting in [
            "[automount]",
            "enabled = false",
            "[interop]",
            "appendWindowsPath = false",
        ] {
            assert!(WSL_CONF_CONTENTS.contains(setting), "missing {setting}");
        }
        let script = harden_script("dev");
        assert!(script.starts_with("set -e\n"), "{script}");
        assert!(script.contains(WSL_CONF), "{script}");
        assert!(script.contains(MARKER_FILE), "{script}");
        // The profile name reaches a shell, so it is quoted where it lands.
        assert!(harden_script("a b'c").contains("'a b'\\''c'"));
    }

    #[test]
    fn a_windows_side_path_is_warned_about_rather_than_silently_accepted() {
        let profile = SandboxProfile::new(
            "dev",
            vec![
                SandboxPath::workspace("/mnt/c/Users/me/repo"),
                SandboxPath::read_only("/home/u/dev/lib"),
            ],
        );
        let policy = profile
            .resolve(SandboxBackendKind::WslDistro, "/home/u")
            .unwrap();
        let warnings = windows_side_paths(&policy);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("/mnt/c/Users/me/repo"), "{warnings:?}");
        assert!(warnings[0].contains("automount is off"), "{warnings:?}");

        // The spellings, and the ones that only look like them: `/mnt/wsl` is
        // WSL's own tree and `/mnt/data` is an ordinary Linux mount point.
        for path in [
            "/mnt/c/x",
            "/mnt/D/x",
            "C:\\Users\\me",
            "\\\\wsl.localhost\\Ubuntu",
        ] {
            assert!(is_windows_side(path), "{path} is on the Windows side");
        }
        for path in ["/mnt/wsl/x", "/mnt/data/repo", "/home/u/mnt/c", "/mnt"] {
            assert!(!is_windows_side(path), "{path} is not");
        }
    }
}
