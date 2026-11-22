//! Desktop entries (`.desktop` files): just enough of the freedesktop spec
//! to read a key and to point every `Exec=` at `eep run`.

/// The value of `key` in `[group]` (exact key: no locale variants).
pub fn get(text: &str, group: &str, key: &str) -> Option<String> {
    let mut in_group = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_group = t == format!("[{group}]");
            continue;
        }
        if in_group
            && let Some((k, v)) = t.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim().to_owned());
        }
    }
    None
}

/// An `Exec=` value split into the parts eep needs.
#[derive(Debug, PartialEq, Eq)]
pub struct Exec {
    /// `VAR=value` pairs from a leading `env` (as in `Exec=env A=1 prog`).
    pub env: Vec<String>,
    /// The program, unquoted: a bare name or an absolute path.
    pub program: String,
    /// Everything after the program, verbatim (field codes included).
    pub rest: String,
}

/// Split an `Exec=` value. `None` when it has no program.
pub fn split_exec(value: &str) -> Option<Exec> {
    let mut rest = value.trim_start();
    let mut env = Vec::new();
    let mut program = next_token(&mut rest)?;
    if program == "env" || program.ends_with("/env") {
        program = next_token(&mut rest)?;
        while program.contains('=') && !program.starts_with('-') {
            env.push(program);
            program = next_token(&mut rest)?;
        }
    }
    Some(Exec {
        env,
        program,
        rest: rest.trim().to_owned(),
    })
}

/// Take one token: either `"quoted \"text\""` or up to the next space.
fn next_token(s: &mut &str) -> Option<String> {
    let t = s.trim_start();
    if t.is_empty() {
        return None;
    }
    if let Some(inner) = t.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = inner.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => out.push(chars.next()?.1),
                '"' => {
                    *s = &inner[i + 1..];
                    return Some(out);
                }
                _ => out.push(c),
            }
        }
        return None; // unterminated quote
    }
    let end = t.find(char::is_whitespace).unwrap_or(t.len());
    *s = &t[end..];
    Some(t[..end].to_owned())
}

/// Quote one argument for an `Exec=` line, only when needed.
pub fn quote_arg(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/._-+=:,@".contains(&c));
    if plain {
        return arg.to_owned();
    }
    let mut q = String::from("\"");
    for c in arg.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            // `\\` at the exec level, then doubled again at the file level.
            q.push_str("\\\\");
        }
        q.push(c);
    }
    q.push('"');
    q
}

/// Rewrite a desktop file for eep:
/// - every `Exec=` (all groups, actions included) goes through `exec`
/// - `[Desktop Entry]` gets `Icon=<icon>` (when given) and `X-Eep-Id=<id>`
/// - `TryExec` and `DBusActivatable` are dropped (they would bypass eep)
pub fn rewrite(text: &str, id: &str, icon: Option<&str>, exec: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len() + 128);
    let mut group = String::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            group = t.to_owned();
            out.push_str(t);
            out.push('\n');
            if group == "[Desktop Entry]" {
                out.push_str(&format!("X-Eep-Id={id}\n"));
                if let Some(i) = icon {
                    out.push_str(&format!("Icon={i}\n"));
                }
            }
            continue;
        }
        let key = t.split_once('=').map(|(k, _)| k.trim());
        match key {
            Some("Exec") => {
                let v = t.split_once('=').map_or("", |(_, v)| v.trim());
                out.push_str(&format!("Exec={}\n", exec(v)));
            }
            Some("TryExec" | "DBusActivatable" | "X-Eep-Id") => {}
            Some("Icon") if group == "[Desktop Entry]" && icon.is_some() => {}
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FF: &str = "[Desktop Entry]
Actions=new-private-window;new-window
Exec=firefox --name firefox %U
Icon=firefox
Name=Firefox
TryExec=firefox
Type=Application

[Desktop Action new-private-window]
Exec=firefox --private-window %U
Name=New Private Window
";

    #[test]
    fn reads_keys() {
        assert_eq!(get(FF, "Desktop Entry", "Icon").as_deref(), Some("firefox"));
        assert_eq!(get(FF, "Desktop Entry", "Nope"), None);
        assert_eq!(
            get(FF, "Desktop Action new-private-window", "Exec").as_deref(),
            Some("firefox --private-window %U")
        );
    }

    #[test]
    fn splits_exec() {
        let e = split_exec("firefox --name firefox %U").expect("exec");
        assert_eq!(
            (e.program.as_str(), e.rest.as_str()),
            ("firefox", "--name firefox %U")
        );
        let e = split_exec(r#""/nix/store/x y/bin/a b" %F"#).expect("exec");
        assert_eq!(
            (e.program.as_str(), e.rest.as_str()),
            ("/nix/store/x y/bin/a b", "%F")
        );
        let e = split_exec("env GDK_BACKEND=x11 FOO=1 gimp %U").expect("exec");
        assert_eq!(e.env, vec!["GDK_BACKEND=x11", "FOO=1"]);
        assert_eq!(e.program, "gimp");
        assert!(split_exec("   ").is_none());
        assert!(split_exec("\"unterminated").is_none());
    }

    #[test]
    fn rewrites_every_exec() {
        let out = rewrite(
            FF,
            "firefox",
            Some("/home/u/.local/share/eep/icons/firefox.png"),
            |v| format!("eep run firefox -- {v}"),
        );
        assert_eq!(out.matches("Exec=eep run firefox -- firefox").count(), 2);
        assert!(!out.contains("TryExec"));
        assert!(out.contains("X-Eep-Id=firefox\n"));
        assert_eq!(out.matches("Icon=").count(), 1);
        assert!(out.contains("Icon=/home/u/.local/share/eep/icons/firefox.png"));
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_arg("/usr/bin/eep"), "/usr/bin/eep");
        assert_eq!(quote_arg("/home/a b/eep"), "\"/home/a b/eep\"");
        assert_eq!(quote_arg("a$b"), "\"a\\\\$b\"");
    }
}
