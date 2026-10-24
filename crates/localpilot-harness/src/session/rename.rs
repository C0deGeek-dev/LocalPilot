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
    pub declaration_row: Option<usize>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AdmissionHint {
    Exact {
        old: String,
        new: String,
        regions: usize,
    },
    DeclarationFirst {
        pair: RenamePair,
        regions: usize,
    },
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
    let mut declaration_row = None;
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
            let declaration = old.parent().is_some_and(|parent| {
                parent.kind() == "function_definition"
                    && parent.child_by_field_name("name") == Some(old)
            });
            if declaration {
                declares_function = true;
                declaration_row.get_or_insert(old.start_position().row);
            }
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
        declaration_row,
    })
}

/// A refusal hint, never an applied patch or checkpoint. Uniform name changes
/// preserve lines, so separated changed-line groups are the actual diff hunks.
pub(super) fn first_bounded_edit(
    before: &str,
    after: &str,
    expected: Option<&RenamePair>,
    max_regions: usize,
) -> Option<AdmissionHint> {
    let evidence = rename_evidence(before, after)?;
    if before.contains('\r') || expected.is_some_and(|pair| pair != &evidence.pair) {
        return None;
    }
    let old: Vec<_> = before.lines().collect();
    let new: Vec<_> = after.lines().collect();
    if old.len() != new.len() {
        return None;
    }
    let mut previous_changed = false;
    let mut regions = 0;
    let mut first = None;
    for (row, (old, new)) in old.iter().zip(&new).enumerate() {
        let changed = old != new;
        if changed {
            first.get_or_insert(row);
            regions += usize::from(!previous_changed);
        }
        previous_changed = changed;
    }
    if regions <= max_regions {
        return None;
    }
    if expected.is_none() && !evidence.declares_function {
        return Some(AdmissionHint::DeclarationFirst {
            pair: evidence.pair,
            regions,
        });
    }
    let row = if expected.is_none() {
        evidence.declaration_row?
    } else {
        first?
    };
    let old = *old.get(row)?;
    let new = *new.get(row)?;
    if old == new || old.is_empty() || before.matches(old).count() != 1 {
        return None;
    }
    Some(AdmissionHint::Exact {
        old: old.to_owned(),
        new: new.to_owned(),
        regions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oversized_uniform_rename_suggests_one_unique_declaration_edit() {
        let before =
            "def old_name(x):\n    return x\n\n\ndef doubled(x):\n    return old_name(x) * 2\n";
        let after = before.replace("old_name", "new_name");
        assert_eq!(
            first_bounded_edit(before, &after, None, 1),
            Some(AdmissionHint::Exact {
                old: "def old_name(x):".into(),
                new: "def new_name(x):".into(),
                regions: 2
            })
        );
        assert!(first_bounded_edit(before, &after, None, 2).is_none());
        assert!(first_bounded_edit(before, &after.replace("* 2", "* 3"), None, 1).is_none());
        let callers = "from money import old_name\n\nvalue = old_name(3)\n";
        let changed = callers.replace("old_name", "new_name");
        let pair = RenamePair {
            old: "old_name".into(),
            new: "new_name".into(),
        };
        assert_eq!(
            first_bounded_edit(callers, &changed, None, 1),
            Some(AdmissionHint::DeclarationFirst {
                pair: pair.clone(),
                regions: 2
            })
        );
        assert_eq!(
            first_bounded_edit(callers, &changed, Some(&pair), 1),
            Some(AdmissionHint::Exact {
                old: "from money import old_name".into(),
                new: "from money import new_name".into(),
                regions: 2,
            })
        );
        assert!(first_bounded_edit(
            callers,
            &callers.replace("old_name", "other_name"),
            Some(&pair),
            1
        )
        .is_none());
        let ambiguous = format!("{before}\n{before}");
        assert!(first_bounded_edit(
            &ambiguous,
            &ambiguous.replace("old_name", "new_name"),
            None,
            1
        )
        .is_none());
    }

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
                    declares_function: source.starts_with("def "),
                    declaration_row: source.starts_with("def ").then_some(0),
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
