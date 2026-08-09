//! Launching external programs on a selection.
//!
//! This tool never edits pixels; it selects photographs and hands them to
//! something that does. The safety property that matters here is that
//! **argv is built directly, never a shell string**. These paths come off a
//! drive rather than from the user, so a filename containing a space, a quote
//! or a newline must be incapable of altering a command. `shell = true` exists
//! as an explicit opt-in and is documented as the user's responsibility.

use crate::db::Catalog;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// GUI app: spawn and leave it running, TUI stays live.
    #[default]
    Detach,
    /// Leave the alternate screen, run in the foreground, come back.
    Wait,
    /// Run and capture output into the status area.
    Term,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Action {
    /// Single character that fires it from the TUI.
    pub key: String,
    pub name: String,
    /// argv. Element zero is the program.
    pub cmd: Vec<String>,
    #[serde(default)]
    pub mode: Mode,
    /// Opt in to shell interpretation. Off by default, and off is what makes
    /// hostile filenames inert.
    #[serde(default)]
    pub shell: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct ActionConfig {
    #[serde(default, rename = "action")]
    pub actions: Vec<Action>,
}

/// Defaults, active only where the binary exists. Chosen for a RAW workflow:
/// the RAW goes to a RAW developer, never the extracted preview.
pub fn defaults() -> Vec<Action> {
    vec![
        Action {
            key: "r".into(),
            name: "RawTherapee".into(),
            cmd: vec!["rawtherapee".into(), "{raw}".into()],
            mode: Mode::Detach,
            shell: false,
        },
        Action {
            key: "d".into(),
            name: "darktable".into(),
            cmd: vec!["darktable".into(), "{raw}".into()],
            mode: Mode::Detach,
            shell: false,
        },
        Action {
            key: "g".into(),
            name: "GIMP".into(),
            cmd: vec!["gimp".into(), "{jpg}".into()],
            mode: Mode::Detach,
            shell: false,
        },
        Action {
            key: "o".into(),
            name: "Open".into(),
            cmd: vec!["xdg-open".into(), "{file}".into()],
            mode: Mode::Detach,
            shell: false,
        },
    ]
}

pub fn load() -> Vec<Action> {
    let path = crate::config::Config::path();
    let configured = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| toml::from_str::<ActionConfig>(&t).ok())
        .map(|c| c.actions)
        .unwrap_or_default();

    let list = if configured.is_empty() {
        defaults()
    } else {
        configured
    };
    list.into_iter().filter(|a| on_path(&a.cmd[0])).collect()
}

fn on_path(program: &str) -> bool {
    if program.contains('/') {
        return std::path::Path::new(program).exists();
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(program).is_file())
        })
        .unwrap_or(false)
}

/// What a placeholder expands to, gathered from the catalog.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    pub all_files: Vec<String>,
    pub raws: Vec<String>,
    pub jpgs: Vec<String>,
    /// The stack under the cursor, whatever its best member is.
    pub current: Option<String>,
    pub dirs: Vec<String>,
    pub query: String,
}

pub fn gather(cat: &Catalog, stack_ids: &[i64], current: Option<i64>, query: &str) -> Result<Selection> {
    let mut sel = Selection {
        query: query.to_string(),
        ..Default::default()
    };

    for &sid in stack_ids {
        let (dir,): (String,) = cat.conn.query_row(
            "SELECT dir FROM stack WHERE id = ?1",
            [sid],
            |r| Ok((r.get(0)?,)),
        )?;
        if !sel.dirs.contains(&dir) {
            sel.dirs.push(dir.clone());
        }

        let mut stmt = cat
            .conn
            .prepare("SELECT name, role FROM file WHERE stack_id = ?1 AND present = 1")?;
        let rows = stmt.query_map([sid], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (name, role) = row?;
            let full = format!("{dir}/{name}");
            match role.as_str() {
                "raw" => sel.raws.push(full.clone()),
                "oocjpg" => sel.jpgs.push(full.clone()),
                _ => {}
            }
            // `derived` and `profile` are deliberately excluded from {files}:
            // handing an extracted preview to an editor is never wanted.
            if role == "raw" || role == "oocjpg" {
                sel.all_files.push(full);
            }
        }
    }

    if let Some(sid) = current {
        let best: Option<String> = cat
            .conn
            .query_row(
                "SELECT s.dir || '/' || f.name
                 FROM file f JOIN stack s ON s.id = f.stack_id
                 WHERE f.stack_id = ?1 AND f.present = 1
                 ORDER BY CASE f.role WHEN 'raw' THEN 0 WHEN 'oocjpg' THEN 1 ELSE 2 END
                 LIMIT 1",
                [sid],
                |r| r.get(0),
            )
            .ok();
        sel.current = best;
    }

    Ok(sel)
}

/// Expand placeholders into argv. Each path becomes its **own** element, so no
/// amount of quoting or whitespace inside a filename can split or join
/// arguments.
pub fn build_argv(action: &Action, sel: &Selection) -> Result<Vec<String>> {
    let mut argv: Vec<String> = Vec::new();

    for part in &action.cmd {
        match part.as_str() {
            "{files}" => argv.extend(sel.all_files.iter().cloned()),
            "{raw}" => argv.extend(sel.raws.iter().cloned()),
            "{jpg}" => argv.extend(sel.jpgs.iter().cloned()),
            "{file}" => {
                if let Some(c) = &sel.current {
                    argv.push(c.clone());
                }
            }
            "{dir}" => argv.extend(sel.dirs.iter().cloned()),
            "{query}" => argv.push(sel.query.clone()),
            other => argv.push(other.to_string()),
        }
    }

    if argv.is_empty() {
        bail!("action `{}` expanded to an empty command", action.name);
    }
    // An action whose only argument was a placeholder that matched nothing
    // would silently run the bare program -- e.g. opening RawTherapee with no
    // file. Better to say so.
    if argv.len() == 1 && action.cmd.len() > 1 {
        bail!(
            "action `{}` matched no files in the selection",
            action.name
        );
    }
    Ok(argv)
}

pub struct Outcome {
    pub description: String,
    pub output: Option<String>,
}

/// Run an action. `Wait` and `Term` callers must have left the alternate
/// screen already.
pub fn run(action: &Action, sel: &Selection) -> Result<Outcome> {
    let argv = build_argv(action, sel)?;

    if action.shell {
        // Explicitly opted in; documented as the user's responsibility.
        let joined = argv.join(" ");
        let status = Command::new("sh").arg("-c").arg(&joined).status()?;
        return Ok(Outcome {
            description: format!("{} (shell) -> {status}", action.name),
            output: None,
        });
    }

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);

    match action.mode {
        Mode::Detach => {
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let child = cmd
                .spawn()
                .with_context(|| format!("launching {}", argv[0]))?;
            Ok(Outcome {
                description: format!(
                    "{} launched (pid {}) on {} file(s)",
                    action.name,
                    child.id(),
                    argv.len() - 1
                ),
                output: None,
            })
        }
        Mode::Wait => {
            let status = cmd
                .status()
                .with_context(|| format!("running {}", argv[0]))?;
            Ok(Outcome {
                description: format!("{} finished ({status})", action.name),
                output: None,
            })
        }
        Mode::Term => {
            let out = cmd
                .output()
                .with_context(|| format!("running {}", argv[0]))?;
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            Ok(Outcome {
                description: format!("{} finished", action.name),
                output: Some(text),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(cmd: &[&str]) -> Action {
        Action {
            key: "r".into(),
            name: "test".into(),
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
            mode: Mode::Detach,
            shell: false,
        }
    }

    fn selection(files: &[&str]) -> Selection {
        Selection {
            all_files: files.iter().map(|s| s.to_string()).collect(),
            raws: files.iter().map(|s| s.to_string()).collect(),
            current: files.first().map(|s| s.to_string()),
            ..Default::default()
        }
    }

    /// The property this module exists for. A filename that looks like shell
    /// syntax must arrive as one intact argument.
    #[test]
    fn hostile_filenames_stay_single_arguments() {
        let hostile = r#"/lib/DSC 01'23"; rm -rf x.NEF"#;
        let argv = build_argv(&action(&["rawtherapee", "{raw}"]), &selection(&[hostile])).unwrap();

        assert_eq!(argv.len(), 2);
        assert_eq!(argv[0], "rawtherapee");
        assert_eq!(
            argv[1], hostile,
            "the filename was altered on its way to argv"
        );
    }

    #[test]
    fn a_newline_in_a_filename_does_not_split_arguments() {
        let nasty = "/lib/two\nlines.NEF";
        let argv = build_argv(&action(&["gimp", "{raw}"]), &selection(&[nasty])).unwrap();
        assert_eq!(argv.len(), 2);
        assert_eq!(argv[1], nasty);
    }

    #[test]
    fn each_file_becomes_its_own_argument() {
        let argv = build_argv(
            &action(&["rawtherapee", "{raw}"]),
            &selection(&["/a/1.NEF", "/a/2.NEF", "/a/3.NEF"]),
        )
        .unwrap();
        assert_eq!(argv, vec!["rawtherapee", "/a/1.NEF", "/a/2.NEF", "/a/3.NEF"]);
    }

    #[test]
    fn placeholders_resolve_to_the_right_roles() {
        let sel = Selection {
            all_files: vec!["/a/1.NEF".into(), "/a/1.JPG".into()],
            raws: vec!["/a/1.NEF".into()],
            jpgs: vec!["/a/1.JPG".into()],
            current: Some("/a/1.NEF".into()),
            dirs: vec!["/a".into()],
            query: "beach".into(),
        };
        assert_eq!(build_argv(&action(&["x", "{raw}"]), &sel).unwrap()[1], "/a/1.NEF");
        assert_eq!(build_argv(&action(&["x", "{jpg}"]), &sel).unwrap()[1], "/a/1.JPG");
        assert_eq!(build_argv(&action(&["x", "{dir}"]), &sel).unwrap()[1], "/a");
        assert_eq!(build_argv(&action(&["x", "{query}"]), &sel).unwrap()[1], "beach");
        assert_eq!(build_argv(&action(&["x", "{files}"]), &sel).unwrap().len(), 3);
    }

    /// Literal arguments pass through untouched, so flags still work.
    #[test]
    fn literal_arguments_are_preserved() {
        let argv = build_argv(
            &action(&["convert", "-resize", "2048x", "{jpg}", "out.jpg"]),
            &Selection {
                jpgs: vec!["/a/1.JPG".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(argv, vec!["convert", "-resize", "2048x", "/a/1.JPG", "out.jpg"]);
    }

    /// Silently opening an editor with no file is worse than an error.
    #[test]
    fn an_action_matching_nothing_is_an_error() {
        let err = build_argv(&action(&["rawtherapee", "{raw}"]), &Selection::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("matched no files"));
    }

    #[test]
    fn defaults_never_hand_a_derived_preview_to_an_editor() {
        for a in defaults() {
            assert!(
                !a.cmd.iter().any(|c| c == "{files}"),
                "{} would receive extracted previews",
                a.name
            );
        }
    }

    #[test]
    fn config_parses_the_documented_shape() {
        let cfg: ActionConfig = toml::from_str(
            r#"
            [[action]]
            key = "r"
            name = "RawTherapee"
            cmd = ["rawtherapee", "{raw}"]
            mode = "detach"

            [[action]]
            key = "e"
            name = "Export"
            cmd = ["cp", "-t", "/tmp", "{jpg}"]
            mode = "wait"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.actions.len(), 2);
        assert_eq!(cfg.actions[0].mode, Mode::Detach);
        assert_eq!(cfg.actions[1].mode, Mode::Wait);
        assert!(!cfg.actions[0].shell);
    }
}
