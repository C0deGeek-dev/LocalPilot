//! Edits to the user's own config file, made the same way by every LocalPilot
//! command that writes it: under one exclusive lock, keeping everything it does
//! not change byte for byte (comments included), refusing when the file changed
//! while it was being edited, and confirming the result by reading it back.

use std::path::Path;

/// What happened to the user config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigEdit {
    /// It already said this; nothing was written.
    Unchanged,
    /// It was replaced and reads back exactly as written.
    Written,
}

/// Why the config edit did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigError {
    /// Nothing was replaced.
    Before(String),
    /// The file was replaced, but reading it back failed or showed something
    /// else; its current content is not confirmed.
    After(String),
}

/// Edit the user config at `path` under the config lock with `edit`, which
/// changes the parsed document in place. `before_replace` is asked last, right
/// before the file is replaced: an error there replaces nothing. `after_replace`
/// runs right after the replace (tests use it to stand in for another writer). An editor outside
/// LocalPilot that writes between the last check and the replace is not
/// detected.
pub(crate) fn edit_user_config(
    path: Option<&Path>,
    before_replace: &dyn Fn() -> Result<(), String>,
    after_replace: &dyn Fn(&Path),
    edit: &dyn Fn(&mut toml_edit::DocumentMut) -> Result<(), String>,
) -> Result<ConfigEdit, ConfigError> {
    let before_ = |message: String| ConfigError::Before(message);
    let path =
        path.ok_or_else(|| before_("there is no user config location on this system".into()))?;
    let directory = path
        .parent()
        .ok_or_else(|| before_("the config path has no folder".into()))?;
    std::fs::create_dir_all(directory).map_err(|error| before_(error.to_string()))?;
    let lock_path = path.with_extension("toml.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|error| before_(format!("could not open {}: {error}", lock_path.display())))?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _guard = lock
        .try_write()
        .map_err(|_| before_("another LocalPilot command is writing the config".into()))?;

    let before = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(before_(format!(
                "could not read {}: {error}",
                path.display()
            )))
        }
    };
    let text =
        String::from_utf8(before.clone()).map_err(|_| before_("the config is not UTF-8".into()))?;
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|error| before_(format!("the config is not valid TOML ({error})")))?;
    edit(&mut document).map_err(before_)?;
    let after = document.to_string();
    if after.as_bytes() == before.as_slice() {
        return Ok(ConfigEdit::Unchanged);
    }
    // Replace only what was read.
    let now = std::fs::read(path).unwrap_or_default();
    if now != before {
        return Err(before_(
            "the config changed while it was being edited".into(),
        ));
    }
    // A private, uniquely named file beside the config (owner-only where the
    // platform has modes), removed again on any failure, so neither a
    // predictable sibling nor a wider-readable copy of the config is left.
    let mut staged =
        tempfile::NamedTempFile::new_in(directory).map_err(|error| before_(error.to_string()))?;
    {
        use std::io::Write as _;
        staged
            .write_all(after.as_bytes())
            .and_then(|()| staged.flush())
            .and_then(|()| staged.as_file().sync_all())
            .map_err(|error| before_(error.to_string()))?;
    }
    before_replace().map_err(before_)?;
    staged
        .persist(path)
        .map_err(|error| before_(error.error.to_string()))?;
    after_replace(path);
    let written = std::fs::read(path)
        .map_err(|error| ConfigError::After(format!("reading it back failed ({error})")))?;
    if written != after.as_bytes() {
        return Err(ConfigError::After(
            "it reads back differently, so something else changed it right after".into(),
        ));
    }
    Ok(ConfigEdit::Written)
}

/// The table at `path` (for example `["search", "managed"]`), created as an
/// implicit table where missing, so only the tables that hold values get a
/// header of their own.
pub(crate) fn table_at<'a>(
    document: &'a mut toml_edit::DocumentMut,
    path: &[&str],
) -> Result<&'a mut toml_edit::Table, String> {
    let mut table = document.as_table_mut();
    for (depth, key) in path.iter().enumerate() {
        let item = table.entry(key).or_insert_with(|| {
            let mut fresh = toml_edit::Table::new();
            fresh.set_implicit(true);
            toml_edit::Item::Table(fresh)
        });
        table = item.as_table_mut().ok_or_else(|| {
            format!(
                "[{}] in the config is not a table",
                path[..=depth].join(".")
            )
        })?;
    }
    Ok(table)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_keeps_comments_and_unrelated_keys_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# mine\n[other]\nkeep = 1 # note\n").unwrap();
        let outcome = edit_user_config(Some(&path), &|| Ok(()), &|_| {}, &|document| {
            table_at(document, &["search", "managed", "ddg"])?["tool"] = toml_edit::value("search");
            Ok(())
        })
        .unwrap();
        assert_eq!(outcome, ConfigEdit::Written);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# mine\n[other]\nkeep = 1 # note\n"),
            "{text}"
        );
        assert!(
            text.contains("[search.managed.ddg]\ntool = \"search\""),
            "{text}"
        );
        assert!(!text.contains("[search]\n"), "implicit parents: {text}");
    }

    #[cfg(unix)]
    #[test]
    fn a_private_config_stays_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[other]\nkeep = 1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        edit_user_config(Some(&path), &|| Ok(()), &|_| {}, &|document| {
            table_at(document, &["search"])?["x"] = toml_edit::value(1);
            Ok(())
        })
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != "config.toml" && name != "config.toml.lock")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_failed_replace_leaves_no_copy_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        // The config's folder entry is a directory, so nothing can replace it.
        let blocked = dir.path().join("blocked.toml");
        std::fs::create_dir_all(&blocked).unwrap();
        let error = edit_user_config(Some(&blocked), &|| Ok(()), &|_| {}, &|document| {
            document["b"] = toml_edit::value(2);
            Ok(())
        })
        .unwrap_err();
        assert!(matches!(error, ConfigError::Before(_)), "{error:?}");
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|name| name == "config.toml"
                || name == "blocked.toml"
                || name == "blocked.toml.lock"),
            "{names:?}"
        );
    }

    #[test]
    fn a_stop_right_before_the_replace_replaces_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        let error = edit_user_config(
            Some(&path),
            &|| Err("stopped".to_string()),
            &|_| {},
            &|document| {
                document["b"] = toml_edit::value(2);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error, ConfigError::Before("stopped".into()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a = 1\n");
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .all(|name| name == "config.toml" || name == "config.toml.lock"),
            "{names:?}"
        );
    }

    #[test]
    fn a_refused_edit_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "search = 1\n").unwrap();
        let error = edit_user_config(Some(&path), &|| Ok(()), &|_| {}, &|document| {
            table_at(document, &["search", "managed"]).map(|_| ())
        })
        .unwrap_err();
        assert_eq!(
            error,
            ConfigError::Before("[search] in the config is not a table".into())
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "search = 1\n");
    }
}
