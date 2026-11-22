//! The environment a Nix-built app gets from `eep run`.
//!
//! Host variables that point a process at Arch's libraries or plugins are
//! removed (a Nix app loading Arch's GTK/Qt/GStreamer modules crashes on
//! ABI mismatches); a few are set so the app finds ewe's fonts, cursors,
//! locales and the GL driver. Change a rule only with evidence, and write
//! the reason next to it (PROTOTYPE.md, Step 2).

use std::ffi::{OsStr, OsString};
use std::path::Path;

/// Variables removed outright, with the reason.
pub const UNSET: &[(&str, &str)] = &[
    (
        "LD_LIBRARY_PATH",
        "Arch libraries inside a Nix app crash it",
    ),
    ("LD_PRELOAD", "Arch libraries inside a Nix app crash it"),
    ("GTK_PATH", "Arch GTK modules: wrong ABI"),
    (
        "GTK_MODULES",
        "names Arch GTK modules the Nix GTK can't load",
    ),
    (
        "GTK3_MODULES",
        "names Arch GTK modules the Nix GTK can't load",
    ),
    ("GIO_EXTRA_MODULES", "Arch GIO modules: wrong ABI"),
    ("GDK_PIXBUF_MODULE_FILE", "Arch pixbuf loaders: wrong ABI"),
    ("GI_TYPELIB_PATH", "Arch typelibs describe Arch's libraries"),
    (
        "GSETTINGS_SCHEMA_DIR",
        "Arch schemas may not match the app's",
    ),
    ("QT_PLUGIN_PATH", "Arch Qt plugins: wrong ABI"),
    ("QT_QPA_PLATFORM_PLUGIN_PATH", "Arch Qt plugins: wrong ABI"),
    ("QML2_IMPORT_PATH", "Arch QML modules: wrong ABI"),
    ("QML_IMPORT_PATH", "Arch QML modules: wrong ABI"),
    ("GST_PLUGIN_PATH", "Arch GStreamer plugins: wrong ABI"),
    ("GST_PLUGIN_PATH_1_0", "Arch GStreamer plugins: wrong ABI"),
    (
        "GST_PLUGIN_SYSTEM_PATH",
        "Arch GStreamer plugins: wrong ABI",
    ),
    (
        "GST_PLUGIN_SYSTEM_PATH_1_0",
        "Arch GStreamer plugins: wrong ABI",
    ),
    ("GST_PLUGIN_SCANNER", "Arch GStreamer scanner: wrong ABI"),
    (
        "GST_PLUGIN_SCANNER_1_0",
        "Arch GStreamer scanner: wrong ABI",
    ),
    ("PYTHONPATH", "host Python modules"),
    ("PYTHONHOME", "host Python"),
    ("PERL5LIB", "host Perl modules"),
    ("PERLLIB", "host Perl modules"),
    (
        "VK_ICD_FILENAMES",
        "Nix finds Vulkan drivers in /run/opengl-driver",
    ),
    (
        "VK_DRIVER_FILES",
        "Nix finds Vulkan drivers in /run/opengl-driver",
    ),
    (
        "VK_ADD_DRIVER_FILES",
        "Nix finds Vulkan drivers in /run/opengl-driver",
    ),
    (
        "__EGL_VENDOR_LIBRARY_DIRS",
        "Nix finds EGL vendors in /run/opengl-driver",
    ),
    (
        "__EGL_VENDOR_LIBRARY_FILENAMES",
        "Nix finds EGL vendors in /run/opengl-driver",
    ),
    (
        "LIBGL_DRIVERS_PATH",
        "Nix finds GL drivers in /run/opengl-driver",
    ),
    (
        "LIBVA_DRIVERS_PATH",
        "Nix finds VA-API drivers in /run/opengl-driver",
    ),
    (
        "VDPAU_DRIVER_PATH",
        "Nix finds VDPAU drivers in /run/opengl-driver",
    ),
    (
        "GBM_BACKENDS_PATH",
        "Nix finds GBM backends in /run/opengl-driver",
    ),
];

/// Qt platform themes ewe sets that Nix's Qt doesn't ship (Gotcha 7).
const FOREIGN_QT_THEMES: &[&str] = &["qt6ct", "qt5ct", "kde"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Unset {
        var: &'static str,
        why: &'static str,
    },
    Set {
        var: &'static str,
        value: OsString,
        why: &'static str,
    },
}

/// What `eep run` changes, given the current environment.
pub fn plan(
    get: impl Fn(&str) -> Option<OsString>,
    home: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Vec<Op> {
    let mut ops: Vec<Op> = UNSET
        .iter()
        .filter(|(v, _)| get(v).is_some())
        .map(|&(var, why)| Op::Unset { var, why })
        .collect();

    // Cursors: Nix's libXcursor doesn't look in /usr/share/icons by itself.
    let mut cursor = OsString::new();
    for p in [home.join(".local/share/icons"), home.join(".icons")] {
        cursor.push(p.as_os_str());
        cursor.push(":");
    }
    cursor.push("/usr/share/icons:/usr/share/pixmaps");
    if let Some(old) = get("XCURSOR_PATH").filter(|v| !v.is_empty()) {
        cursor = join_unique(&old, &cursor);
    }
    ops.push(Op::Set {
        var: "XCURSOR_PATH",
        value: cursor,
        why: "find ewe's cursor theme",
    });

    if exists(Path::new("/etc/fonts/fonts.conf")) && get("FONTCONFIG_FILE").is_none() {
        ops.push(Op::Set {
            var: "FONTCONFIG_FILE",
            value: "/etc/fonts/fonts.conf".into(),
            why: "use ewe's fonts and font settings",
        });
    }
    if exists(Path::new("/usr/lib/locale/locale-archive")) && get("LOCALE_ARCHIVE").is_none() {
        ops.push(Op::Set {
            var: "LOCALE_ARCHIVE",
            value: "/usr/lib/locale/locale-archive".into(),
            why: "the system's locales (to verify against nixpkgs glibcLocales)",
        });
    }
    ops.push(Op::Set {
        var: "NIXOS_OZONE_WL",
        value: "1".into(),
        why: "Electron apps run native Wayland",
    });
    if get("QT_QPA_PLATFORMTHEME")
        .is_some_and(|t| FOREIGN_QT_THEMES.iter().any(|f| t == OsStr::new(f)))
    {
        ops.push(Op::Set {
            var: "QT_QPA_PLATFORMTHEME",
            value: "xdgdesktopportal".into(),
            why: "Nix Qt has no qt6ct; the portal theme follows dark mode",
        });
    }
    ops
}

/// `old` followed by the parts of `new` not already in it (paths are
/// treated as text; a non-UTF-8 search path would be unusual).
fn join_unique(old: &OsStr, new: &OsStr) -> OsString {
    let old = old.to_string_lossy();
    let new = new.to_string_lossy();
    let mut parts: Vec<&str> = old.split(':').collect();
    for p in new.split(':') {
        if !parts.contains(&p) {
            parts.push(p);
        }
    }
    parts.join(":").into()
}

pub fn apply(cmd: &mut std::process::Command, ops: &[Op]) {
    for op in ops {
        match op {
            Op::Unset { var, .. } => {
                cmd.env_remove(var);
            }
            Op::Set { var, value, .. } => {
                cmd.env(var, value);
            }
        }
    }
}

pub fn to_json(ops: &[Op]) -> serde_json::Value {
    serde_json::Value::Array(
        ops.iter()
            .map(|op| match op {
                Op::Unset { var, why } => serde_json::json!({ "unset": var, "why": why }),
                Op::Set { var, value, why } => {
                    serde_json::json!({ "set": var, "value": value.to_string_lossy(), "why": why })
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn run(vars: &[(&str, &str)]) -> Vec<Op> {
        let m: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).into()))
            .collect();
        plan(|k| m.get(k).cloned(), Path::new("/home/u"), |_| true)
    }

    fn set<'a>(ops: &'a [Op], var: &str) -> Option<&'a OsString> {
        ops.iter().find_map(|o| match o {
            Op::Set { var: v, value, .. } if *v == var => Some(value),
            _ => None,
        })
    }

    #[test]
    fn clears_host_plugins() {
        let ops = run(&[
            ("LD_LIBRARY_PATH", "/usr/lib"),
            ("QT_PLUGIN_PATH", "/usr/lib/qt6/plugins"),
            ("HOME", "/home/u"),
        ]);
        let unset: Vec<&str> = ops
            .iter()
            .filter_map(|o| {
                if let Op::Unset { var, .. } = o {
                    Some(*var)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(unset, vec!["LD_LIBRARY_PATH", "QT_PLUGIN_PATH"]);
    }

    #[test]
    fn sets_cursor_fonts_locale() {
        let ops = run(&[]);
        assert_eq!(
            set(&ops, "XCURSOR_PATH")
                .map(|v| v.to_string_lossy().into_owned())
                .as_deref(),
            Some("/home/u/.local/share/icons:/home/u/.icons:/usr/share/icons:/usr/share/pixmaps")
        );
        assert!(set(&ops, "FONTCONFIG_FILE").is_some());
        assert!(set(&ops, "LOCALE_ARCHIVE").is_some());
        assert_eq!(
            set(&ops, "NIXOS_OZONE_WL")
                .map(|v| v.to_string_lossy().into_owned())
                .as_deref(),
            Some("1")
        );
        assert!(set(&ops, "QT_QPA_PLATFORMTHEME").is_none());
    }

    #[test]
    fn keeps_user_cursor_path_first() {
        let ops = run(&[("XCURSOR_PATH", "/opt/cursors:/usr/share/icons")]);
        assert_eq!(
            set(&ops, "XCURSOR_PATH")
                .map(|v| v.to_string_lossy().into_owned())
                .as_deref(),
            Some(
                "/opt/cursors:/usr/share/icons:/home/u/.local/share/icons:/home/u/.icons:/usr/share/pixmaps"
            )
        );
    }

    #[test]
    fn qt6ct_becomes_portal() {
        let ops = run(&[("QT_QPA_PLATFORMTHEME", "qt6ct")]);
        assert_eq!(
            set(&ops, "QT_QPA_PLATFORMTHEME")
                .map(|v| v.to_string_lossy().into_owned())
                .as_deref(),
            Some("xdgdesktopportal")
        );
        let ops = run(&[("QT_QPA_PLATFORMTHEME", "gtk3")]);
        assert!(set(&ops, "QT_QPA_PLATFORMTHEME").is_none());
    }
}
