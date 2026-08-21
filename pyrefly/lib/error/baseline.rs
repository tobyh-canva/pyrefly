/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use pyrefly_config::config::BaselineField;
#[cfg(test)]
use pyrefly_config::config::DEFAULT_BASELINE_FIELDS;
use pyrefly_util::absolutize::Absolutize;
use pyrefly_util::fs_anyhow;

use crate::error::error::Error;
use crate::error::legacy::BaselineError;
use crate::error::legacy::BaselineErrors;

/// Paths in match keys are absolute so comparison does not depend on the baseline file's
/// path format.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BaselineKey(Vec<BaselineValue>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum BaselineValue {
    Missing,
    Number(usize),
    Text(String),
}

impl BaselineValue {
    fn number(value: Option<usize>) -> Self {
        value.map_or(Self::Missing, Self::Number)
    }

    fn text(value: Option<String>) -> Self {
        value.map_or(Self::Missing, Self::Text)
    }
}

/// Normalize a path to an absolute, forward-slash string.
pub(crate) fn normalize_baseline_path(path: &Path, relative_to: &Path) -> String {
    path.absolutize_from(relative_to)
        .to_string_lossy()
        .replace('\\', "/")
}

impl BaselineKey {
    fn from_baseline_error(
        error: &BaselineError,
        relative_to: &Path,
        baseline_fields: &[BaselineField],
    ) -> Self {
        Self(
            baseline_fields
                .iter()
                .map(|field| match field {
                    BaselineField::Line => BaselineValue::number(error.line),
                    BaselineField::Column => BaselineValue::number(error.column),
                    BaselineField::Path => BaselineValue::text(
                        error
                            .path
                            .as_deref()
                            .map(|path| normalize_baseline_path(Path::new(path), relative_to)),
                    ),
                    BaselineField::Name => BaselineValue::text(error.name.clone()),
                    BaselineField::ConciseDescription => {
                        BaselineValue::text(error.concise_description.clone())
                    }
                    BaselineField::Severity => BaselineValue::text(error.severity.clone()),
                    BaselineField::Cell => BaselineValue::number(error.cell),
                })
                .collect(),
        )
    }

    fn from_error(error: &Error, baseline_fields: &[BaselineField]) -> Self {
        let error_range = error.display_range();
        Self(
            baseline_fields
                .iter()
                .map(|field| match field {
                    BaselineField::Line => {
                        BaselineValue::Number(error_range.start.line_within_cell().get() as usize)
                    }
                    BaselineField::Column => {
                        BaselineValue::Number(error_range.start.column().get() as usize)
                    }
                    BaselineField::Path => BaselineValue::Text(
                        error.path().as_path().to_string_lossy().replace('\\', "/"),
                    ),
                    BaselineField::Name => {
                        BaselineValue::Text(error.error_kind().to_name().to_owned())
                    }
                    BaselineField::ConciseDescription => {
                        BaselineValue::Text(error.msg_header().to_owned())
                    }
                    BaselineField::Severity => {
                        BaselineValue::Text(crate::error::legacy::severity_to_str(error.severity()))
                    }
                    BaselineField::Cell => BaselineValue::number(
                        error_range.start.cell().map(|cell| cell.get() as usize),
                    ),
                })
                .collect(),
        )
    }
}

/// A lightweight, keys-only baseline matcher for the language server.
pub struct BaselineProcessor {
    baseline_fields: Vec<BaselineField>,
    baseline_keys: HashSet<BaselineKey>,
}

impl BaselineProcessor {
    /// Load a baseline file. `relative_to` is the base directory that was used
    /// when the baseline was written (i.e. the resolved `--relative-to` value),
    /// so that relative paths in the file are resolved correctly.
    pub fn from_file(
        baseline_path: &Path,
        relative_to: &Path,
        baseline_fields: &[BaselineField],
    ) -> Result<Self> {
        let content = fs_anyhow::read_to_string(baseline_path)?;
        let baseline_file: BaselineErrors = serde_json::from_str(&content)?;
        Ok(Self::from_baseline_errors(
            baseline_file,
            relative_to,
            baseline_fields,
        ))
    }

    fn from_baseline_errors(
        baseline_errors: BaselineErrors,
        relative_to: &Path,
        baseline_fields: &[BaselineField],
    ) -> Self {
        Self {
            baseline_fields: baseline_fields.to_owned(),
            baseline_keys: baseline_errors
                .errors
                .iter()
                .map(|error| BaselineKey::from_baseline_error(error, relative_to, baseline_fields))
                .collect(),
        }
    }

    pub fn matches_baseline(&self, error: &Error) -> bool {
        self.baseline_keys
            .contains(&BaselineKey::from_error(error, &self.baseline_fields))
    }

    /// Baseline suppressions are processed last, after inline and config suppressions.
    pub fn process_errors(&self, shown_errors: &mut Vec<Error>, baseline_errors: &mut Vec<Error>) {
        let (matched, remaining) = shown_errors
            .drain(..)
            .partition(|error| self.matches_baseline(error));
        baseline_errors.extend(matched);
        *shown_errors = remaining;
    }
}

/// The result of classifying unmatched baseline entries after a CLI check.
pub struct BaselinePruningResult {
    pub unused_entry_count: usize,
    pub retained_entries: Vec<BaselineError>,
}

fn is_definitely_unused(
    matched: bool,
    checked: bool,
    try_exists: impl FnOnce() -> std::io::Result<bool>,
) -> bool {
    !matched && (checked || matches!(try_exists(), Ok(false)))
}

/// A baseline matcher that also retains rows and tracks matches for CLI maintenance actions.
pub struct TrackedBaselineProcessor {
    entries: Vec<BaselineError>,
    keys: HashMap<BaselineKey, bool>,
    baseline_fields: Vec<BaselineField>,
    relative_to: PathBuf,
}

impl TrackedBaselineProcessor {
    pub fn from_file(
        baseline_path: &Path,
        relative_to: &Path,
        baseline_fields: &[BaselineField],
    ) -> Result<Self> {
        let content = fs_anyhow::read_to_string(baseline_path)?;
        let baseline_file: BaselineErrors = serde_json::from_str(&content)?;
        Ok(Self::from_baseline_errors(
            baseline_file,
            relative_to,
            baseline_fields,
        ))
    }

    fn from_baseline_errors(
        baseline_errors: BaselineErrors,
        relative_to: &Path,
        baseline_fields: &[BaselineField],
    ) -> Self {
        let mut entries = baseline_errors.errors;
        for entry in &mut entries {
            entry.retain_fields(baseline_fields);
        }
        let keys = entries
            .iter()
            .map(|error| {
                (
                    BaselineKey::from_baseline_error(error, relative_to, baseline_fields),
                    false,
                )
            })
            .collect();
        Self {
            entries,
            keys,
            baseline_fields: baseline_fields.to_owned(),
            relative_to: relative_to.to_owned(),
        }
    }

    /// Baseline suppressions are processed last, after inline and config suppressions.
    pub fn process_errors(
        &mut self,
        shown_errors: &mut Vec<Error>,
        baseline_errors: &mut Vec<Error>,
    ) {
        let mut remaining_errors = Vec::new();

        for error in shown_errors.drain(..) {
            if let Some(matched) = self
                .keys
                .get_mut(&BaselineKey::from_error(&error, &self.baseline_fields))
            {
                *matched = true;
                baseline_errors.push(error);
            } else {
                remaining_errors.push(error);
            }
        }

        *shown_errors = remaining_errors;
    }

    /// Classify unmatched rows conservatively using the scope of the current check.
    ///
    /// An unmatched row is unused only when its file was checked, or when the file
    /// is conclusively absent. Existing unchecked files and filesystem errors are
    /// retained. Duplicate rows sharing a key are classified individually.
    pub fn into_pruning_result(self, checked_paths: &HashSet<String>) -> BaselinePruningResult {
        let mut unused_entry_count = 0;
        let retained_entries = self
            .entries
            .into_iter()
            .filter_map(|entry| {
                let key = BaselineKey::from_baseline_error(
                    &entry,
                    &self.relative_to,
                    &self.baseline_fields,
                );
                let matched = self.keys[&key];
                let path = entry
                    .path
                    .as_deref()
                    .map(|path| normalize_baseline_path(Path::new(path), &self.relative_to));
                let definitely_unused = path.as_ref().is_some_and(|path| {
                    is_definitely_unused(matched, checked_paths.contains(path), || {
                        Path::new(path).try_exists()
                    })
                });
                if definitely_unused {
                    unused_entry_count += 1;
                    None
                } else {
                    Some(entry)
                }
            })
            .collect();
        BaselinePruningResult {
            unused_entry_count,
            retained_entries,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use pyrefly_python::module::Module;
    use pyrefly_python::module_name::ModuleName;
    use pyrefly_python::module_path::ModulePath;
    use ruff_text_size::TextRange;
    use ruff_text_size::TextSize;

    use super::*;
    use crate::config::error_kind::ErrorKind;

    #[test]
    fn test_definitely_unused_is_conservative_about_io_errors() {
        assert!(is_definitely_unused(false, true, || {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "not consulted for checked paths",
            ))
        }));
        assert!(is_definitely_unused(false, false, || Ok(false)));
        assert!(!is_definitely_unused(false, false, || Ok(true)));
        assert!(!is_definitely_unused(false, false, || {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "inconclusive",
            ))
        }));
        assert!(!is_definitely_unused(true, true, || Ok(false)));
    }

    #[test]
    fn test_baseline_key_generation() {
        let module = Module::new(
            ModuleName::from_str("test_module"),
            ModulePath::filesystem(PathBuf::from("/workspace/test/path.py")),
            Arc::new("test content".to_owned()),
        );

        let error = Error::new(
            module,
            TextRange::new(TextSize::new(0), TextSize::new(5)),
            "Test error message".to_owned(),
            Vec::new(),
            ErrorKind::BadReturn,
        );

        let key = BaselineKey::from_error(&error, DEFAULT_BASELINE_FIELDS);

        assert_eq!(
            key,
            BaselineKey(vec![
                BaselineValue::Text("/workspace/test/path.py".to_owned()),
                BaselineValue::Text("bad-return".to_owned()),
                BaselineValue::Number(1),
            ])
        );
    }

    #[test]
    fn test_baseline_matching() {
        let baseline_json = r#"
        {
            "errors": [
                {
                    "line": 1,
                    "column": 3,
                    "stop_line": 1,
                    "stop_column": 5,
                    "path": "/workspace/test.py",
                    "code": -2,
                    "name": "bad-return",
                    "description": "Test error",
                    "concise_description": "Test error"
                }
            ]
        }
        "#;

        let baseline_file: BaselineErrors = serde_json::from_str(baseline_json).unwrap();
        let processor = BaselineProcessor::from_baseline_errors(
            baseline_file,
            Path::new("/workspace"),
            DEFAULT_BASELINE_FIELDS,
        );

        let module = Module::new(
            ModuleName::from_str("test_module"),
            ModulePath::filesystem(PathBuf::from("/workspace/test.py")),
            Arc::new("test content 123456789".to_owned()),
        );
        let module2 = Module::new(
            ModuleName::from_str("test_module2"),
            ModulePath::filesystem(PathBuf::from("/workspace/test2.py")),
            Arc::new("test content 123456789".to_owned()),
        );

        // This error should match (same path, error code, and column)
        let error1 = Error::new(
            module.clone(),
            TextRange::new(TextSize::new(2), TextSize::new(5)),
            "Any error message".to_owned(),
            Vec::new(),
            ErrorKind::BadReturn,
        );
        assert!(processor.matches_baseline(&error1));

        // This error should not match (different column)
        let error2 = Error::new(
            module.clone(),
            TextRange::new(TextSize::new(4), TextSize::new(5)),
            "Test error".to_owned(),
            Vec::new(),
            ErrorKind::BadReturn,
        );
        assert!(!processor.matches_baseline(&error2));

        // This error should not match (different error code)
        let error3 = Error::new(
            module,
            TextRange::new(TextSize::new(2), TextSize::new(5)),
            "Any error message".to_owned(),
            Vec::new(),
            ErrorKind::AssertType,
        );
        assert!(!processor.matches_baseline(&error3));

        // This error should not match (different module)
        let error4 = Error::new(
            module2.clone(),
            TextRange::new(TextSize::new(2), TextSize::new(5)),
            "Any error message".to_owned(),
            Vec::new(),
            ErrorKind::BadReturn,
        );
        assert!(!processor.matches_baseline(&error4));
    }

    #[test]
    fn test_unused_entry_count() {
        let baseline_json = serde_json::json!({
            "errors": [
                {
                    "line": 1, "column": 3, "stop_line": 1, "stop_column": 5,
                    "path": "/workspace/test.py",
                    "code": -2, "name": "bad-return",
                    "description": "test", "concise_description": "test"
                },
                {
                    "line": 7, "column": 3, "stop_line": 7, "stop_column": 5,
                    "path": "/workspace/gone.py",
                    "code": -2, "name": "bad-return",
                    "description": "test", "concise_description": "test"
                }
            ]
        });
        let baseline_file: BaselineErrors = serde_json::from_value(baseline_json).unwrap();
        let mut processor = TrackedBaselineProcessor::from_baseline_errors(
            baseline_file,
            Path::new("/workspace"),
            DEFAULT_BASELINE_FIELDS,
        );

        let module = Module::new(
            ModuleName::from_str("test_module"),
            ModulePath::filesystem(PathBuf::from("/workspace/test.py")),
            Arc::new("test content 123456789".to_owned()),
        );
        let mut shown_errors = vec![Error::new(
            module,
            TextRange::new(TextSize::new(2), TextSize::new(5)),
            "Any error message".to_owned(),
            Vec::new(),
            ErrorKind::BadReturn,
        )];
        let mut baseline_errors = Vec::new();
        processor.process_errors(&mut shown_errors, &mut baseline_errors);

        assert!(shown_errors.is_empty());
        assert_eq!(baseline_errors.len(), 1);
        // The checked `test.py` entry matched, while the absent `gone.py` entry is stale.
        let result =
            processor.into_pruning_result(&HashSet::from(["/workspace/test.py".to_owned()]));
        assert_eq!(result.unused_entry_count, 1);
    }

    #[test]
    fn test_duplicate_entries_are_counted_and_retained_individually() {
        // The same key appears twice for both `test.py` and `gone.py`, so the
        // baseline holds four raw rows across two unique keys.
        let baseline_json = serde_json::json!({
            "errors": [
                {
                    "line": 1, "column": 3, "stop_line": 1, "stop_column": 5,
                    "path": "/workspace/test.py",
                    "code": -2, "name": "bad-return",
                    "description": "first", "concise_description": "first"
                },
                {
                    "line": 1, "column": 3, "stop_line": 1, "stop_column": 5,
                    "path": "/workspace/test.py",
                    "code": -2, "name": "bad-return",
                    "description": "second", "concise_description": "second"
                },
                {
                    "line": 7, "column": 3, "stop_line": 7, "stop_column": 5,
                    "path": "/workspace/gone.py",
                    "code": -2, "name": "bad-return",
                    "description": "gone-a", "concise_description": "gone-a"
                },
                {
                    "line": 7, "column": 3, "stop_line": 7, "stop_column": 5,
                    "path": "/workspace/gone.py",
                    "code": -2, "name": "bad-return",
                    "description": "gone-b", "concise_description": "gone-b"
                }
            ]
        });
        let baseline_file: BaselineErrors = serde_json::from_value(baseline_json).unwrap();
        let mut processor = TrackedBaselineProcessor::from_baseline_errors(
            baseline_file,
            Path::new("/workspace"),
            DEFAULT_BASELINE_FIELDS,
        );

        let module = Module::new(
            ModuleName::from_str("test_module"),
            ModulePath::filesystem(PathBuf::from("/workspace/test.py")),
            Arc::new("test content 123456789".to_owned()),
        );
        let mut shown_errors = vec![Error::new(
            module,
            TextRange::new(TextSize::new(2), TextSize::new(5)),
            "Any error message".to_owned(),
            Vec::new(),
            ErrorKind::BadReturn,
        )];
        let mut baseline_errors = Vec::new();
        processor.process_errors(&mut shown_errors, &mut baseline_errors);

        let result =
            processor.into_pruning_result(&HashSet::from(["/workspace/test.py".to_owned()]));

        // Both `gone.py` rows are unused even though they share a single key, so
        // the count reflects raw rows rather than unique keys.
        assert_eq!(result.unused_entry_count, 2);

        // The surviving entries are the two `test.py` rows, returned in file
        // order rather than as a single deduplicated key.
        assert_eq!(result.retained_entries.len(), 2);
        assert!(
            result
                .retained_entries
                .iter()
                .all(|e| e.path.as_deref() == Some("/workspace/test.py"))
        );
    }

    #[test]
    fn test_configured_baseline_fields_control_matching() {
        let baseline_file: BaselineErrors = serde_json::from_value(serde_json::json!({
            "errors": [{
                "line": 2,
                "name": "bad-return",
                "concise_description": "Test error"
            }]
        }))
        .unwrap();
        let baseline_fields = [
            BaselineField::Line,
            BaselineField::Name,
            BaselineField::ConciseDescription,
        ];
        let processor = BaselineProcessor::from_baseline_errors(
            baseline_file,
            Path::new("/workspace"),
            &baseline_fields,
        );
        let module = Module::new(
            ModuleName::from_str("test_module"),
            ModulePath::filesystem(PathBuf::from("/workspace/test.py")),
            Arc::new("first\nsecond".to_owned()),
        );

        let matching = Error::new(
            module.clone(),
            TextRange::new(TextSize::new(7), TextSize::new(8)),
            "Test error".to_owned(),
            Vec::new(),
            ErrorKind::BadReturn,
        );
        assert!(processor.matches_baseline(&matching));

        let wrong_line = Error::new(
            module,
            TextRange::new(TextSize::new(0), TextSize::new(1)),
            "Test error".to_owned(),
            Vec::new(),
            ErrorKind::BadReturn,
        );
        assert!(!processor.matches_baseline(&wrong_line));
    }

    /// Check that an error matches a baseline entry regardless of how the path is stored.
    fn assert_baseline_path_matches(baseline_path: &str) {
        let cwd = std::env::current_dir().unwrap();
        let abs_path = cwd.join("src/foo.py");

        let baseline_json = serde_json::json!({
            "errors": [{
                "line": 1, "column": 5, "stop_line": 1, "stop_column": 10,
                "path": baseline_path,
                "code": -2, "name": "bad-return",
                "description": "test", "concise_description": "test"
            }]
        });

        let baseline_file: BaselineErrors = serde_json::from_value(baseline_json).unwrap();
        let processor =
            BaselineProcessor::from_baseline_errors(baseline_file, &cwd, DEFAULT_BASELINE_FIELDS);

        let module = Module::new(
            ModuleName::from_str("foo"),
            ModulePath::filesystem(abs_path),
            Arc::new("test content 123456789".to_owned()),
        );
        let error = Error::new(
            module,
            TextRange::new(TextSize::new(4), TextSize::new(10)),
            "err".to_owned(),
            Vec::new(),
            ErrorKind::BadReturn,
        );
        assert!(processor.matches_baseline(&error));
    }

    #[test]
    fn test_baseline_matches_absolute_path() {
        let cwd = std::env::current_dir().unwrap();
        let abs_path = cwd.join("src/foo.py");
        assert_baseline_path_matches(&abs_path.to_string_lossy());
    }

    #[test]
    fn test_baseline_matches_relative_path() {
        assert_baseline_path_matches("src/foo.py");
    }

    /// Verify that backslash paths (Windows) match forward-slash baseline entries.
    #[test]
    fn test_baseline_matches_backslash_error_path() {
        let baseline_json = serde_json::json!({
            "errors": [{
                "line": 1, "column": 5, "stop_line": 1, "stop_column": 10,
                "path": "/workspace/src/foo.py",
                "code": -2, "name": "bad-return",
                "description": "test", "concise_description": "test"
            }]
        });

        let baseline_file: BaselineErrors = serde_json::from_value(baseline_json).unwrap();
        let processor = BaselineProcessor::from_baseline_errors(
            baseline_file,
            Path::new("/workspace"),
            DEFAULT_BASELINE_FIELDS,
        );

        // Simulate a Windows-style path with backslashes in the error.
        let module = Module::new(
            ModuleName::from_str("foo"),
            ModulePath::filesystem(PathBuf::from(r"\workspace\src\foo.py")),
            Arc::new("test content 123456789".to_owned()),
        );
        let error = Error::new(
            module,
            TextRange::new(TextSize::new(4), TextSize::new(10)),
            "err".to_owned(),
            Vec::new(),
            ErrorKind::BadReturn,
        );
        assert!(processor.matches_baseline(&error));
    }

    #[test]
    fn test_baseline_matches_with_non_cwd_relative_to() {
        let cwd = std::env::current_dir().unwrap();
        let abs_path = cwd.join("src/foo.py");
        let relative_to = cwd.join("src");

        let baseline_json = serde_json::json!({
            "errors": [{
                "line": 1, "column": 5, "stop_line": 1, "stop_column": 10,
                "path": "foo.py",
                "code": -2, "name": "bad-return",
                "description": "test", "concise_description": "test"
            }]
        });
        let baseline_file: BaselineErrors = serde_json::from_value(baseline_json).unwrap();
        let processor = BaselineProcessor::from_baseline_errors(
            baseline_file,
            &relative_to,
            DEFAULT_BASELINE_FIELDS,
        );

        let module = Module::new(
            ModuleName::from_str("foo"),
            ModulePath::filesystem(abs_path),
            Arc::new("test content 123456789".to_owned()),
        );
        let error = Error::new(
            module,
            TextRange::new(TextSize::new(4), TextSize::new(10)),
            "err".to_owned(),
            Vec::new(),
            ErrorKind::BadReturn,
        );
        assert!(processor.matches_baseline(&error));
    }
}
