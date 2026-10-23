//! Bounded syntactic evidence for continuing a uniform Python name change.
//! This establishes a change shape, never binding correctness or completion.

use std::{
    ops::ControlFlow,
    time::{Duration, Instant},
};
use tree_sitter::{ParseOptions, Parser, Tree};

pub(super) const SOURCE_CAP: usize = 64 * 1024;
const NODE_CAP: usize = 16_384;
const NAME_CAP: usize = 256;
const PARSE_LIMIT: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RenamePair {
    pub old: String,
    pub new: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct RenameEvidence {
    pub pair: RenamePair,
    pub declares_function: bool,
}

fn identifier(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes.len() <= NAME_CAP
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn parse(parser: &mut Parser, source: &[u8]) -> Option<Tree> {
    parser.reset();
    let start = Instant::now();
    let mut progress = |_: &tree_sitter::ParseState| {
        if start.elapsed() >= PARSE_LIMIT {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let tree = parser.parse_with_options(
        &mut |offset, _| source.get(offset..).unwrap_or_default(),
        None,
        Some(ParseOptions::new().progress_callback(&mut progress)),
    )?;
    (!tree.root_node().has_error() && start.elapsed() < PARSE_LIMIT).then_some(tree)
}

/// Accept only identically structured source with one consistent identifier
/// substitution and byte-identical remaining material, including whitespace.
pub(super) fn rename_evidence(before: &str, after: &str) -> Option<RenameEvidence> {
    if before == after
        || before.len() > SOURCE_CAP
        || after.len() > SOURCE_CAP
        || before.contains('\u{fffd}')
        || after.contains('\u{fffd}')
    {
        return None;
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .ok()?;
    let old_tree = parse(&mut parser, before.as_bytes())?;
    let new_tree = parse(&mut parser, after.as_bytes())?;
    let mut pending = vec![(old_tree.root_node(), new_tree.root_node())];
    let mut count = 0;
    let mut ends = (0, 0);
    let mut pair: Option<RenamePair> = None;
    let mut declares_function = false;
    while let Some((old, new)) = pending.pop() {
        count += 1;
        if count > NODE_CAP
            || old.is_missing()
            || new.is_missing()
            || old.kind_id() != new.kind_id()
            || old.child_count() != new.child_count()
        {
            return None;
        }
        if old.child_count() > 0 {
            if count + pending.len() + old.child_count() > NODE_CAP {
                return None;
            }
            for i in (0..old.child_count()).rev() {
                let index = u32::try_from(i).ok()?;
                pending.push((old.child(index)?, new.child(index)?));
            }
            continue;
        }
        let old_bytes = before.as_bytes().get(old.byte_range())?;
        let new_bytes = after.as_bytes().get(new.byte_range())?;
        if before.as_bytes().get(ends.0..old.start_byte())?
            != after.as_bytes().get(ends.1..new.start_byte())?
        {
            return None;
        }
        if old_bytes != new_bytes {
            if old.kind() != "identifier" || !identifier(old_bytes) || !identifier(new_bytes) {
                return None;
            }
            let found = RenamePair {
                old: std::str::from_utf8(old_bytes).ok()?.to_owned(),
                new: std::str::from_utf8(new_bytes).ok()?.to_owned(),
            };
            if pair.as_ref().is_some_and(|expected| expected != &found) {
                return None;
            }
            declares_function |= old.parent().is_some_and(|parent| {
                parent.kind() == "function_definition"
                    && parent.child_by_field_name("name") == Some(old)
            });
            pair = Some(found);
        }
        ends = (old.end_byte(), new.end_byte());
    }
    if before.as_bytes().get(ends.0..)? != after.as_bytes().get(ends.1..)? {
        return None;
    }
    pair.map(|pair| RenameEvidence {
        pair,
        declares_function,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_code_identifiers_including_interpolated_calls() {
        for source in [
            "def old_name(x):\n    \"\"\"Description.\"\"\"\n    return x\n",
            "from money import old_name\n",
            "value = f\"total={old_name(items)}\"\n",
            "value = old_name(items) + old_name(other)\n",
        ] {
            assert_eq!(
                rename_evidence(source, &source.replace("old_name", "new_name")),
                Some(RenameEvidence {
                    pair: RenamePair {
                        old: "old_name".into(),
                        new: "new_name".into()
                    },
                    declares_function: source.starts_with("def ")
                })
            );
        }
    }

    #[test]
    fn declines_other_material_and_ambiguous_or_invalid_names() {
        for (old, new) in [
            ("# old_name\nx = 1\n", "# new_name\nx = 1\n"),
            ("x = 'old_name'\n", "x = 'new_name'\n"),
            ("x = old_name(1)\n", "x = new_name(2)\n"),
            ("x = old_name(1)\n", "x = new_name (1)\n"),
            (
                "x = old_name(1) # old_name\n",
                "x = new_name(1) # new_name\n",
            ),
            ("x = old_name(a)\n", "x = new_name(b)\n"),
            ("x = old_name(1)\n", "x = new_name(\n"),
            ("x = old_name(1)\n", "x = naïve(1)\n"),
            ("x = 1\n", "x = 1\n"),
            ("x = 1\n", "x = 2\n"),
        ] {
            assert!(rename_evidence(old, new).is_none(), "{old:?} -> {new:?}");
        }
        let huge = format!("{}x = old_name(1)\n", "# padding\n".repeat(SOURCE_CAP));
        assert!(rename_evidence(&huge, &huge.replace("old_name", "new_name")).is_none());
        let many = "x = old_name(1)\n".repeat(3000);
        assert!(rename_evidence(&many, &many.replace("old_name", "new_name")).is_none());
    }
}
